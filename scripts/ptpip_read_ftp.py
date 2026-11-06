#!/usr/bin/env python3
"""PTP/IP client: open a session with a Nikon camera and read one FTP profile (read-only by default).

By default it only reads: it sends OpenSession, 0x90E8 (setup), 0x90EB (info) and 0x90ED(slot), and
nothing else.  The one exception is the EXPERIMENTAL write mode (--write FILE), which also sends
GetDeviceInfo (the model gate) and 0x90EE, replacing or adding a stored profile with the bytes in
FILE; it is a dry run (nothing written) unless --send is given.

Experimental research tool (see docs/nx-field-ftp-profile-session-2026-07-10.md).  Not part of the
nikon-fleet binary; Python 3 standard library only.

It presents NX Field's handshake identity: a 16-byte GUID of the form
`01 00 4e 58` + 6 zero bytes + 6 arbitrary bytes, device name "@@", protocol 1.0, and transaction id
1 on every request (what NX Field sends).  The camera refuses GUIDs without the `01 00 4e 58` prefix
and does not check the name.  Note that a new connection ends any session already open (NX Field on a
phone is dropped), and that the camera must be in its connect-to-smart-device state or it resets the
TCP connection to port 15740.

READ.  The slot must be below the stored profile count that 0x90EB reports: the script refuses
anything else before sending 0x90ED (the Rust reader does the same, src/ptp_usb.rs
`read_ftp_profile_slot`; over USB a read at slot == count stalled the pipe).  A failed setup (0x90E8) or
info (0x90EB) reply, a reply with no data phase, or a blob that does not decode is an error and the
exit status is non-zero.  The blob is decoded as strictly as the Rust decoder
(`decode_ftp_profile`): every structural byte is checked, trailing bytes are an error.  The vectors in
tests/fixtures/ftp_profile_vectors.json are shared by this decoder's unit test
(scripts/test_ptpip_read_ftp.py) and the Rust decoder's parity test (tests/ftp_profile_python_parity.rs),
so the two cannot drift apart unnoticed.

WRITE (--write FILE).  Replaces one stored profile with the bytes in FILE.  Before anything is sent:
  * FILE must decode with the strict decoder above (a garbled or truncated file never reaches the
    camera);
  * the camera's model, from GetDeviceInfo, must be one the write has been confirmed on (`Z 9`,
    `Z6_3`: the Rust gate FTP_WRITE_CONFIRMED_MODELS), unless --force-unconfirmed-model is given;
  * the 0x90E8 setup must have been answered OK (a write without it hung the camera);
  * writes only to a slot below the stored count unless --allow-create is also given.
On the one camera tried, `--slot COUNT --param2 1 --allow-create` ADDS a profile at the front of the
list; `--param2 0` replaces slot N and moves it to the front.  NX Field's accepted writes were each
followed by a Wi-Fi re-association, so read back over a fresh connection after a write.  A write the
camera does not accept exits 1; one whose reply never arrived exits with status 4
("indeterminate": the camera may have taken it; the same status as `nikon-fleet vendor-write-ftp`,
and not 2, which argparse uses for a usage error).  Never pass a slot number the camera has not already
shown to be valid: slot 0xFFFFFFFF hung the camera's network until it was power-cycled.

PORTS.  The profile has two port fields.  `ftp_port` is the FTP/SFTP server port (21 or 22).
`second_port` (8080 in most profiles, probably a proxy port) is the one the Rust encoder's
`FtpProfile.port` writes; do not mix them up when writing a profile back.

The password and the "5 GHz" field are masked in the output.  --out saves the RAW blob, which
contains both in the clear: it is created 0600 and an existing file is never overwritten.  Keep it
private and do not commit it.

usage: ptpip_read_ftp.py CAMERA_IP [--slot N] [--guid HEX32] [--name NAME] [--prefix] [--incr-tid]
                         [--out FILE] [--write FILE [--param2 N] [--send] [--allow-create]
                         [--force-unconfirmed-model]]
"""
import argparse
import os
import socket
import struct
import sys

GUID = bytes.fromhex("01004e58000000000000ff900ae772a7")
NAME = "@@"

EXIT_INDETERMINATE = 4  # as `nikon-fleet vendor-write-ftp`: the profile data was sent, the outcome is unknown
RC_OK = 0x2001
RC_SESSION_ALREADY_OPEN = 0x201E

OP_GET_DEVICE_INFO = 0x1001
OP_OPEN_SESSION = 0x1002
OP_FTP_SETUP = 0x90E8
OP_FTP_INFO = 0x90EB
OP_FTP_READ = 0x90ED
OP_FTP_WRITE = 0x90EE
FTP_SETUP_DATA = bytes([1, 0, 0])

# Mirrors FTP_WRITE_CONFIRMED_MODELS in src/ptp_usb.rs (the Rust side of the same gate).  The parity
# test pins the two lists equal.  Model strings are the camera's DeviceInfo model: "Z 9" has a space,
# "Z6_3" does not.
FTP_WRITE_CONFIRMED_MODELS = ("Z 9", "Z6_3")

# Largest PTP/IP packet accepted.  Real replies are a few hundred bytes (profile) to a few KB
# (DeviceInfo); a declared length beyond this is a desynchronised stream, not a packet.
MAX_PACKET = 4 * 1024 * 1024

# Blob layout literals (src/ptp_usb.rs FTP_BLOB_HEADER and friends).
BLOB_HEADER = bytes([0xF4, 0x01])
NETWORK_TAG = bytes([0x02, 0x03, 0x03])
NETWORK_PAD = bytes(8)
SSID24_LEADS = (0x03, 0x00)
SSID24_TAG_BYTE1 = 0x00
SSID24_TAG_BYTE3 = 0x00
SSID5_TAG_BYTE1 = 0x04
HOST_TAG = 0x01
PORT_SECTION_PAD = bytes(5)
IPV6_SEPARATOR = 0x40
BLOB_TRAILER = 0x81


class ProtocolError(Exception):
    """The camera (or the stream) did something the PTP/IP protocol does not allow."""


class BlobError(Exception):
    """A blob or reply that does not have the layout this decoder knows.

    `what` names the field and `offset` is the byte offset, with the same meaning as the Rust
    `PtpUsbError::FtpBlobMalformed { what, offset, .. }`: the parity test compares both.
    """

    def __init__(self, what, offset, detail):
        super().__init__(f"{what} at byte offset {offset}: {detail}")
        self.what, self.offset, self.detail = what, offset, detail


def parse_guid(text):
    try:
        g = bytes.fromhex(text)
    except ValueError:
        raise SystemExit("--guid must be 32 hex digits (16 bytes)") from None
    if len(g) != 16:
        raise SystemExit("--guid must be 32 hex digits (16 bytes)")
    return g


def u32_arg(text):
    try:
        v = int(text, 0)
    except ValueError:
        raise argparse.ArgumentTypeError(f"{text!r} is not an integer") from None
    if not 0 <= v <= 0xFFFFFFFF:
        raise argparse.ArgumentTypeError(f"{text!r} is outside 0..0xFFFFFFFF")
    return v


# ---------------------------------------------------------------------------------------------
# Strict blob decoding.  A line-for-line counterpart of FtpBlobReader / decode_ftp_profile /
# parse_ftp_profile_info in src/ptp_usb.rs; the shared vectors in tests/fixtures/ftp_profile_vectors.json
# hold the two together.
# ---------------------------------------------------------------------------------------------
class Reader:
    """Bounds-checked cursor.  Every read names what it is for; failures carry the offset."""

    def __init__(self, data):
        self.data = bytes(data)
        self.pos = 0

    def bad(self, what, detail):
        return BlobError(what, self.pos, detail)

    def take(self, what, n):
        remaining = len(self.data) - self.pos
        if n > remaining:
            raise self.bad(what, f"needs {n} byte(s) but only {remaining} remain")
        out = self.data[self.pos:self.pos + n]
        self.pos += n
        return out

    def u8(self, what):
        return self.take(what, 1)[0]

    def u16(self, what):
        return struct.unpack("<H", self.take(what, 2))[0]

    def u32(self, what):
        return struct.unpack("<I", self.take(what, 4))[0]

    def expect(self, what, expected):
        at = self.pos
        got = self.take(what, len(expected))
        if got != expected:
            self.pos = at
            raise self.bad(what, f"expected {expected.hex()}, found {got.hex()}")

    def ptp_string(self, what):
        """`[u8 count incl. null][utf16le]`; count 0 is the empty string.  A non-empty string must end
        in the null, hold no other null and no unpaired surrogate."""
        at = self.pos
        chars = self.u8(what)
        raw = self.take(what, chars * 2)
        units = [struct.unpack_from("<H", raw, 2 * i)[0] for i in range(chars)]
        faults = []
        if units:
            if units[-1] == 0:
                units.pop()
                raw = raw[:-2]
            else:
                faults.append("missing_terminator")
        if 0 in units:
            faults.append("embedded_nul")
        text = ""
        try:
            text = raw.decode("utf-16-le")
        except UnicodeDecodeError:
            faults.append("unpaired_surrogate")
        if faults:
            self.pos = at
            raise self.bad(what, f"string is not well-formed ({', '.join(faults)})")
        return text

    def ascii_u32_len(self, what):
        """`[u32 length in bytes][ascii]`, no null.  The length is checked against the bytes left."""
        at = self.pos
        n = self.u32(what)
        if n > len(self.data) - self.pos:
            self.pos = at
            raise self.bad(what, f"declares {n} byte(s) but only {len(self.data) - at - 4} remain")
        raw = self.take(what, n)
        if not raw.isascii():
            self.pos = at
            raise self.bad(what, "contains non-ASCII bytes")
        return raw.decode("ascii")


def decode_ftp_profile(blob):
    """Decode a 0x90ED reply (or a 0x90EE data-out blob) into its fields.  Strict: an unrecognised
    structural byte, a truncated field or trailing bytes is a BlobError with the offset.  The values
    are returned as read, including the password: mask them with `display_fields` before printing."""
    r = Reader(blob)
    r.expect("blob header", BLOB_HEADER)
    d = {"profile_name": r.ptp_string("profile name")}
    d["timestamps"] = [r.ptp_string("timestamp 1"), r.ptp_string("timestamp 2")]
    r.expect("network tag", NETWORK_TAG)
    addr = r.take("camera address", 4)
    d["camera_ipv4"] = list(reversed(addr))
    d["prefix_len"] = r.u8("camera prefix length")
    r.expect("network padding", NETWORK_PAD)
    tag_at = r.pos
    tag24 = r.take("2.4 GHz SSID tag", 4)
    if tag24[0] not in SSID24_LEADS or tag24[1] != SSID24_TAG_BYTE1 or tag24[3] != SSID24_TAG_BYTE3:
        r.pos = tag_at
        raise r.bad("2.4 GHz SSID tag", f"expected 03|00 00 xx 00, found {tag24.hex()}")
    d["ssid24_lead"], d["ssid24_kind"] = tag24[0], tag24[2]
    d["ssid_24ghz"] = r.ascii_u32_len("2.4 GHz SSID")
    tag_at = r.pos
    tag5 = r.take("5 GHz SSID tag", 2)
    if tag5[1] != SSID5_TAG_BYTE1:
        r.pos = tag_at
        raise r.bad("5 GHz SSID tag", f"expected xx {SSID5_TAG_BYTE1:02x}, found {tag5.hex()}")
    d["ssid5_kind"] = tag5[0]
    d["ssid_5ghz"] = r.ascii_u32_len("5 GHz SSID")
    r.expect("host tag", bytes([HOST_TAG]))
    d["host"] = r.ptp_string("host")
    d["folder"] = r.ptp_string("folder")
    d["ftp_port"] = r.u16("FTP port")
    d["username"] = r.ascii_u32_len("username")
    d["password"] = r.ascii_u32_len("password")
    d["option_flags"] = r.u16("option flags")
    d["second_port"] = r.u16("second port")
    r.expect("port-section padding", PORT_SECTION_PAD)
    ipv6_a = r.ascii_u32_len("IPv6 field 1")
    r.expect("IPv6 separator", bytes([IPV6_SEPARATOR]))
    ipv6_b = r.ascii_u32_len("IPv6 field 2")
    ipv6_c = r.ascii_u32_len("IPv6 field 3")
    d["ipv6_fields"] = [ipv6_a, ipv6_b, ipv6_c]
    r.expect("blob trailer", bytes([BLOB_TRAILER]))
    if r.pos != len(r.data):
        raise r.bad("blob end", f"{len(r.data) - r.pos} unexpected byte(s) after the trailer")
    return d


def parse_ftp_profile_info(data):
    """Decode a 0x90EB reply: u16 header, u32 count, u32 limit, u32 slot, 16-byte GUID, PTP string
    (the body's `<model>_<serial>`), u16 trailer.  Strict on length and the string."""
    r = Reader(data)
    d = {"header": r.u16("info header")}
    d["count"] = r.u32("info count")
    d["limit"] = r.u32("info limit")
    d["slot"] = r.u32("info slot")
    d["guid"] = r.take("info GUID", 16).hex()
    d["device_name"] = r.ptp_string("info device name")
    d["trailer"] = r.u16("info trailer")
    if r.pos != len(r.data):
        raise r.bad("info end", f"{len(r.data) - r.pos} unexpected byte(s) after the trailer")
    return d


def parse_device_info_model(data):
    """The Model string of a GetDeviceInfo (0x1001) dataset, walking the whole dataset so a
    truncated or oddly-shaped one is refused.  Stricter than the Rust parser on strings: a model
    that is not a well-formed PTP string is an error, not a lossy value, because the write gate
    compares it exactly."""
    r = Reader(data)
    r.u16("StandardVersion")
    r.u32("VendorExtensionID")
    r.u16("VendorExtensionVersion")
    r.ptp_string("VendorExtensionDesc")
    r.u16("FunctionalMode")
    for field in ("OperationsSupported", "EventsSupported", "DevicePropertiesSupported", "CaptureFormats", "PlaybackFormats"):
        count = r.u32(field)
        if count * 2 > len(r.data) - r.pos:
            raise r.bad(field, f"declares {count} code(s) but only {len(r.data) - r.pos} byte(s) remain")
        r.take(field, count * 2)
    r.ptp_string("Manufacturer")
    model = r.ptp_string("Model")
    r.ptp_string("DeviceVersion")
    r.ptp_string("SerialNumber")
    return model


def display_fields(d):
    """The decoded profile as printable lines: the password and the "5 GHz" field (probably a Wi-Fi
    passphrase) are replaced by asterisks of the same length."""
    out = dict(d)
    out["camera_ipv4"] = ".".join(str(x) for x in d["camera_ipv4"]) + f"/{d['prefix_len']}"
    del out["prefix_len"]
    out["ssid_5ghz"] = "*" * len(d["ssid_5ghz"])
    out["password"] = "*" * len(d["password"])
    out["ssid24_tag"] = f"lead 0x{d['ssid24_lead']:02x}, kind 0x{d['ssid24_kind']:02x}"
    del out["ssid24_lead"], out["ssid24_kind"]
    out["ssid5_kind"] = f"0x{d['ssid5_kind']:02x}"
    fl = d["option_flags"]
    out["option_flags"] = f"0x{fl:04x}" + (" (SFTP)" if fl & 8 else " (FTP)")
    out["ftp_port"] = f"{d['ftp_port']} (the FTP/SFTP server port)"
    out["second_port"] = f"{d['second_port']} (the port the Rust encoder's FtpProfile.port writes)"
    return out


# ---------------------------------------------------------------------------------------------
# Gates.  Pure functions: each returns None when the action may proceed, else the reason it may not.
# ---------------------------------------------------------------------------------------------
def check_read_slot(slot, count):
    if slot >= count:
        return f"slot {slot} is not below the stored profile count {count} (slots 0..{count}); a read past the end can stall the camera, so it was not sent"
    return None


def check_write_slot(slot, count, allow_create):
    if slot < count:
        return None
    if slot == count and allow_create:
        return None
    return f"slot {slot} is not below the stored count {count}" + ("" if allow_create else " (use --allow-create to try adding)")


def check_model(model, force):
    if force or model in FTP_WRITE_CONFIRMED_MODELS:
        return None
    return (
        f"camera model {model!r} is not one the FTP-profile write has been confirmed on "
        f"({', '.join(repr(m) for m in FTP_WRITE_CONFIRMED_MODELS)}); pass --force-unconfirmed-model to try anyway"
    )


# ---------------------------------------------------------------------------------------------
# Transport
# ---------------------------------------------------------------------------------------------
def recvn(s, n):
    b = b""
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            raise ConnectionError(f"closed after {len(b)}/{n} bytes")
        b += c
    return b


def recv_pkt(s):
    ln, ty = struct.unpack("<II", recvn(s, 8))
    if ln < 8 or ln > MAX_PACKET:
        raise ProtocolError(f"packet type {ty} declares length {ln}, outside 8..{MAX_PACKET}: the stream is out of step")
    return ty, recvn(s, ln - 8)


def send_pkt(s, ty, body):
    s.sendall(struct.pack("<II", len(body) + 8, ty) + body)


class Reply:
    """What one operation collected.  `data_phase` is False when no data container arrived at all
    (which is not the same as a data phase of zero bytes); `deviations` lists replies that carried
    a different transaction id (or a different declared length) than the request."""

    def __init__(self, rc, params, data, data_phase, deviations):
        self.rc, self.params, self.data, self.data_phase, self.deviations = rc, params, data, data_phase, deviations


class Session:
    def __init__(self, ip, incr_tid=False, port=15740, timeout=8, guid=GUID, name=NAME):
        self.incr, self.tid = incr_tid, 0
        self.cmd = self.evt = None
        try:
            self.cmd = socket.create_connection((ip, port), timeout=timeout)
            send_pkt(self.cmd, 1, guid + (name + "\0").encode("utf-16le") + struct.pack("<I", 0x00010000))
            ty, body = recv_pkt(self.cmd)
            if ty != 2:
                raise ProtocolError(f"InitCommandRequest refused: packet type {ty}, body {body.hex()}")
            if len(body) < 20:
                raise ProtocolError(f"InitCommandAck body is {len(body)} byte(s), shorter than the 20 of connection number and GUID")
            self.connnum = struct.unpack_from("<I", body, 0)[0]
            try:
                self.camera_name = body[20:].decode("utf-16le").split("\0")[0]
            except UnicodeDecodeError as e:
                raise ProtocolError(f"InitCommandAck camera name is not UTF-16: {e}") from None
            self.evt = socket.create_connection((ip, port), timeout=timeout)
            send_pkt(self.evt, 3, struct.pack("<I", self.connnum))
            ty, body = recv_pkt(self.evt)
            if ty != 4:
                raise ProtocolError(f"InitEventRequest refused: packet type {ty}, body {body.hex()}")
        except BaseException:
            self.close()
            raise

    def close(self):
        for s in (self.cmd, self.evt):
            if s is not None:
                try:
                    s.close()
                except OSError:
                    pass  # closing a socket that is already dead: nothing left to release
        self.cmd = self.evt = None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False

    def op(self, code, params=(), data_out=None):
        self.tid = self.tid + 1 if self.incr else 1
        tid = self.tid
        dp = 2 if data_out is not None else 1
        send_pkt(self.cmd, 6, struct.pack("<IHI", dp, code, tid) + b"".join(struct.pack("<I", p) for p in params))
        if data_out is not None:
            send_pkt(self.cmd, 9, struct.pack("<IQ", tid, len(data_out)))
            send_pkt(self.cmd, 12, struct.pack("<I", tid) + data_out)
        inn = b""
        data_phase = False
        declared = None
        deviations = []
        while True:
            ty, body = recv_pkt(self.cmd)
            if ty in (9, 10, 12):  # StartData, Data, EndData: all begin with the transaction id
                if len(body) < 4:
                    raise ProtocolError(f"data packet type {ty} is {len(body)} byte(s), too short to hold a transaction id")
                data_phase = True
                got = struct.unpack_from("<I", body, 0)[0]
                if got != tid:
                    deviations.append(f"data packet type {ty} carries transaction id {got}, request used {tid}")
                if ty == 9:
                    if len(body) < 12:
                        raise ProtocolError(f"StartData is {len(body)} byte(s), shorter than the 12 of id and length")
                    declared = struct.unpack_from("<Q", body, 4)[0]
                else:
                    inn += body[4:]
            elif ty == 7:
                if len(body) < 6:
                    raise ProtocolError(f"OperationResponse is {len(body)} byte(s), shorter than the 6 of code and transaction id")
                if (len(body) - 6) % 4:
                    raise ProtocolError(f"OperationResponse parameters are {len(body) - 6} byte(s), not a whole number of u32")
                rc = struct.unpack_from("<H", body, 0)[0]
                got = struct.unpack_from("<I", body, 2)[0]
                if got != tid:
                    deviations.append(f"response carries transaction id {got}, request used {tid}")
                if declared is not None and declared != len(inn):
                    deviations.append(f"StartData declared {declared} byte(s), {len(inn)} arrived")
                rp = [struct.unpack_from("<I", body, 6 + 4 * i)[0] for i in range((len(body) - 6) // 4)]
                return Reply(rc, rp, inn, data_phase, deviations)
            else:
                raise ProtocolError(f"unexpected packet type {ty}: {body.hex()[:80]}")


# ---------------------------------------------------------------------------------------------
# Files
# ---------------------------------------------------------------------------------------------
def save_blob(path, blob):
    """Write the raw profile blob (it contains the FTP password) to a NEW file readable only by the
    owner.  An existing file is an error (FileExistsError), never overwritten; the file is closed
    before this returns, and a short write or a failed flush raises."""
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_BINARY", 0)
    fd = os.open(path, flags, 0o600)
    with os.fdopen(fd, "wb") as f:
        f.write(blob)


def read_blob_file(path):
    try:
        with open(path, "rb") as f:
            return f.read()
    except OSError as e:
        raise SystemExit(f"cannot read {path}: {e}") from None


# ---------------------------------------------------------------------------------------------
# Command
# ---------------------------------------------------------------------------------------------
def build_parser():
    ap = argparse.ArgumentParser(description="Read (or, experimentally, write) a Nikon camera's FTP profile over PTP/IP.")
    ap.add_argument("ip")
    ap.add_argument("--slot", type=u32_arg, default=0, help="profile slot to read or write (must be below the stored count)")
    ap.add_argument("--guid", default=GUID.hex(), help="client GUID, 32 hex digits (default: the one NX Field used)")
    ap.add_argument("--name", default=NAME, help="client device name (not checked by the camera)")
    ap.add_argument("--prefix", action="store_true", help="replay NX Field's queries before the profile ops")
    ap.add_argument("--incr-tid", action="store_true", help="increment transaction ids (NX Field always uses 1)")
    ap.add_argument("--out", help="save the raw blob (contains the password!) to this NEW file, mode 0600")
    ap.add_argument("--write", metavar="FILE", help="EXPERIMENTAL: write this raw blob to --slot (dry run unless --send)")
    ap.add_argument("--param2", type=u32_arg, default=0, help="second parameter of 0x90EE (0 replaces, 1 with --allow-create adds)")
    ap.add_argument("--send", action="store_true", help="actually send the write")
    ap.add_argument("--allow-create", action="store_true", help="allow --slot equal to the stored count (tries to add a profile)")
    ap.add_argument("--force-unconfirmed-model", action="store_true", help="send the write to a model it has not been confirmed on")
    return ap


def run(a):
    new = None
    if a.write:
        # Nothing touches the network until the file is known to be a profile the strict decoder accepts.
        new = read_blob_file(a.write)
        try:
            planned = decode_ftp_profile(new)
        except BlobError as e:
            sys.exit(f"refusing: {a.write} does not decode as a profile blob ({e})")
    if a.out and os.path.lexists(a.out):
        sys.exit(f"refusing: {a.out} already exists (--out never overwrites)")

    with Session(a.ip, a.incr_tid, guid=parse_guid(a.guid), name=a.name) as s:
        print(f"connected: camera name {s.camera_name!r}, connection number {s.connnum}")

        def step(label, code, params=(), data_out=None):
            reply = s.op(code, params, data_out)
            print(
                f"{label:34} -> 0x{reply.rc:04x}"
                + (f" params={[hex(x) for x in reply.params]}" if reply.params else "")
                + (f"  data-in {len(reply.data)}B" if reply.data else "")
            )
            for dev in reply.deviations:
                print(f"   WARNING: {dev}")
            return reply

        reply = step("OpenSession(1)", OP_OPEN_SESSION, (1,))
        if reply.rc not in (RC_OK, RC_SESSION_ALREADY_OPEN):
            sys.exit("OpenSession failed")
        if a.prefix:
            step("GetDeviceInfo", 0x1001)
            step("0x944c(0x90ed,0)", 0x944C, (0x90ED, 0))
            step("0x944b", 0x944B)
            step("0x941c", 0x941C)
            step("0x9435(0)", 0x9435, (0,))
        if a.write:
            reply = step("GetDeviceInfo (model gate)", OP_GET_DEVICE_INFO)
            if reply.rc != RC_OK or not reply.data_phase:
                sys.exit("refusing to write: GetDeviceInfo did not return the camera model")
            try:
                model = parse_device_info_model(reply.data)
            except BlobError as e:
                sys.exit(f"refusing to write: GetDeviceInfo does not parse ({e})")
            why = check_model(model, a.force_unconfirmed_model)
            if why:
                sys.exit(f"refusing: {why}")
            print(f"   camera model {model!r}" + (" (write not confirmed on it; forced)" if model not in FTP_WRITE_CONFIRMED_MODELS else ""))

        reply = step("0x90E8 setup (data-out 01 00 00)", OP_FTP_SETUP, data_out=FTP_SETUP_DATA)
        if reply.rc != RC_OK:
            sys.exit(f"setup (0x90E8) was not accepted (0x{reply.rc:04x}); nothing further was sent")
        reply = step("0x90EB info", OP_FTP_INFO)
        if reply.rc != RC_OK:
            sys.exit(f"info (0x90EB) was not accepted (0x{reply.rc:04x}); nothing further was sent")
        if not reply.data_phase:
            sys.exit("info (0x90EB) answered OK but sent no data")
        try:
            info = parse_ftp_profile_info(reply.data)
        except BlobError as e:
            sys.exit(f"info (0x90EB) does not parse ({e}); nothing further was sent")
        count = info["count"]
        print(f"   profiles stored: {count}, limit {info['limit']}, selected slot {info['slot']}, header 0x{info['header']:04x}, trailer 0x{info['trailer']:04x}")
        print(f"   body {info['device_name']!r}, GUID {info['guid']}")

        if a.write:
            why = check_write_slot(a.slot, count, a.allow_create)
            if why:
                sys.exit(f"refusing: {why}")
            print(f"   write plan: slot {a.slot}, param2 {a.param2}, {len(new)} bytes, profile {planned['profile_name']!r}, host {planned['host']}, {display_fields(planned)['option_flags']}")
            if not a.send:
                print("   dry run: nothing sent (add --send)")
                return
            try:
                reply = step(f"0x90EE({a.slot},{a.param2}) WRITE", OP_FTP_WRITE, (a.slot, a.param2), data_out=new)
            except (OSError, ProtocolError) as e:
                print(f"   write INDETERMINATE: no usable reply came back ({e}); the camera may or may not have taken the profile", file=sys.stderr)
                sys.exit(EXIT_INDETERMINATE)
            if reply.rc != RC_OK:
                sys.exit(f"   write NOT accepted (0x{reply.rc:04x})")
            print("   write accepted")
            print("   the camera may restart its Wi-Fi after a write; read back over a fresh connection")
            return

        why = check_read_slot(a.slot, count)
        if why:
            sys.exit(f"refusing: {why}")
        reply = step(f"0x90ED({a.slot}) read profile", OP_FTP_READ, (a.slot,))
        if reply.rc != RC_OK:
            sys.exit(f"read (0x90ED) was not accepted (0x{reply.rc:04x})")
        if not reply.data_phase or not reply.data:
            sys.exit("read (0x90ED) answered OK but returned no profile data")
        blob = reply.data
        save_failed = False
        if a.out:  # before decoding: a blob the decoder rejects is still the finding
            try:
                save_blob(a.out, blob)
                print(f"   raw blob ({len(blob)}B, contains the password) saved to {a.out}")
            except OSError as e:
                save_failed = True
                print(f"   could not save the raw blob to {a.out}: {e}", file=sys.stderr)
        try:
            d = decode_ftp_profile(blob)
        except BlobError as e:
            sys.exit(f"   the blob ({len(blob)}B) does not decode: {e}")
        for k, v in display_fields(d).items():
            print(f"   {k:13} {v}")
        if save_failed:
            sys.exit(1)


def main(argv=None):
    a = build_parser().parse_args(argv)
    try:
        run(a)
    except (OSError, ProtocolError) as e:  # socket errors, timeouts and resets are OSError
        sys.exit(f"error: {e}")


if __name__ == "__main__":
    main()

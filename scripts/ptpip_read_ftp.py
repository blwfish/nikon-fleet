#!/usr/bin/env python3
"""Read-only PTP/IP client: open a session with a Nikon camera and read one FTP profile.

Experimental research tool (see docs/nx-field-ftp-profile-session-2026-07-10.md).  Not part of the
nikon-fleet binary; Python 3 standard library only.

It presents NX Field's handshake identity: a 16-byte GUID of the form
`01 00 4e 58` + 6 zero bytes + 6 arbitrary bytes, device name "@@", protocol 1.0, and transaction id
1 on every request (what NX Field sends).  The camera refuses GUIDs without the `01 00 4e 58` prefix
and does not check the name.  It then sends only OpenSession, 0x90E8 (setup), 0x90EB (info) and
0x90ED(slot) (read).  Nothing is written to the camera.  Note that a new connection ends any session
already open (NX Field on a phone is dropped), and that the camera must be in its
connect-to-smart-device state or it resets the TCP connection to port 15740.

The password and the "5 GHz" field are masked in the output.  --out saves the RAW blob, which
contains both in the clear: keep that file private and do not commit it.

usage: ptpip_read_ftp.py CAMERA_IP [--slot N] [--guid HEX32] [--name NAME] [--prefix] [--incr-tid]
                         [--out FILE]
"""
import argparse, socket, struct, sys

GUID = bytes.fromhex("01004e58000000000000ff900ae772a7")
NAME = "@@"


def parse_guid(text):
    g = bytes.fromhex(text)
    if len(g) != 16:
        raise SystemExit("--guid must be 32 hex digits (16 bytes)")
    return g


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
    return ty, recvn(s, ln - 8)


def send_pkt(s, ty, body):
    s.sendall(struct.pack("<II", len(body) + 8, ty) + body)


class Session:
    def __init__(self, ip, incr_tid=False, port=15740, timeout=8, guid=GUID, name=NAME):
        self.incr, self.tid = incr_tid, 0
        self.cmd = socket.create_connection((ip, port), timeout=timeout)
        send_pkt(self.cmd, 1, guid + (name + "\0").encode("utf-16le") + struct.pack("<I", 0x00010000))
        ty, body = recv_pkt(self.cmd)
        if ty != 2:
            raise RuntimeError(f"InitCommandRequest refused: packet type {ty}, body {body.hex()}")
        self.connnum = struct.unpack_from("<I", body, 0)[0]
        self.camera_name = body[20:].decode("utf-16le", "replace").split("\0")[0]
        self.evt = socket.create_connection((ip, port), timeout=timeout)
        send_pkt(self.evt, 3, struct.pack("<I", self.connnum))
        ty, body = recv_pkt(self.evt)
        if ty != 4:
            raise RuntimeError(f"InitEventRequest refused: packet type {ty}, body {body.hex()}")

    def op(self, code, params=(), data_out=None):
        self.tid = self.tid + 1 if self.incr else 1
        tid = self.tid
        dp = 2 if data_out is not None else 1
        send_pkt(self.cmd, 6, struct.pack("<IHI", dp, code, tid) + b"".join(struct.pack("<I", p) for p in params))
        if data_out is not None:
            send_pkt(self.cmd, 9, struct.pack("<IQ", tid, len(data_out)))
            send_pkt(self.cmd, 12, struct.pack("<I", tid) + data_out)
        inn = b""
        while True:
            ty, body = recv_pkt(self.cmd)
            if ty in (10, 12):
                inn += body[4:]
            elif ty == 9:
                pass
            elif ty == 7:
                rc = struct.unpack_from("<H", body, 0)[0]
                rp = [struct.unpack_from("<I", body, 6 + 4 * i)[0] for i in range((len(body) - 6) // 4)]
                return rc, rp, inn
            else:
                raise RuntimeError(f"unexpected packet type {ty}: {body.hex()[:80]}")


def ptp_string(b, o):
    n = b[o]
    return b[o + 1:o + 1 + 2 * n].decode("utf-16le").rstrip("\0"), o + 1 + 2 * n


def decode_profile(b):
    d = {}
    o = 2
    d["name"], o = ptp_string(b, o)
    _, o = ptp_string(b, o)
    _, o = ptp_string(b, o)
    d["camera_ip"] = ".".join(str(x) for x in b[o + 3:o + 7][::-1])
    d["prefix"] = b[o + 7]
    o += 16 + 4
    n = struct.unpack_from("<I", b, o)[0]
    d["ssid_24"] = b[o + 4:o + 4 + n].decode("ascii", "replace")
    o += 4 + n + 2
    n = struct.unpack_from("<I", b, o)[0]
    d["field_5ghz"] = "*" * n
    o += 4 + n + 1
    d["host"], o = ptp_string(b, o)
    d["folder"], o = ptp_string(b, o)
    d["port"] = struct.unpack_from("<H", b, o)[0]
    o += 2
    n = struct.unpack_from("<I", b, o)[0]
    d["user"] = b[o + 4:o + 4 + n].decode("ascii", "replace")
    o += 4 + n
    n = struct.unpack_from("<I", b, o)[0]
    d["password"] = "*" * n
    o += 4 + n
    fl, sp = struct.unpack_from("<HH", b, o)
    d["option_flags"] = f"0x{fl:04x}" + (" (SFTP)" if fl & 8 else " (FTP)")
    d["second_port"] = sp
    return d


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("ip")
    ap.add_argument("--slot", type=int, default=0)
    ap.add_argument("--guid", default=GUID.hex(), help="client GUID, 32 hex digits (default: the one NX Field used)")
    ap.add_argument("--name", default=NAME, help="client device name (not checked by the camera)")
    ap.add_argument("--prefix", action="store_true", help="replay NX Field's queries before the profile ops")
    ap.add_argument("--incr-tid", action="store_true", help="increment transaction ids (NX Field always uses 1)")
    ap.add_argument("--out", help="save the raw blob (contains the password!) to this file")
    a = ap.parse_args()

    s = Session(a.ip, a.incr_tid, guid=parse_guid(a.guid), name=a.name)
    print(f"connected: camera name {s.camera_name!r}, connection number {s.connnum}")

    def step(label, code, params=(), data_out=None):
        rc, rp, inn = s.op(code, params, data_out)
        print(f"{label:34} -> 0x{rc:04x}" + (f" params={[hex(x) for x in rp]}" if rp else "") + (f"  data-in {len(inn)}B" if inn else ""))
        return rc, rp, inn

    rc, _, _ = step("OpenSession(1)", 0x1002, (1,))
    if rc not in (0x2001, 0x201e):  # OK, SessionAlreadyOpen
        sys.exit("OpenSession failed")
    if a.prefix:
        step("GetDeviceInfo", 0x1001)
        step("0x944c(0x90ed,0)", 0x944C, (0x90ED, 0))
        step("0x944b", 0x944B)
        step("0x941c", 0x941C)
        step("0x9435(0)", 0x9435, (0,))
    rc, _, _ = step("0x90E8 setup (data-out 01 00 00)", 0x90E8, data_out=bytes([1, 0, 0]))
    rc, _, info = step("0x90EB info", 0x90EB)
    if len(info) >= 14:
        print(f"   profiles stored: {struct.unpack_from('<I', info, 2)[0]}, limit {struct.unpack_from('<I', info, 6)[0]}")
    rc, _, blob = step(f"0x90ED({a.slot}) read profile", 0x90ED, (a.slot,))
    if rc == 0x2001 and blob:
        for k, v in decode_profile(blob).items():
            print(f"   {k:13} {v}")
        if a.out:
            open(a.out, "wb").write(blob)
            print(f"   raw blob ({len(blob)}B, contains the password) saved to {a.out}")
    else:
        print("   no profile data returned")


if __name__ == "__main__":
    main()

"""Tests for ptpip_read_ftp.py: python3 -I scripts/test_ptpip_read_ftp.py   (or python3 -m unittest scripts/test_ptpip_read_ftp.py)

No camera and no network: the sockets are scripted byte streams and the camera session is a fake.
The decoder is held to tests/fixtures/ftp_profile_vectors.json, which tests/ftp_profile_python_parity.rs
holds the Rust decoder (src/ptp_usb.rs) to as well.  `cargo test` runs this file, so CI does too.
"""
import argparse
import contextlib
import io
import json
import os
import stat
import struct
import sys
import tempfile
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import ptpip_read_ftp as ft  # noqa: E402

with open(os.path.join(HERE, "..", "tests", "fixtures", "ftp_profile_vectors.json"), encoding="utf-8") as _f:
    V = json.load(_f)

REFERENCE = bytes.fromhex(next(p["hex"] for p in V["profiles"] if p["name"] == "reference"))


def apply_edit(base, edit):
    b = bytearray(base)
    for off, hx in edit.get("set", []):
        raw = bytes.fromhex(hx)
        b[off:off + len(raw)] = raw
    if "append" in edit:
        b += bytes.fromhex(edit["append"])
    if "truncate" in edit:
        b = b[:edit["truncate"]]
    return bytes(b)


class Vectors(unittest.TestCase):
    """The Python decoder against the vectors the Rust decoder is held to."""

    def test_accepted_blobs_decode_to_the_shared_expected_fields(self):
        self.assertGreaterEqual(len(V["profiles"]), 6)
        for p in V["profiles"]:
            with self.subTest(p["name"]):
                self.assertEqual(ft.decode_ftp_profile(bytes.fromhex(p["hex"])), p["expected"])

    def test_edits_of_the_reference_blob_are_rejected_at_the_rust_field_and_offset(self):
        self.assertGreaterEqual(len(V["rejects"]), 20)
        for r in V["rejects"]:
            with self.subTest(r["name"]):
                with self.assertRaises(ft.BlobError) as cm:
                    ft.decode_ftp_profile(apply_edit(REFERENCE, r["edit"]))
                self.assertEqual((cm.exception.what, cm.exception.offset), (r["what"], r["offset"]))

    def test_every_truncation_of_the_reference_blob_is_rejected_at_the_rust_field_and_offset(self):
        errors = V["truncations"]["errors"]
        self.assertEqual(len(errors), len(REFERENCE))
        for cut, (what, offset) in enumerate(errors):
            with self.assertRaises(ft.BlobError, msg=f"cut {cut}") as cm:
                ft.decode_ftp_profile(REFERENCE[:cut])
            self.assertEqual((cm.exception.what, cm.exception.offset), (what, offset), f"cut {cut}")

    def test_info_replies(self):
        for i in V["infos"]:
            with self.subTest(i["name"]):
                self.assertEqual(ft.parse_ftp_profile_info(bytes.fromhex(i["hex"])), i["expected"])
        for r in V["info_rejects"]:
            with self.subTest(r["name"]):
                with self.assertRaises(ft.BlobError) as cm:
                    ft.parse_ftp_profile_info(bytes.fromhex(r["hex"]))
                self.assertEqual((cm.exception.what, cm.exception.offset), (r["what"], r["offset"]))

    def test_device_info_model_and_the_malformed_models_the_gate_refuses(self):
        for d in V["device_infos"]:
            with self.subTest(d["name"]):
                self.assertEqual(ft.parse_device_info_model(bytes.fromhex(d["hex"])), d["model"])
        for b in V["device_info_model_not_well_formed"]:
            with self.subTest(b["name"]):
                with self.assertRaises(ft.BlobError):
                    ft.parse_device_info_model(bytes.fromhex(b["hex"]))

    def test_truncated_device_info_is_refused_at_every_cut(self):
        raw = bytes.fromhex(V["device_infos"][0]["hex"])
        for cut in range(len(raw)):
            with self.assertRaises(ft.BlobError, msg=f"cut {cut}"):
                ft.parse_device_info_model(raw[:cut])

    def test_the_write_confirmed_models_are_the_rust_gates(self):
        self.assertEqual(list(ft.FTP_WRITE_CONFIRMED_MODELS), V["write_confirmed_models"])


class Display(unittest.TestCase):
    def test_password_and_5ghz_field_are_masked_to_their_length_and_nothing_else_is(self):
        d = ft.decode_ftp_profile(REFERENCE)
        shown = ft.display_fields(d)
        self.assertEqual(shown["password"], "*" * len("12345678"))
        self.assertEqual(shown["ssid_5ghz"], "*" * len("nikon5ghz"))
        text = repr(shown)
        self.assertNotIn("12345678", text)
        self.assertNotIn("nikon5ghz", text)
        self.assertEqual(shown["ssid_24ghz"], "nikon-snoop")
        self.assertEqual(shown["camera_ipv4"], "192.168.66.87/24")
        self.assertEqual(shown["option_flags"], "0x0005 (FTP)")
        self.assertIn("lead 0x03, kind 0x01", shown["ssid24_tag"])

    def test_sftp_bit(self):
        d = ft.decode_ftp_profile(bytes.fromhex(next(p["hex"] for p in V["profiles"] if p["name"] == "sftp_port22")))
        self.assertTrue(ft.display_fields(d)["option_flags"].endswith("(SFTP)"))


class Gates(unittest.TestCase):
    def test_read_slot_is_refused_at_and_above_the_count_and_allowed_below(self):
        for count in (0, 1, 3):
            self.assertIsNone(ft.check_read_slot(count - 1, count) if count else None)
            self.assertIsNotNone(ft.check_read_slot(count, count), f"slot == count {count}")
            self.assertIsNotNone(ft.check_read_slot(count + 1, count))
        self.assertIsNone(ft.check_read_slot(2, 3))
        self.assertIsNotNone(ft.check_read_slot(0xFFFFFFFF, 3), "the slot that hung a camera")
        self.assertIsNotNone(ft.check_read_slot(0, 0), "an empty list has no slot 0")

    def test_write_slot_below_count_ok_at_count_only_with_allow_create_above_never(self):
        self.assertIsNone(ft.check_write_slot(2, 3, False))
        self.assertIsNotNone(ft.check_write_slot(3, 3, False))
        self.assertIsNone(ft.check_write_slot(3, 3, True))
        self.assertIsNotNone(ft.check_write_slot(4, 3, True))
        self.assertIsNotNone(ft.check_write_slot(0xFFFFFFFF, 3, True))
        self.assertIsNotNone(ft.check_write_slot(0xFF, 3, True))

    def test_model_gate_is_exact_and_force_overrides(self):
        for ok in ("Z 9", "Z6_3"):
            self.assertIsNone(ft.check_model(ok, False))
        for bad in ("Z 5", "", "Z6_3 ", "z 9", "Z9", "Z_9", "Z6III", "Z 9_3023668"):
            self.assertIsNotNone(ft.check_model(bad, False), repr(bad))
            self.assertIsNone(ft.check_model(bad, True), repr(bad))

    def test_u32_arg_bounds(self):
        self.assertEqual(ft.u32_arg("0"), 0)
        self.assertEqual(ft.u32_arg("0xFFFFFFFF"), 0xFFFFFFFF)
        for bad in ("-1", "0x100000000", "abc", ""):
            with self.assertRaises(argparse.ArgumentTypeError, msg=bad):
                ft.u32_arg(bad)


def pkt(ty, body):
    return struct.pack("<II", len(body) + 8, ty) + body


class FakeSock:
    def __init__(self, incoming=b""):
        self.buf, self.sent, self.closed = incoming, b"", False

    def recv(self, n):
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def sendall(self, b):
        self.sent += b

    def close(self):
        self.closed = True


def response(rc, tid=1, params=()):
    return pkt(7, struct.pack("<HI", rc, tid) + b"".join(struct.pack("<I", p) for p in params))


def session_over(incoming):
    s = ft.Session.__new__(ft.Session)
    s.incr, s.tid, s.cmd, s.evt = False, 0, FakeSock(incoming), None
    return s


class Transport(unittest.TestCase):
    def test_packet_length_bounds(self):
        # 8 is the smallest legal packet (an empty body); MAX_PACKET the largest accepted.
        self.assertEqual(ft.recv_pkt(FakeSock(struct.pack("<II", 8, 7))), (7, b""))
        with self.assertRaises(ft.ProtocolError):
            ft.recv_pkt(FakeSock(struct.pack("<II", 7, 7)))
        with self.assertRaises(ft.ProtocolError):
            ft.recv_pkt(FakeSock(struct.pack("<II", 0, 7)))
        with self.assertRaises(ft.ProtocolError):
            ft.recv_pkt(FakeSock(struct.pack("<II", ft.MAX_PACKET + 1, 7)))
        # At the limit it is accepted as far as the length check goes (the body is then short).
        with self.assertRaises(ConnectionError):
            ft.recv_pkt(FakeSock(struct.pack("<II", ft.MAX_PACKET, 7)))

    def test_closed_stream_is_an_error_not_a_short_read(self):
        with self.assertRaises(ConnectionError):
            ft.recv_pkt(FakeSock(struct.pack("<II", 20, 7) + b"abc"))

    def test_plain_reply_with_params(self):
        s = session_over(response(0x2001, 1, (5, 6)))
        r = s.op(0x1002, (1,))
        self.assertEqual((r.rc, r.params, r.data, r.data_phase, r.deviations), (0x2001, [5, 6], b"", False, []))
        # the request: operation packet type 6, data phase 1 (none), code, tid 1, one parameter
        self.assertEqual(s.cmd.sent, pkt(6, struct.pack("<IHI", 1, 0x1002, 1) + struct.pack("<I", 1)))

    def test_data_in_phase(self):
        data = pkt(9, struct.pack("<IQ", 1, 3)) + pkt(12, struct.pack("<I", 1) + b"abc") + response(0x2001)
        r = session_over(data).op(0x90EB)
        self.assertEqual((r.data, r.data_phase, r.deviations), (b"abc", True, []))

    def test_empty_data_phase_is_not_no_data_phase(self):
        data = pkt(9, struct.pack("<IQ", 1, 0)) + pkt(12, struct.pack("<I", 1)) + response(0x2001)
        r = session_over(data).op(0x90EB)
        self.assertEqual((r.data, r.data_phase), (b"", True))

    def test_response_with_a_different_transaction_id_is_a_deviation(self):
        r = session_over(response(0x2001, tid=2)).op(0x90E8)
        self.assertEqual(len(r.deviations), 1)
        self.assertIn("transaction id 2", r.deviations[0])

    def test_data_packet_with_a_different_transaction_id_is_a_deviation(self):
        data = pkt(12, struct.pack("<I", 9) + b"x") + response(0x2001)
        r = session_over(data).op(0x90EB)
        self.assertEqual(len(r.deviations), 1)

    def test_startdata_length_disagreeing_with_the_bytes_received_is_a_deviation(self):
        data = pkt(9, struct.pack("<IQ", 1, 5)) + pkt(12, struct.pack("<I", 1) + b"abc") + response(0x2001)
        r = session_over(data).op(0x90EB)
        self.assertEqual(len(r.deviations), 1)
        self.assertIn("declared 5", r.deviations[0])

    def test_malformed_packets_are_protocol_errors(self):
        for name, incoming in [
            ("short response", pkt(7, b"\x01\x20\x01")),
            ("response params not a whole u32", pkt(7, struct.pack("<HI", 0x2001, 1) + b"\x00\x00")),
            ("data packet without a transaction id", pkt(10, b"\x01")),
            ("short StartData", pkt(9, struct.pack("<I", 1))),
            ("unknown packet type", pkt(99, b"")),
        ]:
            with self.subTest(name), self.assertRaises(ft.ProtocolError):
                session_over(incoming).op(0x90EB)

    def test_write_sends_start_and_end_data_with_the_blob(self):
        s = session_over(response(0x2001))
        s.op(0x90EE, (0, 0), data_out=b"BLOB")
        self.assertIn(pkt(12, struct.pack("<I", 1) + b"BLOB"), s.cmd.sent)
        self.assertIn(pkt(9, struct.pack("<IQ", 1, 4)), s.cmd.sent)

    def test_close_closes_both_sockets_and_is_idempotent(self):
        s = session_over(b"")
        s.evt = FakeSock()
        cmd, evt = s.cmd, s.evt
        s.close()
        s.close()
        self.assertTrue(cmd.closed and evt.closed)


class SaveBlob(unittest.TestCase):
    def test_created_private_and_complete(self):
        old = os.umask(0)  # a permissive umask must not widen the mode
        try:
            with tempfile.TemporaryDirectory() as d:
                path = os.path.join(d, "blob.bin")
                ft.save_blob(path, REFERENCE)
                with open(path, "rb") as f:
                    self.assertEqual(f.read(), REFERENCE)
                if os.name == "posix":
                    self.assertEqual(stat.S_IMODE(os.stat(path).st_mode), 0o600)
        finally:
            os.umask(old)

    def test_never_overwrites_an_existing_file(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "blob.bin")
            with open(path, "wb") as f:
                f.write(b"precious")
            with self.assertRaises(FileExistsError):
                ft.save_blob(path, REFERENCE)
            with open(path, "rb") as f:
                self.assertEqual(f.read(), b"precious")

    def test_does_not_follow_a_symlink_at_the_path(self):
        if os.name != "posix":
            self.skipTest("symlinks")
        with tempfile.TemporaryDirectory() as d:
            target = os.path.join(d, "target")
            link = os.path.join(d, "link")
            os.symlink(target, link)
            with self.assertRaises(FileExistsError):
                ft.save_blob(link, REFERENCE)
            self.assertFalse(os.path.exists(target))


# --------------------------------------------------------------------------------------------
# The command flow against a scripted camera.
# --------------------------------------------------------------------------------------------
def info_reply(count, slot=0):
    raw = (
        b"\xc8\x00" + struct.pack("<III", count, 0x14, slot) + b"\xab" * 16
        + bytes([3]) + "Z9".encode("utf-16-le") + b"\x00\x00" + b"\xf4\x01"
    )
    return ft.Reply(0x2001, [], raw, True, [])


def ok(data=None):
    return ft.Reply(0x2001, [], data or b"", data is not None, [])


DEVINFO = {d["name"]: bytes.fromhex(d["hex"]) for d in V["device_infos"]}


class FakeCamera:
    """Stands in for Session: answers each operation from a table and records what was sent."""

    def __init__(self, table):
        self.table, self.sent = table, []
        self.camera_name, self.connnum = "fake", 1

    def __call__(self, ip, incr_tid=False, guid=None, name=None):
        return self

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False

    def op(self, code, params=(), data_out=None):
        self.sent.append((code, tuple(params), data_out))
        entry = self.table[code]
        return entry(params, data_out) if callable(entry) else entry

    def codes(self):
        return [c for c, _, _ in self.sent]


def standard_table(count=3, model="z9"):
    return {
        0x1002: ok(),
        0x1001: ok(DEVINFO[model]),
        0x90E8: ok(),
        0x90EB: info_reply(count),
        0x90ED: ok(REFERENCE),
        0x90EE: ok(),
    }


def run_main(camera, argv):
    out, err = io.StringIO(), io.StringIO()
    with mock.patch.object(ft, "Session", camera), contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        try:
            ft.main(argv)
            code = 0
        except SystemExit as e:
            code = e.code if isinstance(e.code, int) else 1
            if isinstance(e.code, str):
                err.write(e.code)
    return code, out.getvalue(), err.getvalue()


class ReadFlow(unittest.TestCase):
    def test_reads_slot_zero_and_prints_the_decoded_profile_masked(self):
        cam = FakeCamera(standard_table())
        code, out, _ = run_main(cam, ["192.0.2.1"])
        self.assertEqual(code, 0)
        self.assertEqual(cam.codes(), [0x1002, 0x90E8, 0x90EB, 0x90ED])
        self.assertIn("nikon-snoop", out)
        self.assertNotIn("12345678", out)
        self.assertIn("profiles stored: 3", out)

    def test_slot_at_the_count_is_refused_before_0x90ED_is_sent(self):
        for slot, expect_sent in (("2", True), ("3", False), ("4", False), ("0xFFFFFFFF", False)):
            with self.subTest(slot=slot):
                cam = FakeCamera(standard_table(count=3))
                code, _, err = run_main(cam, ["192.0.2.1", "--slot", slot])
                self.assertEqual(0x90ED in cam.codes(), expect_sent)
                self.assertEqual(code, 0 if expect_sent else 1, err)

    def test_empty_profile_list_refuses_slot_zero(self):
        cam = FakeCamera(standard_table(count=0))
        code, _, _ = run_main(cam, ["192.0.2.1"])
        self.assertEqual(code, 1)
        self.assertNotIn(0x90ED, cam.codes())

    def test_failed_setup_stops_before_info_and_read(self):
        table = standard_table()
        table[0x90E8] = ft.Reply(0x2002, [], b"", False, [])
        cam = FakeCamera(table)
        code, _, err = run_main(cam, ["192.0.2.1"])
        self.assertEqual(code, 1)
        self.assertEqual(cam.codes(), [0x1002, 0x90E8])
        self.assertIn("0x2002", err)

    def test_failed_info_or_info_without_data_stops_before_the_read(self):
        for name, reply in [("rc", ft.Reply(0x2002, [], b"", False, [])), ("rc with otherwise valid data", ft.Reply(0x2002, [], info_reply(3).data, True, [])),
                              ("no data phase", ok()), ("garbled", ok(b"\x00" * 10))]:
            with self.subTest(name):
                table = standard_table()
                table[0x90EB] = reply
                cam = FakeCamera(table)
                code, _, _ = run_main(cam, ["192.0.2.1"])
                self.assertEqual(code, 1)
                self.assertNotIn(0x90ED, cam.codes())

    def test_read_that_returns_nothing_or_fails_exits_nonzero(self):
        for name, reply in [("rc", ft.Reply(0x201F, [], b"", False, [])), ("no data phase", ok()), ("empty data phase", ok(b""))]:
            with self.subTest(name):
                table = standard_table()
                table[0x90ED] = reply
                code, _, _ = run_main(FakeCamera(table), ["192.0.2.1"])
                self.assertEqual(code, 1)

    def test_a_blob_that_does_not_decode_exits_nonzero_but_is_still_saved_by_out(self):
        table = standard_table()
        table[0x90ED] = ok(REFERENCE + b"\x00")
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "raw.bin")
            code, _, err = run_main(FakeCamera(table), ["192.0.2.1", "--out", path])
            self.assertEqual(code, 1)
            self.assertIn("blob end", err)
            with open(path, "rb") as f:
                self.assertEqual(f.read(), REFERENCE + b"\x00")

    def test_out_saves_the_raw_blob_and_refuses_an_existing_file_before_connecting(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "raw.bin")
            code, out, _ = run_main(FakeCamera(standard_table()), ["192.0.2.1", "--out", path])
            self.assertEqual(code, 0)
            self.assertIn("saved to", out)
            with open(path, "rb") as f:
                self.assertEqual(f.read(), REFERENCE)
            cam = FakeCamera(standard_table())
            code, _, err = run_main(cam, ["192.0.2.1", "--out", path])
            self.assertEqual(code, 1)
            self.assertEqual(cam.sent, [], "nothing may be sent when --out would overwrite")
            self.assertIn("already exists", err)

    def test_a_deviating_reply_is_reported(self):
        table = standard_table()
        table[0x90E8] = ft.Reply(0x2001, [], b"", False, ["response carries transaction id 7, request used 1"])
        _, out, _ = run_main(FakeCamera(table), ["192.0.2.1"])
        self.assertIn("WARNING: response carries transaction id 7", out)


class WriteFlow(unittest.TestCase):
    def blob_file(self, d, blob=REFERENCE):
        path = os.path.join(d, "new.bin")
        with open(path, "wb") as f:
            f.write(blob)
        return path

    def test_dry_run_sends_no_write(self):
        with tempfile.TemporaryDirectory() as d:
            cam = FakeCamera(standard_table())
            code, out, _ = run_main(cam, ["192.0.2.1", "--write", self.blob_file(d)])
        self.assertEqual(code, 0)
        self.assertNotIn(0x90EE, cam.codes())
        self.assertIn("dry run", out)

    def test_send_writes_with_setup_first_and_the_slot_and_param2(self):
        with tempfile.TemporaryDirectory() as d:
            cam = FakeCamera(standard_table())
            code, out, _ = run_main(cam, ["192.0.2.1", "--write", self.blob_file(d), "--slot", "1", "--param2", "0", "--send"])
        self.assertEqual(code, 0)
        self.assertEqual(cam.codes(), [0x1002, 0x1001, 0x90E8, 0x90EB, 0x90EE])
        self.assertEqual(cam.sent[-1], (0x90EE, (1, 0), REFERENCE))
        self.assertIn("write accepted", out)

    def test_every_model_off_the_gate_is_refused_before_setup_and_write(self):
        raw_z5 = DEVINFO["z5"]
        table = standard_table()
        table[0x1001] = ok(raw_z5)
        with tempfile.TemporaryDirectory() as d:
            cam = FakeCamera(table)
            code, _, err = run_main(cam, ["192.0.2.1", "--write", self.blob_file(d), "--send"])
            self.assertEqual(code, 1)
            self.assertEqual(cam.codes(), [0x1002, 0x1001])
            self.assertIn("'Z 5'", err)
            cam = FakeCamera(table)  # the override
            code, _, _ = run_main(cam, ["192.0.2.1", "--write", self.blob_file(d), "--send", "--force-unconfirmed-model"])
            self.assertEqual(code, 0)
            self.assertIn(0x90EE, cam.codes())

    def test_a_model_the_camera_will_not_state_is_refused(self):
        for name, reply in [("rc", ft.Reply(0x2002, [], b"", False, [])), ("no data", ok()), ("garbled", ok(b"\x01\x02")),
                            ("malformed model", ok(bytes.fromhex(V["device_info_model_not_well_formed"][0]["hex"])))]:
            with self.subTest(name), tempfile.TemporaryDirectory() as d:
                table = standard_table()
                table[0x1001] = reply
                cam = FakeCamera(table)
                code, _, _ = run_main(cam, ["192.0.2.1", "--write", self.blob_file(d), "--send"])
                self.assertEqual(code, 1)
                self.assertNotIn(0x90EE, cam.codes())

    def test_failed_setup_means_no_info_and_no_write(self):
        table = standard_table()
        table[0x90E8] = ft.Reply(0x2002, [], b"", False, [])
        with tempfile.TemporaryDirectory() as d:
            cam = FakeCamera(table)
            code, _, _ = run_main(cam, ["192.0.2.1", "--write", self.blob_file(d), "--send"])
        self.assertEqual(code, 1)
        self.assertNotIn(0x90EB, cam.codes())
        self.assertNotIn(0x90EE, cam.codes())

    def test_a_blob_the_strict_decoder_rejects_never_reaches_the_network(self):
        # Each of these decoded under the old, lax decoder (or hit an IndexError); none may be sent now.
        bad_blobs = {
            "trailing byte": REFERENCE + b"\x00",
            "truncated": REFERENCE[:-1],
            "garbage": b"\x00" * 400,
            "empty": b"",
            "bad trailer": REFERENCE[:-1] + b"\x80",
            "bad ipv6 separator": apply_edit(REFERENCE, {"set": [[237, "41"]]}),
        }
        for name, blob in bad_blobs.items():
            with self.subTest(name), tempfile.TemporaryDirectory() as d:
                cam = FakeCamera(standard_table())
                code, _, err = run_main(cam, ["192.0.2.1", "--write", self.blob_file(d, blob), "--send"])
                self.assertEqual(code, 1)
                self.assertEqual(cam.sent, [])
                self.assertIn("does not decode", err)

    def test_a_missing_blob_file_never_reaches_the_network(self):
        cam = FakeCamera(standard_table())
        code, _, err = run_main(cam, ["192.0.2.1", "--write", "/nonexistent/blob.bin", "--send"])
        self.assertEqual(code, 1)
        self.assertEqual(cam.sent, [])
        self.assertIn("cannot read", err)

    def test_slot_gate_for_writes(self):
        for slot, extra, sent in [("2", [], True), ("3", [], False), ("3", ["--allow-create"], True), ("4", ["--allow-create"], False)]:
            with self.subTest(slot=slot, extra=extra), tempfile.TemporaryDirectory() as d:
                cam = FakeCamera(standard_table(count=3))
                code, _, _ = run_main(cam, ["192.0.2.1", "--write", self.blob_file(d), "--send", "--slot", slot, "--param2", "1"] + extra)
                self.assertEqual(0x90EE in cam.codes(), sent)
                self.assertEqual(code, 0 if sent else 1)

    def test_a_write_the_camera_does_not_accept_exits_nonzero(self):
        table = standard_table()
        table[0x90EE] = ft.Reply(0x201F, [], b"", False, [])
        with tempfile.TemporaryDirectory() as d:
            code, out, err = run_main(FakeCamera(table), ["192.0.2.1", "--write", self.blob_file(d), "--send"])
        self.assertEqual(code, 1)
        self.assertNotIn("write accepted", out)
        self.assertIn("NOT accepted", err)

    def test_a_write_whose_reply_never_arrives_is_indeterminate_status_4(self):
        def timeout(params, data_out):
            raise TimeoutError("timed out")

        table = standard_table()
        table[0x90EE] = timeout
        with tempfile.TemporaryDirectory() as d:
            code, _, err = run_main(FakeCamera(table), ["192.0.2.1", "--write", self.blob_file(d), "--send"])
        self.assertEqual(code, ft.EXIT_INDETERMINATE)
        self.assertEqual(ft.EXIT_INDETERMINATE, 4)
        self.assertIn("INDETERMINATE", err)

    def test_a_network_error_is_a_clean_nonzero_exit(self):
        def reset(params, data_out):
            raise ConnectionResetError("reset")

        table = standard_table()
        table[0x90EB] = reset
        code, _, err = run_main(FakeCamera(table), ["192.0.2.1"])
        self.assertEqual(code, 1)
        self.assertIn("reset", err)


if __name__ == "__main__":
    unittest.main()

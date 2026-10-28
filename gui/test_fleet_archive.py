import hashlib
import json
import os
import subprocess
import sys
import textwrap
import zipfile
from pathlib import Path

import pytest

import fleet_archive
from conftest import GUI_DIR, PROJECT
from fleet_archive import (
    export_archive, firmware_entry_path, import_archive, remove_firmware_entry,
)
from fleet_lib import accept_zip_entry

_OK_SNAPSHOT = (PROJECT / "tests" / "fixtures" / "snapshot_validity" / "snapshots" / "ok.json").read_bytes()
_BAD_SNAPSHOT = (PROJECT / "tests" / "fixtures" / "snapshot_validity" / "snapshots" / "no_transport.json").read_bytes()
_FUTURE = (PROJECT / "tests" / "fixtures" / "snapshot_validity" / "snapshots" / "future_version.json").read_bytes()

BIN = b"firmware-bytes" * 100


def _meta(model="Z 9", version="5.31", data=BIN, **over):
    doc = {"format_version": 1, "model": model, "firmware_version": version,
           "archived_at": "2026-10-28T00:00:00Z", "archived_by": "fleet firmware add",
           "bin_sha256": hashlib.sha256(data).hexdigest(), "bin_size_bytes": len(data),
           "source_path": "/x.bin", "canonical_snapshot_path": None, "notes": None}
    doc.update(over)
    return json.dumps(doc).encode()


def _firmware(root: Path, model="Z 9", version="5.31", data=BIN, meta=None, page=None):
    d = root / "firmware" / model.replace(" ", "_") / version
    d.mkdir(parents=True)
    (d / "firmware.bin").write_bytes(data)
    (d / "metadata.json").write_bytes(meta if meta is not None else _meta(model, version, data))
    if page is not None:
        (d / "source-page.html").write_bytes(page)
    return d


def _data_dir(tmp_path: Path) -> Path:
    dd = tmp_path / "data"
    (dd / "snapshots").mkdir(parents=True)
    (dd / "references").mkdir()
    (dd / "snapshots" / "a.json").write_bytes(_OK_SNAPSHOT)
    (dd / "snapshots" / "b.json").write_bytes(_OK_SNAPSHOT + b"\n")
    (dd / "references" / "Z_9_0000003023668.json").write_bytes(_OK_SNAPSHOT)
    _firmware(dd, page=b"<html>page</html>")
    return dd


def _zip(path: Path, entries) -> Path:
    with zipfile.ZipFile(path, "w") as zf:
        for name, data in entries:
            zf.writestr(name, data)
    return path


# ── export ────────────────────────────────────────────────────────────────────

class TestExport:
    def test_writes_snapshots_references_and_the_whole_firmware_entry(self, tmp_path):
        dd = _data_dir(tmp_path)
        dest = tmp_path / "out.zip"
        report = export_archive(dd, dest)
        assert report.fatal is None and report.skipped == []
        assert report.written == {"snapshots": 2, "references": 1, "firmware": 3}
        with zipfile.ZipFile(dest) as zf:
            assert sorted(zf.namelist()) == [
                "firmware/Z_9/5.31/firmware.bin", "firmware/Z_9/5.31/metadata.json",
                "firmware/Z_9/5.31/source-page.html", "references/Z_9_0000003023668.json",
                "snapshots/a.json", "snapshots/b.json"]
            assert zf.read("firmware/Z_9/5.31/firmware.bin") == BIN

    def test_everything_exported_is_something_import_accepts(self, tmp_path):
        dd = _data_dir(tmp_path)
        export_archive(dd, tmp_path / "out.zip")
        with zipfile.ZipFile(tmp_path / "out.zip") as zf:
            assert all(accept_zip_entry(n) for n in zf.namelist())

    def test_what_is_left_out_is_counted_with_a_reason(self, tmp_path):
        dd = _data_dir(tmp_path)
        (dd / "snapshots" / "bad.json").write_bytes(_BAD_SNAPSHOT)
        (dd / "snapshots" / "future.json").write_bytes(_FUTURE)
        (dd / "snapshots" / "notes.txt").write_text("hi")
        (dd / "snapshots" / "valid_content.txt").write_bytes(_OK_SNAPSHOT)   # right content, a name import would refuse
        (dd / "snapshots" / "sub").mkdir()
        (dd / "references" / "Z_9_x").mkdir()
        (dd / "firmware" / "Z_9_0531.bin").write_bytes(b"legacy flat")
        (dd / "firmware" / "Z_9" / "5.31" / "stray.txt").write_text("x")
        report = export_archive(dd, tmp_path / "out.zip")
        left = dict(report.skipped)
        assert "snapshots/bad.json" in left and "json" in left["snapshots/bad.json"]
        assert "unsupported_version" in left["snapshots/future.json"]
        assert "snapshots/notes.txt" in left
        assert left["snapshots/valid_content.txt"] == "not a .json file with a plain name"
        assert left["snapshots/sub"] == "not a regular file"
        assert left["references/Z_9_x"] == "not a regular file"
        assert "legacy flat" in left["firmware/Z_9_0531.bin"]
        assert "firmware/Z_9/5.31/stray.txt" in left
        assert report.written == {"snapshots": 2, "references": 1, "firmware": 3}
        assert "Left out 8 item(s)" in report.summary()

    def test_a_firmware_bin_at_any_other_depth_is_not_exported_and_is_counted(self, tmp_path):
        dd = _data_dir(tmp_path)
        (dd / "firmware" / "Z_9" / "firmware.bin").write_bytes(b"too shallow")
        deeper = dd / "firmware" / "Z_9" / "5.31" / "deeper"
        deeper.mkdir()
        (deeper / "firmware.bin").write_bytes(b"too deep")
        report = export_archive(dd, tmp_path / "out.zip")
        with zipfile.ZipFile(tmp_path / "out.zip") as zf:
            assert [n for n in zf.namelist() if n.endswith("firmware.bin")] == ["firmware/Z_9/5.31/firmware.bin"]
        skipped = {n for n, _ in report.skipped}
        assert {"firmware/Z_9/firmware.bin", "firmware/Z_9/5.31/deeper"} <= skipped

    def test_a_firmware_entry_whose_binary_does_not_match_its_metadata_is_left_out_whole(self, tmp_path):
        dd = _data_dir(tmp_path)
        (dd / "firmware" / "Z_9" / "5.31" / "firmware.bin").write_bytes(BIN + b"!")
        report = export_archive(dd, tmp_path / "out.zip")
        assert report.written["firmware"] == 0
        assert any("5.31" in n and ("bytes" in why or "SHA-256" in why) for n, why in report.skipped)

    def test_a_binary_without_metadata_is_left_out_as_incomplete(self, tmp_path):
        dd = _data_dir(tmp_path)
        (dd / "firmware" / "Z_9" / "5.31" / "metadata.json").unlink()
        report = export_archive(dd, tmp_path / "out.zip")
        assert report.written["firmware"] == 0
        assert any("incomplete" in why for _n, why in report.skipped)

    def test_a_symlinked_snapshot_is_left_out(self, tmp_path):
        dd = _data_dir(tmp_path)
        (dd / "snapshots" / "link.json").symlink_to(dd / "snapshots" / "a.json")
        report = export_archive(dd, tmp_path / "out.zip")
        assert ("snapshots/link.json", "not a regular file") in report.skipped

    def test_a_missing_data_dir_exports_an_empty_zip(self, tmp_path):
        report = export_archive(tmp_path / "nothing", tmp_path / "out.zip")
        assert report.fatal is None and sum(report.written.values()) == 0
        assert zipfile.ZipFile(tmp_path / "out.zip").namelist() == []

    def test_a_failure_midway_leaves_the_previous_backup_untouched(self, tmp_path, monkeypatch):
        dd = _data_dir(tmp_path)
        dest = tmp_path / "out.zip"
        dest.write_bytes(b"PREVIOUS BACKUP")
        calls = []
        real = zipfile.ZipFile.write

        def flaky(self, *a, **kw):
            calls.append(a)
            if len(calls) == 3:
                raise OSError(28, "No space left on device")
            return real(self, *a, **kw)
        monkeypatch.setattr(zipfile.ZipFile, "write", flaky)
        report = export_archive(dd, dest)
        assert report.fatal and "No space left" in report.fatal
        assert dest.read_bytes() == b"PREVIOUS BACKUP"
        assert [p.name for p in tmp_path.iterdir() if p.name.startswith(".out.zip")] == []   # temp file removed
        assert "previous export" in report.summary()
        assert sum(report.written.values()) == 0     # nothing is claimed as written

    def test_an_unwritable_destination_is_a_report_not_an_exception(self, tmp_path):
        dd = _data_dir(tmp_path)
        report = export_archive(dd, tmp_path / "no-such-dir" / "out.zip")
        assert report.fatal


# ── import ────────────────────────────────────────────────────────────────────

class TestImport:
    def test_round_trip_preserves_bytes(self, tmp_path):
        src = _data_dir(tmp_path)
        export_archive(src, tmp_path / "out.zip")
        dst = tmp_path / "dst"
        report = import_archive(tmp_path / "out.zip", dst)
        assert report.imported == {"snapshots": 2, "references": 1, "firmware": 3}
        assert report.skipped_existing == [] and report.rejected == [] and report.errors == []
        for rel in ("snapshots/a.json", "snapshots/b.json", "references/Z_9_0000003023668.json",
                    "firmware/Z_9/5.31/firmware.bin", "firmware/Z_9/5.31/metadata.json",
                    "firmware/Z_9/5.31/source-page.html"):
            assert (dst / rel).read_bytes() == (src / rel).read_bytes(), rel

    def test_existing_files_are_never_overwritten_and_are_counted(self, tmp_path):
        zpath = _zip(tmp_path / "in.zip", [("snapshots/a.json", _OK_SNAPSHOT),
                                           ("references/Z_9_0000003023668.json", _OK_SNAPSHOT)])
        dst = tmp_path / "dst"
        (dst / "snapshots").mkdir(parents=True)
        (dst / "references").mkdir()
        (dst / "snapshots" / "a.json").write_bytes(b"MINE")
        (dst / "references" / "Z_9_0000003023668.json").write_bytes(b"MY REFERENCE")
        report = import_archive(zpath, dst)
        assert (dst / "snapshots" / "a.json").read_bytes() == b"MINE"
        assert (dst / "references" / "Z_9_0000003023668.json").read_bytes() == b"MY REFERENCE"
        assert sorted(report.skipped_existing) == ["references/Z_9_0000003023668.json", "snapshots/a.json"]
        assert sum(report.imported.values()) == 0
        assert "skipped 2 already present" in report.summary()

    def test_a_file_that_appears_between_the_check_and_the_write_is_still_not_overwritten(self, tmp_path, monkeypatch):
        # The existence check is a courtesy; the create-new open is the guard.
        monkeypatch.setattr(fleet_archive, "_exists", lambda p: False)
        dst = tmp_path / "dst"
        (dst / "snapshots").mkdir(parents=True)
        (dst / "snapshots" / "a.json").write_bytes(b"MINE")
        report = import_archive(_zip(tmp_path / "in.zip", [("snapshots/a.json", _OK_SNAPSHOT)]), dst)
        assert (dst / "snapshots" / "a.json").read_bytes() == b"MINE"
        assert report.skipped_existing == ["snapshots/a.json"] and report.imported["snapshots"] == 0

    def test_an_existing_firmware_entry_is_left_whole(self, tmp_path):
        src = _data_dir(tmp_path)
        export_archive(src, tmp_path / "out.zip")
        dst = tmp_path / "dst"
        d = _firmware(dst, data=b"OTHER BYTES")
        report = import_archive(tmp_path / "out.zip", dst)
        assert (d / "firmware.bin").read_bytes() == b"OTHER BYTES"
        assert not (d / "source-page.html").exists()
        assert report.imported["firmware"] == 0
        assert len(report.skipped_existing) == 3

    def test_a_dangling_symlink_at_the_destination_counts_as_existing(self, tmp_path):
        dst = tmp_path / "dst"
        (dst / "snapshots").mkdir(parents=True)
        (dst / "snapshots" / "a.json").symlink_to(tmp_path / "nowhere")
        report = import_archive(_zip(tmp_path / "in.zip", [("snapshots/a.json", _OK_SNAPSHOT)]), dst)
        assert report.skipped_existing == ["snapshots/a.json"]
        assert not (tmp_path / "nowhere").exists()

    def test_invalid_snapshot_content_is_rejected_not_counted_as_imported(self, tmp_path):
        zpath = _zip(tmp_path / "in.zip", [
            ("snapshots/good.json", _OK_SNAPSHOT), ("snapshots/bad.json", _BAD_SNAPSHOT),
            ("snapshots/future.json", _FUTURE), ("snapshots/text.json", b"not json"),
            ("references/ref_bad.json", b"{}")])
        dst = tmp_path / "dst"
        report = import_archive(zpath, dst)
        assert report.imported["snapshots"] == 1 and report.imported["references"] == 0
        assert sorted(n for n, _ in report.rejected) == [
            "references/ref_bad.json", "snapshots/bad.json", "snapshots/future.json", "snapshots/text.json"]
        assert not (dst / "snapshots" / "bad.json").exists()

    def test_paths_the_rule_refuses_are_rejected_and_counted(self, tmp_path):
        names = ["snapshots//a.json", "../x.json", "snapshots/./a.json", "other/a.json",
                 "firmware/Z_9//5.31/firmware.bin", "snapshots/a.txt", "firmware/Z_9_0531.bin"]
        zpath = _zip(tmp_path / "in.zip", [(n, _OK_SNAPSHOT) for n in names])
        dst = tmp_path / "dst"
        report = import_archive(zpath, dst)
        assert sorted(n for n, _ in report.rejected) == sorted(names)
        assert sum(report.imported.values()) == 0
        assert not dst.exists() or not any(dst.rglob("*"))

    def test_directory_entries_are_ignored_and_counted_separately(self, tmp_path):
        zpath = _zip(tmp_path / "in.zip", [("snapshots/", b""), ("snapshots/a.json", _OK_SNAPSHOT)])
        report = import_archive(zpath, tmp_path / "dst")
        assert report.ignored_directories == 1 and report.rejected == []
        assert report.imported["snapshots"] == 1

    def test_a_duplicate_entry_name_is_rejected_after_the_first(self, tmp_path):
        with pytest.warns(UserWarning):
            zpath = _zip(tmp_path / "in.zip", [("snapshots/a.json", _OK_SNAPSHOT), ("snapshots/a.json", _BAD_SNAPSHOT)])
        dst = tmp_path / "dst"
        report = import_archive(zpath, dst)
        assert report.imported["snapshots"] == 1
        assert report.rejected == [("snapshots/a.json", "duplicate entry name")]
        assert (dst / "snapshots" / "a.json").read_bytes() == _OK_SNAPSHOT

    def test_per_entry_size_limit_at_below_above(self, tmp_path, monkeypatch):
        body = _OK_SNAPSHOT.rstrip() + b"\n"            # a valid file of known length
        monkeypatch.setattr(fleet_archive, "MAX_JSON_ENTRY_BYTES", len(body))
        zpath = _zip(tmp_path / "in.zip", [("snapshots/below.json", body.rstrip()),
                                           ("snapshots/at.json", body),
                                           ("snapshots/above.json", body + b" ")])
        report = import_archive(zpath, tmp_path / "dst")
        got = sorted(p.name for p in (tmp_path / "dst" / "snapshots").iterdir())
        assert got == ["at.json", "below.json"]
        assert [n for n, _ in report.rejected] == ["snapshots/above.json"]
        assert "limit" in report.rejected[0][1]

    def test_a_header_that_understates_the_size_cannot_smuggle_more_bytes_through(self, tmp_path, monkeypatch):
        # zipfile stops reading at the size the header declares, so an entry that
        # lies about its size arrives truncated and then fails validation.
        zpath = _zip(tmp_path / "in.zip", [("snapshots/a.json", _OK_SNAPSHOT)])
        original_infolist = zipfile.ZipFile.infolist

        def lying(self):
            infos = original_infolist(self)
            for i in infos:
                i.file_size = 10
            return infos
        monkeypatch.setattr(zipfile.ZipFile, "infolist", lying)
        report = import_archive(zpath, tmp_path / "dst")
        assert sum(report.imported.values()) == 0
        assert len(report.rejected) + len(report.errors) == 1
        assert not (tmp_path / "dst" / "snapshots" / "a.json").exists()

    def test_the_total_limit_stops_the_import_and_says_so(self, tmp_path, monkeypatch):
        monkeypatch.setattr(fleet_archive, "MAX_TOTAL_BYTES", len(_OK_SNAPSHOT) * 2)
        names = ["snapshots/a.json", "snapshots/b.json", "snapshots/c.json"]
        zpath = _zip(tmp_path / "in.zip", [(n, _OK_SNAPSHOT) for n in names])
        report = import_archive(zpath, tmp_path / "dst")
        assert report.stopped and "in total" in report.stopped
        assert report.imported["snapshots"] == 2          # at the limit is allowed, one more is not
        assert "stopped early" in report.summary()

    def test_a_write_failure_is_reported_per_file_and_the_rest_continue(self, tmp_path, monkeypatch):
        zpath = _zip(tmp_path / "in.zip", [("snapshots/a.json", _OK_SNAPSHOT), ("snapshots/b.json", _OK_SNAPSHOT),
                                           ("references/Z_9_0000003023668.json", _OK_SNAPSHOT)])
        real = fleet_archive._write_new

        def failing(path, data):
            if path.name == "a.json":
                raise OSError(28, "No space left on device")
            return real(path, data)
        monkeypatch.setattr(fleet_archive, "_write_new", failing)
        report = import_archive(zpath, tmp_path / "dst")
        assert [n for n, _ in report.errors] == ["snapshots/a.json"]
        assert "No space left" in report.errors[0][1]
        assert report.imported == {"snapshots": 1, "references": 1, "firmware": 0}
        assert "1 failed" in report.summary()

    def test_a_directory_in_the_way_is_a_reported_error_not_a_crash(self, tmp_path):
        dst = tmp_path / "dst"
        dst.mkdir()
        (dst / "snapshots").write_text("a file where a directory should be")
        report = import_archive(_zip(tmp_path / "in.zip", [("snapshots/a.json", _OK_SNAPSHOT)]), dst)
        assert len(report.errors) == 1 and sum(report.imported.values()) == 0

    def test_a_file_that_is_not_a_zip_is_fatal_and_imports_nothing(self, tmp_path):
        bogus = tmp_path / "x.zip"
        bogus.write_bytes(b"this is not a zip")
        report = import_archive(bogus, tmp_path / "dst")
        assert report.fatal and "Nothing was imported" in report.summary()

    def test_a_missing_file_is_fatal(self, tmp_path):
        assert import_archive(tmp_path / "gone.zip", tmp_path / "dst").fatal

    def test_a_corrupt_entry_is_one_error_and_the_others_still_import(self, tmp_path):
        zpath = tmp_path / "in.zip"
        with zipfile.ZipFile(zpath, "w", zipfile.ZIP_DEFLATED) as zf:
            zf.writestr("snapshots/a.json", _OK_SNAPSHOT)
            zf.writestr("snapshots/b.json", _OK_SNAPSHOT)
        raw = bytearray(zpath.read_bytes())
        # flip a byte inside the first entry's compressed data (after its 30-byte header + name)
        raw[30 + len("snapshots/a.json") + 5] ^= 0xFF
        zpath.write_bytes(bytes(raw))
        report = import_archive(zpath, tmp_path / "dst")
        assert len(report.errors) + len(report.rejected) == 1
        assert report.imported["snapshots"] == 1

    # firmware

    def test_a_firmware_entry_with_a_wrong_checksum_is_rejected_whole(self, tmp_path):
        meta = _meta(bin_sha256="0" * 64)
        zpath = _zip(tmp_path / "in.zip", [("firmware/Z_9/5.31/firmware.bin", BIN),
                                           ("firmware/Z_9/5.31/metadata.json", meta)])
        dst = tmp_path / "dst"
        report = import_archive(zpath, dst)
        assert report.imported["firmware"] == 0
        assert sorted(n for n, _ in report.rejected) == ["firmware/Z_9/5.31/firmware.bin", "firmware/Z_9/5.31/metadata.json"]
        assert all("SHA-256" in why for _, why in report.rejected)
        assert not (dst / "firmware" / "Z_9" / "5.31" / "firmware.bin").exists()
        assert not (dst / "firmware" / "Z_9" / "5.31" / "firmware.bin.part").exists()

    def test_a_truncated_binary_is_rejected_by_its_recorded_size(self, tmp_path):
        zpath = _zip(tmp_path / "in.zip", [("firmware/Z_9/5.31/firmware.bin", BIN[:-1]),
                                           ("firmware/Z_9/5.31/metadata.json", _meta())])
        report = import_archive(zpath, tmp_path / "dst")
        assert report.imported["firmware"] == 0 and "bytes" in report.rejected[0][1]

    def test_a_binary_without_metadata_and_metadata_without_a_binary_are_rejected(self, tmp_path):
        zpath = _zip(tmp_path / "in.zip", [("firmware/Z_9/5.31/firmware.bin", BIN),
                                           ("firmware/Z_6/1.00/metadata.json", _meta("Z 6", "1.00"))])
        report = import_archive(zpath, tmp_path / "dst")
        assert report.imported["firmware"] == 0
        assert len(report.rejected) == 2 and all("incomplete" in why for _, why in report.rejected)

    def test_a_directory_that_does_not_match_the_metadata_is_rejected(self, tmp_path):
        zpath = _zip(tmp_path / "in.zip", [("firmware/Z_8/5.31/firmware.bin", BIN),
                                           ("firmware/Z_8/5.31/metadata.json", _meta("Z 9", "5.31"))])
        report = import_archive(zpath, tmp_path / "dst")
        assert report.imported["firmware"] == 0 and "directory" in report.rejected[0][1]

    def test_a_metadata_with_the_wrong_format_version_is_rejected(self, tmp_path):
        zpath = _zip(tmp_path / "in.zip", [("firmware/Z_9/5.31/firmware.bin", BIN),
                                           ("firmware/Z_9/5.31/metadata.json", _meta(format_version=2))])
        assert import_archive(zpath, tmp_path / "dst").imported["firmware"] == 0

    def test_a_failed_firmware_import_leaves_no_partial_entry(self, tmp_path, monkeypatch):
        zpath = _zip(tmp_path / "in.zip", [("firmware/Z_9/5.31/firmware.bin", BIN),
                                           ("firmware/Z_9/5.31/metadata.json", _meta())])
        real = fleet_archive._publish

        def failing(tmp, dest):
            if dest.name == "metadata.json":
                real.__globals__["_discard"](tmp)
                raise OSError(5, "I/O error")
            return real(tmp, dest)
        monkeypatch.setattr(fleet_archive, "_publish", failing)
        dst = tmp_path / "dst"
        report = import_archive(zpath, dst)
        assert report.imported["firmware"] == 0 and len(report.errors) == 1
        assert not (dst / "firmware").exists()        # no firmware.bin without its metadata.json, no empty directories

    def test_the_metadata_is_written_last_so_a_crash_never_leaves_a_listed_half_entry(self, tmp_path, monkeypatch):
        order = []
        real = fleet_archive._publish
        monkeypatch.setattr(fleet_archive, "_publish", lambda tmp, dest: (order.append(dest.name), real(tmp, dest))[1])
        zpath = _zip(tmp_path / "in.zip", [("firmware/Z_9/5.31/firmware.bin", BIN),
                                           ("firmware/Z_9/5.31/metadata.json", _meta()),
                                           ("firmware/Z_9/5.31/source-page.html", b"<html/>")])
        assert import_archive(zpath, tmp_path / "dst").imported["firmware"] == 3
        assert order == ["firmware.bin", "source-page.html", "metadata.json"]

    def test_firmware_binary_size_limit_at_below_above(self, tmp_path, monkeypatch):
        monkeypatch.setattr(fleet_archive, "MAX_BIN_ENTRY_BYTES", len(BIN))
        at = _zip(tmp_path / "at.zip", [("firmware/Z_9/5.31/firmware.bin", BIN),
                                        ("firmware/Z_9/5.31/metadata.json", _meta())])
        assert import_archive(at, tmp_path / "dst").imported["firmware"] == 2
        big = BIN + b"x"
        above = _zip(tmp_path / "above.zip", [("firmware/Z_9/5.32/firmware.bin", big),
                                              ("firmware/Z_9/5.32/metadata.json", _meta(version="5.32", data=big))])
        report = import_archive(above, tmp_path / "dst2")
        assert report.imported["firmware"] == 0 and any("limit" in why for _, why in report.rejected)

    def test_entry_count_limit_at_below_above(self, tmp_path, monkeypatch):
        monkeypatch.setattr(fleet_archive, "MAX_ENTRIES", 2)
        for n, refused in ((1, False), (2, False), (3, True)):
            zpath = _zip(tmp_path / f"in{n}.zip", [(f"snapshots/{i}.json", _OK_SNAPSHOT) for i in range(n)])
            report = import_archive(zpath, tmp_path / f"dst{n}")
            assert bool(report.fatal) is refused, n
            if refused:
                assert "entries" in report.fatal and sum(report.imported.values()) == 0

    def test_importing_twice_imports_nothing_the_second_time(self, tmp_path):
        src = _data_dir(tmp_path)
        export_archive(src, tmp_path / "out.zip")
        dst = tmp_path / "dst"
        import_archive(tmp_path / "out.zip", dst)
        again = import_archive(tmp_path / "out.zip", dst)
        assert sum(again.imported.values()) == 0
        assert len(again.skipped_existing) == 6


# ── a write that fails for real (RLIMIT_FSIZE), not a monkeypatched one ──────────

_LIMITED_IMPORT = textwrap.dedent("""
    import json, resource, signal, sys
    sys.dont_write_bytecode = True
    sys.path.insert(0, {gui!r})
    from fleet_archive import import_archive
    signal.signal(signal.SIGXFSZ, signal.SIG_IGN)      # the write fails with EFBIG instead of killing us
    resource.setrlimit(resource.RLIMIT_FSIZE, ({limit}, {limit}))
    r = import_archive(sys.argv[1], sys.argv[2])
    print(json.dumps({{"imported": r.imported, "errors": [n for n, _ in r.errors],
                      "skipped": r.skipped_existing, "fatal": r.fatal}}))
""")


def _import_with_file_size_limit(zpath, dst, limit):
    out = subprocess.run([sys.executable, "-c", _LIMITED_IMPORT.format(gui=str(GUI_DIR), limit=limit),
                          str(zpath), str(dst)], capture_output=True, text=True, timeout=60, check=False)
    assert out.returncode == 0, out.stderr
    return json.loads(out.stdout)


def _files_under(root: Path) -> list:
    return sorted(str(p.relative_to(root)) for p in root.rglob("*") if p.is_file() or p.is_symlink())


class TestAFailedWriteLeavesNothing:
    def test_a_snapshot_that_cannot_be_written_leaves_no_file_and_can_be_imported_later(self, tmp_path):
        assert len(_OK_SNAPSHOT) > 100
        zpath = _zip(tmp_path / "in.zip", [("snapshots/a.json", _OK_SNAPSHOT)])
        dst = tmp_path / "dst"
        got = _import_with_file_size_limit(zpath, dst, 100)
        assert got["errors"] == ["snapshots/a.json"] and got["imported"]["snapshots"] == 0
        assert _files_under(dst) == []                      # no truncated file, no temporary file
        again = import_archive(zpath, dst)                  # a later import is not told "already present"
        assert again.imported["snapshots"] == 1 and again.skipped_existing == []
        assert (dst / "snapshots" / "a.json").read_bytes() == _OK_SNAPSHOT

    def test_a_reference_that_cannot_be_written_leaves_no_file(self, tmp_path):
        zpath = _zip(tmp_path / "in.zip", [("references/Z_9_0000003023668.json", _OK_SNAPSHOT)])
        dst = tmp_path / "dst"
        got = _import_with_file_size_limit(zpath, dst, 100)
        assert got["errors"] == ["references/Z_9_0000003023668.json"]
        assert _files_under(dst) == []

    def test_a_binary_that_cannot_be_written_leaves_no_entry(self, tmp_path):
        zpath = _zip(tmp_path / "in.zip", [("firmware/Z_9/5.31/firmware.bin", BIN),
                                           ("firmware/Z_9/5.31/metadata.json", _meta())])
        dst = tmp_path / "dst"
        got = _import_with_file_size_limit(zpath, dst, 100)
        assert got["errors"] == ["firmware/Z_9/5.31"] and got["imported"]["firmware"] == 0
        assert _files_under(dst) == [] and not (dst / "firmware").exists()

    def test_metadata_that_cannot_be_written_takes_back_the_binary_and_a_truncated_file_never_appears(self, tmp_path):
        small = b"x" * 50
        meta = _meta(data=small)
        assert 200 < len(meta)                              # the binary fits under the limit, the metadata does not
        zpath = _zip(tmp_path / "in.zip", [("firmware/Z_9/5.31/firmware.bin", small),
                                           ("firmware/Z_9/5.31/metadata.json", meta)])
        dst = tmp_path / "dst"
        got = _import_with_file_size_limit(zpath, dst, 200)
        assert got["errors"] == ["firmware/Z_9/5.31"]
        assert _files_under(dst) == []
        again = import_archive(zpath, dst)
        assert again.imported["firmware"] == 2 and again.skipped_existing == []     # not "already present" forever
        assert (dst / "firmware/Z_9/5.31/metadata.json").read_bytes() == meta


class TestNothingIsClobbered:
    def _zip_entry(self, tmp_path):
        return _zip(tmp_path / "in.zip", [("firmware/Z_9/5.31/firmware.bin", BIN),
                                          ("firmware/Z_9/5.31/metadata.json", _meta())])

    def test_an_existing_part_file_is_left_untouched(self, tmp_path):
        dst = tmp_path / "dst"
        d = dst / "firmware" / "Z_9" / "5.31"
        d.mkdir(parents=True)
        (d / "firmware.bin.part").write_bytes(b"a running download")
        report = import_archive(self._zip_entry(tmp_path), dst)
        assert (d / "firmware.bin.part").read_bytes() == b"a running download"
        assert _files_under(dst) == ["firmware/Z_9/5.31/firmware.bin.part"]
        assert len(report.skipped_existing) == 2 and report.imported["firmware"] == 0

    def test_an_existing_binary_without_metadata_is_left_untouched(self, tmp_path):
        dst = tmp_path / "dst"
        d = dst / "firmware" / "Z_9" / "5.31"
        d.mkdir(parents=True)
        (d / "firmware.bin").write_bytes(b"someone else's bytes")
        report = import_archive(self._zip_entry(tmp_path), dst)
        assert (d / "firmware.bin").read_bytes() == b"someone else's bytes"
        assert _files_under(dst) == ["firmware/Z_9/5.31/firmware.bin"]
        assert report.imported["firmware"] == 0

    def test_a_binary_that_appears_after_the_check_is_not_replaced_and_nothing_else_is_left(self, tmp_path, monkeypatch):
        monkeypatch.setattr(fleet_archive, "_exists", lambda p: False)      # the pre-check misses it
        dst = tmp_path / "dst"
        d = dst / "firmware" / "Z_9" / "5.31"
        d.mkdir(parents=True)
        (d / "firmware.bin").write_bytes(b"appeared meanwhile")
        report = import_archive(self._zip_entry(tmp_path), dst)
        assert (d / "firmware.bin").read_bytes() == b"appeared meanwhile"
        assert _files_under(dst) == ["firmware/Z_9/5.31/firmware.bin"]      # no metadata, no temporary file
        assert len(report.skipped_existing) == 2

    def test_a_part_file_that_appears_after_the_check_survives_a_conflicting_import(self, tmp_path, monkeypatch):
        # The import's own staging name is unique, so it cannot collide with, or delete, a .part.
        real = fleet_archive._publish

        def intruder(tmp, dest):
            if dest.name == "firmware.bin":
                (dest.parent / "firmware.bin.part").write_bytes(b"a download that started meanwhile")
            return real(tmp, dest)
        monkeypatch.setattr(fleet_archive, "_publish", intruder)
        dst = tmp_path / "dst"
        report = import_archive(self._zip_entry(tmp_path), dst)
        assert (dst / "firmware/Z_9/5.31/firmware.bin.part").read_bytes() == b"a download that started meanwhile"
        assert report.imported["firmware"] == 2

    def test_a_later_file_that_appears_takes_back_only_what_this_import_made(self, tmp_path, monkeypatch):
        real_exists = fleet_archive._exists
        monkeypatch.setattr(fleet_archive, "_exists", lambda p: False if p.name == "source-page.html" else real_exists(p))
        zpath = _zip(tmp_path / "in.zip", [("firmware/Z_9/5.31/firmware.bin", BIN),
                                           ("firmware/Z_9/5.31/metadata.json", _meta()),
                                           ("firmware/Z_9/5.31/source-page.html", b"<html/>")])
        dst = tmp_path / "dst"
        d = dst / "firmware" / "Z_9" / "5.31"
        d.mkdir(parents=True)
        (d / "source-page.html").write_bytes(b"theirs")
        report = import_archive(zpath, dst)
        assert (d / "source-page.html").read_bytes() == b"theirs"
        assert _files_under(dst) == ["firmware/Z_9/5.31/source-page.html"]
        assert report.imported["firmware"] == 0

    def test_a_directory_the_import_created_is_removed_when_it_rejects_the_entry(self, tmp_path):
        zpath = _zip(tmp_path / "in.zip", [("firmware/Z_9/5.31/firmware.bin", BIN),
                                           ("firmware/Z_9/5.31/metadata.json", _meta(bin_sha256="0" * 64))])
        dst = tmp_path / "dst"
        import_archive(zpath, dst)
        assert not (dst / "firmware").exists()

    def test_a_directory_that_was_already_there_is_kept(self, tmp_path):
        zpath = _zip(tmp_path / "in.zip", [("firmware/Z_9/5.31/firmware.bin", BIN),
                                           ("firmware/Z_9/5.31/metadata.json", _meta(bin_sha256="0" * 64))])
        dst = tmp_path / "dst"
        (dst / "firmware" / "Z_9").mkdir(parents=True)
        import_archive(zpath, dst)
        assert (dst / "firmware" / "Z_9").is_dir() and not (dst / "firmware" / "Z_9" / "5.31").exists()


class TestNameTiedToContent:
    def test_a_reference_under_its_cameras_own_name_is_imported(self, tmp_path):
        zpath = _zip(tmp_path / "in.zip", [("references/Z_9_0000003023668.json", _OK_SNAPSHOT)])
        report = import_archive(zpath, tmp_path / "dst")
        assert report.imported["references"] == 1 and report.rejected == []

    @pytest.mark.parametrize("name", ["references/Z_9_OTHER.json", "references/other.json",
                                      "references/Z 9_0000003023668.json", "references/z_9_0000003023668.json"])
    def test_a_reference_under_any_other_name_is_rejected_and_counted(self, tmp_path, name):
        report = import_archive(_zip(tmp_path / "in.zip", [(name, _OK_SNAPSHOT)]), tmp_path / "dst")
        assert report.imported["references"] == 0
        assert [n for n, _ in report.rejected] == [name] and "referenced as Z_9_0000003023668.json" in report.rejected[0][1]
        assert not (tmp_path / "dst").exists() or _files_under(tmp_path / "dst") == []

    @pytest.mark.parametrize("serial", ["../escape", "a/b", "a\\b", ".", "..", "", "a\x00b"])
    def test_a_reference_whose_camera_is_not_a_plain_name_is_rejected(self, tmp_path, serial):
        doc = json.loads(_OK_SNAPSHOT)
        doc["camera"]["serial"] = serial
        raw = json.dumps(doc).encode()
        report = import_archive(_zip(tmp_path / "in.zip", [("references/Z_9_x.json", raw)]), tmp_path / "dst")
        assert report.imported["references"] == 0 and len(report.rejected) == 1
        assert "plain file-name component" in report.rejected[0][1] or "referenced as" in report.rejected[0][1]

    def test_snapshots_are_not_tied_to_a_name(self, tmp_path):
        report = import_archive(_zip(tmp_path / "in.zip", [("snapshots/anything at all.json", _OK_SNAPSHOT)]),
                                tmp_path / "dst")
        assert report.imported["snapshots"] == 1

    def test_export_leaves_out_a_reference_import_would_reject(self, tmp_path):
        dd = _data_dir(tmp_path)
        (dd / "references" / "Z_9_WRONG.json").write_bytes(_OK_SNAPSHOT)
        report = export_archive(dd, tmp_path / "out.zip")
        assert report.written["references"] == 1
        assert [n for n, why in report.skipped if "referenced as" in why] == ["references/Z_9_WRONG.json"]

    def test_a_firmware_entry_whose_metadata_model_matches_its_directory_is_imported(self, tmp_path):
        zpath = _zip(tmp_path / "in.zip", [("firmware/Z6_3/2.00/firmware.bin", BIN),
                                           ("firmware/Z6_3/2.00/metadata.json", _meta("Z6_3", "2.00"))])
        assert import_archive(zpath, tmp_path / "dst").imported["firmware"] == 2

    @pytest.mark.parametrize("model,version,folder", [("Z 9", "5.31", "Z_8/5.31"), ("Z 9", "5.32", "Z_9/5.31"),
                                                     ("Z6_3", "2.00", "Z_6_3/2.00")])
    def test_a_firmware_entry_whose_metadata_names_another_entry_is_rejected(self, tmp_path, model, version, folder):
        zpath = _zip(tmp_path / "in.zip", [(f"firmware/{folder}/firmware.bin", BIN),
                                           (f"firmware/{folder}/metadata.json", _meta(model, version))])
        report = import_archive(zpath, tmp_path / "dst")
        assert report.imported["firmware"] == 0 and len(report.rejected) == 2


class TestExportOddities:
    def test_a_file_dated_before_1980_does_not_fail_the_export(self, tmp_path):
        dd = _data_dir(tmp_path)
        os.utime(dd / "snapshots" / "a.json", (0, 0))
        report = export_archive(dd, tmp_path / "out.zip")
        assert report.fatal is None and report.written["snapshots"] == 2
        with zipfile.ZipFile(tmp_path / "out.zip") as zf:
            assert zf.read("snapshots/a.json") == _OK_SNAPSHOT
            assert zf.getinfo("snapshots/a.json").date_time[0] >= 1980


# ── removing a firmware entry ─────────────────────────────────────────────────

class TestRemoveFirmwareEntry:
    def test_removes_the_entry_and_the_empty_model_directory(self, tmp_path):
        root = tmp_path / "firmware"
        d = _firmware(tmp_path, page=b"x")
        remove_firmware_entry(root, str(d))
        assert not d.exists() and not (root / "Z_9").exists() and root.exists()

    def test_keeps_the_model_directory_while_another_version_is_there(self, tmp_path):
        root = tmp_path / "firmware"
        d1 = _firmware(tmp_path, version="5.30")
        d2 = _firmware(tmp_path, version="5.31")
        remove_firmware_entry(root, str(d1))
        assert not d1.exists() and d2.exists()

    def test_other_archives_are_untouched(self, tmp_path):
        root = tmp_path / "firmware"
        _firmware(tmp_path, "Z 9", "5.31")
        other = _firmware(tmp_path, "Z6_3", "2.00")
        remove_firmware_entry(root, str(root / "Z_9" / "5.31"))
        assert (other / "firmware.bin").exists()

    def test_a_directory_with_unknown_files_is_not_deleted_as_a_tree(self, tmp_path):
        root = tmp_path / "firmware"
        d = _firmware(tmp_path)
        (d / "mine.txt").write_text("keep")
        with pytest.raises(OSError):
            remove_firmware_entry(root, str(d))
        assert (d / "mine.txt").read_text() == "keep"

    @pytest.mark.parametrize("rel", [".", "Z_9", "Z_9/5.31/extra", "../elsewhere/x", "Z_9/..", "Z_9/5.31/.."])
    def test_a_path_that_is_not_exactly_model_slash_version_is_refused(self, tmp_path, rel):
        root = tmp_path / "firmware"
        _firmware(tmp_path)
        (tmp_path / "elsewhere" / "x").mkdir(parents=True)
        (tmp_path / "elsewhere" / "x" / "firmware.bin").write_bytes(b"precious")
        with pytest.raises(ValueError):
            remove_firmware_entry(root, str(root / rel))
        assert (root / "Z_9" / "5.31" / "firmware.bin").exists()
        assert (tmp_path / "elsewhere" / "x" / "firmware.bin").exists()

    def test_a_path_outside_the_root_is_refused_even_when_its_tail_names_a_real_entry(self, tmp_path):
        root = tmp_path / "firmware"
        _firmware(tmp_path)                               # root/Z_9/5.31 exists
        outside = tmp_path / "other" / "Z_9" / "5.31"
        outside.mkdir(parents=True)
        (outside / "firmware.bin").write_bytes(b"not ours")
        with pytest.raises(ValueError):
            firmware_entry_path(root, str(outside))
        with pytest.raises(ValueError):
            remove_firmware_entry(root, str(outside))
        assert (outside / "firmware.bin").read_bytes() == b"not ours"
        assert (root / "Z_9" / "5.31" / "firmware.bin").exists()

    def test_a_symlinked_version_directory_is_refused(self, tmp_path):
        root = tmp_path / "firmware"
        real = tmp_path / "real"
        real.mkdir()
        (real / "firmware.bin").write_bytes(b"precious")
        (root / "Z_9").mkdir(parents=True)
        (root / "Z_9" / "5.31").symlink_to(real)
        with pytest.raises(ValueError):
            remove_firmware_entry(root, str(root / "Z_9" / "5.31"))
        assert (real / "firmware.bin").exists()

    def test_a_symlinked_model_directory_is_refused(self, tmp_path):
        root = tmp_path / "firmware"
        root.mkdir()
        elsewhere = tmp_path / "elsewhere"
        (elsewhere / "5.31").mkdir(parents=True)
        (elsewhere / "5.31" / "firmware.bin").write_bytes(b"precious")
        (root / "Z_9").symlink_to(elsewhere)
        with pytest.raises(ValueError):
            remove_firmware_entry(root, str(root / "Z_9" / "5.31"))
        assert (elsewhere / "5.31" / "firmware.bin").exists()

    def test_an_entry_the_cli_reported_as_unreadable_can_be_removed(self, tmp_path):
        root = tmp_path / "firmware"
        d = _firmware(tmp_path, meta=b"{ nope")
        remove_firmware_entry(root, str(d))
        assert not d.exists()

    def test_a_leftover_part_file_goes_with_the_entry(self, tmp_path):
        root = tmp_path / "firmware"
        d = _firmware(tmp_path)
        (d / "firmware.bin.part").write_bytes(b"half")
        remove_firmware_entry(root, str(d))
        assert not d.exists()


def test_the_symlink_cases_need_os_support():
    assert hasattr(os, "symlink")

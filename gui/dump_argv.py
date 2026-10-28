"""Print, as JSON, every argv the Tk GUI can build, for tests/gui_cli_argv_parity.rs.

The Rust test feeds each one to the real `nikon-fleet` with NIKON_FLEET_PARSE_ONLY=1 (parse, run nothing), so a flag renamed on the
Rust side (or a flag the GUI invents) fails `cargo test` instead of a user's click. Every free-text
value is dashed (`-guest`) to pin the `--flag=value` form; every builder in fleet_lib that the GUI
calls is listed here, so a new builder with no entry is the gap to close.
"""
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import fleet_lib as L

DATA = "/tmp/fleet data"
DASHED = "-guest"


def ftp_values() -> dict:
    values = {key: DASHED for key, _flag, _label in L.FTP_TEXT_FIELDS}
    values["port"] = 21
    return values


def cases() -> list:
    g = L.global_argv(DATA)
    ftp = ftp_values()
    return [
        ("discover", g + L.discover_argv()),
        ("ls", g + L.ls_argv()),
        ("firmware ls", g + L.firmware_ls_argv()),
        ("settings show", L.global_argv(DATA)[:1] + L.settings_show_argv()),
        ("settings set-data-dir", L.set_data_dir_argv("/tmp/x")),
        ("settings set-data-dir --unset", L.set_data_dir_argv(None)),
        ("snapshot", g + L.snapshot_argv(DASHED, DASHED)),
        ("snapshot no label", g + L.snapshot_argv("123", "")),
        ("ref set", g + L.ref_set_argv(DATA, "Z_9_1_a_20260101T000000Z.json")),
        ("firmware add", g + L.firmware_add_argv("/tmp/-fw.bin", DASHED, DASHED)),
        ("firmware add --force", g + L.firmware_add_argv("/tmp/fw.bin", "Z 9", "5.31", force=True)),
        ("firmware browse", g + L.firmware_browse_argv("-Z 9")),
        ("vendor-read", L.global_argv(DATA)[:1] + L.vendor_read_argv(0xD053, DASHED)),
        ("vendor-read no serial", L.global_argv(DATA)[:1] + L.vendor_read_argv(0xD053, None)),
        ("vendor-write-ftp", L.global_argv(DATA)[:1] + L.vendor_write_ftp_argv(ftp, DASHED, dry_run=False)),
        ("vendor-write-ftp dry-run", L.global_argv(DATA)[:1] + L.vendor_write_ftp_argv(ftp, None, dry_run=True)),
    ]


if __name__ == "__main__":
    json.dump([{"name": name, "argv": argv} for name, argv in cases()], sys.stdout)

//! Guards the boundary flagged in full-review nikon-fleet-20260909-7b2e#16:
//! `gui/fleet_gui.py` builds `vendor-write-ftp`/`vendor-read` argv by hand
//! (it can't call into the Rust crate directly, unlike the egui GUI), and
//! nothing previously caught a future rename/removal of a flag on the Rust
//! side before a user hit the resulting clap parse error at runtime.
//!
//! This isn't a full argv-vs-argv comparison (that would need parsing
//! Python), just a one-way check: every flag `fleet_gui.py` is known to
//! pass is still accepted by the compiled CLI's own `--help` output. Keep
//! the flag lists below in sync with `gui/fleet_gui.py`'s `_do_read`/
//! `_do_write` argv-building code if either side changes.

use std::process::Command;

fn cli_help(args: &[&str]) -> String {
    let bin = env!("CARGO_BIN_EXE_nikon-fleet");
    let output = Command::new(bin).args(args).output().expect("failed to run CLI binary");
    // clap prints --help to stdout and exits 0.
    String::from_utf8(output.stdout).expect("CLI --help output was not valid UTF-8")
}

#[test]
fn vendor_write_ftp_accepts_every_flag_fleet_gui_py_sends() {
    let help = cli_help(&["vendor-write-ftp", "--help"]);
    for flag in [
        "--profile-name",
        "--ssid-24ghz",
        "--ssid-5ghz",
        "--host",
        "--port",
        "--username",
        "--password-stdin",
        "--serial",
        "--dry-run",
    ] {
        assert!(
            help.contains(flag),
            "vendor-write-ftp --help does not mention {flag:?} — gui/fleet_gui.py's \
             _do_write builds this flag into its argv and would break silently \
             at runtime if it were renamed/removed.\n\nFull --help output:\n{help}"
        );
    }
}

#[test]
fn vendor_read_accepts_every_flag_fleet_gui_py_sends() {
    let help = cli_help(&["vendor-read", "--help"]);
    for flag in ["--serial"] {
        assert!(
            help.contains(flag),
            "vendor-read --help does not mention {flag:?} — gui/fleet_gui.py's \
             _do_read builds this flag into its argv.\n\nFull --help output:\n{help}"
        );
    }
    // vendor-read's propcode is a positional argument, not a flag — clap
    // still names it in --help; a plain presence check is enough to catch
    // a rename of the argument itself breaking the CLI's own usage text.
    assert!(
        help.to_lowercase().contains("propcode"),
        "vendor-read --help no longer mentions a propcode argument.\n\nFull --help output:\n{help}"
    );
}

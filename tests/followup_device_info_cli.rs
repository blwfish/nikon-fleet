//! `nikon-fleet device-info`, through the real binary, without a camera: its `--help` (the
//! read-only claim and the verdict vocabulary a script author needs), its flags, and that it is
//! a CLI-only command. What it prints for a dataset is pinned by the unit tests in `src/main.rs`;
//! reading a real body needs hardware.

use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("run CLI")
}

fn parse_only(args: &[&str]) -> Output {
    Command::new(BIN).env("NIKON_FLEET_PARSE_ONLY", "1").args(args).output().expect("run CLI")
}

fn help() -> String {
    let out = run(&["device-info", "--help"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).expect("help is UTF-8")
}

#[test]
fn help_says_it_is_read_only_and_uses_the_raw_ptp_path_like_vendor_read() {
    let h = help();
    assert!(h.contains("Read-only"), "{h}");
    assert!(h.contains("changes nothing"), "{h}");
    assert!(h.contains("raw PTP path like vendor-read"), "{h}");
    assert!(h.contains("0x1001"), "{h}");
}

#[test]
fn help_names_the_serial_line_and_every_verdict() {
    let h = help();
    assert!(h.contains("match|differ|ptp_empty|usb_empty|both_empty"), "{h}");
    for verdict in ["match", "differ", "ptp_empty", "usb_empty", "both_empty"] {
        assert!(h.contains(verdict), "{verdict} missing from:\n{h}");
    }
    assert!(h.contains("--serial") && h.contains("--json"), "{h}");
    assert!(h.contains("Exit status: 0"), "{h}");
}

#[test]
fn the_flags_parse_and_unknown_ones_are_a_usage_error() {
    for args in [
        vec!["device-info"],
        vec!["device-info", "--json"],
        vec!["device-info", "--serial", "0000003023668"],
        vec!["device-info", "--serial=id-1", "--json"],
    ] {
        let out = parse_only(&args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
    for args in [vec!["device-info", "--bogus"], vec!["device-info", "0xD053"], vec!["device-info", "--serial"]] {
        assert_eq!(parse_only(&args).status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn the_command_is_listed_in_the_top_level_help_and_vendor_read_is_unchanged() {
    let out = run(&["--help"]);
    let top = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(top.contains("device-info"), "{top}");
    let vr = run(&["vendor-read", "--help"]);
    let vr = String::from_utf8_lossy(&vr.stdout).into_owned();
    assert!(vr.contains("propcode") || vr.contains("PROPCODE"), "{vr}");
    assert!(!vr.contains("device-info"), "{vr}");
}

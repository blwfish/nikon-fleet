//! The FTP-profile write commands through the real binary, for everything that needs no camera:
//! what `--help` promises (the exit statuses, the two write forms, what `--port` is), argument
//! parsing, and the refusals that come before any USB I/O.
//!
//! Findings nikon-fleet-20261105-b7d3#05, #11, #12, #13, #37.

use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("run CLI")
}

fn help_of(args: &[&str]) -> String {
    let out = run(&[args, &["--help"]].concat());
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    // clap wraps long help; compare with single spaces.
    String::from_utf8(out.stdout).expect("help is UTF-8").split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_blob_command_documents_its_two_write_forms_the_reorder_and_every_exit_status() {
    let help = help_of(&["vendor-write-ftp-blob"]);
    for needle in [
        "Exit status:",
        "0 the camera accepted the profile and a read-back of slot 0 showed exactly what was sent",
        "1 any other error",
        "2 command-line usage error",
        "4 the camera MAY have changed",
        "answered OK but the read-back did not match",
        "--slot N --param2 0 replace the stored profile N",
        "--slot COUNT --param2 1 add a profile at the FRONT",
        "--slot COUNT --param2 0 does NOT add",
    ] {
        assert!(help.contains(needle), "{needle:?} missing from:\n{help}");
    }
    assert!(!help.contains("untested"), "the add was done on a Z 9 over USB: {help}");
    assert!(!help.contains("idempotent"), "{help}");
    // A repeated write is not harmless, and the help says why.
    assert!(help.contains("a repeated add stores a duplicate"), "{help}");
}

#[test]
fn the_write_commands_say_that_port_is_not_the_ftp_port() {
    let help = help_of(&["vendor-write-ftp"]);
    assert!(help.contains("It is NOT the FTP port"), "{help}");
    assert!(help.contains("the encoder fixes that at 21"), "{help}");
    assert!(help.contains("8080 in every profile read from a camera"), "{help}");
}

#[test]
fn the_exit_four_advice_does_not_send_the_reader_to_a_camera_they_have_not_tried() {
    for cmd in ["vendor-write-ftp", "vendor-write-ftp-blob", "vendor-read-ftp", "vendor-op-support"] {
        let help = help_of(&[cmd]);
        assert!(!help.contains("not yet tried on a camera"), "{cmd}: {help}");
        assert!(!help.contains("Never run over USB"), "{cmd}: {help}");
        assert!(!help.contains("has not been run over USB"), "{cmd}: {help}");
    }
    assert!(help_of(&["vendor-write-ftp"]).contains("vendor-read-ftp --slot 0"));
}

#[test]
fn vendor_set_string_help_says_artist_and_copyright_need_the_force_flag() {
    let help = help_of(&["vendor-set-string"]);
    assert!(help.contains("Artist and Copyright have never been written on any body"), "{help}");
    assert!(help.contains("refused without this"), "{help}");
    assert!(help.contains("Runs the same checks as the write"), "--dry-run is documented as validating: {help}");
}

#[test]
fn param2_takes_decimal_or_prefixed_hex_through_the_one_shared_parser() {
    // clap validates the arguments before it prints the help that follows them.
    for ok in ["0", "1", "0x1", "0X1", "4294967295", "0xFFFFFFFF"] {
        let out = run(&["vendor-write-ftp-blob", "--file", "x", "--param2", ok, "--help"]);
        assert_eq!(out.status.code(), Some(0), "{ok}: {}", String::from_utf8_lossy(&out.stderr));
    }
    for bad in ["", "abc", "-1", "4294967296", "0x", "0x100000000", "01x"] {
        // `--param2=VALUE`, so a leading dash is read as part of the value.
        let out = run(&["vendor-write-ftp-blob", "--file", "x", &format!("--param2={bad}"), "--help"]);
        assert_eq!(out.status.code(), Some(2), "{bad:?} must be a usage error");
        assert!(String::from_utf8_lossy(&out.stderr).contains("invalid"), "{bad:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
}

#[test]
fn the_blob_file_is_read_before_any_camera_is_touched() {
    let out = run(&["vendor-write-ftp-blob", "--file", "/nonexistent-dir-for-test/blob.bin", "--dry-run"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("reading /nonexistent-dir-for-test/blob.bin"), "{err}");
}

/// b7d3#13: the dry run asks the same library function the write asks, so a property the write
/// would refuse is refused with no camera attached (it never gets as far as opening one).
#[test]
fn vendor_set_string_dry_run_refuses_an_unknown_property_before_any_camera() {
    let out = run(&["vendor-set-string", "0x1234", "x", "--dry-run"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("is not one of the string properties this writes") && err.contains("nothing was sent"), "{err}");
    assert!(err.contains("0xd072 Artist") && err.contains("0xd090 Image comment"), "the allowed list is named: {err}");
}

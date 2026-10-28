//! The documented exit statuses of the raw-USB vendor commands, through the real binary.
//!
//! The statuses themselves (`vendor-write-ftp` exits 4 for an indeterminate write) are pinned by
//! unit tests of `exit_code_for` in `src/main.rs`; the real path needs a camera. What a binary test
//! can pin is that the status is DOCUMENTED where a script author looks: `--help`.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

fn help_of(args: &[&str]) -> String {
    let out = Command::new(BIN).args(args).arg("--help").output().expect("run CLI");
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).expect("help is UTF-8")
}

#[test]
fn vendor_write_ftp_help_documents_every_exit_status_it_can_return() {
    let help = help_of(&["vendor-write-ftp"]);
    assert!(help.contains("Exit status:"), "{help}");
    for line in [
        "0  the camera accepted the profile",
        "1  any other error",
        "2  command-line usage error",
        "4  the profile data was sent but the write then failed: the camera MAY have changed",
    ] {
        assert!(help.contains(line), "{line:?} missing from:\n{help}");
    }
    assert!(!help.contains("3  refused"), "3 is the firmware archive's status, not this command's: {help}");
}

#[test]
fn the_other_vendor_command_and_the_archive_commands_do_not_claim_the_write_status() {
    assert!(!help_of(&["vendor-read"]).contains("MAY have changed"));
    // The archive's own table is unchanged and has no 4.
    let add = help_of(&["firmware", "add"]);
    assert!(add.contains("3  refused: that model and version is already in the archive"), "{add}");
    assert!(!add.contains("\n  4 "), "{add}");
}

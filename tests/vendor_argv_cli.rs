//! Feeds `nikon_fleet::vendor_argv`'s output to the real CLI binary, so a
//! rename/removal of a flag, or a value the argv builder mangles, fails here
//! instead of at runtime in the GUI (finding nikon-fleet-20260909-7b2e#12:
//! the egui GUI now runs the vendor ops as `nikon-fleet` subprocesses).
//!
//! Nothing here touches USB: `vendor-write-ftp` is run with `--dry-run`
//! (encodes and prints, never opens a device), and `vendor-read` is run with
//! a trailing `--help` (clap validates the preceding args, then prints help
//! and exits before the command body).

use std::io::Write;
use std::process::{Command, Stdio};

use nikon_fleet::ptp_usb::{FtpProfile, encode_ftp_profile, hex_bytes};
use nikon_fleet::vendor_argv::{vendor_read_argv, vendor_write_ftp_argv};

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

fn profile() -> FtpProfile {
    FtpProfile {
        // Leading dashes and spaces on purpose: the `--flag=value` form must
        // carry them through clap intact.
        profile_name: "-CLAUDE cam".to_string(),
        ssid_24ghz: "--net24".to_string(),
        ssid_5ghz: "net 5".to_string(),
        host: "ftp.example.com".to_string(),
        port: 2121,
        username: "-user".to_string(),
        password: "pa ss--word".to_string(),
    }
}

fn run_with_stdin(args: &[String], stdin: &str) -> std::process::Output {
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn CLI");
    child.stdin.take().expect("piped stdin").write_all(stdin.as_bytes()).expect("write stdin");
    child.wait_with_output().expect("wait CLI")
}

#[test]
fn write_argv_round_trips_every_field_through_the_real_cli() {
    let p = profile();
    let mut argv = vendor_write_ftp_argv(&p, None);
    argv.push("--dry-run".to_string());
    let out = run_with_stdin(&argv, &p.password);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "CLI rejected the argv: {argv:?}\nstderr: {}", String::from_utf8_lossy(&out.stderr));

    // The dry-run preview encodes the profile with the password redacted;
    // equality with our own encoding of the same profile proves every field
    // (and the stdin-delivered password's LENGTH) arrived intact.
    let expected = hex_bytes(&encode_ftp_profile(&p.with_password_redacted()).expect("encodes"));
    assert!(stdout.contains(&expected), "dry-run blob differs from the expected encoding.\nstdout: {stdout}\nexpected: {expected}");
    assert!(!stdout.contains(&p.password), "dry-run output leaked the plaintext password: {stdout}");
}

#[test]
fn write_argv_with_serial_is_accepted() {
    let p = profile();
    let mut argv = vendor_write_ftp_argv(&p, Some("0000003029610"));
    argv.push("--dry-run".to_string());
    let out = run_with_stdin(&argv, &p.password);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn write_password_length_is_the_stdin_length() {
    // Different password lengths must change the encoded blob; a CLI that
    // ignored stdin (or read it empty) would produce identical previews.
    let p = profile();
    let mut argv = vendor_write_ftp_argv(&p, None);
    argv.push("--dry-run".to_string());
    let long = run_with_stdin(&argv, "0123456789").stdout;
    let short = run_with_stdin(&argv, "0").stdout;
    assert_ne!(long, short);
}

#[test]
fn read_argv_is_accepted_by_the_real_cli() {
    for serial in [None, Some("0000003029610"), Some("-weird")] {
        let mut argv = vendor_read_argv(0xD053, serial);
        argv.push("--help".to_string());
        let out = Command::new(BIN).args(&argv).output().expect("run CLI");
        assert!(
            out.status.success(),
            "CLI rejected vendor-read argv {argv:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn dry_run_of(p: &FtpProfile) -> std::process::Output {
    let mut argv = vendor_write_ftp_argv(p, None);
    argv.push("--dry-run".to_string());
    run_with_stdin(&argv, &p.password)
}

/// The CLI used to encode an all-empty profile and, without --dry-run, send it to the camera:
/// only the GUIs checked the required fields. Every required field is refused on its own, with
/// its name, before anything is encoded, previewed or sent (`--dry-run` shares the refusal, so
/// the preview never shows a profile the real write would not send).
#[test]
fn every_required_field_is_checked_by_the_cli_before_anything_is_encoded() {
    let fields: [(&str, fn(&mut FtpProfile)); 6] = [
        ("profile name", |p| p.profile_name = String::new()),
        ("SSID (2.4GHz)", |p| p.ssid_24ghz = String::new()),
        ("SSID (5GHz)", |p| p.ssid_5ghz = String::new()),
        ("host", |p| p.host = String::new()),
        ("username", |p| p.username = String::new()),
        ("password", |p| p.password = String::new()),
    ];
    for (name, blank) in fields {
        let mut p = profile();
        blank(&mut p);
        let out = dry_run_of(&p);
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert_eq!(out.status.code(), Some(1), "{name}: {stdout}{stderr}");
        assert!(stderr.contains(&format!("missing field(s): {name}")), "{name}: {stderr}");
        assert!(!stdout.contains("Would write"), "{name}: a refused profile must not be previewed: {stdout}");
    }
}

#[test]
fn an_all_empty_profile_names_every_field_and_a_blank_but_not_empty_one_is_refused_too() {
    let empty = FtpProfile {
        profile_name: String::new(), ssid_24ghz: String::new(), ssid_5ghz: String::new(),
        host: String::new(), port: 21, username: String::new(), password: String::new(),
    };
    let out = dry_run_of(&empty);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("profile name, SSID (2.4GHz), SSID (5GHz), host, username, password"), "{stderr}");
    assert!(out.stdout.is_empty());

    // Whitespace-only counts as empty for every field but the password (a space can be a real
    // password character): the boundary is pinned on both sides.
    let mut blank = profile();
    blank.profile_name = "   ".to_string();
    assert_eq!(dry_run_of(&blank).status.code(), Some(1));
    let mut space_password = profile();
    space_password.password = " ".to_string();
    let ok = dry_run_of(&space_password);
    assert_eq!(ok.status.code(), Some(0), "{}", String::from_utf8_lossy(&ok.stderr));
}

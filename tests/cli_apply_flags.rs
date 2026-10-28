//! The CLI's `transplant`, `restore`, `snapshot` and `check` flags that put them on the same
//! ops path as the MCP worker (`--json`, `--allow-partial`). No camera: argument parsing and the
//! checks that run before the SDK is opened.

use std::path::Path;
use std::process::{Command, Output};

use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

fn help(cmd: &str) -> String {
    let o = Command::new(BIN).arg(cmd).arg("--help").output().expect("spawn CLI");
    assert!(o.status.success());
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn snapshot_file(dir: &Path) -> std::path::PathBuf {
    let mut s = Snapshot::new(
        Camera { model: "Z 9".into(), serial: "1".into(), firmware: "5.31".into() },
        Transport::Usb,
        "2026-10-27T00:00:00Z".into(),
    );
    s.insert("kNkMAIDCapability_ISO", 2, json!(100));
    let p = dir.join("snap.json");
    s.save_to_file(&p).unwrap();
    p
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("--data-dir")
        .arg(dir)
        .arg("--sdk-bundle")
        .arg(dir.join("no-such-sdk-bundle"))
        .args(args)
        .output()
        .expect("spawn CLI")
}

#[test]
fn the_four_commands_offer_json_and_the_two_writers_offer_allow_partial() {
    for cmd in ["snapshot", "check", "restore", "transplant"] {
        assert!(help(cmd).contains("--json"), "{cmd} lacks --json");
    }
    for cmd in ["restore", "transplant"] {
        assert!(help(cmd).contains("--allow-partial"), "{cmd} lacks --allow-partial");
    }
}

#[test]
fn json_does_not_change_what_is_checked_before_the_sdk() {
    let d = tempfile::tempdir().unwrap();
    let snap = snapshot_file(d.path());
    // A missing snapshot fails on its own message, with or without --json, and prints no JSON.
    for json in [false, true] {
        let mut args = vec!["transplant", "--dry-run", "no-such-snapshot.json"];
        if json {
            args.push("--json");
        }
        let o = run(d.path(), &args);
        assert!(!o.status.success());
        assert!(String::from_utf8_lossy(&o.stdout).trim().is_empty(), "stdout must stay empty on a refusal: json={json}");
        assert!(String::from_utf8_lossy(&o.stderr).contains("no-such-snapshot.json"));
    }
    // A bad opt-in is refused before the SDK, again with --json.
    let o = run(d.path(), &["transplant", "--dry-run", "--json", snap.to_str().unwrap(), "--allow-raw-code", "Nope"]);
    assert!(!o.status.success());
    assert!(!String::from_utf8_lossy(&o.stderr).contains("SDK bundle not found"));
    assert!(String::from_utf8_lossy(&o.stdout).trim().is_empty());
    // A good request gets as far as the missing SDK bundle.
    let o = run(d.path(), &["restore", "--dry-run", "--json", "--allow-partial", snap.to_str().unwrap()]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("SDK bundle not found"), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).trim().is_empty());
}

#[test]
fn a_label_that_starts_with_a_dash_is_a_value_not_a_flag() {
    // `snapshot --label -guest` used to die in clap ("unexpected argument '-g'"); it now gets as
    // far as the missing SDK bundle, which is the first thing the command checks.
    let d = tempfile::tempdir().unwrap();
    for args in [&["snapshot", "--label", "-guest"][..], &["snapshot", "--label=-guest"], &["snapshot", "--label", "--odd"]] {
        let o = run(d.path(), args);
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {err}");
        assert!(err.contains("SDK bundle not found"), "{args:?}: {err}");
        assert!(!err.contains("unexpected argument"), "{args:?}: {err}");
    }
    // A real flag after the label is still a flag.
    let o = run(d.path(), &["snapshot", "--label", "x", "--allow-partial"]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("SDK bundle not found"));
}

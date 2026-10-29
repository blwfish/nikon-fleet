//! The exit statuses of `nikon-fleet restore`, through the real CLI binary.
//!
//! An apply that is refused before it can write exits 6, so a caller (the GUI) can tell "nothing
//! was written" from "this may have written" (1 with no report). These run with no camera and a
//! bundle path that does not exist: each refusal here is raised before the SDK, or at the missing
//! SDK, which is before any write by construction. A dry run never writes and keeps its plain 1.

use std::path::Path;
use std::process::{Command, Output};

use nikon_fleet::ops::apply::{EXIT_REFUSED_BEFORE_WRITE, EXIT_STALE_PLAN};
use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

fn write_snapshot(dir: &Path) -> std::path::PathBuf {
    let mut s = Snapshot::new(
        Camera { model: "Z 9".into(), serial: "1".into(), firmware: "5.31".into() },
        Transport::Usb,
        "2026-10-27T00:00:00Z".into(),
    );
    s.insert("kNkMAIDCapability_Aperture", 1, json!(56));
    let path = dir.join("snap.json");
    s.save_to_file(&path).unwrap();
    path
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("--data-dir")
        .arg(dir)
        .arg("--sdk-bundle")
        .arg(dir.join("no-such-sdk-bundle"))
        .arg("restore")
        .args(args)
        .output()
        .expect("spawn CLI")
}

fn code(o: &Output) -> i32 {
    o.status.code().expect("the CLI exited by itself")
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// What each case's message proves: it names where the run stopped, so a case cannot pass because
/// it stopped somewhere else (every apply stopped at the missing SDK exits 6 whatever came first).
const AT_THE_SDK: &str = "SDK bundle not found";

#[test]
fn an_apply_stopped_at_the_sdk_exits_six_and_a_dry_run_exits_one() {
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    let snap = snap.to_str().unwrap();
    let apply = run(d.path(), &["--json", snap]);
    assert_eq!(code(&apply), EXIT_REFUSED_BEFORE_WRITE, "{}", err(&apply));
    assert!(err(&apply).contains(AT_THE_SDK), "stopped at the SDK: {}", err(&apply));
    let dry = run(d.path(), &["--json", "--dry-run", snap]);
    assert_eq!(code(&dry), 1, "{}", err(&dry));
    assert!(err(&dry).contains(AT_THE_SDK), "{}", err(&dry));
}

#[test]
fn the_snapshot_and_the_selection_are_checked_before_the_sdk_is_opened() {
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    let snap = snap.to_str().unwrap();
    // each of these stops on its own message: the SDK check, which would also exit 6, is never reached
    let bad_only = run(d.path(), &["--only", "Apertur", snap]);
    assert_eq!(code(&bad_only), EXIT_REFUSED_BEFORE_WRITE, "{}", err(&bad_only));
    assert!(err(&bad_only).contains("no property by that name") && !err(&bad_only).contains(AT_THE_SDK), "{}", err(&bad_only));
    let missing = run(d.path(), &["--json", "no-such-snapshot.json"]);
    assert_eq!(code(&missing), EXIT_REFUSED_BEFORE_WRITE, "{}", err(&missing));
    assert!(err(&missing).contains("no-such-snapshot.json") && !err(&missing).contains(AT_THE_SDK), "{}", err(&missing));
    // a dry run stops at the same places with the plain failure
    let dry_missing = run(d.path(), &["--dry-run", "no-such-snapshot.json"]);
    assert_eq!(code(&dry_missing), 1);
    assert!(!err(&dry_missing).contains(AT_THE_SDK), "{}", err(&dry_missing));
    // a good selection gets past the check and stops at the SDK
    let good_only = run(d.path(), &["--only", "Aperture", snap]);
    assert!(err(&good_only).contains(AT_THE_SDK), "{}", err(&good_only));
}

#[test]
fn the_flags_that_hold_an_apply_to_a_plan_are_checked_by_the_parser_before_anything_runs() {
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    let snap = snap.to_str().unwrap();
    // Misuse is the parser's, before any of this: clap's own status. (That a held apply exits 5 on a
    // plan that no longer matches needs a camera behind the SDK: it is pinned in-process, with the
    // library's fake camera, in `restore_on_a_fake_camera` in src/main.rs.)
    assert_eq!(code(&run(d.path(), &["--expect-digest", "d", snap])), 2);
    assert_eq!(code(&run(d.path(), &["--expect-summary", "s", snap])), 2);
    assert_eq!(code(&run(d.path(), &["--dry-run", "--expect-digest", "d", "--expect-summary", "s", snap])), 2);
    assert_eq!(code(&run(d.path(), &[])), 2);
    // given both, the run gets as far as the SDK
    let held = run(d.path(), &["--expect-digest", "d", "--expect-summary", "s", snap]);
    assert!(err(&held).contains(AT_THE_SDK), "{}", err(&held));
}

#[test]
fn the_help_documents_every_status_a_caller_tests() {
    let out = Command::new(BIN).args(["restore", "--help"]).output().expect("run CLI");
    let help = String::from_utf8(out.stdout).unwrap();
    for status in [0, 1, 2, EXIT_STALE_PLAN, EXIT_REFUSED_BEFORE_WRITE] {
        assert!(help.contains(&format!("\n  {status}  ")), "status {status} missing from:\n{help}");
    }
    for needle in ["may have written", "applied_unverified", "leaves out the advice to pass --allow-partial"] {
        assert!(help.contains(needle), "{needle:?} missing from:\n{help}");
    }
}

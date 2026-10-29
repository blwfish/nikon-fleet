//! The exit statuses of `fleet transplant`, through the real CLI binary.
//!
//! An apply that is refused before it can write exits 6, so a caller can tell "nothing was
//! written" from "this may have written" (1 with no report). These run with no camera and a bundle
//! path that does not exist: each refusal here is raised before the SDK, or at the missing SDK,
//! which is before any write by construction. A dry run never writes and keeps its plain 1.

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
    s.insert(
        "kNkMAIDCapability_MeteringMode",
        1,
        json!({"value_index": 0, "default_index": 0, "elem_count": 2, "elem_type": 2, "elem_bytes": 4, "values": [1, 2]}),
    );
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
        .arg("transplant")
        .args(args)
        .output()
        .expect("spawn CLI")
}

fn code(o: &Output) -> i32 {
    o.status.code().expect("the CLI exited by itself")
}

#[test]
fn an_apply_stopped_at_the_missing_sdk_exits_six_and_a_dry_run_exits_one() {
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    let snap = snap.to_str().unwrap();
    let apply = run(d.path(), &["--json", snap]);
    assert_eq!(code(&apply), EXIT_REFUSED_BEFORE_WRITE, "{}", String::from_utf8_lossy(&apply.stderr));
    assert!(String::from_utf8_lossy(&apply.stderr).contains("SDK bundle not found"), "the message is kept");
    let dry = run(d.path(), &["--json", "--dry-run", snap]);
    assert_eq!(code(&dry), 1, "{}", String::from_utf8_lossy(&dry.stderr));
}

#[test]
fn an_apply_stopped_by_a_bad_opt_in_or_a_missing_snapshot_exits_six() {
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    let snap = snap.to_str().unwrap();
    let bad_opt_in = run(d.path(), &["--allow-raw-code", "MeteringMod", snap]);
    assert_eq!(code(&bad_opt_in), EXIT_REFUSED_BEFORE_WRITE);
    let missing = run(d.path(), &["--json", "no-such-snapshot.json"]);
    assert_eq!(code(&missing), EXIT_REFUSED_BEFORE_WRITE, "{}", String::from_utf8_lossy(&missing.stderr));
    assert_eq!(code(&run(d.path(), &["--dry-run", "no-such-snapshot.json"])), 1);
}

#[test]
fn an_apply_given_a_plan_to_hold_to_exits_six_when_refused_and_two_when_misused() {
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    let snap = snap.to_str().unwrap();
    let held = run(d.path(), &["--expect-digest", "d", "--expect-summary", "s", snap]);
    assert_eq!(code(&held), EXIT_REFUSED_BEFORE_WRITE, "{}", String::from_utf8_lossy(&held.stderr));
    // Misuse is the parser's, before any of this: clap's own status, whatever is documented here.
    assert_eq!(code(&run(d.path(), &["--expect-digest", "d", snap])), 2);
    assert_eq!(code(&run(d.path(), &[])), 2);
}

#[test]
fn the_help_documents_every_status_a_caller_tests() {
    let out = Command::new(BIN).args(["transplant", "--help"]).output().expect("run CLI");
    let help = String::from_utf8(out.stdout).unwrap();
    for status in [EXIT_STALE_PLAN, EXIT_REFUSED_BEFORE_WRITE] {
        assert!(help.contains(&format!("\n  {status}  ")), "status {status} missing from:\n{help}");
    }
    assert!(help.contains("may have written"), "{help}");
    assert!(help.contains("leaves out the advice to pass --allow-partial"), "{help}");
}

//! `fleet transplant --allow-raw-code` through the real CLI binary.
//!
//! The opt-in is validated against the source snapshot *before* the SDK is
//! opened, so these run with no camera and a bundle path that does not exist:
//! a bad opt-in must fail on its own message, and a good one must get past
//! validation and fail only later, at the missing SDK bundle. That ordering is
//! the property under test: a mistyped name that "succeeded" would look exactly
//! like a safe transplant that opted nothing in.

use std::path::Path;
use std::process::{Command, Output};

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
    s.insert("kNkMAIDCapability_Copyright", 2, json!("me"));
    let path = dir.join("snap.json");
    s.save_to_file(&path).unwrap();
    path
}

fn run(dir: &Path, snap: &Path, extra: &[&str]) -> Output {
    Command::new(BIN)
        .arg("--data-dir")
        .arg(dir)
        .arg("--sdk-bundle")
        .arg(dir.join("no-such-sdk-bundle"))
        .arg("transplant")
        .arg("--dry-run")
        .arg(snap)
        .args(extra)
        .output()
        .expect("spawn CLI")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn unknown_property_is_rejected_before_the_sdk_is_touched() {
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    let o = run(d.path(), &snap, &["--allow-raw-code", "MeteringMod"]);
    assert!(!o.status.success());
    let e = stderr(&o);
    assert!(e.contains("no property by that name"), "{e}");
    assert!(!e.contains("SDK bundle not found"), "must fail on the opt-in, not reach the SDK: {e}");
}

#[test]
fn non_raw_code_property_is_rejected_before_the_sdk_is_touched() {
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    let o = run(d.path(), &snap, &["--allow-raw-code", "Copyright"]);
    assert!(!o.status.success());
    let e = stderr(&o);
    assert!(e.contains("not a raw-code enum"), "{e}");
    assert!(!e.contains("SDK bundle not found"), "{e}");
}

#[test]
fn valid_opt_ins_pass_validation_and_fail_only_at_the_missing_sdk() {
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    for name in ["MeteringMode", "kNkMAIDCapability_MeteringMode"] {
        let o = run(d.path(), &snap, &["--allow-raw-code", name]);
        assert!(!o.status.success());
        let e = stderr(&o);
        assert!(e.contains("SDK bundle not found"), "{name}: validation should have passed: {e}");
    }
    // And with no opt-in at all.
    let e = stderr(&run(d.path(), &snap, &[]));
    assert!(e.contains("SDK bundle not found"), "{e}");
}

#[test]
fn flag_is_repeatable_and_one_bad_name_fails_the_whole_run() {
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    let o = run(d.path(), &snap, &["--allow-raw-code", "MeteringMode", "--allow-raw-code", "Nope"]);
    assert!(stderr(&o).contains("no property by that name"), "{}", stderr(&o));
}

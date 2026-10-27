//! `restore --only` through the real CLI binary.
//!
//! The selection is validated against the snapshot *before* the SDK is
//! opened, so these run with no camera and a bundle path that does not exist:
//! a bad name must fail on its own message, and a good one must get past
//! validation and fail only at the missing SDK bundle. A mistyped name that
//! "succeeded" would look exactly like an undo that had nothing to do.

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
    s.insert("kNkMAIDCapability_Aperture", 1, json!(56));
    s.insert("kNkMAIDCapability_ISO", 2, json!(100));
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
        .arg("restore")
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
    for bad in ["Apertur", "aperture", ""] {
        let o = run(d.path(), &snap, &["--only", bad]);
        assert!(!o.status.success(), "{bad:?}");
        let e = stderr(&o);
        assert!(e.contains("no property by that name"), "{bad:?}: {e}");
        assert!(!e.contains("SDK bundle not found"), "must fail on the selection, not reach the SDK: {e}");
    }
}

#[test]
fn valid_names_pass_validation_and_fail_only_at_the_missing_sdk() {
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    for args in [
        vec!["--only", "Aperture"],
        vec!["--only", "kNkMAIDCapability_Aperture"],
        vec!["--only", "Aperture", "--only", "ISO"],
        vec!["--only", "Aperture", "--only", "kNkMAIDCapability_Aperture"],
    ] {
        let e = stderr(&run(d.path(), &snap, &args));
        assert!(e.contains("SDK bundle not found"), "{args:?}: validation should have passed: {e}");
    }
    // And with no --only at all: the unrestricted restore is unchanged.
    let e = stderr(&run(d.path(), &snap, &[]));
    assert!(e.contains("SDK bundle not found"), "{e}");
}

#[test]
fn one_bad_name_fails_the_whole_selection() {
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    let o = run(d.path(), &snap, &["--only", "Aperture", "--only", "Nope"]);
    assert!(stderr(&o).contains("no property by that name"), "{}", stderr(&o));
}

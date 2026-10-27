//! `nikon-fleet baseline seed` through the real binary (no camera needed).

use std::path::Path;
use std::process::{Command, Output};

use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

fn source(dir: &Path, firmware: &str) -> std::path::PathBuf {
    let mut s = Snapshot::new(
        Camera { model: "Z 9".into(), serial: "1".into(), firmware: firmware.into() },
        Transport::Usb,
        "2026-10-27T12:00:00Z".into(),
    );
    s.insert("kNkMAIDCapability_A", 10, json!(1));
    s.insert("kNkMAIDCapability_B", 20, json!(2));
    let p = dir.join("seed-source.json");
    s.save_to_file(&p).unwrap();
    p
}

fn seed(src: &Path, out: &Path, extra: &[&str]) -> Output {
    Command::new(BIN).arg("baseline").arg("seed").arg(src).arg("--out-dir").arg(out).args(extra).output().unwrap()
}

#[test]
fn seed_writes_the_baseline_and_says_to_rebuild() {
    let d = tempfile::tempdir().unwrap();
    let out = d.path().join("b");
    let o = seed(&source(d.path(), "5.31"), &out, &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("rebuild"));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(out.join("Z_9.json")).unwrap()).unwrap();
    assert_eq!(v["readable"].as_array().unwrap().len(), 2);
    assert_eq!(v["readable"][0], json!({"code": 10, "name": "kNkMAIDCapability_A"}));
}

#[test]
fn seeding_twice_needs_force() {
    let d = tempfile::tempdir().unwrap();
    let out = d.path().join("b");
    let src = source(d.path(), "5.31");
    assert!(seed(&src, &out, &[]).status.success());
    let again = seed(&src, &out, &[]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("--force"));
    assert!(seed(&src, &out, &["--force"]).status.success());
}

#[test]
fn a_snapshot_without_firmware_is_refused_and_nothing_is_written() {
    let d = tempfile::tempdir().unwrap();
    let out = d.path().join("b");
    let o = seed(&source(d.path(), ""), &out, &[]);
    assert!(!o.status.success());
    assert!(!out.join("Z_9.json").exists());
}

//! `nikon-fleet baseline seed` through the real binary (no camera needed).

use std::path::Path;
use std::process::{Command, Output};

use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

fn source(dir: &Path, firmware: &str) -> std::path::PathBuf {
    source_of(dir, "seed-source.json", "Z 9", firmware, &[10, 20])
}

fn source_of(dir: &Path, name: &str, model: &str, firmware: &str, codes: &[u32]) -> std::path::PathBuf {
    let mut s = Snapshot::new(
        Camera { model: model.into(), serial: "1".into(), firmware: firmware.into() },
        Transport::Usb,
        "2026-10-27T12:00:00Z".into(),
    );
    for c in codes {
        s.insert(format!("kNkMAIDCapability_C{c}"), *c, json!(1));
    }
    let p = dir.join(name);
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
    assert_eq!(v["readable"][0], json!({"code": 10, "name": "kNkMAIDCapability_C10"}));
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

#[test]
fn force_will_not_shrink_a_baseline_without_allow_shrink() {
    let d = tempfile::tempdir().unwrap();
    let out = d.path().join("b");
    let full = source_of(d.path(), "full.json", "Z 9", "5.31", &[10, 20, 30]);
    assert!(seed(&full, &out, &[]).status.success());
    let before = std::fs::read_to_string(out.join("Z_9.json")).unwrap();

    // A partial capture of the same model: --force alone is refused, and the file is untouched.
    let partial = source_of(d.path(), "partial.json", "Z 9", "5.31", &[10, 20]);
    let refused = seed(&partial, &out, &["--force"]);
    let err = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success());
    assert!(err.contains("no longer lists 1 capability") && err.contains("kNkMAIDCapability_C30") && err.contains("--allow-shrink"), "{err}");
    assert_eq!(std::fs::read_to_string(out.join("Z_9.json")).unwrap(), before);

    // The second flag is the deliberate way through; a growing replacement needs no second flag.
    assert!(seed(&partial, &out, &["--force", "--allow-shrink"]).status.success());
    assert_eq!(std::fs::read_to_string(out.join("Z_9.json")).unwrap().matches("\"code\"").count(), 2);
    let grown = source_of(d.path(), "grown.json", "Z 9", "5.40", &[10, 20, 30]);
    assert!(seed(&grown, &out, &["--force"]).status.success());
}

#[test]
fn allow_shrink_means_nothing_without_force() {
    let d = tempfile::tempdir().unwrap();
    let o = seed(&source(d.path(), "5.31"), &d.path().join("b"), &["--allow-shrink"]);
    assert_eq!(o.status.code(), Some(2), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!d.path().join("b").exists());
}

#[test]
fn seeding_a_model_the_build_does_not_register_says_it_is_not_active() {
    let d = tempfile::tempdir().unwrap();
    let out = d.path().join("b");
    let z5 = source_of(d.path(), "z5.json", "Z 5", "1.43", &[10, 20]);
    let o = seed(&z5, &out, &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("NOT ACTIVE") && text.contains("BUILTIN_SOURCES") && text.contains("Z_5.json"), "{text}");
    assert!(!text.to_lowercase().contains("rebuild to use it"), "rebuilding alone would not help: {text}");
    // A registered model's file is only stale until the next build.
    let z9 = seed(&source(d.path(), "5.31"), &out, &[]);
    assert!(String::from_utf8_lossy(&z9.stdout).contains("rebuild to use it"));
}

//! The CLI's `ref set` and the GUI's "Set ref" refuse an edited copy with the SAME rule and the same
//! words: both call `gui_argv::refuse_edited_reference`. This runs the real binary on an edited copy and
//! requires its message to carry the library's text, so a CLI that stopped calling the shared function
//! (and re-encoded the rule, or dropped it) fails here. The GUI half is
//! `set_reference_refuses_an_edited_copy_as_the_cli_does_and_writes_nothing` in `gui/src/main.rs`.
//! No camera.

use nikon_fleet::gui_argv::EditedCopyRefused;
use nikon_fleet::snapshot::{Camera, Snapshot, Transport};

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

#[test]
fn cli_ref_set_refuses_an_edited_copy_with_the_shared_wording_and_promotes_a_captured_snapshot() {
    let d = tempfile::tempdir().unwrap();
    let snaps = d.path().join("snapshots");
    std::fs::create_dir_all(&snaps).unwrap();
    let captured = Snapshot::new(Camera { model: "Z 9".into(), serial: "S1".into(), firmware: "5.31".into() }, Transport::Usb, "2026-01-01T00:00:00Z".into());
    let copy = nikon_fleet::snapshot_edit::copy_for_edit(&captured, "a.json", Some("copy"), "2026-02-02T00:00:00Z");
    let (a, c) = (snaps.join("a.json"), snaps.join("copy.json"));
    std::fs::write(&a, captured.to_pretty_json().unwrap()).unwrap();
    std::fs::write(&c, copy.to_pretty_json().unwrap()).unwrap();
    let run = |p: &std::path::Path| common::cli(BIN).arg("--data-dir").arg(d.path()).args(["ref", "set"]).arg(p).output().unwrap();

    let o = run(&c);
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success() && stderr.contains(&EditedCopyRefused.to_string()), "{stderr}");
    assert!(!d.path().join("references").exists());
    assert!(run(&a).status.success());
}

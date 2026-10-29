//! Feeds `nikon_fleet::gui_argv`'s output to the real CLI binary, so a renamed or removed flag, or a
//! value the builder mangles, fails here instead of at runtime in the egui GUI (which runs `restore`
//! and `transplant` as subprocesses, with exactly these argv).
//!
//! Two kinds of check, neither needs a camera. `NIKON_FLEET_PARSE_ONLY=1` parses the command line
//! and runs nothing: it proves the real parser accepts each shape (plan and apply, with and without a
//! selection, with values that start with `-`). A selection that names a property the snapshot does
//! not have, run for real against a missing SDK, stops on that name (a message only the parsed
//! `--only` / `--allow-raw-code` can produce): it proves the flag reached the command's logic.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output};

use nikon_fleet::gui_argv::{restore_argv, transplant_argv};
use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

fn write_snapshot(dir: &Path) -> String {
    let mut s = Snapshot::new(
        Camera { model: "Z 9".into(), serial: "1".into(), firmware: "5.31".into() },
        Transport::Usb,
        "2026-10-27T00:00:00Z".into(),
    );
    s.insert("kNkMAIDCapability_Aperture", 1, json!(56));
    s.insert("kNkMAIDCapability_MeteringMode", 2, json!({"value_index": 0, "default_index": 0, "elem_count": 2, "elem_type": 2, "elem_bytes": 4, "values": [1, 2]}));
    let path = dir.join("snap.json");
    s.save_to_file(&path).unwrap();
    path.to_str().unwrap().to_string()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

fn parse_only(argv: &[String]) -> Output {
    Command::new(BIN).args(argv).env("NIKON_FLEET_PARSE_ONLY", "1").output().expect("spawn CLI")
}

fn run(dir: &Path, argv: &[String]) -> Output {
    let mut full = vec!["--sdk-bundle".to_string(), dir.join("no-such-sdk-bundle").to_str().unwrap().to_string()];
    full.extend_from_slice(argv);
    Command::new(BIN).args(full).output().expect("spawn CLI")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn every_shape_of_restore_argv_is_accepted_by_the_real_parser() {
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    let dir = d.path().to_str().unwrap();
    let only = set(&["kNkMAIDCapability_Aperture"]);
    for (what, argv) in [
        ("plan", restore_argv(dir, "1", None, None, &snap)),
        ("plan, limited", restore_argv(dir, "1", Some(&only), None, &snap)),
        ("apply", restore_argv(dir, "1", None, Some(("abc123", "Z 9 1: restore from a.json: 3 would change; only: all")), &snap)),
        ("apply, limited", restore_argv(dir, "1", Some(&only), Some(("abc123", "s")), &snap)),
        ("dashes", restore_argv(dir, "-1", None, Some(("-d", "-s: --x")), &snap)),
    ] {
        let argv = argv.unwrap();
        let out = parse_only(&argv);
        assert!(out.status.success(), "{what}: the real parser rejected {argv:?}: {}", stderr(&out));
    }
}

#[test]
fn every_shape_of_transplant_argv_is_accepted_by_the_real_parser() {
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    let dir = d.path().to_str().unwrap();
    for (what, argv) in [
        ("plan", transplant_argv(dir, "1", &BTreeSet::new(), None, &snap)),
        ("plan, opted in", transplant_argv(dir, "1", &set(&["kNkMAIDCapability_MeteringMode"]), None, &snap)),
        ("apply", transplant_argv(dir, "1", &BTreeSet::new(), Some(("abc123", "summary")), &snap)),
        ("apply, opted in", transplant_argv(dir, "1", &set(&["kNkMAIDCapability_MeteringMode"]), Some(("abc", "-s")), &snap)),
    ] {
        let out = parse_only(&argv);
        assert!(out.status.success(), "{what}: the real parser rejected {argv:?}: {}", stderr(&out));
    }
}

#[test]
fn the_parser_still_refuses_what_the_builders_never_produce() {
    // the guard of this test file: the parse-only hook is not accepting everything
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    let mut argv = restore_argv(d.path().to_str().unwrap(), "1", None, None, &snap).unwrap();
    argv.insert(2, "--no-such-flag".to_string());
    assert_eq!(parse_only(&argv).status.code(), Some(2));
}

#[test]
fn a_selection_in_the_argv_is_the_selection_the_command_works_on() {
    let d = tempfile::tempdir().unwrap();
    let snap = write_snapshot(d.path());
    let dir = d.path().to_str().unwrap();
    // a name the snapshot has gets past the selection check and stops at the missing SDK ...
    let good = run(d.path(), &restore_argv(dir, "1", Some(&set(&["kNkMAIDCapability_Aperture"])), None, &snap).unwrap());
    assert!(stderr(&good).contains("SDK bundle not found"), "{}", stderr(&good));
    // ... one it does not have is refused on its name, which only a parsed --only can reach
    let bad = run(d.path(), &restore_argv(dir, "1", Some(&set(&["kNkMAIDCapability_Apertur"])), None, &snap).unwrap());
    assert!(stderr(&bad).contains("no property by that name"), "{}", stderr(&bad));
    // the same for a transplant's opt-in
    let opted = run(d.path(), &transplant_argv(dir, "1", &set(&["kNkMAIDCapability_Meterin"]), None, &snap));
    assert!(stderr(&opted).contains("no property by that name in the source snapshot"), "{}", stderr(&opted));
    let known = run(d.path(), &transplant_argv(dir, "1", &set(&["kNkMAIDCapability_MeteringMode"]), None, &snap));
    assert!(stderr(&known).contains("SDK bundle not found"), "{}", stderr(&known));
}

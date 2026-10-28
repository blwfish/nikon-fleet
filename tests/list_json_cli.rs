//! `ls` and `ref list` through the real CLI binary: the text output must be
//! unchanged, and `--json` must put exactly one JSON document on stdout.

use std::path::Path;
use std::process::{Command, Output};

use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

fn snap(model: &str, serial: &str, label: Option<&str>) -> Snapshot {
    let mut s = Snapshot::new(
        Camera { model: model.into(), serial: serial.into(), firmware: "5.31".into() },
        Transport::Usb,
        "2026-10-27T12:00:00Z".into(),
    );
    s.label = label.map(String::from);
    s.insert("kNkMAIDCapability_A", 1, json!(1));
    s
}

fn run(data_dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN).arg("--data-dir").arg(data_dir).args(args).output().expect("spawn CLI")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn json_of(o: &Output) -> Value {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    // The whole of stdout must parse: no banner, no trailing text.
    serde_json::from_str(&stdout(o)).unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}): {}", stdout(o)))
}

fn setup() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("snapshots");
    std::fs::create_dir_all(&dir).unwrap();
    snap("Z 9", "1", None).save_to_file(dir.join("Z_9_1_snap_a.json")).unwrap();
    snap("Z6_3", "2", Some("base")).save_to_file(dir.join("Z6_3_2_snap_b.json")).unwrap();
    std::fs::write(dir.join("Z_9_1_snap_broken.json"), "{ nope").unwrap();
    d
}

#[test]
fn ls_text_output_is_unchanged() {
    let d = setup();
    let out = stdout(&run(d.path(), &["ls"]));
    let want = "Z6_3_2_snap_b.json\n  Z6_3 2  label=base  fw=5.31  2026-10-27T12:00:00Z\n\
                Z_9_1_snap_a.json\n  Z 9 1  label=-  fw=5.31  2026-10-27T12:00:00Z\n";
    assert!(out.starts_with(want), "{out}");
    assert!(out.contains("Z_9_1_snap_broken.json  (failed to load: JSON error:"), "{out}");
}

#[test]
fn ls_text_messages_for_missing_empty_and_unmatched() {
    let d = tempfile::tempdir().unwrap();
    let missing = stdout(&run(d.path(), &["ls"]));
    assert!(missing.starts_with("(no snapshots — directory ") && missing.contains("does not exist)"), "{missing}");
    std::fs::create_dir_all(d.path().join("snapshots")).unwrap();
    assert_eq!(stdout(&run(d.path(), &["ls"])), "(no snapshots match)\n");
    let d = setup();
    assert_eq!(stdout(&run(d.path(), &["ls", "--camera", "Z 9_nobody"])), "(no snapshots match)\n");
}

#[test]
fn ls_json_is_one_document_with_every_file_accounted_for() {
    let d = setup();
    let v = json_of(&run(d.path(), &["ls", "--json"]));
    assert_eq!(v["dir_exists"], json!(true));
    assert_eq!(v["skipped_dir_entries"], json!(0));
    let rows: Vec<(&str, &str)> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["file"].as_str().unwrap(), e["status"].as_str().unwrap()))
        .collect();
    assert_eq!(
        rows,
        [("Z6_3_2_snap_b.json", "ok"), ("Z_9_1_snap_a.json", "ok"), ("Z_9_1_snap_broken.json", "unreadable")]
    );
    assert_eq!(v["entries"][0]["label"], json!("base"));
    assert_eq!(v["entries"][1]["label"], json!(null));
    assert_eq!(v["entries"][2]["error_kind"], json!("json"));
}

#[test]
fn ls_json_respects_the_camera_filter_and_reports_a_missing_directory() {
    let d = setup();
    let v = json_of(&run(d.path(), &["ls", "--json", "--camera", "Z6_3_2"]));
    assert_eq!(v["entries"].as_array().unwrap().len(), 1);
    assert_eq!(v["entries"][0]["file"], json!("Z6_3_2_snap_b.json"));
    let empty = tempfile::tempdir().unwrap();
    let v = json_of(&run(empty.path(), &["ls", "--json"]));
    assert_eq!((v["dir_exists"].clone(), v["entries"].clone()), (json!(false), json!([])));
}

#[test]
fn ref_list_text_and_json() {
    let d = tempfile::tempdir().unwrap();
    let missing = stdout(&run(d.path(), &["ref", "list"]));
    assert!(missing.starts_with("(no references — directory "), "{missing}");
    let v = json_of(&run(d.path(), &["ref", "list", "--json"]));
    assert_eq!(v["dir_exists"], json!(false));

    let refs = d.path().join("references");
    std::fs::create_dir_all(&refs).unwrap();
    snap("Z 9", "1", None).save_to_file(refs.join("Z_9_1.json")).unwrap();
    std::fs::write(refs.join("Z_9_2.json"), "{").unwrap();
    assert_eq!(stdout(&run(d.path(), &["ref", "list"])), "Z_9_1.json\nZ_9_2.json\n");
    let v = json_of(&run(d.path(), &["ref", "list", "--json"]));
    assert_eq!(v["entries"][0]["serial"], json!("1"));
    assert_eq!(v["entries"][1]["status"], json!("unreadable"));
}

#[test]
fn ls_json_marks_the_snapshot_promoted_by_ref_set_and_only_that_one() {
    let d = setup();
    let snaps = d.path().join("snapshots");
    // Two readable Z 9 snapshots with the same serial; promote one through the real CLI.
    snap("Z 9", "1", Some("second")).save_to_file(snaps.join("Z_9_1_snap_c.json")).unwrap();
    let set = run(d.path(), &["ref", "set", snaps.join("Z_9_1_snap_c.json").to_str().unwrap()]);
    assert!(set.status.success(), "{}", String::from_utf8_lossy(&set.stderr));

    let v = json_of(&run(d.path(), &["ls", "--json"]));
    let flag = |file: &str| {
        v["entries"].as_array().unwrap().iter().find(|e| e["file"] == json!(file)).map(|e| e.get("is_reference").cloned())
    };
    assert_eq!(flag("Z_9_1_snap_c.json"), Some(Some(json!(true))));
    assert_eq!(flag("Z_9_1_snap_a.json"), Some(Some(json!(false))));
    assert_eq!(flag("Z6_3_2_snap_b.json"), Some(Some(json!(false))));
    // An unreadable file has no verdict at all.
    assert_eq!(flag("Z_9_1_snap_broken.json"), Some(None));
    assert_eq!(v["reference_read_failures"], json!(0));

    // `ref list --json` lists the reference file itself and carries no marker.
    let r = json_of(&run(d.path(), &["ref", "list", "--json"]));
    assert!(r["entries"][0].get("is_reference").is_none());
}

#[test]
fn ls_json_applies_the_shared_validity_rules_to_every_file() {
    // The same directory the library test and (in time) both GUIs read: `format_version`,
    // `properties` and `transport` are all required; a future version is its own kind.
    let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/snapshot_validity");
    let expected: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&std::fs::read_to_string(fx.join("expected.json")).unwrap()).unwrap();
    let v = json_of(&run(&fx, &["ls", "--json"]));
    let got: std::collections::BTreeMap<String, String> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let verdict = match e["status"].as_str().unwrap() {
                "ok" => "ok".to_string(),
                _ => e["error_kind"].as_str().unwrap().to_string(),
            };
            (e["file"].as_str().unwrap().to_string(), verdict)
        })
        .collect();
    assert_eq!(got, expected);

    // What a GUI lists for a valid file: identity, label, time, property count, reference mark.
    let ok = v["entries"].as_array().unwrap().iter().find(|e| e["file"] == json!("ok.json")).unwrap();
    assert_eq!(
        ok,
        &json!({"file": "ok.json", "status": "ok", "model": "Z 9", "serial": "0000003023668", "firmware": "5.31",
                "label": "fixture", "captured_at": "2026-10-27T12:00:00Z", "property_count": 2, "is_reference": false})
    );
}

#[test]
fn ref_set_refuses_a_serial_that_would_write_outside_references_and_reports_a_replacement() {
    let d = tempfile::tempdir().unwrap();
    let snaps = d.path().join("snapshots");
    std::fs::create_dir_all(&snaps).unwrap();
    snap("Z 9", "../../escape", None).save_to_file(snaps.join("hostile.json")).unwrap();
    let o = run(d.path(), &["ref", "set", snaps.join("hostile.json").to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("cannot be a reference"), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!d.path().join("references").exists(), "nothing created for a refused name");
    assert!(!d.path().join("escape_.json").exists() && !d.path().parent().unwrap().join("escape.json").exists());

    // Promoting twice replaces, and says what was replaced.
    snap("Z 9", "1", Some("first")).save_to_file(snaps.join("a.json")).unwrap();
    snap("Z 9", "1", Some("second")).save_to_file(snaps.join("b.json")).unwrap();
    let first = run(d.path(), &["ref", "set", snaps.join("a.json").to_str().unwrap()]);
    assert!(first.status.success() && !stdout(&first).contains("replaced"), "{}", stdout(&first));
    let second = run(d.path(), &["ref", "set", snaps.join("b.json").to_str().unwrap()]);
    assert!(stdout(&second).contains("replaced the previous reference (captured 2026-10-27T12:00:00Z, label first)"), "{}", stdout(&second));
    let again = run(d.path(), &["ref", "set", snaps.join("b.json").to_str().unwrap()]);
    assert!(stdout(&again).contains("already the reference"), "{}", stdout(&again));
}

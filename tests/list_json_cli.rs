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

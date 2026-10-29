//! `nikon-fleet show` through the real binary: what a stored snapshot holds, read from the file
//! alone (no camera, no SDK), with missing data shown as missing and failures as exit status 1.

use std::path::Path;
use std::process::{Command, Output};

use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

fn snap() -> Snapshot {
    let mut s = Snapshot::new(
        Camera { model: "Z 9".into(), serial: "30012345".into(), firmware: "5.31".into() },
        Transport::Usb,
        "2026-10-27T12:00:00Z".into(),
    );
    s.label = Some("base".into());
    s.insert("kNkMAIDCapability_Quality", 0xd053, json!({"elem_type": 7, "value_index": 3, "elem_count": 4, "elem_bytes": 12, "default_index": 0, "values": ["Basic", "Normal", "Fine", "RAW"]}));
    // The Z 9's WBTune shape: the position is in value_index, `value` is junk 0.0.
    s.insert("kNkMAIDCapability_WBTuneAuto", 0x1001, json!({"value": 0.0, "default": 0.0, "value_index": 1224, "default_index": 0, "lower": 0.0, "upper": 1224.0, "steps": 1225}));
    s.insert("kNkMAIDCapability_Beep", 7, json!(true));
    s.insert("kNkMAIDCapability_Broken", 8, json!({"value": 0.0, "default": 0.0, "value_index": 1225, "default_index": 0, "lower": 0.0, "upper": 1224.0, "steps": 1225}));
    s
}

fn data_dir() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("snapshots")).unwrap();
    std::fs::create_dir_all(d.path().join("references")).unwrap();
    snap().save_to_file(d.path().join("snapshots/Z_9_30012345_base_20261027T120000Z.json")).unwrap();
    snap().save_to_file(d.path().join("references/Z_9_30012345.json")).unwrap();
    d
}

fn run_in(cwd: &Path, data: &Path, args: &[&str]) -> Output {
    // `--raw`: these tests pin the numbers as stored, with the code column. Whether names are
    // shown depends on the sdk-runtime/ of the machine; followup_labels_show_cli.rs covers that
    // with files it writes itself.
    let (cmd, rest) = args.split_first().expect("a subcommand");
    let raw: &[&str] = if *cmd == "show" { &["--raw"] } else { &[] };
    Command::new(BIN)
        .current_dir(cwd)
        .arg("--data-dir")
        .arg(data)
        // A bundle that does not exist: `show` must not touch the SDK.
        .arg("--sdk-bundle")
        .arg("/nonexistent/sdk")
        .arg(cmd)
        .args(raw)
        .args(rest)
        .output()
        .expect("spawn CLI")
}

fn run(data: &Path, args: &[&str]) -> Output {
    run_in(data, data, args)
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

const NAME: &str = "Z_9_30012345_base_20261027T120000Z.json";

#[test]
fn a_name_from_ls_shows_a_header_then_aligned_property_lines() {
    let d = data_dir();
    let o = run(d.path(), &["show", NAME]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let want = "Z 9 30012345  fw 5.31  label=base  captured 2026-10-27T12:00:00Z  (4 properties)\n\
                Beep       [0x0007] = Yes\n\
                Broken     [0x0008] = (malformed: stepped range position 1225 is past its 1225 steps)\n\
                Quality    [0xd053] = RAW\n\
                WBTuneAuto [0x1001] = 1224.0\n";
    assert_eq!(out(&o), want);
    assert_eq!(err(&o), "");
}

#[test]
fn a_stepped_range_shows_its_position_and_an_unusable_position_is_not_zero() {
    let d = data_dir();
    let text = out(&run(d.path(), &["show", NAME]));
    assert!(text.contains("WBTuneAuto [0x1001] = 1224.0\n"), "{text}");
    let broken = text.lines().find(|l| l.starts_with("Broken")).unwrap();
    assert!(broken.contains("(malformed: ") && !broken.ends_with("= 0.0"), "{broken}");
}

#[test]
fn a_reference_is_found_by_its_bare_name_too() {
    let d = data_dir();
    let o = run(d.path(), &["show", "Z_9_30012345.json"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(out(&o).starts_with("Z 9 30012345  fw 5.31"), "{}", out(&o));
}

#[test]
fn a_path_is_used_as_given_and_so_is_a_file_in_the_current_directory() {
    let d = data_dir();
    let elsewhere = tempfile::tempdir().unwrap();
    let abs = d.path().join("references/Z_9_30012345.json");
    let o = run_in(elsewhere.path(), d.path(), &["show", abs.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    // A bare name that is in no data directory directory falls back to the current directory.
    snap().save_to_file(elsewhere.path().join("local.json")).unwrap();
    let o = run_in(elsewhere.path(), d.path(), &["show", "local.json"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(out(&o).contains("Beep"), "{}", out(&o));
    // A relative path with a directory part is a path, not a name to look up in snapshots/.
    let o = run_in(d.path(), d.path(), &["show", "references/Z_9_30012345.json"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
}

#[test]
fn the_filter_matches_name_value_and_code_ignoring_case_and_reports_the_count() {
    let d = data_dir();
    let by = |f: &str| out(&run(d.path(), &["show", NAME, "--filter", f]));
    let text = by("RAW");
    assert!(text.contains("Quality") && !text.contains("Beep"), "{text}");
    assert!(text.ends_with("1 of 4 properties match \"RAW\"\n"), "{text}");
    assert!(by("beep").contains("Beep"), "name, other case");
    assert!(by("0XD053").contains("Quality"), "hex code, other case");
    assert!(by("4097").contains("WBTuneAuto"), "decimal code of 0x1001");
    assert!(by("malformed").contains("Broken"), "a problem is found by its word");
    assert!(by("no such thing").ends_with("(no properties match \"no such thing\")\n"));
    assert_eq!(out(&run(d.path(), &["show", NAME, "--filter", ""])).lines().count(), 5, "an empty filter is no filter");
}

#[test]
fn json_is_one_document_with_header_rows_kinds_and_notes() {
    let d = data_dir();
    let o = run(d.path(), &["show", NAME, "--json", "--filter", "b"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let v: Value = serde_json::from_str(&out(&o)).unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}): {}", out(&o)));
    assert_eq!(v["header"]["model"], "Z 9");
    assert_eq!(v["header"]["property_count"], 4);
    assert_eq!(v["header"]["label"], "base");
    let rows: Vec<(&str, &str, &str)> = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["name"].as_str().unwrap(), r["value"]["display"].as_str().unwrap(), r["value"]["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(rows[0], ("kNkMAIDCapability_Beep", "Yes", "bool"));
    assert_eq!(rows[1].2, "malformed");
    assert_eq!(v["rows"][1]["value"]["notes"], json!(["index_out_of_range", "no_number"]));
    assert_eq!(v["filtered_out"].as_u64().unwrap() + v["rows"].as_array().unwrap().len() as u64, 4);
    assert_eq!(v["truncated"], false);
}

#[test]
fn a_file_that_is_not_there_or_not_a_snapshot_exits_1_and_says_which() {
    let d = data_dir();
    let o = run(d.path(), &["show", "nope.json"]);
    assert_eq!((o.status.code(), out(&o)), (Some(1), String::new()));
    assert!(err(&o).contains("nope.json"), "{}", err(&o));

    std::fs::write(d.path().join("snapshots/broken.json"), "{ nope").unwrap();
    let o = run(d.path(), &["show", "broken.json"]);
    assert_eq!((o.status.code(), out(&o)), (Some(1), String::new()));
    assert!(err(&o).contains("broken.json") && err(&o).contains("JSON error"), "{}", err(&o));

    let old = snap().to_pretty_json().unwrap().replace("\"format_version\": 1", "\"format_version\": 9");
    std::fs::write(d.path().join("snapshots/old.json"), old).unwrap();
    let o = run(d.path(), &["show", "old.json", "--json"]);
    assert_eq!((o.status.code(), out(&o)), (Some(1), String::new()), "no JSON on a failure");
    assert!(err(&o).contains("unsupported format_version 9"), "{}", err(&o));

    let o = run(d.path(), &["show", ".."]);
    assert_eq!(o.status.code(), Some(1));
    assert!(err(&o).contains("not a plain file name"), "{}", err(&o));
}

#[test]
fn a_missing_argument_is_a_usage_error_with_status_2() {
    let d = data_dir();
    assert_eq!(run(d.path(), &["show"]).status.code(), Some(2));
    assert_eq!(run(d.path(), &["show", NAME, "--no-such-flag"]).status.code(), Some(2));
}

#[test]
fn show_reads_only_and_creates_nothing() {
    let d = data_dir();
    let listing = |p: &Path| -> Vec<String> {
        let mut v: Vec<String> = walk(p);
        v.sort();
        v
    };
    fn walk(p: &Path) -> Vec<String> {
        std::fs::read_dir(p)
            .unwrap()
            .flat_map(|e| {
                let path = e.unwrap().path();
                if path.is_dir() { walk(&path) } else { vec![format!("{}:{}", path.display(), std::fs::metadata(&path).unwrap().len())] }
            })
            .collect()
    }
    let before = listing(d.path());
    assert_eq!(run(d.path(), &["show", NAME]).status.code(), Some(0));
    assert_eq!(run(d.path(), &["show", NAME, "--json"]).status.code(), Some(0));
    assert_eq!(listing(d.path()), before);
}

#[test]
fn text_from_a_file_cannot_inject_terminal_escapes() {
    let d = data_dir();
    let mut s = snap();
    s.camera.model = "Z\u{1b}[2J 9".into();
    s.insert("kNkMAIDCapability_Evil", 9, json!("line one\nline two\u{1b}]0;pwned\u{7}"));
    s.save_to_file(d.path().join("snapshots/evil.json")).unwrap();
    let o = run(d.path(), &["show", "evil.json"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let text = out(&o);
    assert!(!text.contains('\u{1b}') && !text.contains('\u{7}'), "{text:?}");
    assert_eq!(text.lines().count(), 1 + 5, "a newline inside a value stays inside its line: {text:?}");
    // --json is escaped by JSON itself and stays one parseable document.
    let o = run(d.path(), &["show", "evil.json", "--json"]);
    assert!(serde_json::from_str::<Value>(&out(&o)).is_ok());
}

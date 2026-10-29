//! `nikon-fleet show` with names, through the real binary: display names, value names and range
//! scales read from files beside `--schema`, the code column behind `--codes`, `--raw` for the
//! numbers as stored, and a note on stderr when the names are not there. The schema and labels
//! files are written here (invented, not the SDK's), so nothing depends on the machine's sdk-runtime/.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

const CONFIG: &str = r#"<ProfileVersion>1.0</ProfileVersion>
<model:Test 1>
	<version>common</version>
	<caplist>
		<capability:kNkMAIDCapability_ExposureMode-33041>
			<description>0,11100,"Mode One"</description>
			<resourcestringList>
				<resourcestring>0,1,"First"</resourcestring>
				<resourcestring>1,2,"Second one"</resourcestring>
			</resourcestringList>
		</capability>
		<capability:kNkMAIDCapability_Beep-33100>
			<description>0,11100,"kNkMAIDCapability_Beep"</description>
		</capability>
	</caplist>
</model>
<model:Test 1>
	<version>2.0</version>
	<caplist>
		<capability:kNkMAIDCapability_ExposureMode-33041>
			<description>0,11100,"Mode One (2.0)"</description>
			<resourcestringList>
				<resourcestring>0,1,"First"</resourcestring>
				<resourcestring>1,2,"Second one"</resourcestring>
			</resourcestringList>
		</capability>
	</caplist>
</model>
"#;

const LABELS: &str = r#"{"source":{"extractor_version":1},
 "base_capabilities":{"9":"Base Nine"},
 "capabilities":{"ExposureMode":{"enum":"eX","values":{"2":"Third one","3":"Fourth one"}}}}"#;

fn snap(firmware: &str) -> Snapshot {
    let mut s = Snapshot::new(
        Camera { model: "Test 1".into(), serial: "77".into(), firmware: firmware.into() },
        Transport::Usb,
        "2026-10-27T12:00:00Z".into(),
    );
    s.label = Some("base".into());
    let mode = |i: u64| json!({"elem_type": 2, "value_index": i, "elem_count": 5, "elem_bytes": 4, "default_index": 0, "values": [0, 1, 2, 3, 9]});
    s.insert("kNkMAIDCapability_ExposureMode", 33041, mode(2));
    s.insert("kNkMAIDCapability_Beep", 33100, json!(true));
    s.insert("kNkMAIDCapability_WBTuneAuto", 33200, json!({"value": 0.0, "default": 0.0, "value_index": 612, "default_index": 0, "lower": 0.0, "upper": 1224.0, "steps": 1225}));
    s.insert("kNkMAIDCapability_Gain", 33201, json!({"value": 3.5, "default": 0.0, "value_index": 0, "default_index": 0, "lower": 0.0, "upper": 10.0, "steps": 0}));
    s.insert("kNkMAIDCapability_Quality", 33202, json!({"elem_type": 7, "value_index": 1, "elem_count": 12, "elem_bytes": 1, "default_index": 0, "values": ["F","i","n","e","","R","A","W",""]}));
    s.insert("cap_0x9", 9, json!("Test 1"));
    s
}

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    /// A data directory holding the snapshot (firmware `fw`) and a `runtime/` with the two files.
    fn new(fw: &str, config: Option<&str>, labels: Option<&str>) -> Env {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("snapshots")).unwrap();
        snap(fw).save_to_file(dir.path().join("snapshots/s.json")).unwrap();
        std::fs::create_dir_all(dir.path().join("runtime")).unwrap();
        if let Some(c) = config {
            std::fs::write(dir.path().join("runtime/MaidLayer.config"), c).unwrap();
        }
        if let Some(l) = labels {
            std::fs::write(dir.path().join("runtime/maid-labels.json"), l).unwrap();
        }
        Env { dir }
    }

    fn schema(&self) -> PathBuf {
        self.dir.path().join("runtime/MaidLayer.config")
    }

    fn show(&self, args: &[&str]) -> Output {
        self.show_with_schema(&self.schema(), args)
    }

    fn show_with_schema(&self, schema: &Path, args: &[&str]) -> Output {
        Command::new(BIN)
            .current_dir(self.dir.path())
            .arg("--data-dir")
            .arg(self.dir.path())
            .arg("--sdk-bundle")
            .arg("/nonexistent/sdk")
            .arg("--schema")
            .arg(schema)
            .arg("show")
            .arg("s.json")
            .args(args)
            .output()
            .expect("spawn CLI")
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Runs of spaces collapsed to one, so a check does not depend on how wide the columns came out.
fn squash(text: &str) -> String {
    text.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

fn full() -> Env {
    Env::new("1.00", Some(CONFIG), Some(LABELS))
}

#[test]
fn by_default_codes_are_hidden_and_names_labels_and_scales_are_shown() {
    let e = full();
    let o = e.show(&[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let want = "Test 1 77  fw 1.00  label=base  captured 2026-10-27T12:00:00Z  (6 properties)\n\
                cap_0x9       Base Nine = Test 1\n\
                Beep                    = Yes\n\
                ExposureMode  Mode One  = Third one (2)\n\
                Gain                    = 3.5 (0..10)\n\
                Quality                 = RAW\n\
                WBTuneAuto              = 612.0 (0..1224, 1225 steps)\n";
    assert_eq!(out(&o), want);
    assert_eq!(err(&o), "", "everything asked for was found: nothing to say");
}

#[test]
fn codes_add_the_column_and_the_filter_finds_a_code_with_or_without_it() {
    let e = full();
    let text = out(&e.show(&["--codes"]));
    assert!(text.contains("ExposureMode  Mode One  [0x8111] = Third one (2)\n"), "{text}");
    assert!(text.contains("Beep                    [0x814c] = Yes\n"), "{text}");
    for f in ["0x8111", "8111", "33041"] {
        let hidden = out(&e.show(&["--filter", f]));
        assert!(hidden.contains("ExposureMode") && !hidden.contains("[0x"), "filter {f} with the column hidden: {hidden}");
        assert!(hidden.ends_with(&format!("1 of 6 properties match {f:?}\n")), "{hidden}");
    }
}

#[test]
fn the_filter_finds_a_display_name_and_a_label() {
    let e = full();
    assert!(out(&e.show(&["--filter", "mode one"])).contains("ExposureMode"));
    assert!(out(&e.show(&["--filter", "third one"])).contains("ExposureMode"));
    assert!(!out(&e.show(&["--raw", "--filter", "third one"])).contains("ExposureMode"), "raw has no label to find");
}

#[test]
fn no_names_drops_the_display_names_but_keeps_labels_and_scales() {
    let e = full();
    let text = out(&e.show(&["--no-names"]));
    assert!(!text.contains("Mode One") && !text.contains("Base Nine"), "{text}");
    assert!(squash(&text).contains("ExposureMode = Third one (2)") && text.contains("= 3.5 (0..10)"), "{text}");
    // --names is the default, spelled out.
    assert_eq!(out(&e.show(&["--names"])), out(&e.show(&[])));
}

#[test]
fn raw_is_the_numbers_as_stored_with_the_code_column_and_says_nothing_on_stderr() {
    let e = Env::new("1.00", None, None);
    let o = e.show(&["--raw"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let want = "Test 1 77  fw 1.00  label=base  captured 2026-10-27T12:00:00Z  (6 properties)\n\
                cap_0x9      [0x0009] = Test 1\n\
                Beep         [0x814c] = Yes\n\
                ExposureMode [0x8111] = 2\n\
                Gain         [0x81b1] = 3.5\n\
                Quality      [0x81b2] = RAW\n\
                WBTuneAuto   [0x81b0] = 612.0\n";
    assert_eq!(out(&o), want);
    assert_eq!(err(&o), "", "--raw is a choice, not a gap");
    // With the files there, --raw still ignores them.
    assert_eq!(out(&full().show(&["--raw"])), want);
}

#[test]
fn conflicting_flags_are_a_usage_error() {
    let e = full();
    for args in [&["--raw", "--names"][..], &["--names", "--no-names"][..]] {
        assert_eq!(e.show(args).status.code(), Some(2), "{args:?}");
    }
    assert_eq!(e.show(&["--raw", "--no-names"]).status.code(), Some(0), "redundant, not contradictory");
}

#[test]
fn without_the_files_the_numbers_are_shown_and_one_line_on_stderr_says_why() {
    let e = Env::new("1.00", None, None);
    let o = e.show(&[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    // A range's scale needs no SDK file: it is shown with nothing loaded.
    assert!(squash(&out(&o)).contains("ExposureMode = 2\n") && squash(&out(&o)).contains("WBTuneAuto = 612.0 (0..1224, 1225 steps)\n"), "{}", out(&o));
    assert!(!out(&o).contains("Third one"), "{}", out(&o));
    let e_text = err(&o);
    assert_eq!(e_text.lines().count(), 1, "{e_text}");
    assert!(e_text.starts_with("note: names unavailable, showing raw values: "), "{e_text}");
    assert!(e_text.contains("MaidLayer.config") && e_text.contains("maid-labels.json") && e_text.contains("extract-maid-labels.py"), "{e_text}");
}

#[test]
fn only_the_missing_part_is_reported_and_the_other_still_names_things() {
    let e = Env::new("1.00", Some(CONFIG), None);
    let o = e.show(&["--filter", "exposuremode"]);
    let text = out(&o);
    assert!(text.contains("Mode One"), "the schema's display name is there: {text}");
    assert!(text.contains("= 2  [value_not_in_labels]"), "value 2 is the document's, which is missing; the schema's list lacks it: {text}");
    let note = err(&o);
    assert!(note.contains("maid-labels.json") && !note.contains("MaidLayer.config"), "{note}");
    let e = Env::new("1.00", None, Some(LABELS));
    let o = e.show(&[]);
    assert!(squash(&out(&o)).contains("ExposureMode = Third one (2)"), "the document alone names 2: {}", out(&o));
    assert!(out(&o).contains("cap_0x9") && out(&o).contains("Base Nine"), "{}", out(&o));
    assert!(err(&o).contains("MaidLayer.config") && !err(&o).contains("maid-labels.json"), "{}", err(&o));
}

#[test]
fn a_schema_that_does_not_know_the_model_is_said_not_ignored() {
    let e = full();
    let mut s = snap("1.00");
    s.camera.model = "Unknown 9".into();
    s.save_to_file(e.dir.path().join("snapshots/s.json")).unwrap();
    let o = e.show(&[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(err(&o).contains("no section for Unknown 9"), "{}", err(&o));
    assert!(squash(&out(&o)).contains("ExposureMode = Third one (2)"), "the document still names it: {}", out(&o));
}

#[test]
fn the_snapshots_firmware_chooses_the_schema_generation() {
    let old = out(&Env::new("1.00", Some(CONFIG), Some(LABELS)).show(&["--filter", "exposuremode"]));
    assert!(old.contains("Mode One ") && !old.contains("(2.0)"), "{old}");
    let new = out(&Env::new("2.00", Some(CONFIG), Some(LABELS)).show(&["--filter", "exposuremode"]));
    assert!(new.contains("Mode One (2.0)"), "{new}");
}

#[test]
fn a_value_the_list_does_not_have_is_marked_and_shown() {
    let e = full();
    let mut s = snap("1.00");
    s.insert("kNkMAIDCapability_ExposureMode", 33041, json!({"elem_type": 2, "value_index": 4, "elem_count": 5, "elem_bytes": 4, "default_index": 0, "values": [0, 1, 2, 3, 9]}));
    s.save_to_file(e.dir.path().join("snapshots/s.json")).unwrap();
    let text = out(&e.show(&["--filter", "exposuremode"]));
    assert!(text.contains("= 9  [value_not_in_labels]\n"), "{text}");
}

#[test]
fn json_carries_display_names_labels_scales_notes_and_coverage() {
    let e = full();
    let o = e.show(&["--json"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let v: Value = serde_json::from_str(&out(&o)).unwrap_or_else(|x| panic!("not one JSON document ({x}): {}", out(&o)));
    let row = |n: &str| v["rows"].as_array().unwrap().iter().find(|r| r["name"] == n).unwrap_or_else(|| panic!("no row {n}")).clone();
    let mode = row("kNkMAIDCapability_ExposureMode");
    assert_eq!(mode["display_name"], "Mode One");
    assert_eq!(mode["value"]["display"], "Third one (2)");
    assert_eq!(mode["value"]["label"], "Third one");
    assert_eq!(mode["value"]["label_source"], "sdk_doc");
    assert_eq!(mode["value"]["notes"], json!([]));
    assert_eq!(row("kNkMAIDCapability_WBTuneAuto")["value"]["scale"], json!({"lower": 0.0, "upper": 1224.0, "steps": 1225}));
    assert_eq!(row("cap_0x9")["display_name"], "Base Nine");
    assert!(row("kNkMAIDCapability_Quality").get("display_name").is_none(), "unknown: absent, not empty");
    assert_eq!(v["coverage"]["properties"], 6);
    assert_eq!(v["coverage"]["with_display_name"], 3, "cap_0x9, Beep and ExposureMode; the schema lacks the other three codes");
    assert!(v.get("labels_note").is_none());
    // Raw JSON has none of it.
    let raw: Value = serde_json::from_str(&out(&e.show(&["--json", "--raw"]))).unwrap();
    assert!(raw.get("coverage").is_none() && raw["rows"][0].get("display_name").is_none() && raw["rows"][0]["value"].get("scale").is_none(), "{raw}");
}

#[test]
fn json_says_when_the_names_are_unavailable_and_stdout_stays_one_document() {
    let e = Env::new("1.00", None, None);
    let o = e.show(&["--json"]);
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    let note = v["labels_note"].as_str().expect("the JSON says why");
    assert!(note.contains("maid-labels.json"), "{note}");
    assert!(err(&o).starts_with("note: names unavailable"), "{}", err(&o));
    assert_eq!(v["coverage"]["with_display_name"], 0);
}

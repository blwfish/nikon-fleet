//! `vendor-sweep` and `vendor-sweep-diff` through the real binary. No camera: the sweep is only run as far as the
//! arguments and the refusal of a body that is not there; the diff is run on dumps built here.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Output;

use nikon_fleet::prop_sweep::{CodeRange, SWEEP_FORMAT_VERSION, SweepBody, SweepCounts, SweepDump, SweepEntry, parse_prop_desc};

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

fn run(data_dir: &Path, args: &[&str]) -> Output {
    common::cli(BIN).arg("--data-dir").arg(data_dir).args(args).output().expect("spawn CLI")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// A u8 enumeration [0, 1, 2], read-write, with this current value.
fn enum_entry(code: u16, current: u8) -> SweepEntry {
    let mut b = Vec::new();
    b.extend(code.to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend([1, 0, current, 2, 3, 0, 0, 1, 2]);
    SweepEntry::Present {
        raw_hex: b.iter().map(|x| format!("{x:02x}")).collect(),
        listed_in_device_info: false,
        desc: parse_prop_desc(&b, code),
    }
}

fn dump(label: &str, entries: &[(u16, u8)]) -> SweepDump {
    SweepDump {
        format_version: SWEEP_FORMAT_VERSION,
        captured_at: "2026-11-06T10:00:00Z".into(),
        label: Some(label.into()),
        body: SweepBody { model: "Z 9".into(), usb_serial: "S1".into(), firmware: "C:5.31".into() },
        ranges: vec![CodeRange { lo: 0xD000, hi: 0xD0FF }],
        counts: SweepCounts { asked: 256, present: entries.len() as u32, not_supported: 256 - entries.len() as u32, refused: 0, failed: 0 },
        aborted: None,
        device_info_props: vec![],
        entries: entries.iter().map(|(c, v)| (format!("0x{c:04x}"), enum_entry(*c, *v))).collect::<BTreeMap<_, _>>(),
    }
}

#[test]
fn a_range_that_is_reversed_or_unparseable_is_refused_by_the_parser_before_anything_runs() {
    let d = tempfile::tempdir().unwrap();
    for bad in ["0xD100-0xD000", "", "0x10000", "D000", "0xD000-"] {
        let o = run(d.path(), &["vendor-sweep", "--range", bad]);
        assert_eq!(o.status.code(), Some(2), "{bad:?}: {}", stderr(&o));
    }
    assert!(stderr(&run(d.path(), &["vendor-sweep", "--range", "0xD100-0xD000"])).contains("reversed"));
    assert!(!d.path().join("sweeps").exists(), "nothing was created");
}

#[test]
fn a_sweep_of_a_body_that_is_not_there_says_so_and_saves_nothing() {
    let d = tempfile::tempdir().unwrap();
    let o = run(d.path(), &["vendor-sweep", "--serial", "NO-SUCH-BODY", "--range", "0xD000-0xD003"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("NO-SUCH-BODY"), "{}", stderr(&o));
    assert!(!d.path().join("sweeps").exists(), "no dump of a body that never answered");
}

fn two_dumps(d: &Path) -> (String, String) {
    let sweeps = d.join("sweeps");
    let a = dump("before", &[(0xD010, 1), (0xD020, 0)]).save_new(&sweeps).unwrap();
    let mut after = dump("after", &[(0xD010, 2), (0xD020, 0)]);
    after.captured_at = "2026-11-06T10:05:00Z".into();
    let b = after.save_new(&sweeps).unwrap();
    (a.file_name().unwrap().to_string_lossy().into(), b.file_name().unwrap().to_string_lossy().into())
}

#[test]
fn the_diff_names_the_property_that_moved_and_finds_the_dumps_by_file_name() {
    let d = tempfile::tempdir().unwrap();
    let (a, b) = two_dumps(d.path());
    let o = run(d.path(), &["vendor-sweep-diff", &a, &b]);
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("0xd010  CHANGED") && out.contains("current: 1 (0x1) -> 2 (0x2)") && !out.contains("0xd020"), "{out}");
    assert!(out.contains("\"before\"") && out.contains("\"after\""), "the labels are shown: {out}");
}

#[test]
fn the_diff_as_json_is_json_and_a_path_as_given_beats_the_sweeps_directory() {
    let d = tempfile::tempdir().unwrap();
    let (a, b) = two_dumps(d.path());
    let (pa, pb) = (d.path().join("sweeps").join(&a), d.path().join("sweeps").join(&b));
    let other = tempfile::tempdir().unwrap(); // a data dir with no sweeps/: only the full paths can work
    let o = run(other.path(), &["vendor-sweep-diff", pa.to_str().unwrap(), pb.to_str().unwrap(), "--json"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).expect("JSON");
    assert_eq!(v["changes"][0]["code"], "0xd010");
    assert_eq!(v["changes"][0]["kind"], "changed");
    assert_eq!(v["compared"], 256);
}

#[test]
fn a_noise_dump_sets_aside_what_moves_on_its_own_and_keep_noise_puts_it_back() {
    let d = tempfile::tempdir().unwrap();
    let sweeps = d.path().join("sweeps");
    let save = |l: &str, e: &[(u16, u8)], t: &str| {
        let mut x = dump(l, e);
        x.captured_at = t.into();
        x.save_new(&sweeps).unwrap().file_name().unwrap().to_string_lossy().into_owned()
    };
    let a = save("a", &[(0xD010, 1), (0xD020, 0)], "2026-11-06T10:00:00Z");
    let b = save("b", &[(0xD010, 2), (0xD020, 1)], "2026-11-06T10:01:00Z");
    let n = save("n", &[(0xD010, 1), (0xD020, 2)], "2026-11-06T10:02:00Z");
    let o = run(d.path(), &["vendor-sweep-diff", &a, &b, "--noise", &n]);
    let out = stdout(&o);
    assert!(out.contains("1 differ") && out.contains("set aside as noise") && out.contains("0xd020"), "{out}");
    let kept = stdout(&run(d.path(), &["vendor-sweep-diff", &a, &b, "--noise", &n, "--keep-noise"]));
    assert!(kept.contains("2 differ") && !kept.contains("set aside"), "{kept}");
    // --keep-noise without a noise dump is a usage error, not a silent no-op
    assert_eq!(run(d.path(), &["vendor-sweep-diff", &a, &b, "--keep-noise"]).status.code(), Some(2));
}

#[test]
fn a_missing_or_unreadable_dump_is_named_in_the_error() {
    let d = tempfile::tempdir().unwrap();
    let (a, _) = two_dumps(d.path());
    let o = run(d.path(), &["vendor-sweep-diff", &a, "nothing-here.json"]);
    assert!(!o.status.success() && stderr(&o).contains("nothing-here.json"), "{}", stderr(&o));
    // a file that is not a sweep at all
    std::fs::write(d.path().join("sweeps").join("junk.json"), "{ not json").unwrap();
    let o = run(d.path(), &["vendor-sweep-diff", &a, "junk.json"]);
    assert!(!o.status.success() && stderr(&o).contains("junk.json"), "{}", stderr(&o));
    // a dump of another format version is refused, not half-read
    let text = std::fs::read_to_string(d.path().join("sweeps").join(&a)).unwrap().replace("\"format_version\": 1", "\"format_version\": 2");
    std::fs::write(d.path().join("sweeps").join("v2.json"), text).unwrap();
    let o = run(d.path(), &["vendor-sweep-diff", &a, "v2.json"]);
    assert!(!o.status.success() && stderr(&o).contains("format_version 2"), "{}", stderr(&o));
}

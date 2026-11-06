//! `nikon-fleet edit …` through the real binary: copy a stored snapshot, change the copy, and the rules that keep a
//! snapshot read from a camera (and a reference) from being changed. No camera.

use std::path::Path;
use std::process::Output;

use nikon_fleet::extended::{
    Extended, ExtendedProp, FtpProfileEntry, FtpState, FtpStore, IptcSlotEntry, IptcState, IptcStore, PropState, ReadFrom, hex_encode, prop_key,
};
use nikon_fleet::iptc::{IptcPreset, encode_iptc_preset};
use nikon_fleet::ptp_usb::{FtpProfile, encode_ftp_profile};
use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
use serde_json::json;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");
const VR: &str = "kNkMAIDCapability_VibrationReduction";

fn run(data_dir: &Path, args: &[&str]) -> Output {
    common::cli(BIN).arg("--data-dir").arg(data_dir).args(args).output().expect("spawn CLI")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn ok(o: Output) -> String {
    assert!(o.status.success(), "stderr: {}\nstdout: {}", err(&o), out(&o));
    out(&o)
}

fn profile(name: &str) -> FtpProfileEntry {
    let blob = encode_ftp_profile(&FtpProfile {
        profile_name: name.into(),
        ssid_24ghz: "net".into(),
        ssid_5ghz: "wifi-key-12345".into(),
        host: "10.0.0.2".into(),
        username: "u".into(),
        password: "pw-secret".into(),
        port: 8080,
    })
    .unwrap();
    FtpProfileEntry { slot: 0, hex: hex_encode(&blob) }
}

fn captured(dir: &Path) -> &'static str {
    let mut s = Snapshot::new(Camera { model: "Z 9".into(), serial: "S1".into(), firmware: "5.31".into() }, Transport::Usb, "2026-11-05T10:00:00Z".into());
    s.label = Some("base".into());
    s.insert(VR, 34053, json!({"default_index": 2, "elem_bytes": 4, "elem_count": 3, "elem_type": 2, "value_index": 2, "values": [0, 1, 2]}));
    s.insert("kNkMAIDCapability_ExposureComp", 33000, json!({"default": 0.0, "default_index": 15, "lower": -5.0, "steps": 31, "upper": 5.0, "value": 0.0, "value_index": 15}));
    let mut props = std::collections::BTreeMap::new();
    for (code, name, state) in [
        (0xD072u16, "Artist", PropState::Text { text: "Brian".into() }),
        (0xD073, "Copyright", PropState::Text { text: "(c)".into() }),
        (0xD090, "Image comment", PropState::Text { text: String::new() }),
        (0xD053, "EnableCopyright", PropState::Bytes { hex: "01".into() }),
        (0xD060, "IPTCPresetSelect", PropState::Bytes { hex: "00".into() }),
    ] {
        props.insert(prop_key(code), ExtendedProp { code: u32::from(code), name: name.into(), state });
    }
    let mut fields = vec![String::new(); 14];
    fields[2] = "Hello".into();
    s.extended = Some(Extended {
        read_from: ReadFrom { model: "Z 9".into(), usb_serial: "S1".into() },
        props,
        ftp: FtpState::Read(FtpStore {
            count: 3,
            limit: 20,
            active_slot: Some(0),
            device_name: "Z 9_S1".into(),
            truncated: false,
            profiles: ["alpha", "beta", "gamma"].iter().enumerate().map(|(i, n)| FtpProfileEntry { slot: i as u32, ..profile(n) }).collect(),
        }),
        iptc: IptcState::Read(IptcStore {
            count: 1,
            truncated: false,
            slots: vec![IptcSlotEntry { slot: 1, hex: hex_encode(&encode_iptc_preset(&IptcPreset { name: "Race".into(), fields }).unwrap()) }],
        }),
    });
    std::fs::create_dir_all(dir.join("snapshots")).unwrap();
    s.save_to_file(dir.join("snapshots").join("captured.json")).unwrap();
    "captured.json"
}

fn load(dir: &Path, file: &str) -> Snapshot {
    Snapshot::load_from_file(dir.join("snapshots").join(file)).unwrap()
}

#[test]
fn a_copy_is_a_new_marked_file_and_the_original_is_untouched() {
    let d = tempfile::tempdir().unwrap();
    let src = captured(d.path());
    let before = std::fs::read(d.path().join("snapshots").join(src)).unwrap();
    let copy = ok(run(d.path(), &["edit", "copy", src, "--label", "no-vr"]));
    assert!(copy.ends_with(".json") && copy != src, "{copy}");
    assert_eq!(std::fs::read(d.path().join("snapshots").join(src)).unwrap(), before);
    let c = load(d.path(), &copy);
    assert_eq!((c.label.as_deref(), c.is_edited()), (Some("no-vr"), true));
    let ls = ok(run(d.path(), &["ls"]));
    assert!(ls.contains(&format!("{copy}\n  Z 9 S1  label=no-vr")) && ls.contains("[edited copy]"), "{ls}");
    assert!(!ls.contains(&format!("{src}\n  Z 9 S1  label=base  fw=5.31  2026-11-05T10:00:00Z  [edited")), "{ls}");
}

#[test]
fn a_snapshot_read_from_a_camera_is_refused_by_every_edit_command_and_stays_as_it_was() {
    let d = tempfile::tempdir().unwrap();
    let src = captured(d.path());
    let before = std::fs::read(d.path().join("snapshots").join(src)).unwrap();
    for args in [
        vec!["edit", "set", src, "VibrationReduction", "0"],
        vec!["edit", "set-string", src, "artist", "X"],
        vec!["edit", "set-switch", src, "enable-copyright", "0"],
        vec!["edit", "set-iptc", src, "1", "headline", "X"],
        vec!["edit", "ftp-delete", src, "0"],
        vec!["edit", "ftp-move", src, "0", "1"],
    ] {
        let o = run(d.path(), &args);
        assert!(!o.status.success(), "{args:?}");
        assert!(err(&o).contains("not_a_copy"), "{args:?}: {}", err(&o));
    }
    assert_eq!(std::fs::read(d.path().join("snapshots").join(src)).unwrap(), before);
    assert!(ok(run(d.path(), &["edit", "log", src])).contains("was read from a camera"));
}

#[test]
fn edits_to_a_copy_are_kept_and_logged_in_order_without_any_secret() {
    let d = tempfile::tempdir().unwrap();
    let copy = ok(run(d.path(), &["edit", "copy", captured(d.path())]));
    let c = copy.as_str();
    assert!(ok(run(d.path(), &["edit", "set", c, "VibrationReduction", "0"])).contains("VibrationReduction: 2 -> 0"));
    ok(run(d.path(), &["edit", "set", c, "ExposureComp", "-0.3333"])); // a value starting with '-'
    ok(run(d.path(), &["edit", "set-string", c, "Image comment", "-a note"]));
    ok(run(d.path(), &["edit", "set-switch", c, "enable-copyright", "0"]));
    ok(run(d.path(), &["edit", "set-iptc", c, "1", "Writer/editor", "BLW"]));
    ok(run(d.path(), &["edit", "ftp-delete", c, "0"]));
    ok(run(d.path(), &["edit", "ftp-move", c, "1", "0"]));
    let s = load(d.path(), c);
    assert_eq!(s.properties[VR].value["value_index"], 0);
    assert_eq!(s.properties["kNkMAIDCapability_ExposureComp"].value["value_index"], 14);
    let FtpState::Read(store) = &s.extended.as_ref().unwrap().ftp else { panic!() };
    assert_eq!(store.count, 2);
    let log = ok(run(d.path(), &["edit", "log", c]));
    let order: Vec<&str> = ["property:kNkMAIDCapability_VibrationReduction", "property:kNkMAIDCapability_ExposureComp", "string:0xd090", "switch:0xd053", "iptc:1:writereditor", "ftp:delete", "ftp:move"]
        .into_iter()
        .collect();
    let mut at = 0;
    for item in order {
        let pos = log[at..].find(item).unwrap_or_else(|| panic!("{item} after byte {at} in:\n{log}"));
        at += pos + item.len();
    }
    for secret in ["pw-secret", "wifi-key-12345"] {
        assert!(!log.contains(secret) && !std::fs::read_to_string(d.path().join("snapshots").join(c)).unwrap().contains("pw-secret\""), "{secret}");
    }
}

#[test]
fn a_refused_edit_names_the_reason_and_leaves_the_file_byte_for_byte() {
    let d = tempfile::tempdir().unwrap();
    let copy = ok(run(d.path(), &["edit", "copy", captured(d.path())]));
    let bytes = std::fs::read(d.path().join("snapshots").join(&copy)).unwrap();
    for (args, tag) in [
        (vec!["edit", "set", &copy, "VibrationReduction", "9"], "value_not_allowed"),
        (vec!["edit", "set", &copy, "NoSuchThing", "1"], "no_such_property"),
        (vec!["edit", "set", &copy, "ExposureComp", "5.1"], "value_not_allowed"),
        (vec!["edit", "set-string", &copy, "artist", "日本"], "bad_text"),
        (vec!["edit", "set-string", &copy, "nonsense", "x"], "unknown_item"),
        (vec!["edit", "set-switch", &copy, "iptc-preset", "14"], "bad_value"),
        (vec!["edit", "set-iptc", &copy, "7", "headline", "x"], "no_such_slot"),
        (vec!["edit", "ftp-delete", &copy, "3"], "ftp_index"),
        (vec!["edit", "ftp-move", &copy, "0", "3"], "ftp_index"),
        (vec!["edit", "set", "../captured.json", "VibrationReduction", "0"], "unsafe_name"),
        (vec!["edit", "set", "missing.json", "VibrationReduction", "0"], "not_found"),
    ] {
        let o = run(d.path(), &args);
        assert!(!o.status.success() && err(&o).contains(tag), "{args:?} should fail with {tag}: {}", err(&o));
    }
    assert_eq!(std::fs::read(d.path().join("snapshots").join(&copy)).unwrap(), bytes);
}

#[test]
fn an_edit_that_changes_nothing_says_so() {
    let d = tempfile::tempdir().unwrap();
    let copy = ok(run(d.path(), &["edit", "copy", captured(d.path())]));
    let o = ok(run(d.path(), &["edit", "set", &copy, "VibrationReduction", "2"]));
    assert!(o.contains("no change"), "{o}");
    assert!(ok(run(d.path(), &["edit", "log", &copy])).contains("no changes yet"));
}

#[test]
fn an_edited_copy_cannot_become_the_reference_but_a_captured_snapshot_can() {
    let d = tempfile::tempdir().unwrap();
    let src = captured(d.path());
    let copy = ok(run(d.path(), &["edit", "copy", src]));
    let o = run(d.path(), &["ref", "set", d.path().join("snapshots").join(&copy).to_str().unwrap()]);
    assert!(!o.status.success() && err(&o).contains("edited copy"), "{}", err(&o));
    assert!(!d.path().join("references").exists() || std::fs::read_dir(d.path().join("references")).unwrap().next().is_none());
    ok(run(d.path(), &["ref", "set", d.path().join("snapshots").join(src).to_str().unwrap()]));
    assert!(d.path().join("references").read_dir().unwrap().count() == 1);
    // and a reference can be copied, which leaves it as it was
    let reference = d.path().join("references").read_dir().unwrap().next().unwrap().unwrap().path();
    let before = std::fs::read(&reference).unwrap();
    ok(run(d.path(), &["edit", "copy", reference.to_str().unwrap(), "--label", "from-ref"]));
    assert_eq!(std::fs::read(&reference).unwrap(), before);
}

#[test]
fn a_label_that_the_mcp_counts_by_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let src = captured(d.path());
    let o = run(d.path(), &["edit", "copy", src, "--label", "mcp-x"]);
    assert!(!o.status.success() && err(&o).contains("reserved_label"), "{}", err(&o));
}

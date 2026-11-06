//! The Rust FTP-profile decoder (`ptp_usb::decode_ftp_profile`, `parse_ftp_profile_info`,
//! `parse_device_info`, the `FTP_WRITE_CONFIRMED_MODELS` gate) against the golden vectors in
//! `tests/fixtures/ftp_profile_vectors.json`, which `scripts/test_ptpip_read_ftp.py` checks the
//! Python probe `scripts/ptpip_read_ftp.py` against too. Both decoders must give the same fields
//! for the same accepted blobs and the same (field, byte offset) for every rejected one, so the
//! Python tool cannot drift from the Rust decoder unnoticed (review finding #85).
//!
//! The last test runs the Python unit tests themselves, so a plain `cargo test` (which is what CI
//! runs; there is no separate Python job) exercises the script.

use nikon_fleet::ptp_usb::{FTP_WRITE_CONFIRMED_MODELS, PtpUsbError, StringAnomaly, decode_ftp_profile, parse_device_info, parse_ftp_profile_info};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;

fn vectors() -> Value {
    serde_json::from_str(include_str!("fixtures/ftp_profile_vectors.json")).expect("fixture is JSON")
}

fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0 && s.is_ascii(), "odd or non-ASCII hex");
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex digit")).collect()
}

fn apply_edit(base: &[u8], edit: &Value) -> Vec<u8> {
    let mut b = base.to_vec();
    for set in edit["set"].as_array().map(Vec::as_slice).unwrap_or_default() {
        let off = set[0].as_u64().expect("offset") as usize;
        let raw = unhex(set[1].as_str().expect("hex"));
        b[off..off + raw.len()].copy_from_slice(&raw);
    }
    if let Some(a) = edit["append"].as_str() {
        b.extend(unhex(a));
    }
    if let Some(t) = edit["truncate"].as_u64() {
        b.truncate(t as usize);
    }
    b
}

/// The (field, byte offset) of a malformed-blob error; anything else fails the test.
fn blob_error(r: Result<impl std::fmt::Debug, PtpUsbError>) -> Value {
    match r {
        Err(PtpUsbError::FtpBlobMalformed { what, offset, .. }) => json!([what, offset]),
        other => panic!("expected FtpBlobMalformed, got {other:?}"),
    }
}

fn reference_blob(v: &Value) -> Vec<u8> {
    let p = v["profiles"].as_array().unwrap().iter().find(|p| p["name"] == "reference").expect("reference profile");
    unhex(p["hex"].as_str().unwrap())
}

#[test]
fn every_accepted_blob_decodes_to_the_fields_the_python_decoder_is_held_to() {
    let v = vectors();
    let profiles = v["profiles"].as_array().unwrap();
    assert!(profiles.len() >= 6, "the fixture lost profiles");
    for p in profiles {
        let name = p["name"].as_str().unwrap();
        let d = decode_ftp_profile(&unhex(p["hex"].as_str().unwrap())).unwrap_or_else(|e| panic!("{name}: {e}"));
        let got = json!({
            "profile_name": d.profile_name,
            "timestamps": d.timestamps,
            "camera_ipv4": d.camera_network.ipv4,
            "prefix_len": d.camera_network.prefix_len,
            "ssid_24ghz": d.ssid_24ghz,
            "ssid24_lead": d.ssid24_lead,
            "ssid24_kind": d.ssid24_kind,
            "ssid_5ghz": d.ssid_5ghz,
            "ssid5_kind": d.ssid5_kind,
            "host": d.host,
            "folder": d.folder,
            "username": d.username,
            "password": d.password,
            "ftp_port": d.ftp_port,
            "option_flags": d.option_flags,
            "second_port": d.second_port,
            "ipv6_fields": d.ipv6_fields,
        });
        assert_eq!(got, p["expected"], "{name}: the Rust decoder disagrees with the fixture");
    }
}

#[test]
fn every_edit_of_the_reference_blob_is_rejected_at_the_same_field_and_offset() {
    let v = vectors();
    let reference = reference_blob(&v);
    let rejects = v["rejects"].as_array().unwrap();
    assert!(rejects.len() >= 20, "the fixture lost reject vectors");
    for r in rejects {
        let blob = apply_edit(&reference, &r["edit"]);
        assert_eq!(blob_error(decode_ftp_profile(&blob)), json!([r["what"], r["offset"]]), "{}", r["name"]);
    }
}

#[test]
fn every_truncation_of_the_reference_blob_is_rejected_at_the_same_field_and_offset() {
    let v = vectors();
    let reference = reference_blob(&v);
    let errors = v["truncations"]["errors"].as_array().unwrap();
    assert_eq!(errors.len(), reference.len(), "one entry per cut length 0..len");
    for (cut, expected) in errors.iter().enumerate() {
        assert_eq!(blob_error(decode_ftp_profile(&reference[..cut])), *expected, "cut to {cut} byte(s)");
    }
}

#[test]
fn info_replies_parse_to_the_fixture_fields_and_the_bad_ones_fail_at_the_same_place() {
    let v = vectors();
    for i in v["infos"].as_array().unwrap() {
        let info = parse_ftp_profile_info(&unhex(i["hex"].as_str().unwrap())).unwrap();
        let guid: String = info.guid.iter().map(|b| format!("{b:02x}")).collect();
        let got = json!({
            "header": info.header, "count": info.count, "limit": info.limit, "slot": info.slot,
            "guid": guid, "device_name": info.device_name.text, "trailer": info.trailer,
        });
        assert_eq!(got, i["expected"], "{}", i["name"]);
    }
    for r in v["info_rejects"].as_array().unwrap() {
        let got = blob_error(parse_ftp_profile_info(&unhex(r["hex"].as_str().unwrap())));
        assert_eq!(got, json!([r["what"], r["offset"]]), "{}", r["name"]);
    }
}

#[test]
fn device_info_models_match_and_the_malformed_ones_are_flagged_by_rust_and_refused_by_python() {
    let v = vectors();
    for d in v["device_infos"].as_array().unwrap() {
        let ds = parse_device_info(&unhex(d["hex"].as_str().unwrap())).unwrap();
        assert_eq!(ds.model.text, d["model"].as_str().unwrap(), "{}", d["name"]);
        assert!(ds.model.anomalies.is_empty(), "{}", d["name"]);
    }
    // The Rust reader is lossy here (it returns the string with an anomaly); the Python write gate
    // refuses such a model outright. Pin that Rust really flags each of them, so "refused" in the
    // Python test is a statement about a string that is genuinely malformed.
    let expected = [StringAnomaly::MissingTerminator, StringAnomaly::EmbeddedNul, StringAnomaly::UnpairedSurrogate];
    let bad = v["device_info_model_not_well_formed"].as_array().unwrap();
    assert_eq!(bad.len(), expected.len());
    for (b, anomaly) in bad.iter().zip(expected) {
        let ds = parse_device_info(&unhex(b["hex"].as_str().unwrap())).unwrap();
        assert!(ds.model.anomalies.contains(&anomaly), "{}: {:?}", b["name"], ds.model.anomalies);
    }
}

#[test]
fn the_write_confirmed_model_list_is_the_one_the_python_probe_is_held_to() {
    let v = vectors();
    let listed: Vec<&str> = v["write_confirmed_models"].as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect();
    assert_eq!(listed, FTP_WRITE_CONFIRMED_MODELS, "update tests/fixtures/ftp_profile_vectors.json and ptpip_read_ftp.py together");
}

/// Runs `scripts/test_ptpip_read_ftp.py`. The interpreter is looked for as `python3`, `python`, `py`;
/// without one the test says so and passes, unless `NIKON_FLEET_REQUIRE_PYTHON` is set (then it
/// fails: set it wherever a missing interpreter should not hide the Python checks).
#[test]
fn the_python_probe_unit_tests_pass() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = root.join("scripts").join("test_ptpip_read_ftp.py");
    assert!(script.is_file(), "{} is missing", script.display());
    for interpreter in ["python3", "python", "py"] {
        let out = match Command::new(interpreter).args(["-I", "-B"]).arg(&script).current_dir(&root).output() {
            Ok(out) => out,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => panic!("could not run {interpreter}: {e}"),
        };
        assert!(
            out.status.success(),
            "{interpreter} {} failed ({:?}):\n{}\n{}",
            script.display(),
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    if std::env::var_os("NIKON_FLEET_REQUIRE_PYTHON").is_some() {
        panic!("no Python interpreter (python3, python, py) on PATH and NIKON_FLEET_REQUIRE_PYTHON is set");
    }
    eprintln!("SKIPPED: no Python interpreter found; scripts/test_ptpip_read_ftp.py was not run");
}

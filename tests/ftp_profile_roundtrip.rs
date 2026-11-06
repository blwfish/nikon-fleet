//! Writing a decoded FTP profile back (`ptp_usb::encode_decoded_ftp_profile`): a profile that was read and not
//! changed comes back byte for byte, for every real blob in the fixtures; and a changed field is the only difference.

use nikon_fleet::ptp_usb::{DecodedFtpProfile, decode_ftp_profile, encode_decoded_ftp_profile};
use serde_json::Value;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex digit")).collect()
}

fn real_blobs() -> Vec<(String, Vec<u8>)> {
    let v: Value = serde_json::from_str(include_str!("fixtures/ftp_profile_vectors.json")).expect("fixture is JSON");
    v["profiles"].as_array().unwrap().iter().map(|p| (p["name"].as_str().unwrap().to_string(), unhex(p["hex"].as_str().unwrap()))).collect()
}

#[test]
fn every_real_profile_is_written_back_byte_for_byte() {
    let blobs = real_blobs();
    assert!(blobs.len() >= 6, "the fixture holds the real blobs this pins");
    for (name, blob) in blobs {
        let decoded = decode_ftp_profile(&blob).unwrap_or_else(|e| panic!("{name}: {e}"));
        let back = encode_decoded_ftp_profile(&decoded).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(back, blob, "{name}: writing back what was read changed {} byte(s)", back.iter().zip(&blob).filter(|(a, b)| a != b).count());
    }
}

/// One edit, and how to put back just the field it touched (from the original).
type Edit = (&'static str, fn(&mut DecodedFtpProfile), fn(&mut DecodedFtpProfile, &DecodedFtpProfile));

const EDITS: [Edit; 9] = [
    ("profile_name", |d| d.profile_name.push('X'), |d, o| d.profile_name = o.profile_name.clone()),
    ("ssid_24ghz", |d| d.ssid_24ghz.push('X'), |d, o| d.ssid_24ghz = o.ssid_24ghz.clone()),
    ("ssid_5ghz", |d| d.ssid_5ghz.push('X'), |d, o| d.ssid_5ghz = o.ssid_5ghz.clone()),
    ("host", |d| d.host = "files.example.org".into(), |d, o| d.host = o.host.clone()),
    ("folder", |d| d.folder = "/incoming".into(), |d, o| d.folder = o.folder.clone()),
    ("username", |d| d.username.push('X'), |d, o| d.username = o.username.clone()),
    ("password", |d| d.password.push('X'), |d, o| d.password = o.password.clone()),
    ("ftp_port", |d| d.ftp_port = 2222, |d, o| d.ftp_port = o.ftp_port),
    ("option_flags", |d| d.option_flags ^= 0x0008, |d, o| d.option_flags = o.option_flags),
];

/// For every real blob and every edit: what is written reads back as what was set; and putting back only the field
/// that was edited, in the decoded result, gives the original profile and (written again) the original bytes. That
/// second half is what says no OTHER field moved: comparing the result with the same edit applied to a clone, as this
/// test once did, cannot see a second field changing, because both sides would change it the same way.
#[test]
fn changing_one_field_changes_that_field_and_nothing_else() {
    let blobs = real_blobs();
    assert!(blobs.len() >= 6, "more than one real profile: the fixture holds {}", blobs.len());
    for (name, blob) in blobs {
        let original = decode_ftp_profile(&blob).unwrap();
        for (what, edit, restore) in EDITS {
            let mut d = original.clone();
            edit(&mut d);
            let new_blob = encode_decoded_ftp_profile(&d).unwrap_or_else(|e| panic!("{name} / {what}: {e}"));
            let mut decoded = decode_ftp_profile(&new_blob).unwrap_or_else(|e| panic!("{name} / {what}: the result does not decode: {e}"));
            assert_eq!(decoded, d, "{name} / {what}: what is written reads back as what was set");
            assert_ne!(new_blob, blob, "{name} / {what} changed something");
            restore(&mut decoded, &original);
            assert_eq!(decoded, original, "{name} / {what}: something besides the edited field changed");
            assert_eq!(encode_decoded_ftp_profile(&decoded).unwrap(), blob, "{name} / {what}: and the bytes are the original's again");
        }
    }
}

/// Every FTP profile in every snapshot of a data directory: `NIKON_FLEET_ROUNDTRIP_DIR=<data dir> cargo test -p
/// nikon-fleet --test ftp_profile_roundtrip -- --ignored --nocapture`. Not run by default: it needs profiles read from
/// your own bodies, which this repository does not hold.
#[test]
#[ignore = "reads the snapshots of a data directory named by NIKON_FLEET_ROUNDTRIP_DIR"]
fn the_profiles_in_a_real_data_directory_are_written_back_byte_for_byte() {
    use nikon_fleet::extended::{FtpState, hex_decode};
    use nikon_fleet::snapshot::Snapshot;
    let dir = std::env::var("NIKON_FLEET_ROUNDTRIP_DIR").expect("NIKON_FLEET_ROUNDTRIP_DIR names a data directory");
    let (mut seen, mut differ) = (0, Vec::new());
    for entry in std::fs::read_dir(std::path::Path::new(&dir).join("snapshots")).unwrap().flatten() {
        let Ok(snap) = Snapshot::load_from_file(entry.path()) else { continue };
        let Some(FtpState::Read(store)) = snap.extended.as_ref().map(|e| e.ftp.clone()) else { continue };
        for p in store.profiles {
            let blob = hex_decode(&p.hex).unwrap();
            let Ok(d) = decode_ftp_profile(&blob) else { continue };
            seen += 1;
            if encode_decoded_ftp_profile(&d).ok().as_deref() != Some(blob.as_slice()) {
                differ.push(format!("{} slot {}", entry.file_name().to_string_lossy(), p.slot));
            }
        }
    }
    println!("{seen} profile(s) read, {} that do not come back byte for byte", differ.len());
    assert!(differ.is_empty(), "{differ:?}");
}

#[test]
fn text_a_camera_made_profile_holds_is_written_back_even_where_the_encoder_would_not_write_it_new() {
    // The decoder reads the byte-length fields as UTF-8, so a profile made in the camera's menu may hold non-ASCII
    // text there. Writing such a profile back unchanged must not refuse it (the edit layer is what restricts new text).
    let (_, blob) = real_blobs().into_iter().next().unwrap();
    let mut d = decode_ftp_profile(&blob).unwrap();
    d.username = "Zo\u{eb}".into();
    d.ssid_24ghz = "caf\u{e9}".into();
    let back = encode_decoded_ftp_profile(&d).expect("non-ASCII in a byte-length field is written as the UTF-8 it was read as");
    assert_eq!(decode_ftp_profile(&back).unwrap(), d);
    // control characters and over-long text are still refused
    let mut bad = decode_ftp_profile(&blob).unwrap();
    bad.host = "a\u{1}b".into();
    assert!(encode_decoded_ftp_profile(&bad).is_err());
    let mut long = decode_ftp_profile(&blob).unwrap();
    long.ssid_24ghz = "x".repeat(33);
    assert!(encode_decoded_ftp_profile(&long).is_err());
    long.ssid_24ghz = "x".repeat(32);
    assert!(encode_decoded_ftp_profile(&long).is_ok(), "the limit itself is allowed");
}

//! Tests of the layout spec itself: that it is well-formed, that what it states is what the encoder,
//! the decoder, the spans and the gate do, and that a new variant of the blob is a change to the
//! table and nothing else. (That the spec reproduces the hand-written code it replaced is
//! `ftp_golden`.)

use super::*;
use crate::ptp_usb::ftp_golden::{Rng, gen_profile};
use serde_json::Value;

// ── fixtures ─────────────────────────────────────────────────────────────────────────────────

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex digit")).collect()
}

fn fixture_blob(name: &str) -> Vec<u8> {
    let v: Value = serde_json::from_str(include_str!("../../../tests/fixtures/ftp_profile_vectors.json")).unwrap();
    let p = v["profiles"].as_array().unwrap().iter().find(|p| p["name"] == name).unwrap_or_else(|| panic!("no fixture {name}"));
    unhex(p["hex"].as_str().unwrap())
}

fn reference() -> Vec<u8> {
    fixture_blob("reference")
}

/// The profile of the 317- and 331-byte captures ("C2").
fn c2() -> FtpProfile {
    FtpProfile {
        profile_name: "C2".into(),
        ssid_24ghz: "nikon-snoop".into(),
        ssid_5ghz: "nikon5ghz".into(),
        host: "192.168.66.1".into(),
        port: 8080,
        username: "fleet".into(),
        password: "12345678".into(),
    }
}

/// A copy of the canonical table for a test to edit: the "new variant" of the layout.
fn variant(edit: impl FnOnce(&mut Vec<FtpLayoutEntry>)) -> Vec<FtpLayoutEntry> {
    let mut v = FTP_LAYOUT.to_vec();
    edit(&mut v);
    v
}

fn at(layout: &[FtpLayoutEntry], name: &str) -> usize {
    layout.iter().position(|e| e.name == name).unwrap_or_else(|| panic!("no entry {name}"))
}

fn slots_of(entry: &FtpLayoutEntry) -> Vec<FtpSlot> {
    match entry.wire {
        FtpWire::Literal(_) => vec![],
        FtpWire::Pattern(bytes) => bytes
            .iter()
            .filter_map(|b| match b {
                FtpTagByte::Var { slot, .. } => Some(*slot),
                FtpTagByte::Exact(_) => None,
            })
            .collect(),
        FtpWire::Value { slot, .. } => vec![slot],
    }
}

// ── the table is well-formed ─────────────────────────────────────────────────────────────────

#[test]
fn the_layout_places_every_slot_and_every_profile_field_exactly_once() {
    let placed: Vec<FtpSlot> = FTP_LAYOUT.iter().flat_map(slots_of).collect();
    for slot in FtpSlot::ALL {
        assert_eq!(placed.iter().filter(|s| *s == slot).count(), 1, "{slot:?} must appear exactly once");
    }
    assert_eq!(placed.len(), FtpSlot::ALL.len());
    let keys: Vec<&str> = FtpSlot::ALL.iter().map(|s| s.key()).collect();
    for (i, k) in keys.iter().enumerate() {
        assert!(!keys[..i].contains(k), "duplicate slot key {k}");
    }
    for field in FtpField::ALL {
        let sources = FTP_LAYOUT
            .iter()
            .filter(|e| matches!(e.wire, FtpWire::Value { kind: FtpValueKind::Text(_, FtpTextSource::Profile(f)), .. } if f == field))
            .count();
        assert_eq!(sources, 1, "{field:?} must be written from exactly one place");
        assert!(encoding_of(FTP_LAYOUT, field).is_some());
        assert_eq!(field.spec().encoding(), encoding_of(FTP_LAYOUT, field).unwrap());
    }
    let ports = FTP_LAYOUT
        .iter()
        .filter(|e| matches!(e.wire, FtpWire::Value { kind: FtpValueKind::U16(FtpU16Source::ProfilePort), .. }))
        .count();
    assert_eq!(ports, 1, "FtpProfile.port is written in exactly one place");
    for (i, e) in FTP_LAYOUT.iter().enumerate() {
        assert!(!e.name.is_empty());
        assert!(FTP_LAYOUT[..i].iter().all(|o| o.name != e.name), "duplicate section name {}", e.name);
    }
}

#[test]
fn the_value_kinds_agree_with_the_types_of_their_slots() {
    let blank = DecodedFtpProfile::blank();
    for e in FTP_LAYOUT {
        if let FtpWire::Value { slot, kind, .. } = e.wire {
            let held = blank.get(slot);
            let ok = matches!(
                (kind, &held),
                (FtpValueKind::Text(..), FtpVal::Text(_))
                    | (FtpValueKind::U8(_), FtpVal::U8(_))
                    | (FtpValueKind::U16(_), FtpVal::U16(_))
                    | (FtpValueKind::NetworkAddress, FtpVal::Ipv4(_))
            );
            assert!(ok, "{}: {kind:?} cannot be stored in {slot:?}, which holds {held:?}", e.name);
        }
        if let FtpWire::Pattern(bytes) = e.wire {
            for b in bytes {
                if let FtpTagByte::Var { slot, allowed, written } = *b {
                    assert!(matches!(DecodedFtpProfile::blank().get(slot), FtpVal::U8(_)), "{slot:?} holds a tag byte");
                    if let Some(set) = allowed {
                        assert!(set.contains(&written), "{slot:?}: the encoder writes a byte its own decoder rejects");
                    }
                }
            }
        }
        // A fixed text must be something the encoder can lay out.
        if let FtpWire::Value { kind: FtpValueKind::Text(enc, FtpTextSource::Fixed(t)), .. } = e.wire {
            assert!(enc.encode(e.name, t).is_ok(), "{}", e.name);
        }
    }
}

#[test]
fn each_slot_is_its_own_field_of_the_decoded_profile() {
    // A slot that aliased another slot's field would show up as two slots changing together.
    let blank = DecodedFtpProfile::blank();
    for slot in FtpSlot::ALL {
        let probe = match blank.get(*slot) {
            FtpVal::Text(_) => FtpVal::Text("probe".into()),
            FtpVal::U8(_) => FtpVal::U8(0xA7),
            FtpVal::U16(_) => FtpVal::U16(0xA7A7),
            FtpVal::Ipv4(_) => FtpVal::Ipv4([9, 8, 7, 6]),
        };
        let mut d = blank.clone();
        d.set(*slot, probe.clone());
        for other in FtpSlot::ALL {
            let expect = if other == slot { probe.clone() } else { blank.get(*other) };
            assert_eq!(d.get(*other), expect, "setting {slot:?} changed {other:?}");
        }
    }
}

#[test]
#[should_panic(expected = "layout bug: slot Host holds text")]
fn a_value_of_the_wrong_type_for_its_slot_is_a_loud_layout_bug() {
    DecodedFtpProfile::blank().set(FtpSlot::Host, FtpVal::U16(1));
}

#[test]
fn the_layout_is_pinned_literally_in_wire_order() {
    // By literal names and fixed values, not by the constants the table uses: changing the blob is a
    // deliberate edit of this test.
    let names: Vec<&str> = FTP_LAYOUT.iter().map(|e| e.name).collect();
    assert_eq!(
        names,
        [
            "blob header", "profile name", "timestamp 1", "timestamp 2", "network tag", "camera address",
            "camera prefix length", "network padding", "2.4 GHz SSID tag", "2.4 GHz SSID", "5 GHz SSID tag",
            "5 GHz SSID", "host tag", "host", "folder", "FTP port", "username", "password", "option flags",
            "second port", "port-section padding", "IPv6 field 1", "IPv6 separator", "IPv6 field 2", "IPv6 field 3",
            "blob trailer",
        ]
    );
    let secrets: Vec<&str> = secret_slots(FTP_LAYOUT).map(|s| s.key()).collect();
    assert_eq!(secrets, ["ssid_5ghz", "password"], "the secrets, in wire order");
    let literals: Vec<(&str, &[u8])> = FTP_LAYOUT
        .iter()
        .filter_map(|e| match e.wire {
            FtpWire::Literal(b) => Some((e.name, b)),
            _ => None,
        })
        .collect();
    let want_literals: [(&str, &[u8]); 7] = [
        ("blob header", &[0xf4, 0x01]),
        ("network tag", &[0x02, 0x03, 0x03]),
        ("network padding", &[0; 8]),
        ("host tag", &[0x01]),
        ("port-section padding", &[0; 5]),
        ("IPv6 separator", &[0x40]),
        ("blob trailer", &[0x81]),
    ];
    assert_eq!(literals, want_literals);
    // What the encoder writes where it does not take the value from the caller.
    let fixed: Vec<(&str, String)> = FTP_LAYOUT
        .iter()
        .filter_map(|e| match e.wire {
            FtpWire::Value { kind: FtpValueKind::Text(_, FtpTextSource::Fixed(t)), .. } => Some((e.name, format!("{t:?}"))),
            FtpWire::Value { kind: FtpValueKind::U16(FtpU16Source::Fixed(v)), .. } => Some((e.name, v.to_string())),
            FtpWire::Value { kind: FtpValueKind::U8(FtpU8Source::Fixed(v)), .. } => Some((e.name, v.to_string())),
            _ => None,
        })
        .collect();
    let zeros = "\"00000000T000000\"".to_string();
    let v6 = "\"0000:0000:0000:0000:0000:0000:0000:0000\"".to_string();
    assert_eq!(
        fixed,
        [
            ("timestamp 1", zeros.clone()),
            ("timestamp 2", zeros),
            ("folder", "\"\"".to_string()),
            ("FTP port", "21".to_string()),
            ("option flags", "5".to_string()),
            ("IPv6 field 1", v6.clone()),
            ("IPv6 field 2", v6.clone()),
            ("IPv6 field 3", v6),
        ]
    );
    let tag24: Vec<_> = match FTP_LAYOUT[at(FTP_LAYOUT, "2.4 GHz SSID tag")].wire {
        FtpWire::Pattern(b) => b.to_vec(),
        _ => panic!(),
    };
    assert_eq!(
        tag24,
        [
            FtpTagByte::Var { slot: FtpSlot::Ssid24Lead, allowed: Some(&[0x03, 0x00]), written: 0x03 },
            FtpTagByte::Exact(0x00),
            FtpTagByte::Var { slot: FtpSlot::Ssid24Kind, allowed: None, written: 0x01 },
            FtpTagByte::Exact(0x00),
        ]
    );
    let tag5: Vec<_> = match FTP_LAYOUT[at(FTP_LAYOUT, "5 GHz SSID tag")].wire {
        FtpWire::Pattern(b) => b.to_vec(),
        _ => panic!(),
    };
    assert_eq!(tag5, [FtpTagByte::Var { slot: FtpSlot::Ssid5Kind, allowed: None, written: 0x04 }, FtpTagByte::Exact(0x04)]);
    // Wire forms of the caller's fields.
    let enc: Vec<(FtpField, FtpEncoding)> = FtpField::ALL.iter().map(|f| (*f, f.spec().encoding())).collect();
    assert_eq!(
        enc,
        [
            (FtpField::ProfileName, FtpEncoding::Utf16U8Len),
            (FtpField::Ssid24Ghz, FtpEncoding::AsciiU32Len),
            (FtpField::Ssid5Ghz, FtpEncoding::AsciiU32Len),
            (FtpField::Host, FtpEncoding::Utf16U8Len),
            (FtpField::Username, FtpEncoding::AsciiU32Len),
            (FtpField::Password, FtpEncoding::AsciiU32Len),
        ]
    );
    assert_eq!(FTP_LAYOUT[at(FTP_LAYOUT, "folder")].wire, FtpWire::Value {
        slot: FtpSlot::Folder,
        kind: FtpValueKind::Text(FtpEncoding::Utf16U8LenOptional, FtpTextSource::Fixed("")),
        secret: false
    });
}

// ── the forms ────────────────────────────────────────────────────────────────────────────────

#[test]
fn text_forms_lay_out_and_bound_themselves() {
    let enc = |e: FtpEncoding, s: &str| e.encode("f", s);
    // u32 form: byte length, no terminator.
    assert_eq!(enc(FtpEncoding::AsciiU32Len, "").unwrap(), [0, 0, 0, 0]);
    assert_eq!(enc(FtpEncoding::AsciiU32Len, "ab").unwrap(), [2, 0, 0, 0, b'a', b'b']);
    // UTF-16 form: unit count including the terminator; an empty string is the terminator alone.
    assert_eq!(enc(FtpEncoding::Utf16U8Len, "").unwrap(), [1, 0, 0]);
    assert_eq!(enc(FtpEncoding::Utf16U8Len, "a").unwrap(), [2, b'a', 0, 0, 0]);
    assert_eq!(enc(FtpEncoding::Utf16U8Len, "😀").unwrap(), [3, 0x3d, 0xd8, 0x00, 0xde, 0, 0], "a surrogate pair is two units");
    // The optional form differs from the plain one only for the empty string.
    assert_eq!(enc(FtpEncoding::Utf16U8LenOptional, "").unwrap(), [0]);
    assert_eq!(enc(FtpEncoding::Utf16U8LenOptional, "a").unwrap(), enc(FtpEncoding::Utf16U8Len, "a").unwrap());
    // The one-byte count: 254 units + terminator = 255 fits, 255 units does not, and neither wraps.
    for e in [FtpEncoding::Utf16U8Len, FtpEncoding::Utf16U8LenOptional] {
        let ok = enc(e, &"a".repeat(254)).unwrap();
        assert_eq!((ok[0], ok.len()), (255, 1 + 255 * 2), "{e:?}");
        let err = enc(e, &"a".repeat(255)).unwrap_err();
        assert!(matches!(err, PtpUsbError::FtpFieldTooLong { field: "f", len: 255, max: 254, unit: "UTF-16 units" }), "{err:?}");
        let err = enc(e, &"😀".repeat(128)).unwrap_err();
        assert!(matches!(err, PtpUsbError::FtpFieldTooLong { len: 256, .. }), "units, not characters: {err:?}");
    }
    assert!(FtpEncoding::AsciiU32Len.encoder_ascii_only());
    assert!(!FtpEncoding::Utf16U8Len.encoder_ascii_only());
    assert!(!FtpEncoding::Utf16U8LenOptional.encoder_ascii_only());
    assert_eq!(FtpEncoding::AsciiU32Len.unit(), FtpLenUnit::Bytes);
    assert_eq!(FtpEncoding::Utf16U8Len.unit(), FtpLenUnit::Utf16Units);
    assert_eq!(FtpEncoding::Utf16U8LenOptional.unit(), FtpLenUnit::Utf16Units);
}

// ── the gate is the table ────────────────────────────────────────────────────────────────────

/// A decoded profile the gate accepts: the reference capture.
fn encodable() -> DecodedFtpProfile {
    let d = decode_with(FTP_LAYOUT, &reference()).unwrap().0;
    assert!(encodable_with(FTP_LAYOUT, &d).is_some());
    d
}

/// Another value of the same type as `v`, differing by one at the low end of its range (so a value at,
/// below and above a threshold are all reachable by the callers).
fn other_than(v: &FtpVal, step: i32) -> FtpVal {
    match v {
        FtpVal::Text(t) => FtpVal::Text(format!("{t}x")),
        FtpVal::U8(b) => FtpVal::U8((i32::from(*b) + step).rem_euclid(256) as u8),
        FtpVal::U16(w) => FtpVal::U16((i32::from(*w) + step).rem_euclid(65536) as u16),
        FtpVal::Ipv4(a) => FtpVal::Ipv4([a[0].wrapping_add(step as u8), a[1], a[2], a[3]]),
    }
}

#[test]
fn the_gate_refuses_a_change_to_every_value_the_table_says_the_encoder_fixes() {
    let base = encodable();
    let mut checked = Vec::new();
    for e in FTP_LAYOUT {
        let fixed_slots: Vec<FtpSlot> = match e.wire {
            FtpWire::Literal(_) => vec![],
            FtpWire::Pattern(bytes) => bytes
                .iter()
                .filter_map(|b| match b {
                    FtpTagByte::Var { slot, .. } => Some(*slot),
                    _ => None,
                })
                .collect(),
            FtpWire::Value { slot, kind, .. } => match kind {
                FtpValueKind::Text(_, FtpTextSource::Profile(_)) | FtpValueKind::U16(FtpU16Source::ProfilePort) => vec![],
                _ => vec![slot],
            },
        };
        for slot in fixed_slots {
            // at the fixed value: encodable (the base); one below and one above: not.
            assert!(encodable_with(FTP_LAYOUT, &base).is_some());
            for step in [-1, 1] {
                let mut d = base.clone();
                d.set(slot, other_than(&base.get(slot), step));
                assert!(encodable_with(FTP_LAYOUT, &d).is_none(), "{slot:?} {step:+} must make the profile unwritable");
            }
            checked.push(slot);
        }
    }
    // Pinned: these are the slots the encoder fixes today.
    let keys: Vec<&str> = checked.iter().map(|s| s.key()).collect();
    assert_eq!(
        keys,
        [
            "timestamp_1", "timestamp_2", "camera_address", "camera_prefix_len", "ssid24_lead", "ssid24_kind",
            "ssid5_kind", "folder", "ftp_port", "option_flags", "ipv6_field_1", "ipv6_field_2", "ipv6_field_3",
        ]
    );
}

#[test]
fn the_gate_threshold_values_are_exact() {
    // ftp_port 21 is the only port; 20 and 22 (the SFTP port seen in a profile) are not.
    let base = encodable();
    for (port, ok) in [(20u16, false), (21, true), (22, false)] {
        let mut d = base.clone();
        d.ftp_port = port;
        assert_eq!(encodable_with(FTP_LAYOUT, &d).is_some(), ok, "ftp_port {port}");
    }
    for (flags, ok) in [(0x0004u16, false), (0x0005, true), (0x0006, false), (0x0001, false)] {
        let mut d = base.clone();
        d.option_flags = flags;
        assert_eq!(encodable_with(FTP_LAYOUT, &d).is_some(), ok, "option flags {flags:#06x}");
    }
    // The 2.4 GHz lead byte: 03 is written, 00 is decoded but not written, 02 never decodes.
    for (lead, ok) in [(0x02u8, false), (0x03, true), (0x00, false), (0x04, false)] {
        let mut d = base.clone();
        d.ssid24_lead = lead;
        assert_eq!(encodable_with(FTP_LAYOUT, &d).is_some(), ok, "lead {lead:#04x}");
    }
}

#[test]
fn the_gate_carries_caller_fields_and_checks_ascii_exactly_where_the_wire_form_is_ascii_only() {
    let base = encodable();
    // Each caller field is copied through, whatever its value.
    for field in FtpField::ALL {
        let slot = FTP_LAYOUT
            .iter()
            .find_map(|e| match e.wire {
                FtpWire::Value { slot, kind: FtpValueKind::Text(_, FtpTextSource::Profile(f)), .. } if f == field => Some(slot),
                _ => None,
            })
            .unwrap();
        let ascii_only = field.spec().encoding().encoder_ascii_only();
        let mut d = base.clone();
        d.set(slot, FtpVal::Text("pl\u{e9}in".into()));
        let got = encodable_with(FTP_LAYOUT, &d);
        assert_eq!(got.is_some(), !ascii_only, "{field:?}: non-ASCII is refused iff the encoder writes it as ASCII");
        if let Some(p) = got {
            assert_eq!(p.text(field), "pl\u{e9}in");
        }
        let mut d = base.clone();
        d.set(slot, FtpVal::Text("plain".into()));
        assert_eq!(encodable_with(FTP_LAYOUT, &d).unwrap().text(field), "plain");
    }
    // The second port is the one the encoder takes from FtpProfile.port, at any value.
    for port in [0u16, 1, 8080, 65535] {
        let mut d = base.clone();
        d.second_port = port;
        assert_eq!(encodable_with(FTP_LAYOUT, &d).unwrap().port, port);
    }
}

// ── decoder errors come from the table ───────────────────────────────────────────────────────

fn malformed(r: Result<(DecodedFtpProfile, FtpSecretSpans), PtpUsbError>) -> (&'static str, usize, String) {
    match r {
        Err(PtpUsbError::FtpBlobMalformed { what, offset, detail }) => (what, offset, detail),
        other => panic!("expected FtpBlobMalformed, got {other:?}"),
    }
}

#[test]
fn every_truncation_is_reported_at_the_section_the_table_names_in_wire_order() {
    let blob = reference();
    let mut last_index = 0;
    let mut seen = std::collections::BTreeSet::new();
    for cut in 0..blob.len() {
        let (what, offset, _) = malformed(decode_with(FTP_LAYOUT, &blob[..cut]));
        let index = at(FTP_LAYOUT, what);
        assert!(index >= last_index, "cut {cut}: {what} came after a later section");
        assert!(offset <= cut, "cut {cut}: offset {offset} is past the end");
        last_index = index;
        seen.insert(what);
    }
    let all: std::collections::BTreeSet<&str> = FTP_LAYOUT.iter().map(|e| e.name).collect();
    assert_eq!(seen, all, "every section of the table is the first to run out of bytes for some cut");
}

#[test]
fn tag_errors_state_the_pattern_the_table_has() {
    let blob = reference();
    let tag24 = 99; // offset of the 2.4 GHz SSID tag in the reference capture
    assert_eq!(&blob[tag24..tag24 + 4], [0x03, 0x00, 0x01, 0x00]);
    let mut b = blob.clone();
    b[tag24] = 0x02;
    assert_eq!(malformed(decode_with(FTP_LAYOUT, &b)), ("2.4 GHz SSID tag", 99, "expected 03|00 00 xx 00, found 02 00 01 00".to_string()));
    let mut b = blob.clone();
    b[tag24 + 3] = 0x01;
    assert_eq!(malformed(decode_with(FTP_LAYOUT, &b)).2, "expected 03|00 00 xx 00, found 03 00 01 01");
    // The free byte (`xx`) takes any value, and is kept.
    let mut b = blob.clone();
    b[tag24 + 2] = 0xEE;
    assert_eq!(decode_with(FTP_LAYOUT, &b).unwrap().0.ssid24_kind, 0xEE);
    // The 5 GHz tag's fixed second byte.
    let tag5 = 99 + 4 + 4 + "nikon-snoop".len();
    assert_eq!(&blob[tag5..tag5 + 2], [0x04, 0x04]);
    let mut b = blob.clone();
    b[tag5 + 1] = 0x05;
    assert_eq!(malformed(decode_with(FTP_LAYOUT, &b)), ("5 GHz SSID tag", tag5, "expected xx 04, found 04 05".to_string()));
    // A literal says what it expected.
    let mut b = blob;
    b[0] = 0;
    assert_eq!(malformed(decode_with(FTP_LAYOUT, &b)), ("blob header", 0, "expected f4 01, found 00 01".to_string()));
}

#[test]
fn trailing_bytes_are_reported_at_the_end_of_the_last_section() {
    let mut b = reference();
    let len = b.len();
    b.push(0);
    assert_eq!(malformed(decode_with(FTP_LAYOUT, &b)), ("blob end", len, "1 unexpected byte(s) after the trailer".to_string()));
}

#[test]
fn the_spans_are_the_content_of_the_values_the_table_marks_secret() {
    let blob = reference();
    let (d, spans) = decode_with(FTP_LAYOUT, &blob).unwrap();
    let keys: Vec<&str> = spans.spans.iter().map(|s| s.field).collect();
    assert_eq!(keys, ["ssid_5ghz", "password"]);
    assert_eq!(&blob[spans.of("ssid_5ghz").unwrap().clone()], b"nikon5ghz");
    assert_eq!(&blob[spans.of("password").unwrap().clone()], b"12345678");
    assert_eq!(spans.of("username"), None, "not a secret");
    assert_eq!(spans.of("no such field"), None);
    assert_eq!(spans.ranges().count(), 2);
    assert_eq!(d.secret_texts(), ["nikon5ghz", "12345678"]);
    assert_eq!(d.with_secrets_masked().password, "********");
    assert_eq!(d.with_secrets_masked().ssid_5ghz, "*********");
    assert_eq!(d.with_secrets_masked().username, "fleet");
    // The mask has the secret's BYTE length (what a display of the length shows), not its character count.
    let mut multibyte = d.clone();
    multibyte.password = "p\u{e9}\u{4e2d}".into(); // 1 + 2 + 3 bytes, 3 characters
    assert_eq!(multibyte.with_secrets_masked().password, "******");
    assert_eq!(multibyte.secret_texts(), ["nikon5ghz", "p\u{e9}\u{4e2d}"]);
}

#[test]
fn what_debug_hides_is_what_the_table_marks_secret() {
    // The Debug impls are hand-written (they print field names the derive would), so pin them to the
    // table: a secret slot never prints its value, any other text slot does.
    let base = encodable();
    for e in FTP_LAYOUT {
        let FtpWire::Value { slot, kind, secret } = e.wire else { continue };
        let FtpValueKind::Text(..) = kind else { continue };
        let marker = format!("MARK-{}", slot.key());
        let mut d = base.clone();
        d.set(slot, FtpVal::Text(marker.clone()));
        assert_eq!(!format!("{d:?}").contains(&marker), secret, "DecodedFtpProfile Debug, slot {slot:?}");
        if let FtpValueKind::Text(_, FtpTextSource::Profile(field)) = kind {
            let mut p = c2();
            match field {
                FtpField::ProfileName => p.profile_name = marker.clone(),
                FtpField::Ssid24Ghz => p.ssid_24ghz = marker.clone(),
                FtpField::Ssid5Ghz => p.ssid_5ghz = marker.clone(),
                FtpField::Host => p.host = marker.clone(),
                FtpField::Username => p.username = marker.clone(),
                FtpField::Password => p.password = marker.clone(),
            }
            assert_eq!(!format!("{p:?}").contains(&marker), secret, "FtpProfile Debug, field {field:?}");
        }
    }
}

// ── the encoder is the table ─────────────────────────────────────────────────────────────────

#[test]
fn encode_then_decode_then_gate_recovers_the_profile_and_the_network_blocks_the_gate() {
    let mut rng = Rng(0x00F7_5EED_1A10);
    let mut encoded = 0;
    let mut writable = 0;
    for _ in 0..1500 {
        let (p, net) = gen_profile(&mut rng);
        let Ok(blob) = encode_with(FTP_LAYOUT, &p, &net) else { continue };
        encoded += 1;
        let (d, _) = decode_with(FTP_LAYOUT, &blob).unwrap_or_else(|e| panic!("own output does not decode: {e:?} for {p:?}"));
        assert_eq!(d.camera_network, net);
        assert_eq!(d.second_port, p.port);
        assert_eq!(d.folder, "");
        let back = encodable_with(FTP_LAYOUT, &d);
        if net == FtpCameraNetwork::CAPTURED {
            let back = back.expect("the encoder's own output is writable");
            assert_eq!(back, p);
            assert_eq!(encode_with(FTP_LAYOUT, &back, &net).unwrap(), blob);
            writable += 1;
        } else {
            assert!(back.is_none(), "a non-captured network is not what the encoder writes by default: {net:?}");
        }
    }
    assert!(encoded > 600 && writable > 300, "the property must not be vacuous: {encoded} encoded, {writable} writable");
}

#[test]
fn the_encoder_reports_the_first_bad_field_in_wire_order() {
    let mut p = c2();
    p.password = "é".into(); // later in the blob
    p.host = "h".repeat(200); // earlier
    let err = encode_with(FTP_LAYOUT, &p, &FtpCameraNetwork::CAPTURED).unwrap_err();
    assert!(matches!(err, PtpUsbError::FtpFieldTooLong { field: "host", len: 200, max: 199, .. }), "{err:?}");
    p.host = "h".into();
    p.ssid_24ghz = "\t".into(); // earlier than the password
    let err = encode_with(FTP_LAYOUT, &p, &FtpCameraNetwork::CAPTURED).unwrap_err();
    assert!(matches!(err, PtpUsbError::FtpFieldControlChar { field: "ssid_24ghz" }), "{err:?}");
}

// ── a new variant of the blob is a change to the table ───────────────────────────────────────

/// The 331-byte capture: the 317-byte profile with a destination folder `Claude` and option flags
/// `0x0001`. Writing it is NOT enabled in the real table (nothing has shown what a camera does with
/// it); this builds it from a copy of the table to show that enabling it is a table edit.
fn layout_331() -> Vec<FtpLayoutEntry> {
    variant(|v| {
        let folder = at(v, "folder");
        if let FtpWire::Value { slot, secret, .. } = v[folder].wire {
            v[folder].wire = FtpWire::Value {
                slot,
                kind: FtpValueKind::Text(FtpEncoding::Utf16U8LenOptional, FtpTextSource::Fixed("Claude")),
                secret,
            };
        }
        let flags = at(v, "option flags");
        if let FtpWire::Value { slot, secret, .. } = v[flags].wire {
            v[flags].wire = FtpWire::Value { slot, kind: FtpValueKind::U16(FtpU16Source::Fixed(0x0001)), secret };
        }
    })
}

#[test]
fn a_table_edit_makes_the_encoder_write_the_331_byte_capture_byte_for_byte() {
    let blob331 = fixture_blob("z9_331_folder");
    let blob317 = fixture_blob("z9_317");
    assert_eq!((blob331.len(), blob317.len()), (331, 317));
    let v = layout_331();
    let net = FtpCameraNetwork::CAPTURED;
    assert_eq!(encode_with(&v, &c2(), &net).unwrap(), blob331, "the variant table writes the capture");
    assert_eq!(encode_with(FTP_LAYOUT, &c2(), &net).unwrap(), blob317, "the real table still writes the 317-byte form");
    assert_ne!(blob331, blob317);
}

#[test]
fn a_table_edit_makes_the_gate_accept_the_331_byte_form_and_the_real_gate_still_refuses_it() {
    let blob331 = fixture_blob("z9_331_folder");
    let blob317 = fixture_blob("z9_317");
    let v = layout_331();
    // The decoder is the same for both: the 331 form decodes under the real table already.
    let (d331, _) = decode_with(FTP_LAYOUT, &blob331).unwrap();
    assert_eq!(decode_with(&v, &blob331).unwrap().0, d331);
    assert_eq!(d331.folder, "Claude");
    // The real gate refuses it (the scope limit of this change): a folder, and flags 0x0001.
    assert_eq!(encodable_with(FTP_LAYOUT, &d331), None);
    assert_eq!(d331.as_encodable(), None);
    // The variant's gate takes it, and the profile it returns re-encodes to the capture.
    let p = encodable_with(&v, &d331).expect("the variant's gate accepts the 331-byte capture");
    assert_eq!(p, c2());
    assert_eq!(encode_with(&v, &p, &FtpCameraNetwork::CAPTURED).unwrap(), blob331);
    // And only that: the 317-byte form (empty folder, flags 5) is no longer what the variant writes.
    let (d317, _) = decode_with(&v, &blob317).unwrap();
    assert_eq!(encodable_with(&v, &d317), None);
    assert!(encodable_with(FTP_LAYOUT, &d317).is_some());
}

#[test]
fn a_table_edit_adds_a_section_to_the_encoder_and_the_decoder_alike() {
    const EXTRA: FtpLayoutEntry = lit("extra section", &[0xAA, 0xBB]);
    let v = variant(|v| {
        let after_host_tag = at(v, "host tag") + 1;
        v.insert(after_host_tag, EXTRA);
    });
    let blob = encode_with(&v, &c2(), &FtpCameraNetwork::CAPTURED).unwrap();
    let plain = encode_with(FTP_LAYOUT, &c2(), &FtpCameraNetwork::CAPTURED).unwrap();
    assert_eq!(blob.len(), plain.len() + 2);
    // The new section sits where the table put it, in the output of the encoder...
    let at_host = plain.windows(2).position(|w| w == [0x01, 0x0d]).expect("host tag then the host count");
    assert_eq!(blob[at_host + 1..at_host + 3], [0xAA, 0xBB]);
    // ...and the decoder of the same table reads it back, and names it in an error.
    assert!(decode_with(&v, &blob).is_ok());
    let (what, offset, detail) = malformed(decode_with(&v, &plain));
    assert_eq!((what, offset), ("extra section", at_host + 1), "{detail}");
    assert_eq!(detail, "expected aa bb, found 0d 31");
    // The real decoder does not accept the new blob.
    assert_eq!(malformed(decode_with(FTP_LAYOUT, &blob)).0, "host");
}

#[test]
fn a_table_edit_makes_a_value_secret_for_the_spans_the_masking_and_the_content_search() {
    let v = variant(|v| {
        let i = at(v, "username");
        if let FtpWire::Value { slot, kind, .. } = v[i].wire {
            v[i].wire = FtpWire::Value { slot, kind, secret: true };
        }
    });
    let blob = reference();
    let (d, spans) = decode_with(&v, &blob).unwrap();
    let keys: Vec<&str> = spans.spans.iter().map(|s| s.field).collect();
    assert_eq!(keys, ["ssid_5ghz", "username", "password"], "in wire order");
    assert_eq!(&blob[spans.of("username").unwrap().clone()], b"fleet");
    assert_eq!(d.secret_texts_with(&v), ["nikon5ghz", "fleet", "12345678"]);
    assert_eq!(d.masked_with(&v).username, "*****");
    assert_eq!(d.masked_with(FTP_LAYOUT).username, "fleet", "the real table does not mask it");
}

#[test]
fn a_table_edit_narrows_a_tag_and_the_decoder_and_its_error_follow() {
    const NARROW: &[FtpTagByte] = &[
        FtpTagByte::Var { slot: FtpSlot::Ssid24Lead, allowed: Some(&[0x03]), written: 0x03 },
        FtpTagByte::Exact(0x00),
        FtpTagByte::Var { slot: FtpSlot::Ssid24Kind, allowed: None, written: 0x01 },
        FtpTagByte::Exact(0x00),
    ];
    let v = variant(|v| {
        let i = at(v, "2.4 GHz SSID tag");
        v[i] = pattern("2.4 GHz SSID tag", NARROW);
    });
    let mut blob = reference();
    assert!(decode_with(&v, &blob).is_ok());
    blob[99] = 0x00;
    assert!(decode_with(FTP_LAYOUT, &blob).is_ok(), "the real table accepts the camera-made lead byte");
    assert_eq!(malformed(decode_with(&v, &blob)), ("2.4 GHz SSID tag", 99, "expected 03 00 xx 00, found 00 00 01 00".to_string()));
}

#[test]
fn a_table_edit_changes_what_the_encoder_writes_where_the_table_fixes_a_value() {
    // The same blob layout with a different constant: the FTP port 22 and a tag byte.
    const PORT22: FtpLayoutEntry = value("FTP port", FtpSlot::FtpPort, FtpValueKind::U16(FtpU16Source::Fixed(22)));
    let v = variant(|v| {
        let i = at(v, "FTP port");
        v[i] = PORT22;
    });
    let blob = encode_with(&v, &c2(), &FtpCameraNetwork::CAPTURED).unwrap();
    let plain = encode_with(FTP_LAYOUT, &c2(), &FtpCameraNetwork::CAPTURED).unwrap();
    let diff: Vec<usize> = (0..blob.len()).filter(|&i| blob[i] != plain[i]).collect();
    assert_eq!(diff.len(), 1, "only the port byte moves");
    assert_eq!((plain[diff[0]], blob[diff[0]]), (21, 22));
    let (d, _) = decode_with(&v, &blob).unwrap();
    assert_eq!(d.ftp_port, 22);
    assert_eq!(encodable_with(&v, &d), Some(c2()));
    assert_eq!(encodable_with(FTP_LAYOUT, &d), None);
}

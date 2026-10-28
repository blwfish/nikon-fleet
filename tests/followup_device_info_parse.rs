//! `ptp_usb::parse_device_info`: the ISO 15740 DeviceInfo dataset (PTP operation 0x1001), parsed
//! strictly. The datasets here are spelled out from the spec's layout by `DatasetBuilder`, not by
//! the code under test, and the expected values are written out literally.
//!
//! Layout (all little-endian): StandardVersion u16, VendorExtensionID u32, VendorExtensionVersion
//! u16, VendorExtensionDesc string, FunctionalMode u16, five arrays (OperationsSupported,
//! EventsSupported, DevicePropertiesSupported, CaptureFormats, PlaybackFormats; each a u32 count and
//! that many u16), then Manufacturer, Model, DeviceVersion, SerialNumber strings. A string is a u8
//! count N of UTF-16 code units INCLUDING the terminating NUL (0 = empty, no further bytes), then
//! N code units.

use nikon_fleet::ptp_usb::{DeviceInfoFault, PtpUsbError, SerialVerdict, StringAnomaly, parse_device_info};

/// A PTP string as the spec lays it out: count of code units including the NUL, then the units.
fn pstr(s: &str) -> Vec<u8> {
    if s.is_empty() {
        return vec![0];
    }
    let units: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    pstr_units(&units)
}

/// A PTP string from raw code units (the caller supplies any NUL), with the count byte in front.
fn pstr_units(units: &[u16]) -> Vec<u8> {
    let mut out = vec![u8::try_from(units.len()).expect("a PTP string holds at most 255 code units")];
    out.extend(units.iter().flat_map(|u| u.to_le_bytes()));
    out
}

fn arr(codes: &[u16]) -> Vec<u8> {
    let mut out = (codes.len() as u32).to_le_bytes().to_vec();
    out.extend(codes.iter().flat_map(|c| c.to_le_bytes()));
    out
}

const FIELDS: [&str; 14] = [
    "StandardVersion",
    "VendorExtensionID",
    "VendorExtensionVersion",
    "VendorExtensionDesc",
    "FunctionalMode",
    "OperationsSupported",
    "EventsSupported",
    "DevicePropertiesSupported",
    "CaptureFormats",
    "PlaybackFormats",
    "Manufacturer",
    "Model",
    "DeviceVersion",
    "SerialNumber",
];

/// A dataset as a list of encoded fields in spec order, so a test can cut at any field boundary.
struct DatasetBuilder {
    fields: Vec<Vec<u8>>,
}

impl DatasetBuilder {
    /// A Nikon-looking body: vendor extension 0x0000000a, "Z 9", serial 0000003023668.
    fn golden() -> Self {
        DatasetBuilder {
            fields: vec![
                100u16.to_le_bytes().to_vec(),
                0x0000_000au32.to_le_bytes().to_vec(),
                100u16.to_le_bytes().to_vec(),
                pstr("Nikon"),
                0u16.to_le_bytes().to_vec(),
                arr(&[0x1001, 0x1002, 0x1003, 0x943b]),
                arr(&[0x4006]),
                arr(&[0x5001, 0xd053, 0xd0b4]),
                arr(&[0x3801]),
                arr(&[0x3000, 0x3801]),
                pstr("Nikon Corporation"),
                pstr("Z 9"),
                pstr("5.31"),
                pstr("0000003023668"),
            ],
        }
    }

    fn with(mut self, field: &str, bytes: Vec<u8>) -> Self {
        let i = FIELDS.iter().position(|f| *f == field).expect("a DeviceInfo field name");
        self.fields[i] = bytes;
        self
    }

    fn bytes(&self) -> Vec<u8> {
        self.fields.concat()
    }

    /// Byte offset where field `i` starts.
    fn start_of(&self, i: usize) -> usize {
        self.fields[..i].iter().map(Vec::len).sum()
    }
}

fn malformed(r: Result<impl std::fmt::Debug, PtpUsbError>) -> (&'static str, usize, DeviceInfoFault) {
    match r {
        Err(PtpUsbError::DeviceInfoMalformed { field, offset, fault }) => (field, offset, fault),
        other => panic!("expected DeviceInfoMalformed, got {other:?}"),
    }
}

#[test]
fn golden_dataset_parses_every_field_to_its_literal_value() {
    let d = parse_device_info(&DatasetBuilder::golden().bytes()).unwrap();
    assert_eq!(d.standard_version, 100);
    assert_eq!(d.vendor_extension_id, 0x0a);
    assert_eq!(d.vendor_extension_version, 100);
    assert_eq!(d.vendor_extension_desc.text, "Nikon");
    assert_eq!(d.functional_mode, 0);
    assert_eq!(d.operations_supported, [0x1001, 0x1002, 0x1003, 0x943b]);
    assert_eq!(d.events_supported, [0x4006]);
    assert_eq!(d.device_properties_supported, [0x5001, 0xd053, 0xd0b4]);
    assert_eq!(d.capture_formats, [0x3801]);
    assert_eq!(d.playback_formats, [0x3000, 0x3801]);
    assert_eq!(d.manufacturer.text, "Nikon Corporation");
    assert_eq!(d.model.text, "Z 9");
    assert_eq!(d.device_version.text, "5.31");
    assert_eq!(d.serial_number.text, "0000003023668");
    assert!(d.trailing.is_empty());
    for s in [&d.vendor_extension_desc, &d.manufacturer, &d.model, &d.device_version, &d.serial_number] {
        assert!(s.anomalies.is_empty(), "{s:?}");
    }
}

#[test]
fn a_hand_written_byte_literal_parses_the_same_way_as_the_builder() {
    // The smallest valid dataset, byte by byte from the spec: every string empty (one 0 byte each),
    // every array empty (a zero u32), then one string "Z 9" as a literal in the Model slot.
    let mut raw: Vec<u8> = vec![
        0x64, 0x00, // StandardVersion 100
        0x0a, 0x00, 0x00, 0x00, // VendorExtensionID 10
        0x65, 0x00, // VendorExtensionVersion 101
        0x00, // VendorExtensionDesc: empty
        0x01, 0x00, // FunctionalMode 1
    ];
    raw.extend_from_slice(&[0, 0, 0, 0].repeat(5)); // five empty arrays
    raw.push(0x00); // Manufacturer: empty
    raw.extend_from_slice(&[0x04, b'Z', 0, b' ', 0, b'9', 0, 0, 0]); // Model: 4 units "Z 9" + NUL
    raw.push(0x00); // DeviceVersion: empty
    raw.push(0x00); // SerialNumber: empty
    assert_eq!(raw.len(), 2 + 4 + 2 + 1 + 2 + 20 + 1 + 9 + 1 + 1);
    let d = parse_device_info(&raw).unwrap();
    assert_eq!((d.standard_version, d.vendor_extension_id, d.vendor_extension_version, d.functional_mode), (100, 10, 101, 1));
    assert_eq!(d.model.text, "Z 9");
    assert_eq!(d.manufacturer.text, "");
    assert_eq!(d.device_version.text, "");
    assert_eq!(d.serial_number.text, "");
    assert!(d.operations_supported.is_empty() && d.playback_formats.is_empty());
    assert!(d.trailing.is_empty(), "the whole literal was consumed");
}

#[test]
fn an_empty_serial_is_a_length_byte_of_zero_and_consumes_exactly_that_byte() {
    // Trailing 0xaa proves the empty string took one byte, not two (nor zero).
    let mut raw = DatasetBuilder::golden().with("SerialNumber", vec![0]).bytes();
    raw.push(0xaa);
    let d = parse_device_info(&raw).unwrap();
    assert_eq!(d.serial_number.text, "");
    assert!(d.serial_number.anomalies.is_empty());
    assert_eq!(d.trailing, [0xaa]);
}

#[test]
fn a_string_of_only_its_terminator_is_empty_and_clean() {
    let d = parse_device_info(&DatasetBuilder::golden().with("SerialNumber", vec![1, 0, 0]).bytes()).unwrap();
    assert_eq!(d.serial_number.text, "");
    assert!(d.serial_number.anomalies.is_empty());
}

#[test]
fn every_truncation_of_the_golden_dataset_is_a_typed_error_naming_the_field_that_was_cut() {
    let b = DatasetBuilder::golden();
    let full = b.bytes();
    let ends: Vec<usize> = (0..FIELDS.len()).map(|i| b.start_of(i) + b.fields[i].len()).collect();
    assert_eq!(*ends.last().unwrap(), full.len());
    for len in 0..full.len() {
        // The first field whose end lies beyond the cut is the one that cannot be read.
        let cut_field = FIELDS[ends.iter().position(|e| *e > len).unwrap()];
        let (field, offset, fault) = malformed(parse_device_info(&full[..len]));
        assert_eq!(field, cut_field, "prefix of {len} bytes");
        assert!(offset <= len, "prefix of {len} bytes reports offset {offset} beyond its end");
        assert!(matches!(fault, DeviceInfoFault::Truncated { .. } | DeviceInfoFault::CountExceedsData { .. }), "{fault:?}");
    }
    // At the boundary: the whole dataset parses, and one byte more is trailing, not an error.
    assert!(parse_device_info(&full).is_ok());
}

#[test]
fn truncation_at_each_field_boundary_fails_the_next_field_and_one_byte_more_does_not_help_the_cut_one() {
    let b = DatasetBuilder::golden();
    let full = b.bytes();
    for i in 1..FIELDS.len() {
        let at = b.start_of(i);
        // Exactly at the boundary (previous field complete): the NEXT field is the one reported.
        assert_eq!(malformed(parse_device_info(&full[..at])).0, FIELDS[i], "cut at the start of {}", FIELDS[i]);
        // One byte below the boundary: the previous field is the one cut.
        assert_eq!(malformed(parse_device_info(&full[..at - 1])).0, FIELDS[i - 1], "cut inside {}", FIELDS[i - 1]);
        // One byte above the boundary: still inside field i (every field here is longer than 1 byte
        // except the empty-string length bytes, which the golden dataset does not use).
        if b.fields[i].len() > 1 {
            assert_eq!(malformed(parse_device_info(&full[..at + 1])).0, FIELDS[i], "cut one byte into {}", FIELDS[i]);
        }
    }
}

#[test]
fn array_counts_at_below_and_above_the_remaining_bytes() {
    let b = DatasetBuilder::golden();
    let full = b.bytes();
    // The five array fields, FIELDS[5..10].
    for (i, field) in FIELDS.iter().enumerate().skip(5).take(5) {
        let count_at = b.start_of(i);
        let remaining = full.len() - (count_at + 4);
        let with_count = |count: u32| {
            let mut raw = full.clone();
            raw[count_at..count_at + 4].copy_from_slice(&count.to_le_bytes());
            raw
        };
        let max_fitting = (remaining / 2) as u32;
        // At the limit the count itself is acceptable: whatever fails later is not a count error.
        let at = parse_device_info(&with_count(max_fitting));
        assert!(!matches!(at, Err(PtpUsbError::DeviceInfoMalformed { fault: DeviceInfoFault::CountExceedsData { .. }, .. })), "{field}: {at:?}");
        // One element over what the remaining bytes can hold is refused as a count error, naming this field.
        let (f, offset, fault) = malformed(parse_device_info(&with_count(max_fitting + 1)));
        assert_eq!(f, *field);
        assert_eq!(offset, count_at + 4);
        assert_eq!(fault, DeviceInfoFault::CountExceedsData { count: max_fitting + 1, needed: u64::from(max_fitting + 1) * 2, available: remaining });
        // The extreme: refused up front (no 8 GiB allocation), not an overflow or a panic.
        let (f, _, fault) = malformed(parse_device_info(&with_count(u32::MAX)));
        assert_eq!(f, *field);
        assert!(matches!(fault, DeviceInfoFault::CountExceedsData { count: u32::MAX, .. }), "{fault:?}");
    }
}

#[test]
fn an_array_count_that_fits_but_eats_the_following_fields_is_still_an_error() {
    // Operations declares as many elements as the rest of the dataset has bytes for: it parses its
    // array, and the strings after it are then missing. Never a silently shortened result.
    let b = DatasetBuilder::golden();
    let mut raw = b.bytes();
    let count_at = b.start_of(5);
    let remaining = raw.len() - (count_at + 4);
    raw[count_at..count_at + 4].copy_from_slice(&((remaining / 2) as u32).to_le_bytes());
    assert_eq!(malformed(parse_device_info(&raw)).0, "EventsSupported");
}

#[test]
fn string_length_at_below_above_the_remaining_bytes() {
    // SerialNumber is last: N units need 2N bytes.
    let n = 5usize;
    let mut serial = vec![n as u8];
    serial.extend(std::iter::repeat_n(b'x', 2 * n));
    let ok = DatasetBuilder::golden().with("SerialNumber", serial.clone()).bytes();
    // At: exactly 2N bytes present (no NUL -> parses, flagged).
    assert!(parse_device_info(&ok).is_ok());
    // Below: one byte short.
    let short = &ok[..ok.len() - 1];
    let (field, _, fault) = malformed(parse_device_info(short));
    assert_eq!(field, "SerialNumber");
    assert_eq!(fault, DeviceInfoFault::Truncated { needed: 2 * n, available: 2 * n - 1 });
    // Above: a byte more is trailing data, not part of the string.
    let mut long = ok.clone();
    long.push(0x77);
    assert_eq!(parse_device_info(&long).unwrap().trailing, [0x77]);
}

#[test]
fn max_length_strings_are_255_units_and_one_byte_short_is_an_error() {
    // Count byte 255: 254 characters and the NUL.
    let text = "a".repeat(254);
    let max = pstr(&text);
    assert_eq!(max.len(), 1 + 255 * 2);
    assert_eq!(max[0], 255);
    let d = parse_device_info(&DatasetBuilder::golden().with("SerialNumber", max.clone()).bytes()).unwrap();
    assert_eq!(d.serial_number.text, text);
    assert!(d.serial_number.anomalies.is_empty());
    // The same in a middle field: the next field must still be found at the right offset.
    let d = parse_device_info(&DatasetBuilder::golden().with("Manufacturer", max.clone()).bytes()).unwrap();
    assert_eq!((d.manufacturer.text.len(), d.model.text.as_str(), d.serial_number.text.as_str()), (254, "Z 9", "0000003023668"));
    // 255 declared, 509 bytes present.
    let (field, _, fault) = malformed(parse_device_info(&DatasetBuilder::golden().with("SerialNumber", max[..max.len() - 1].to_vec()).bytes()));
    assert_eq!(field, "SerialNumber");
    assert_eq!(fault, DeviceInfoFault::Truncated { needed: 510, available: 509 });
}

#[test]
fn trailing_bytes_are_reported_not_ignored() {
    let full = DatasetBuilder::golden().bytes();
    assert!(parse_device_info(&full).unwrap().trailing.is_empty());
    for extra in [vec![0x00], vec![0xde, 0xad, 0xbe, 0xef], vec![0x5a; 1000]] {
        let mut raw = full.clone();
        raw.extend_from_slice(&extra);
        let d = parse_device_info(&raw).unwrap();
        assert_eq!(d.trailing, extra);
        // Everything before the tail is unaffected.
        assert_eq!(d.serial_number.text, "0000003023668");
    }
}

#[test]
fn a_string_without_its_terminator_is_kept_and_flagged() {
    let d = parse_device_info(&DatasetBuilder::golden().with("SerialNumber", pstr_units(&[0x61, 0x62])).bytes()).unwrap();
    assert_eq!(d.serial_number.text, "ab");
    assert_eq!(d.serial_number.anomalies, [StringAnomaly::MissingTerminator]);
}

#[test]
fn a_nul_before_the_last_unit_is_kept_and_flagged() {
    let d = parse_device_info(&DatasetBuilder::golden().with("SerialNumber", pstr_units(&[0x61, 0, 0x62, 0])).bytes()).unwrap();
    assert_eq!(d.serial_number.text, "a\0b");
    assert_eq!(d.serial_number.anomalies, [StringAnomaly::EmbeddedNul]);
    // A string that is only NULs: the last is the terminator, the others are embedded.
    let d = parse_device_info(&DatasetBuilder::golden().with("SerialNumber", pstr_units(&[0, 0])).bytes()).unwrap();
    assert_eq!(d.serial_number.text, "\0");
    assert_eq!(d.serial_number.anomalies, [StringAnomaly::EmbeddedNul]);
}

#[test]
fn unpaired_surrogates_decode_lossily_and_are_flagged_but_pairs_are_not() {
    let parse_serial = |units: &[u16]| parse_device_info(&DatasetBuilder::golden().with("SerialNumber", pstr_units(units)).bytes()).unwrap().serial_number;
    // A lone high surrogate, a lone low surrogate, and a high one followed by an ordinary unit.
    for (units, text) in [
        (vec![0xd800u16, 0], "\u{fffd}"),
        (vec![0xdc00, 0], "\u{fffd}"),
        (vec![0xd800, 0x78, 0], "\u{fffd}x"),
        (vec![0x78, 0xdc00, 0x79, 0], "x\u{fffd}y"),
    ] {
        let s = parse_serial(&units);
        assert_eq!((s.text.as_str(), s.anomalies.as_slice()), (text, &[StringAnomaly::UnpairedSurrogate][..]), "{units:x?}");
    }
    // A valid pair (U+1F4F7) is two units, counted as two by the length byte, and is clean.
    let s = parse_serial(&[0xd83d, 0xdcf7, 0]);
    assert_eq!(s.text, "\u{1f4f7}");
    assert!(s.anomalies.is_empty());
    // Ordinary BMP text.
    assert_eq!(parse_serial(&[0xe9, 0]).text, "\u{e9}");
}

#[test]
fn anomalies_combine_in_a_fixed_order_and_a_lossy_string_does_not_hide_a_later_field() {
    let d = parse_device_info(&DatasetBuilder::golden().with("Manufacturer", pstr_units(&[0xd800, 0, 0x61])).bytes()).unwrap();
    assert_eq!(
        d.manufacturer.anomalies,
        [StringAnomaly::MissingTerminator, StringAnomaly::EmbeddedNul, StringAnomaly::UnpairedSurrogate]
    );
    assert_eq!(d.serial_number.text, "0000003023668", "fields after a lossy string are still read at the right place");
}

#[test]
fn the_error_text_names_the_field_and_the_offset() {
    let b = DatasetBuilder::golden();
    let full = b.bytes();
    let at = b.start_of(13);
    let err = parse_device_info(&full[..at + 3]).unwrap_err().to_string();
    assert!(err.contains("SerialNumber") && err.contains(&format!("offset {}", at + 1)), "{err}");
    assert!(err.contains("remain"), "{err}");
}

#[test]
fn single_byte_corruptions_never_panic() {
    // Deterministic pseudo-random corruption of the golden bytes: whatever comes out, it is an Ok
    // or an Err, never a panic or an out-of-memory allocation.
    let full = DatasetBuilder::golden().bytes();
    let mut x: u32 = 0x1234_5678;
    for _ in 0..20_000 {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let mut raw = full.clone();
        let pos = (x >> 8) as usize % raw.len();
        raw[pos] = (x >> 24) as u8;
        if x & 1 == 1 {
            raw.truncate((x >> 4) as usize % (raw.len() + 1));
        }
        let _ = parse_device_info(&raw);
    }
}

#[test]
fn the_serial_verdict_is_a_closed_set_with_exact_comparison() {
    for (ptp, usb, verdict, tag) in [
        ("0000003023668", "0000003023668", SerialVerdict::Match, "match"),
        ("0000003023668", "0000003023669", SerialVerdict::Differ, "differ"),
        ("", "0000003023668", SerialVerdict::PtpEmpty, "ptp_empty"),
        ("0000003023668", "", SerialVerdict::UsbEmpty, "usb_empty"),
        ("", "", SerialVerdict::BothEmpty, "both_empty"),
        // Exact: no trimming and no case folding, so a near-miss is `differ`, not `match`.
        ("0000003023668", "0000003023668 ", SerialVerdict::Differ, "differ"),
        ("abc", "ABC", SerialVerdict::Differ, "differ"),
        // A whitespace-only serial is not empty.
        (" ", "", SerialVerdict::UsbEmpty, "usb_empty"),
    ] {
        let v = SerialVerdict::compare(ptp, usb);
        assert_eq!((v, v.tag()), (verdict, tag), "PTP {ptp:?} USB {usb:?}");
    }
}

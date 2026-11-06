//! The IPTC presets a body stores (`0x9412` read, `0x9413` write), decoded.
//!
//! Layout, read off a Z 9 on 2026-11-06 (a preset typed in at the camera with a 1..3 character entry per field, then
//! confirmed by writing a profile in this layout to an empty slot and reading back the same bytes):
//!
//! ```text
//! 01 00 00 00            u32, 1 in every slot seen
//! c9 00                  u16 0x00C9 (201) in every slot seen
//! 01 00                  u16 record kind: 1 = an IPTC preset, 2 = a shorter record (slots 11..13 of a Z 9)
//! 00 00 00 00            u32, 0 in every slot seen
//! 15 entries             each [u32 LE length incl. the NUL][UTF-8 bytes][NUL]; an empty entry is a zero length
//!                        and no bytes. The first is the preset's NAME, then the 14 fields in `IPTC_FIELDS` order.
//! ```
//!
//! An empty preset is the 12-byte header and 15 zero lengths: 72 bytes. Slot 0 is a directory (a u32 count, then one
//! record per slot), not a preset.
//!
//! The stored form is the camera's own bytes; this module only reads and builds them. The decoder is strict, so that
//! whatever it accepts as a preset encodes back to the same bytes (`decode` then `encode` is the identity; a test
//! pins it): a header that is not the one every slot seen carries, an entry of length 1 (a bare NUL, which the
//! encoder writes as a zero length) and an entry with a NUL inside are errors, not values to quietly rewrite. Same layout on a Z 9 and a
//! Z6III (a preset on the Z6III, left from the July NX Field test, holds `Captions`, `Id`, `Headline`, ... and decodes
//! with this code unchanged). The July capture notes describe "two leading entries, an empty string and `1`": that is
//! this header's last zero u32 and the preset's name `1`. A write that left out the 8 header bytes before them was
//! refused with `0xA210` by a Z 9.

use serde::Serialize;

/// The 14 fields, in wire order. The one list: the viewer, the diff and the encoder read it.
pub const IPTC_FIELDS: [&str; 14] = [
    "Caption",
    "Event ID",
    "Headline",
    "Object name",
    "City",
    "State",
    "Country",
    "Category",
    "Supplemental categories",
    "Byline",
    "Byline title",
    "Writer/editor",
    "Credit",
    "Source",
];

/// Header bytes before the entries.
pub const IPTC_HEADER_LEN: usize = 12;
/// The record kind of an IPTC preset.
pub const IPTC_KIND_PRESET: u16 = 1;

/// The 12 header bytes of a record of `kind`: the one place the layout in the module doc is spelled out, read by the
/// decoder (to check a preset's header) and the encoders.
fn header_bytes(kind: u16) -> [u8; IPTC_HEADER_LEN] {
    let k = kind.to_le_bytes();
    [0x01, 0, 0, 0, 0xC9, 0, k[0], k[1], 0, 0, 0, 0]
}

/// The header every IPTC preset carries.
pub fn iptc_preset_header() -> [u8; IPTC_HEADER_LEN] {
    header_bytes(IPTC_KIND_PRESET)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IptcPreset {
    /// The preset's name as the camera menu shows it (empty for an unused preset).
    pub name: String,
    /// One value per [`IPTC_FIELDS`] entry, in order.
    pub fields: Vec<String>,
}

impl IptcPreset {
    /// Nothing typed anywhere: an unused preset.
    pub fn is_empty(&self) -> bool {
        self.name.is_empty() && self.fields.iter().all(String::is_empty)
    }
}

/// What a slot's bytes are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IptcRecord {
    Preset(IptcPreset),
    /// A record of a kind this code has no layout for: its kind and the LENGTH of the bytes after the header. The
    /// bytes themselves are not carried here; a snapshot keeps the whole slot as hex (`IptcSlotEntry::hex`) and a
    /// restore writes from that.
    Other { kind: u16, body_len: usize },
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Decode one slot. Strict: a short header, (for a preset) a header other than [`iptc_preset_header`], a length that
/// runs past the end, an entry without its NUL, an entry of length 1 (only the NUL: the encoder writes an empty entry as
/// a zero length, so this would not round-trip), a NUL inside an entry (the encoder refuses it), text that is not UTF-8,
/// a count of entries other than 15, or bytes after the last entry is an error that says where. A record of another
/// kind is reported as [`IptcRecord::Other`] without its header being judged: nothing is known of its layout.
pub fn decode_iptc_slot(bytes: &[u8]) -> Result<IptcRecord, String> {
    if bytes.len() < IPTC_HEADER_LEN {
        return Err(format!("{} byte(s), shorter than the {IPTC_HEADER_LEN}-byte header", bytes.len()));
    }
    let kind = u16::from_le_bytes([bytes[6], bytes[7]]);
    if kind != IPTC_KIND_PRESET {
        return Ok(IptcRecord::Other { kind, body_len: bytes.len() - IPTC_HEADER_LEN });
    }
    let canonical = iptc_preset_header();
    if bytes[..IPTC_HEADER_LEN] != canonical {
        return Err(format!(
            "header {} is not the one every preset slot seen carries ({}); the camera's bytes would not survive a decode and re-encode",
            crate::extended::hex_encode(&bytes[..IPTC_HEADER_LEN]),
            crate::extended::hex_encode(&canonical)
        ));
    }
    let mut pos = IPTC_HEADER_LEN;
    let mut entries: Vec<String> = Vec::with_capacity(15);
    while pos < bytes.len() {
        let len = u32_at(bytes, pos).ok_or_else(|| format!("offset {pos}: a length needs 4 bytes, {} left", bytes.len() - pos))? as usize;
        pos += 4;
        if len == 0 {
            entries.push(String::new());
            continue;
        }
        if len == 1 {
            return Err(format!("offset {}: an entry of length 1 (just a NUL); an empty entry is a zero length", pos - 4));
        }
        let body = bytes
            .get(pos..pos.saturating_add(len))
            .ok_or_else(|| format!("offset {pos}: an entry of {len} byte(s) runs past the end ({} left)", bytes.len() - pos))?;
        let (text, nul) = body.split_at(len - 1);
        if nul != [0] {
            return Err(format!("offset {pos}: an entry of {len} byte(s) does not end in a NUL"));
        }
        let text = std::str::from_utf8(text).map_err(|e| format!("offset {pos}: entry is not UTF-8 ({e})"))?;
        if text.contains('\0') {
            return Err(format!("offset {pos}: an entry has a NUL inside it"));
        }
        entries.push(text.to_string());
        pos += len;
    }
    if entries.len() != 1 + IPTC_FIELDS.len() {
        return Err(format!("{} entries, expected {} (a name and {} fields)", entries.len(), 1 + IPTC_FIELDS.len(), IPTC_FIELDS.len()));
    }
    let name = entries.remove(0);
    Ok(IptcRecord::Preset(IptcPreset { name, fields: entries }))
}

/// The length prefix of a non-empty entry of `text_len` bytes: the text and its NUL, which must fit the u32.
fn entry_length_prefix(text_len: usize) -> Result<u32, String> {
    u32::try_from(text_len)
        .ok()
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| format!("an entry of {text_len} bytes is too long to length-prefix"))
}

/// The entries of a record as the `0x9413' data phase carries them, with no header: each
/// `[u32 LE length incl. the NUL][UTF-8][NUL]`, an empty one just a zero length. The ONE entry encoder: the preset
/// encoder and the raw-entries encoder in `ptp_usb` both build their bytes through it. Refuses an entry with a NUL
/// (it would end the entry early) and one whose length plus NUL does not fit the u32.
pub fn encode_entries<S: AsRef<str>>(entries: impl IntoIterator<Item = S>) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for e in entries {
        let e = e.as_ref();
        if e.contains('\0') {
            return Err(format!("{e:?} contains a NUL"));
        }
        if e.is_empty() {
            out.extend_from_slice(&0u32.to_le_bytes());
        } else {
            let len = entry_length_prefix(e.len())?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(e.as_bytes());
            out.push(0);
        }
    }
    Ok(out)
}

/// A preset-kind record built from raw entries of any count: [`iptc_preset_header`] then [`encode_entries`]. What a
/// slot's data phase has to start with (a Z 9 refused a write without the header, `0xA210`).
pub fn encode_preset_record<S: AsRef<str>>(entries: impl IntoIterator<Item = S>) -> Result<Vec<u8>, String> {
    let mut out = iptc_preset_header().to_vec();
    out.extend(encode_entries(entries)?);
    Ok(out)
}

/// The bytes for a preset: the header every slot seen carries, then the 15 entries. Refuses text with a NUL (it would
/// end the entry early) and a field count other than 14.
pub fn encode_iptc_preset(p: &IptcPreset) -> Result<Vec<u8>, String> {
    if p.fields.len() != IPTC_FIELDS.len() {
        return Err(format!("{} field(s), expected {}", p.fields.len(), IPTC_FIELDS.len()));
    }
    encode_preset_record(std::iter::once(&p.name).chain(&p.fields))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slot 2 of a Z 9 as read on 2026-11-06 after the owner typed a preset named "A" with an entry in every field.
    const SLOT2_HEX: &str = "01000000c90001000000000002000000410002000000430003000000455600020000004800030000004f4e0002000000430002000000530003000000434f0003000000434100030000005343000200000042000300000042540004000000572f45000300000043520003000000534f00";
    /// An unused preset (slots 1..10 of that Z 9): 72 bytes.
    const EMPTY_HEX: &str = "01000000c900010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
    /// A kind-2 record (slots 11..13).
    const KIND2_HEX: &str = "01000000c9000200000000000000000000000000";

    fn bytes(hex: &str) -> Vec<u8> {
        crate::extended::hex_decode(hex).unwrap()
    }

    #[test]
    fn the_real_slot_decodes_to_the_name_and_the_fourteen_fields_in_order() {
        let IptcRecord::Preset(p) = decode_iptc_slot(&bytes(SLOT2_HEX)).unwrap() else { panic!() };
        assert_eq!(p.name, "A");
        let got: Vec<&str> = p.fields.iter().map(String::as_str).collect();
        assert_eq!(got, ["C", "EV", "H", "ON", "C", "S", "CO", "CA", "SC", "B", "BT", "W/E", "CR", "SO"]);
    }

    #[test]
    fn an_unused_preset_is_72_bytes_and_decodes_empty() {
        let b = bytes(EMPTY_HEX);
        assert_eq!(b.len(), 72);
        let IptcRecord::Preset(p) = decode_iptc_slot(&b).unwrap() else { panic!() };
        assert!(p.is_empty());
    }

    #[test]
    fn encoding_reproduces_the_cameras_bytes_exactly() {
        for hex in [SLOT2_HEX, EMPTY_HEX] {
            let IptcRecord::Preset(p) = decode_iptc_slot(&bytes(hex)).unwrap() else { panic!() };
            assert_eq!(encode_iptc_preset(&p).unwrap(), bytes(hex));
        }
    }

    #[test]
    fn a_record_of_another_kind_is_reported_not_forced_into_the_preset_layout() {
        assert_eq!(decode_iptc_slot(&bytes(KIND2_HEX)).unwrap(), IptcRecord::Other { kind: 2, body_len: 8 });
    }

    #[test]
    fn malformed_slots_are_errors_that_say_where() {
        let good = bytes(SLOT2_HEX);
        assert!(decode_iptc_slot(&good[..11]).is_err(), "header cut short");
        assert!(decode_iptc_slot(&good[..good.len() - 1]).unwrap_err().contains("past the end"), "last entry cut by one byte");
        let mut extra = good.clone();
        extra.extend_from_slice(&0u32.to_le_bytes());
        assert!(decode_iptc_slot(&extra).unwrap_err().contains("16 entries"), "one entry too many");
        let mut no_nul = good.clone();
        no_nul[IPTC_HEADER_LEN + 4 + 1] = b'x'; // the NUL of the name "A"
        assert!(decode_iptc_slot(&no_nul).unwrap_err().contains("NUL"));
        let mut not_utf8 = good;
        not_utf8[IPTC_HEADER_LEN + 4] = 0xFF;
        assert!(decode_iptc_slot(&not_utf8).unwrap_err().contains("UTF-8"));
        assert!(decode_iptc_slot(&[]).is_err());
        assert!(decode_iptc_slot(&bytes(&EMPTY_HEX[..EMPTY_HEX.len() - 8])).unwrap_err().contains("14 entries"), "one entry too few");
    }

    #[test]
    fn encoding_refuses_a_nul_and_a_wrong_field_count_and_counts_bytes_not_characters() {
        let mut p = IptcPreset { name: "é".into(), fields: vec![String::new(); 14] };
        assert_eq!(&encode_iptc_preset(&p).unwrap()[IPTC_HEADER_LEN..IPTC_HEADER_LEN + 7], [3, 0, 0, 0, 0xC3, 0xA9, 0]);
        p.fields[3] = "a\0b".into();
        assert!(encode_iptc_preset(&p).is_err());
        p.fields.pop();
        assert!(encode_iptc_preset(&p).unwrap_err().contains("13 field"));
    }

    // ── strictness: whatever decodes as a preset encodes back to the same bytes ──

    #[test]
    fn a_header_other_than_the_canonical_one_is_an_error_for_a_preset_naming_the_header() {
        let good = bytes(SLOT2_HEX);
        // Every header byte except the two of the record kind (6, 7: they select the layout) is pinned.
        for at in (0..6).chain(8..IPTC_HEADER_LEN) {
            let mut b = good.clone();
            b[at] ^= 0x01;
            let err = decode_iptc_slot(&b).unwrap_err();
            assert!(err.contains("header"), "byte {at}: {err}");
        }
    }

    #[test]
    fn a_record_of_another_kind_is_not_judged_by_the_preset_header() {
        // Nothing is known of its layout, so a different u32 at 0 is still just "a record of kind 2".
        let mut b = bytes(KIND2_HEX);
        b[0] = 7;
        b[8] = 9;
        assert_eq!(decode_iptc_slot(&b).unwrap(), IptcRecord::Other { kind: 2, body_len: 8 });
    }

    #[test]
    fn an_entry_that_is_only_a_nul_is_an_error_and_a_zero_length_is_the_empty_entry() {
        // Length 1 and length 0 are the two spellings of "empty"; only the second round-trips.
        let mut b = iptc_preset_header().to_vec();
        for _ in 0..15 {
            b.extend_from_slice(&0u32.to_le_bytes());
        }
        let IptcRecord::Preset(p) = decode_iptc_slot(&b).unwrap() else { panic!() };
        assert!(p.is_empty());
        assert_eq!(encode_iptc_preset(&p).unwrap(), b, "the zero-length form is the canonical one");
        // Entry 0 as [1, 0, 0, 0][NUL] instead of [0, 0, 0, 0]: one byte longer, same value.
        let mut one = iptc_preset_header().to_vec();
        one.extend_from_slice(&[1, 0, 0, 0, 0]);
        for _ in 0..14 {
            one.extend_from_slice(&0u32.to_le_bytes());
        }
        let err = decode_iptc_slot(&one).unwrap_err();
        assert!(err.contains("length 1"), "{err}");
    }

    #[test]
    fn an_entry_with_a_nul_inside_is_an_error_not_a_value_the_encoder_then_refuses() {
        let mut b = iptc_preset_header().to_vec();
        b.extend_from_slice(&[4, 0, 0, 0, b'a', 0, b'b', 0]); // "a\0b"
        for _ in 0..14 {
            b.extend_from_slice(&0u32.to_le_bytes());
        }
        let err = decode_iptc_slot(&b).unwrap_err();
        assert!(err.contains("NUL inside"), "{err}");
    }

    #[test]
    fn decoding_then_encoding_is_the_identity_for_every_one_byte_change_that_still_decodes() {
        let mut decoded = 0;
        let mut rejected = 0;
        for hex in [SLOT2_HEX, EMPTY_HEX] {
            let good = bytes(hex);
            for at in 0..good.len() {
                for v in [0u8, 1, 2, 0x41, 0xC9, 0xFF] {
                    let mut b = good.clone();
                    b[at] = v;
                    match decode_iptc_slot(&b) {
                        Ok(IptcRecord::Preset(p)) => {
                            decoded += 1;
                            assert_eq!(encode_iptc_preset(&p).unwrap(), b, "byte {at} set to {v:#04x} decodes but does not re-encode to itself");
                        }
                        Ok(IptcRecord::Other { .. }) | Err(_) => rejected += 1,
                    }
                }
            }
        }
        assert!(decoded > 50 && rejected > 50, "the sweep must exercise both outcomes ({decoded} decoded, {rejected} rejected)");
    }

    // ── the entry encoder ────────────────────────────────────────────────

    #[test]
    fn the_length_prefix_counts_the_nul_and_stops_at_the_u32() {
        assert_eq!(entry_length_prefix(1), Ok(2));
        assert_eq!(entry_length_prefix(u32::MAX as usize - 1), Ok(u32::MAX));
        assert!(entry_length_prefix(u32::MAX as usize).is_err(), "text plus NUL no longer fits");
        assert!(entry_length_prefix(u32::MAX as usize + 1).is_err());
    }

    #[test]
    fn raw_entries_are_length_prefixed_with_the_nul_counted_and_an_empty_one_is_just_a_zero() {
        assert_eq!(encode_entries(&[] as &[&str]).unwrap(), Vec::<u8>::new());
        assert_eq!(encode_entries([""]).unwrap(), [0, 0, 0, 0]);
        assert_eq!(encode_entries(["1"]).unwrap(), [2, 0, 0, 0, b'1', 0]);
        assert_eq!(encode_entries(["", "ab", ""]).unwrap(), [0, 0, 0, 0, 3, 0, 0, 0, b'a', b'b', 0, 0, 0, 0, 0]);
        assert_eq!(encode_entries(["é"]).unwrap(), [3, 0, 0, 0, 0xC3, 0xA9, 0], "bytes, not characters");
        assert!(encode_entries(["a\0b"]).is_err(), "an interior NUL would end the entry early");
    }

    #[test]
    fn a_preset_record_is_the_header_then_the_raw_entries() {
        let entries = ["A", "", "é", "x"];
        let mut want = iptc_preset_header().to_vec();
        want.extend(encode_entries(entries).unwrap());
        assert_eq!(encode_preset_record(entries).unwrap(), want);
        assert_eq!(&want[..IPTC_HEADER_LEN], bytes("01000000c900010000000000").as_slice(), "the bytes every real slot starts with");
    }
}

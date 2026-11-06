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
//! The stored form is the camera's own bytes; this module only reads and builds them. The Z6III capture in
//! `docs/nx-field-session-2026-07-09.md` describes a different leading pair of entries; that layout was refused with
//! `0xA210` by a Z 9, so the two bodies (or the read and write directions) may differ and a Z6III must be checked.

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
    /// A record of a kind this code has no layout for (kind, and the bytes after the header).
    Other { kind: u16, body_len: usize },
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Decode one slot. Strict: a short header, a length that runs past the end, an entry without its NUL, text that is
/// not UTF-8, a count of entries other than 15, or bytes after the last entry is an error that says where.
pub fn decode_iptc_slot(bytes: &[u8]) -> Result<IptcRecord, String> {
    if bytes.len() < IPTC_HEADER_LEN {
        return Err(format!("{} byte(s), shorter than the {IPTC_HEADER_LEN}-byte header", bytes.len()));
    }
    let kind = u16::from_le_bytes([bytes[6], bytes[7]]);
    if kind != IPTC_KIND_PRESET {
        return Ok(IptcRecord::Other { kind, body_len: bytes.len() - IPTC_HEADER_LEN });
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
        let body = bytes
            .get(pos..pos.saturating_add(len))
            .ok_or_else(|| format!("offset {pos}: an entry of {len} byte(s) runs past the end ({} left)", bytes.len() - pos))?;
        let (text, nul) = body.split_at(len - 1);
        if nul != [0] {
            return Err(format!("offset {pos}: an entry of {len} byte(s) does not end in a NUL"));
        }
        let text = std::str::from_utf8(text).map_err(|e| format!("offset {pos}: entry is not UTF-8 ({e})"))?;
        entries.push(text.to_string());
        pos += len;
    }
    if entries.len() != 1 + IPTC_FIELDS.len() {
        return Err(format!("{} entries, expected {} (a name and {} fields)", entries.len(), 1 + IPTC_FIELDS.len(), IPTC_FIELDS.len()));
    }
    let name = entries.remove(0);
    Ok(IptcRecord::Preset(IptcPreset { name, fields: entries }))
}

/// The bytes for a preset: the header every slot seen carries, then the 15 entries. Refuses text with a NUL (it would
/// end the entry early) and a field count other than 14.
pub fn encode_iptc_preset(p: &IptcPreset) -> Result<Vec<u8>, String> {
    if p.fields.len() != IPTC_FIELDS.len() {
        return Err(format!("{} field(s), expected {}", p.fields.len(), IPTC_FIELDS.len()));
    }
    let mut out = vec![0x01, 0, 0, 0, 0xC9, 0, IPTC_KIND_PRESET as u8, 0, 0, 0, 0, 0];
    for e in std::iter::once(&p.name).chain(&p.fields) {
        if e.contains('\0') {
            return Err(format!("{e:?} contains a NUL"));
        }
        if e.is_empty() {
            out.extend_from_slice(&0u32.to_le_bytes());
        } else {
            out.extend_from_slice(&(e.len() as u32 + 1).to_le_bytes());
            out.extend_from_slice(e.as_bytes());
            out.push(0);
        }
    }
    Ok(out)
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
}

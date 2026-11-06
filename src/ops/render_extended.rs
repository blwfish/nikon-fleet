//! The extended section of a snapshot (`crate::extended`) as the viewers show it.
//!
//! Secrets (the FTP password and the field the layout marks as one) are masked in `value` and, for a
//! viewer that has a reveal control, carried once in [`ProfileField::secret`]: that field is never
//! serialized and never printed by `Debug`, so a `--json` result, a log line or a test failure message
//! cannot leak it. `RenderOptions::reveal_secrets` puts the clear text in `value` instead (the CLI's
//! `--reveal`), and then there is nothing left to hide.

use serde::Serialize;

use crate::extended::{Extended, FtpProfileEntry, FtpState, IptcState, PropState};
use crate::iptc::{IPTC_FIELDS, IptcRecord};
use crate::ptp_usb::DecodedFtpProfile;

/// Text that must not appear in `Debug` output or serialized data.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} byte(s) hidden>", self.0.len())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileField {
    pub label: &'static str,
    /// What is shown: asterisks for a secret unless the clear text was asked for.
    pub value: String,
    /// The clear text of a masked field, for a viewer's reveal control. Not serialized.
    #[serde(skip)]
    pub secret: Option<Secret>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RenderedFtpProfile {
    pub slot: u32,
    /// The slot the body reports as selected.
    pub active: bool,
    pub bytes: usize,
    /// Why the blob did not decode (then `fields` is empty and the file still holds the bytes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undecoded: Option<String>,
    pub fields: Vec<ProfileField>,
}

/// How a stored property reads. Closed: the viewers match on it exhaustively, so a new state has to be placed in each.
/// Serialized as its snake_case tag (`text`, `bytes`, `undecodable`, `not_supported`, `read_failed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RowState {
    Text,
    Bytes,
    Undecodable,
    NotSupported,
    ReadFailed,
}

impl RowState {
    /// A value the body did not give (not a value that happens to be empty, and not a model that lacks the property).
    pub fn is_problem(self) -> bool {
        match self {
            RowState::Undecodable | RowState::ReadFailed => true,
            RowState::Text | RowState::Bytes | RowState::NotSupported => false,
        }
    }
}

/// The state of the FTP profile list as shown. Closed; serialized as `read`, `not_on_model` or `read_failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FtpView {
    Read,
    NotOnModel,
    ReadFailed,
}

/// The state of the IPTC presets as shown. Closed; serialized as `read`, `not_read`, `not_on_model` or `read_failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IptcView {
    Read,
    NotRead,
    NotOnModel,
    ReadFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExtendedRow {
    pub name: String,
    pub code: u32,
    pub display: String,
    pub state: RowState,
}

impl ExtendedRow {
    pub fn code_hex(&self) -> String {
        format!("{:#06x}", self.code)
    }

    /// A value the body did not give (not a value that happens to be empty).
    pub fn is_problem(&self) -> bool {
        self.state.is_problem()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RenderedFtp {
    pub state: FtpView,
    /// Why there is nothing to show, or what to know about what is shown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_slot: Option<u32>,
    pub profiles: Vec<RenderedFtpProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IptcFieldRow {
    pub label: &'static str,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RenderedIptcSlot {
    pub slot: u32,
    /// The preset's name (empty for an unused one or another kind of record).
    pub name: String,
    /// A preset with any text in it.
    pub in_use: bool,
    /// The record kind when it is not a preset (a Z 9's slots 11..13 are kind 2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub other_kind: Option<u16>,
    /// Why the slot did not decode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undecoded: Option<String>,
    pub fields: Vec<IptcFieldRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RenderedIptc {
    pub state: IptcView,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    pub slots: Vec<RenderedIptcSlot>,
}

impl RenderedIptc {
    /// The slots a person wants to see: those with text, and any that could not be decoded.
    pub fn shown(&self) -> impl Iterator<Item = &RenderedIptcSlot> {
        self.slots.iter().filter(|s| s.in_use || s.undecoded.is_some())
    }

    /// How many slots are preset-kind and empty, and how many are another record kind.
    pub fn unused_and_other(&self) -> (usize, usize) {
        let other = self.slots.iter().filter(|s| s.other_kind.is_some()).count();
        let unused = self.slots.iter().filter(|s| !s.in_use && s.other_kind.is_none() && s.undecoded.is_none()).count();
        (unused, other)
    }
}

pub fn slot_matches(s: &RenderedIptcSlot, needle: &str) -> bool {
    needle.is_empty()
        || "iptc".contains(needle)
        || s.name.to_lowercase().contains(needle)
        || s.fields.iter().any(|f| f.label.to_lowercase().contains(needle) || f.value.to_lowercase().contains(needle))
}

fn render_iptc(state: &IptcState) -> RenderedIptc {
    let empty = |state: IptcView, note: Option<String>| RenderedIptc { state, note, count: None, slots: Vec::new() };
    match state {
        IptcState::NotRead => empty(IptcView::NotRead, Some("this snapshot was taken before IPTC was captured".to_string())),
        IptcState::NotOnModel => empty(IptcView::NotOnModel, Some("this model has no IPTC presets".to_string())),
        IptcState::ReadFailed { tag, detail } => empty(IptcView::ReadFailed, Some(format!("not read: {tag} ({detail})"))),
        IptcState::Read(store) => RenderedIptc {
            state: IptcView::Read,
            note: store.truncated.then(|| format!("only the first {} of {} slots were read", store.slots.len(), store.count)),
            count: Some(store.count),
            slots: store
                .slots
                .iter()
                .map(|entry| match entry.decode() {
                    Ok(IptcRecord::Preset(p)) => RenderedIptcSlot {
                        slot: entry.slot,
                        in_use: !p.is_empty(),
                        other_kind: None,
                        undecoded: None,
                        fields: IPTC_FIELDS.iter().zip(&p.fields).map(|(label, value)| IptcFieldRow { label, value: value.clone() }).collect(),
                        name: p.name,
                    },
                    Ok(IptcRecord::Other { kind, .. }) => RenderedIptcSlot {
                        slot: entry.slot,
                        name: String::new(),
                        in_use: false,
                        other_kind: Some(kind),
                        undecoded: None,
                        fields: Vec::new(),
                    },
                    Err(why) => RenderedIptcSlot {
                        slot: entry.slot,
                        name: String::new(),
                        in_use: false,
                        other_kind: None,
                        undecoded: Some(why),
                        fields: Vec::new(),
                    },
                })
                .collect(),
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RenderedExtended {
    /// The body the data was read from, as `model serial`.
    pub read_from: String,
    pub props: Vec<ExtendedRow>,
    pub ftp: RenderedFtp,
    pub iptc: RenderedIptc,
}

impl RenderedExtended {
    /// Whether any secret is masked here, so a viewer knows whether to offer the reveal control.
    pub fn has_secrets(&self) -> bool {
        self.ftp.profiles.iter().flat_map(|p| &p.fields).any(|f| f.secret.is_some())
    }
}

pub(crate) fn prop_row(p: &crate::extended::ExtendedProp) -> ExtendedRow {
    let (display, state) = match &p.state {
        PropState::Text { text } => (format!("{text:?}"), RowState::Text),
        PropState::Bytes { hex } => (format!("0x{hex}"), RowState::Bytes),
        PropState::Undecodable { hex, why } => (format!("(not a PTP string: {why}) 0x{hex}"), RowState::Undecodable),
        PropState::NotSupported => ("(not on this model)".to_string(), RowState::NotSupported),
        PropState::ReadFailed { tag, .. } => (format!("(read failed: {tag})"), RowState::ReadFailed),
    };
    ExtendedRow { name: p.name.clone(), code: p.code, display, state }
}

fn field(label: &'static str, value: impl Into<String>) -> ProfileField {
    ProfileField { label, value: value.into(), secret: None }
}

fn secret_field(label: &'static str, clear: &str, masked: &str, reveal: bool) -> ProfileField {
    if reveal || clear.is_empty() || clear == masked {
        return field(label, clear);
    }
    ProfileField { label, value: masked.to_string(), secret: Some(Secret(clear.to_string())) }
}

/// The label of the field that is probably the Wi-Fi passphrase. The camera stores it where the encoder calls the 5 GHz
/// SSID, and nobody has confirmed on a body that it is the passphrase, so the label says what is known (where it is
/// stored) and that the meaning is unconfirmed. Starts with "Wi-Fi key" for the viewers that look for the secret by it.
pub(crate) const WIFI_KEY_LABEL: &str = "Wi-Fi key (stored in the 5 GHz SSID field; meaning unconfirmed)";

/// The fields a person reads, in the order the camera's menu would give them. The rest (the two
/// timestamps, the structural tag bytes, the IPv6 placeholders) stay in the stored blob and are named
/// in `--json` only as the blob's length: they carry nothing a person sets.
pub(crate) fn profile_fields(p: &DecodedFtpProfile, reveal: bool) -> Vec<ProfileField> {
    let masked = p.with_secrets_masked();
    let net = &p.camera_network;
    vec![
        field("Name", &p.profile_name),
        field("Protocol", if p.is_sftp() { "SFTP" } else { "FTP" }),
        field("Host", &p.host),
        field("Port", p.ftp_port.to_string()),
        field("Folder", &p.folder),
        field("Username", &p.username),
        secret_field("Password", &p.password, &masked.password, reveal),
        field("Wi-Fi network (2.4 GHz)", &p.ssid_24ghz),
        secret_field(WIFI_KEY_LABEL, &p.ssid_5ghz, &masked.ssid_5ghz, reveal),
        field("Camera address", format!("{}.{}.{}.{}/{}", net.ipv4[0], net.ipv4[1], net.ipv4[2], net.ipv4[3], net.prefix_len)),
        field("Option flags", format!("{:#06x}", p.option_flags)),
        field("Second port", p.second_port.to_string()),
    ]
}

fn render_profile(entry: &FtpProfileEntry, active: bool, reveal: bool) -> RenderedFtpProfile {
    let blob = entry.blob();
    let bytes = blob.as_ref().map_or(entry.hex.len() / 2, Vec::len);
    let decoded = entry.decode();
    let (undecoded, fields) = match &decoded {
        Ok(p) => (None, profile_fields(p, reveal)),
        Err(e) => (Some(e.clone()), Vec::new()),
    };
    RenderedFtpProfile { slot: entry.slot, active, bytes, undecoded, fields }
}

pub fn render_extended(e: &Extended, reveal_secrets: bool) -> RenderedExtended {
    let props = e.props.values().map(prop_row).collect();
    let ftp = match &e.ftp {
        FtpState::NotOnModel => RenderedFtp {
            state: FtpView::NotOnModel,
            note: Some("this model has no FTP profiles".to_string()),
            count: None,
            limit: None,
            active_slot: None,
            profiles: Vec::new(),
        },
        FtpState::ReadFailed { tag, detail } => RenderedFtp {
            state: FtpView::ReadFailed,
            note: Some(format!("not read: {tag} ({detail})")),
            count: None,
            limit: None,
            active_slot: None,
            profiles: Vec::new(),
        },
        FtpState::Read(store) => RenderedFtp {
            state: FtpView::Read,
            note: store
                .truncated
                .then(|| format!("only the first {} of {} profiles were read", store.profiles.len(), store.count)),
            count: Some(store.count),
            limit: Some(store.limit),
            active_slot: store.active_slot,
            profiles: store
                .profiles
                .iter()
                .map(|p| render_profile(p, store.active_slot == Some(p.slot), reveal_secrets))
                .collect(),
        },
    };
    RenderedExtended { read_from: format!("{} {}", e.read_from.model, e.read_from.usb_serial).trim().to_string(), props, ftp, iptc: render_iptc(&e.iptc) }
}

/// A line filter for the extended part: the same rule as the property rows (a case-insensitive
/// substring), over names, displayed values and codes. A profile matches on any shown field except a
/// masked secret, or on the word "ftp"/"profile".
pub fn row_matches(row: &ExtendedRow, needle: &str) -> bool {
    needle.is_empty()
        || row.name.to_lowercase().contains(needle)
        || row.display.to_lowercase().contains(needle)
        || row.code_hex().contains(needle)
        || row.code.to_string().contains(needle)
}

pub fn profile_matches(p: &RenderedFtpProfile, needle: &str) -> bool {
    needle.is_empty()
        || "ftp profile".contains(needle)
        || p.fields.iter().any(|f| f.label.to_lowercase().contains(needle) || f.value.to_lowercase().contains(needle))
}

impl RenderedExtended {
    /// The text the CLI prints after the property rows. `needle` is the lowercased, trimmed filter.
    pub fn to_text(&self, needle: &str) -> String {
        let mut out = format!("\nExtended (read from {}):\n", self.read_from);
        let rows: Vec<&ExtendedRow> = self.props.iter().filter(|r| row_matches(r, needle)).collect();
        let width = rows.iter().map(|r| r.name.chars().count()).max().unwrap_or(0);
        for r in &rows {
            out.push_str(&format!("  {:<width$} [{}] = {}\n", r.name, r.code_hex(), r.display));
        }
        let profiles: Vec<&RenderedFtpProfile> = self.ftp.profiles.iter().filter(|p| profile_matches(p, needle)).collect();
        let ftp_shown = needle.is_empty() || !profiles.is_empty() || "ftp".contains(needle);
        if ftp_shown {
            match self.ftp.state {
                FtpView::Read => {
                    out.push_str(&format!(
                        "  FTP profiles: {} stored (limit {}), active slot {}\n",
                        self.ftp.count.unwrap_or(0),
                        self.ftp.limit.unwrap_or(0),
                        self.ftp.active_slot.map_or("none".to_string(), |s| s.to_string()),
                    ));
                }
                FtpView::NotOnModel | FtpView::ReadFailed => out.push_str(&format!("  FTP profiles: {}\n", self.ftp.note.as_deref().unwrap_or("-"))),
            }
            if let Some(note) = self.ftp.note.as_deref().filter(|_| self.ftp.state == FtpView::Read) {
                out.push_str(&format!("  note: {note}\n"));
            }
            for p in profiles {
                out.push_str(&format!("  [{}]{} {} byte(s)\n", p.slot, if p.active { " (active)" } else { "" }, p.bytes));
                if let Some(why) = &p.undecoded {
                    out.push_str(&format!("      could not be decoded: {why}\n"));
                }
                for f in &p.fields {
                    out.push_str(&format!("      {:<24} {}\n", f.label, f.value));
                }
            }
        }
        let slots: Vec<&RenderedIptcSlot> = self.iptc.shown().filter(|s| slot_matches(s, needle)).collect();
        let iptc_shown = needle.is_empty() || !slots.is_empty();
        if iptc_shown {
            match self.iptc.state {
                IptcView::Read => {
                    let (unused, other) = self.iptc.unused_and_other();
                    let in_use = self.iptc.slots.iter().filter(|s| s.in_use).count();
                    out.push_str(&format!(
                        "  IPTC presets: {} of {} in use ({unused} empty{})\n",
                        in_use,
                        self.iptc.count.unwrap_or(0),
                        if other > 0 { format!(", {other} of another record kind") } else { String::new() },
                    ));
                    if let Some(note) = &self.iptc.note {
                        out.push_str(&format!("  note: {note}\n"));
                    }
                }
                IptcView::NotRead | IptcView::NotOnModel | IptcView::ReadFailed => {
                    out.push_str(&format!("  IPTC presets: {}\n", self.iptc.note.as_deref().unwrap_or("-")))
                }
            }
            for s in slots {
                out.push_str(&format!("  [{}] {:?}\n", s.slot, s.name));
                if let Some(why) = &s.undecoded {
                    out.push_str(&format!("      could not be decoded: {why}\n"));
                }
                for f in s.fields.iter().filter(|f| !f.value.is_empty()) {
                    out.push_str(&format!("      {:<24} {}\n", f.label, f.value));
                }
            }
        }
        if rows.is_empty() && !ftp_shown && !iptc_shown {
            out.push_str("  (nothing matches the filter)\n");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extended::{ExtendedProp, FtpStore, ReadFrom, hex_encode};
    use crate::ptp_usb::{FtpProfile, encode_ftp_profile};
    use std::collections::BTreeMap;

    fn blob(password: &str) -> Vec<u8> {
        encode_ftp_profile(&FtpProfile {
            profile_name: "rig".into(),
            ssid_24ghz: "net24".into(),
            ssid_5ghz: "wifi-key-xyz".into(),
            host: "10.0.0.2".into(),
            username: "fleet".into(),
            password: password.into(),
            port: 8080,
        })
        .unwrap()
    }

    fn ext(profiles: Vec<Vec<u8>>) -> Extended {
        let mut props = BTreeMap::new();
        props.insert(
            "0xd072".to_string(),
            ExtendedProp { code: 0xd072, name: "Artist".into(), state: PropState::Text { text: "Brian".into() } },
        );
        props.insert(
            "0xd053".to_string(),
            ExtendedProp { code: 0xd053, name: "EnableCopyright".into(), state: PropState::Bytes { hex: "01".into() } },
        );
        props.insert("0xd060".to_string(), ExtendedProp { code: 0xd060, name: "IPTCPresetSelect".into(), state: PropState::NotSupported });
        let n = profiles.len() as u32;
        Extended {
            read_from: ReadFrom { model: "Z 9".into(), usb_serial: "123".into() },
            props,
            iptc: Default::default(),
            ftp: FtpState::Read(FtpStore {
                count: n,
                limit: 20,
                active_slot: Some(0),
                device_name: String::new(),
                truncated: false,
                profiles: profiles
                    .iter()
                    .enumerate()
                    .map(|(i, b)| FtpProfileEntry { slot: i as u32, hex: hex_encode(b) })
                    .collect(),
            }),
        }
    }

    #[test]
    fn secrets_are_masked_by_default_and_the_clear_text_is_kept_only_in_the_hidden_field() {
        let r = render_extended(&ext(vec![blob("hunter2-pass")]), false);
        let p = &r.ftp.profiles[0];
        let pw = p.fields.iter().find(|f| f.label == "Password").unwrap();
        assert_eq!(pw.value, "************", "same length as the secret, nothing else");
        assert_eq!(pw.secret.as_ref().unwrap().reveal(), "hunter2-pass");
        assert!(r.has_secrets());
        let key = p.fields.iter().find(|f| f.label.starts_with("Wi-Fi key")).unwrap();
        assert_eq!(key.value, "************");
        // The label does not state the unconfirmed mapping as fact.
        assert!(key.label.contains("unconfirmed") && key.label.contains("5 GHz SSID"), "{}", key.label);
        // Not secrets: shown as they are.
        assert_eq!(p.fields.iter().find(|f| f.label == "Username").unwrap().value, "fleet");
        assert_eq!(p.fields.iter().find(|f| f.label == "Host").unwrap().value, "10.0.0.2");
    }

    #[test]
    fn json_and_debug_never_carry_the_secret_unless_it_was_asked_for() {
        let r = render_extended(&ext(vec![blob("hunter2-pass")]), false);
        let json = serde_json::to_string(&r).unwrap();
        let debug = format!("{r:?}");
        let text = r.to_text("");
        for (what, s) in [("json", &json), ("debug", &debug), ("text", &text)] {
            assert!(!s.contains("hunter2-pass"), "{what} leaked the password: {s}");
            assert!(!s.contains("wifi-key-xyz"), "{what} leaked the Wi-Fi key");
        }
        let revealed = render_extended(&ext(vec![blob("hunter2-pass")]), true);
        assert!(serde_json::to_string(&revealed).unwrap().contains("hunter2-pass"));
        assert!(revealed.to_text("").contains("hunter2-pass"));
        assert!(!revealed.has_secrets(), "nothing is masked once revealed");
    }

    #[test]
    fn an_empty_password_is_shown_empty_not_as_a_mask() {
        // 8 chars is the encoder's minimum, so build the empty case from a decoded profile directly.
        let mut p = crate::ptp_usb::decode_ftp_profile(&blob("hunter2-pass")).unwrap();
        p.password = String::new();
        let fields = profile_fields(&p, false);
        let pw = fields.iter().find(|f| f.label == "Password").unwrap();
        assert_eq!(pw.value, "");
        assert!(pw.secret.is_none());
    }

    #[test]
    fn a_blob_that_does_not_decode_is_reported_with_its_size_not_dropped() {
        let e = ext(vec![vec![1, 2, 3]]);
        let r = render_extended(&e, false);
        let p = &r.ftp.profiles[0];
        assert_eq!(p.bytes, 3);
        assert!(p.undecoded.is_some() && p.fields.is_empty());
        assert!(r.to_text("").contains("could not be decoded"));
    }

    #[test]
    fn rows_say_what_each_state_means() {
        let r = render_extended(&ext(vec![]), false);
        let by = |n: &str| r.props.iter().find(|p| p.name == n).unwrap();
        assert_eq!(by("Artist").display, "\"Brian\"");
        assert_eq!(by("EnableCopyright").display, "0x01");
        assert_eq!(by("IPTCPresetSelect").display, "(not on this model)");
        assert!(!by("IPTCPresetSelect").is_problem(), "a model without it is not a failure");
        assert_eq!(r.ftp.count, Some(0));
        assert!(r.to_text("").contains("FTP profiles: 0 stored"));
    }

    #[test]
    fn the_filter_keeps_matching_rows_and_profiles_and_says_when_nothing_matches() {
        let r = render_extended(&ext(vec![blob("hunter2-pass")]), false);
        let t = r.to_text("artist");
        assert!(t.contains("Artist") && !t.contains("EnableCopyright"));
        assert!(!t.contains("Host"), "profiles do not match 'artist'");
        assert!(r.to_text("10.0.0.2").contains("Host"));
        assert!(r.to_text("zzz").contains("nothing matches"));
        assert!(!r.to_text("hunter").contains("Password"), "a secret is not searchable through its mask");
    }

    #[test]
    fn iptc_presets_with_text_are_shown_with_their_fields_and_empty_ones_are_counted() {
        use crate::extended::{IptcSlotEntry, IptcStore};
        const SLOT: &str = "01000000c90001000000000002000000410002000000430003000000455600020000004800030000004f4e0002000000430002000000530003000000434f0003000000434100030000005343000200000042000300000042540004000000572f45000300000043520003000000534f00";
        const EMPTY: &str = "01000000c900010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
        const KIND2: &str = "01000000c9000200000000000000000000000000";
        let mut e = ext(vec![]);
        e.iptc = IptcState::Read(IptcStore {
            count: 4,
            truncated: false,
            slots: vec![
                IptcSlotEntry { slot: 1, hex: EMPTY.into() },
                IptcSlotEntry { slot: 2, hex: SLOT.into() },
                IptcSlotEntry { slot: 3, hex: KIND2.into() },
                IptcSlotEntry { slot: 4, hex: "0102".into() },
            ],
        });
        let r = render_extended(&e, false);
        let in_use: Vec<_> = r.iptc.shown().map(|s| s.slot).collect();
        assert_eq!(in_use, [2, 4], "the preset with text, and the one that did not decode");
        let s2 = &r.iptc.slots[1];
        assert_eq!(s2.name, "A");
        assert_eq!(s2.fields[0], IptcFieldRow { label: "Caption", value: "C".into() });
        assert_eq!(s2.fields[11], IptcFieldRow { label: "Writer/editor", value: "W/E".into() });
        assert_eq!(r.iptc.unused_and_other(), (1, 1));
        let t = r.to_text("");
        assert!(t.contains("IPTC presets: 1 of 4 in use (1 empty, 1 of another record kind)"), "{t}");
        assert!(t.contains("[2] \"A\"") && t.contains("Supplemental categories") && t.contains("could not be decoded"), "{t}");
        assert!(r.to_text("writer").contains("W/E") && !r.to_text("writer").contains("Host"));
        assert!(!r.to_text("zzz").contains("W/E") && r.to_text("zzz").contains("nothing matches"));
    }

    #[test]
    fn iptc_states_without_slots_say_why() {
        for (state, view, tag) in [(IptcState::NotRead, IptcView::NotRead, "not_read"), (IptcState::NotOnModel, IptcView::NotOnModel, "not_on_model")] {
            let mut e = ext(vec![]);
            e.iptc = state;
            let r = render_extended(&e, false);
            assert_eq!(r.iptc.state, view);
            assert_eq!(serde_json::to_value(r.iptc.state).unwrap(), serde_json::json!(tag), "the --json shape is the same words");
            assert!(r.to_text("").contains("IPTC presets: "), "{tag}");
        }
    }

    #[test]
    fn the_state_vocabularies_serialize_as_their_documented_tags_and_only_two_row_states_are_problems() {
        let tag = |v: serde_json::Value| v.as_str().unwrap().to_string();
        let rows = [
            (RowState::Text, "text", false),
            (RowState::Bytes, "bytes", false),
            (RowState::Undecodable, "undecodable", true),
            (RowState::NotSupported, "not_supported", false),
            (RowState::ReadFailed, "read_failed", true),
        ];
        for (state, expected, problem) in rows {
            assert_eq!(tag(serde_json::to_value(state).unwrap()), expected);
            assert_eq!(state.is_problem(), problem, "{expected}");
        }
        for (state, expected) in [(FtpView::Read, "read"), (FtpView::NotOnModel, "not_on_model"), (FtpView::ReadFailed, "read_failed")] {
            assert_eq!(tag(serde_json::to_value(state).unwrap()), expected);
        }
        for (state, expected) in [(IptcView::Read, "read"), (IptcView::NotRead, "not_read"), (IptcView::NotOnModel, "not_on_model"), (IptcView::ReadFailed, "read_failed")] {
            assert_eq!(tag(serde_json::to_value(state).unwrap()), expected);
        }
        // Every stored property state maps to a row state, and the failed ones are the problems.
        let row = |state: PropState| prop_row(&crate::extended::ExtendedProp { code: 0xd072, name: "Artist".into(), state }).state;
        assert_eq!(row(PropState::Text { text: String::new() }), RowState::Text, "an empty string is a value, not a problem");
        assert_eq!(row(PropState::Bytes { hex: "01".into() }), RowState::Bytes);
        assert_eq!(row(PropState::Undecodable { hex: "ff".into(), why: "w".into() }), RowState::Undecodable);
        assert_eq!(row(PropState::NotSupported), RowState::NotSupported);
        assert_eq!(row(PropState::ReadFailed { tag: "t".into(), detail: "d".into() }), RowState::ReadFailed);
    }

    #[test]
    fn the_models_without_ftp_say_so() {
        let mut e = ext(vec![]);
        e.ftp = FtpState::NotOnModel;
        let r = render_extended(&e, false);
        assert_eq!(r.ftp.state, FtpView::NotOnModel);
        assert_eq!(serde_json::to_value(r.ftp.state).unwrap(), serde_json::json!("not_on_model"));
        assert!(r.to_text("").contains("no FTP profiles"));
    }
}

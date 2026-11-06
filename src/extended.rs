//! The part of a body's settings that the MAID SDK does not expose, kept in the snapshot beside the
//! MAID properties: the owner strings (Artist, Copyright, Image comment), the two switches that go
//! with them, and every stored FTP profile.
//!
//! Each model offers a different set, as it does for the MAID properties, so what is read follows
//! what the body has. DeviceInfo cannot say: a Z 9 lists 21 properties and none of the five `0xD0xx`
//! ones, and its operation list has none of `0x90E8..0x90EF`, yet it reads all five properties and
//! serves FTP profiles (measured 2026-11-06). So the properties are decided by asking (a body that lacks
//! one answers `DevicePropNotSupported`, recorded as [`PropState::NotSupported`]), and the FTP profiles
//! by the capability table's list of models known not to have them ([`crate::ptp_usb::MODEL_CAPS`], read through
//! [`crate::ptp_usb::ftp_absent`]: asking a Z 5 costs a 5 s
//! timeout and teaches nothing); every other model is asked, and a failure is recorded with its cause.
//!
//! ## What the source can produce, and what happens to each item
//!
//! | item | disposition |
//! |---|---|
//! | `0xD072` Artist, `0xD073` Copyright, `0xD090` Image comment | `extracted`: text (a value that does not decode as a PTP string keeps its bytes) |
//! | `0xD053` EnableCopyright, `0xD060` IPTCPresetSelect | `extracted`: the property's bytes, hex |
//! | each stored FTP profile (`0x90ED(n)`, `n < count`) | `raw-only`: the whole blob, hex. The decoded fields are derived when shown, by `decode_ftp_profile`, so the password lives in one place only |
//! | the FTP list header (`0x90EB`: count, limit, active slot, device name) | `extracted` |
//! | the FTP list beyond [`crate::ptp_usb::FTP_LIST_MAX_READ`] profiles | `dropped-with-reason`: the camera reports a limit of 20; a larger count is a number this code does not trust, and [`FtpStore::truncated`] says so |
//! | the FTP list header's `guid` (16 bytes), `header` and `trailer` words (`0x90EB`) | `dropped-with-reason`: `ptp_usb::FtpProfileInfo` documents the layout as inferred, not documented; `header` (200) and `trailer` (500) were the same in every reply seen, and the guid's meaning is unknown. Nothing restores or compares them; the profile blobs are what a restore writes |
//! | the FTP device name's string anomalies (`PtpString::anomalies`) | `dropped-with-reason`: [`FtpStore::device_name`] is informational (shown, never written or compared), so a name that decoded with anomalies is stored as its text only |
//! | each IPTC slot (`0x9412(n)`, `n` from 1 to the directory's count) | `raw-only`: the whole record, hex. Decoded when shown (`crate::iptc`); a slot of another record kind is kept as such |
//! | the IPTC directory (`0x9412(0)`) beyond its first `u32`, the slot count | `dropped-with-reason`: the bytes after the count are of unknown meaning (the wire form was inferred from an NX capture and has not been run against a body), and nothing restores or diffs them |
//! | IPTC slots past [`IPTC_READ_MAX`] | `dropped-with-reason`: the count is the camera's and is not trusted past the cap; [`IptcStore::truncated`] says so |
//! | a vendor read's `warnings` and `response_params` (every `ExtendedSource` read) | `dropped-with-reason`: the source trait returns bytes only. The warning is "this model is not surveyed", and the fleet's bodies are surveyed, so it is empty for them; a body that is not would be recorded as clean |
//! | the session's identity beyond model and USB serial (`usb_warnings`, the DeviceInfo dataset) | `dropped-with-reason`: [`ReadFrom`] is used only to check the data came from the body the snapshot is of; the rest is diagnostic and the MAID capture records its own |
//!
//! Passwords are stored (the point is to be able to restore them) and shown masked unless the user
//! asks; see `DecodedFtpProfile::with_secrets_masked`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::iptc::{IptcRecord, decode_iptc_slot};
use crate::ptp_usb::{
    FtpListState, FtpWriteOutcome, IptcWriteOutcome, PtpResponseCode, PtpUsbError, PtpUsbSession, decode_ftp_profile,
    decode_ptp_string_value, ftp_absent, iptc_absent,
};

/// PTP response `DevicePropNotSupported`: the one definition (the property sweep uses it too). A test checks it
/// against the name `ptp_usb` gives the code.
pub(crate) const RESP_DEVICE_PROP_NOT_SUPPORTED: u16 = 0x200A;

/// How a raw-PTP property is turned into a stored value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropKind {
    /// A PTP string (`[u8 count][UTF-16LE]`).
    Text,
    /// Opaque bytes, stored as hex.
    Bytes,
}

/// Every raw-PTP property this module reads, with the name it is shown under. The one list: the capture
/// walks it, the viewer and the diff look names up in it.
pub const EXTENDED_PROPS: [(u16, &str, PropKind); 5] = [
    (0xD072, "Artist", PropKind::Text),
    (0xD073, "Copyright", PropKind::Text),
    (0xD090, "Image comment", PropKind::Text),
    (0xD053, "EnableCopyright", PropKind::Bytes),
    (0xD060, "IPTCPresetSelect", PropKind::Bytes),
];

// Which models are known to lack FTP profiles or IPTC presets (the Z 5: owner, 2026-11-06; its `vendor-read-ftp` times out on
// the `0x90E8` setup, and its DeviceInfo lists neither `0x9412` nor `0x9413`) is the `ftp` / `iptc` column of the one
// capability table, `ptp_usb::MODEL_CAPS`, read through `ptp_usb::ftp_absent` / `iptc_absent`. A model the table does not
// know is NOT absent: it is asked, so an unrecognized body loses nothing. Unlike the FTP skip, DeviceInfo does list the
// IPTC operations on the bodies that have them (Z 9), so the IPTC skip is for a known case, not the test.

/// At most this many IPTC slots are read; the directory's count above it is not trusted ([`IptcStore::truncated`]).
pub const IPTC_READ_MAX: u32 = 32;

/// Key of a property in [`Extended::props`]: `0xd072`.
pub fn prop_key(code: u16) -> String {
    format!("{code:#06x}")
}

/// Why a read produced no value, as a closed vocabulary (the wire form of [`PropState`]'s `state`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PropState {
    Text { text: String },
    Bytes { hex: String },
    /// The bytes came back but are not a valid PTP string; kept so nothing is lost.
    Undecodable { hex: String, why: String },
    /// The body answered `DevicePropNotSupported` (0x200A): it does not have this property. (Not decided from
    /// DeviceInfo: a Z 9 lists 21 properties and none of these five, yet reads all of them.)
    NotSupported,
    /// The body lists it and the read failed. `tag` is `PtpUsbError::tag()`.
    ReadFailed { tag: String, detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtendedProp {
    pub code: u32,
    pub name: String,
    #[serde(flatten)]
    pub state: PropState,
}

/// One stored FTP profile: the blob exactly as `0x90ED` returned it.
///
/// `Debug` does not print the blob: it holds the password and the Wi-Fi key, and every type that holds one (the
/// store, the section, the snapshot) prints through this. The file (`Serialize`) is where the blob is kept, on purpose.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FtpProfileEntry {
    pub slot: u32,
    /// Lowercase hex, no separators. Holds the password.
    pub hex: String,
}

impl std::fmt::Debug for FtpProfileEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FtpProfileEntry").field("slot", &self.slot).field("hex", &format_args!("<{} byte(s) hidden>", self.hex.len() / 2)).finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FtpStore {
    /// `0x90EB` count: how many profiles the body says it holds.
    pub count: u32,
    /// `0x90EB` limit (20 in every reply seen).
    pub limit: u32,
    /// The slot `0x90EB` reports as selected; `None` when the body reports none (`0xff`).
    pub active_slot: Option<u32>,
    /// The body's `<model>_<serial>` string from `0x90EB`.
    pub device_name: String,
    /// `count` is above `FTP_LIST_MAX_READ`, so `profiles` is a prefix of the list.
    pub truncated: bool,
    pub profiles: Vec<FtpProfileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FtpState {
    /// The capability table says the model has no FTP profiles ([`crate::ptp_usb::ftp_absent`]): not asked.
    NotOnModel,
    ReadFailed { tag: String, detail: String },
    Read(FtpStore),
}

/// One IPTC slot as the camera returned it (`0x9412(slot)`): the whole record, hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IptcSlotEntry {
    pub slot: u32,
    pub hex: String,
}

impl IptcSlotEntry {
    pub fn decode(&self) -> Result<IptcRecord, String> {
        decode_iptc_slot(&hex_decode(&self.hex)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IptcStore {
    /// The directory's count (`0x9412(0)`'s first u32): how many slots the body says it has, numbered from 1.
    pub count: u32,
    /// `count` is above [`IPTC_READ_MAX`], so `slots` is a prefix.
    pub truncated: bool,
    pub slots: Vec<IptcSlotEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum IptcState {
    /// The snapshot was taken before IPTC was captured: says nothing about the body.
    #[default]
    NotRead,
    /// The capability table says the model has no IPTC presets ([`crate::ptp_usb::iptc_absent`]): not asked.
    NotOnModel,
    ReadFailed { tag: String, detail: String },
    Read(IptcStore),
}

/// Which body the extended data was read from, checked against the snapshot's camera before it is attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFrom {
    pub model: String,
    /// The USB `iSerialNumber` of the body the raw session opened (empty when it could not be read).
    pub usb_serial: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extended {
    pub read_from: ReadFrom,
    /// Keyed by [`prop_key`]; every entry of [`EXTENDED_PROPS`] is present, each in one of the [`PropState`]s.
    pub props: BTreeMap<String, ExtendedProp>,
    pub ftp: FtpState,
    /// Absent in a section written before IPTC was captured, which reads as [`IptcState::NotRead`].
    #[serde(default)]
    pub iptc: IptcState,
}

impl Extended {
    /// The prop for `code`, if recorded.
    pub fn prop(&self, code: u16) -> Option<&ExtendedProp> {
        self.props.get(&prop_key(code))
    }

    /// Nothing could be read at all (the raw session did not open, or DeviceInfo failed): every item is a
    /// recorded failure with the same cause, so the snapshot says what is missing and why.
    pub fn all_failed(read_from: ReadFrom, tag: &str, detail: &str) -> Self {
        let props = EXTENDED_PROPS
            .iter()
            .map(|&(code, name, _)| {
                let state = PropState::ReadFailed { tag: tag.to_string(), detail: detail.to_string() };
                (prop_key(code), ExtendedProp { code: u32::from(code), name: name.to_string(), state })
            })
            .collect();
        Extended {
            read_from,
            props,
            ftp: FtpState::ReadFailed { tag: tag.to_string(), detail: detail.to_string() },
            iptc: IptcState::ReadFailed { tag: tag.to_string(), detail: detail.to_string() },
        }
    }

    /// Every entry of [`Self::props`] sits under the key its own code gives ([`prop_key`]). Edit and restore look a
    /// property up by key; diff and display go by the code inside, so a hand-edited file where they disagree would be
    /// read two ways. Checked when a snapshot is loaded.
    pub fn check_keys(&self) -> Result<(), String> {
        for (key, p) in &self.props {
            let expected = u16::try_from(p.code).map(prop_key);
            if expected.as_deref() != Ok(key.as_str()) {
                return Err(format!("extended.props entry {key:?} holds property code {:#x}, which belongs under {}", p.code, expected.unwrap_or_else(|_| "no key (not a 16-bit code)".to_string())));
            }
        }
        Ok(())
    }

    /// Whether this was read from the body the snapshot says it is of. Only a mismatch that can be seen
    /// refuses: a side with no USB serial is not evidence of a different body.
    pub fn check_body(&self, model: &str, serial: &str) -> Result<(), String> {
        let model_differs = !self.read_from.model.is_empty() && self.read_from.model != model;
        let serial_differs =
            !self.read_from.usb_serial.is_empty() && !serial.is_empty() && self.read_from.usb_serial != serial;
        if model_differs || serial_differs {
            return Err(format!(
                "the extended data was read from {} (serial {:?}) but the snapshot is of {} (serial {:?})",
                self.read_from.model, self.read_from.usb_serial, model, serial
            ));
        }
        Ok(())
    }
}

pub fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn hex_decode(text: &str) -> Result<Vec<u8>, String> {
    if text.len() % 2 != 0 {
        return Err(format!("odd number of hex digits ({})", text.len()));
    }
    (0..text.len())
        .step_by(2)
        .map(|i| {
            text.get(i..i + 2)
                .filter(|pair| pair.bytes().all(|b| b.is_ascii_hexdigit()))
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| format!("{:?} at offset {i} is not a hex byte", text.get(i..i + 2).unwrap_or("?")))
        })
        .collect()
}

impl FtpProfileEntry {
    /// The blob's bytes.
    pub fn blob(&self) -> Result<Vec<u8>, String> {
        hex_decode(&self.hex)
    }

    /// The profile decoded for display or comparison. The strict decoder refuses a blob it does not
    /// fully understand; the error says where.
    pub fn decode(&self) -> Result<crate::ptp_usb::DecodedFtpProfile, String> {
        let blob = self.blob()?;
        decode_ftp_profile(&blob).map_err(|e| e.to_string())
    }
}

/// What the capture needs from a raw-PTP session. A trait so the reading logic runs against a scripted
/// source in tests; the real implementation is for [`PtpUsbSession`].
pub trait ExtendedSource {
    fn identity(&mut self) -> Result<ReadFrom, PtpUsbError>;
    fn prop_bytes(&mut self, code: u16) -> Result<Vec<u8>, PtpUsbError>;
    fn ftp_list(&mut self) -> Result<FtpListState, PtpUsbError>;
    /// The directory count and the first `max` slots (slot numbers from 1), each slot's bytes as returned.
    fn iptc_slots(&mut self, max: u32) -> Result<(u32, Vec<(u32, Vec<u8>)>), PtpUsbError>;
}

impl<T: crate::ptp_usb::BulkTransport> ExtendedSource for PtpUsbSession<T> {
    fn identity(&mut self) -> Result<ReadFrom, PtpUsbError> {
        let report = self.read_device_info_report()?;
        Ok(ReadFrom { model: report.usb_model, usb_serial: report.usb_serial })
    }

    fn prop_bytes(&mut self, code: u16) -> Result<Vec<u8>, PtpUsbError> {
        self.get_device_prop_value(code).map(|r| r.data)
    }

    fn ftp_list(&mut self) -> Result<FtpListState, PtpUsbError> {
        self.read_ftp_list()
    }

    fn iptc_slots(&mut self, max: u32) -> Result<(u32, Vec<(u32, Vec<u8>)>), PtpUsbError> {
        self.read_iptc_slots(max)
    }
}

/// What the restore needs from a raw-PTP session beyond reading: one method per kind of write, each returning
/// whether the write's own read-back matched. A trait so the restore logic runs against a scripted body in tests.
pub trait ExtendedSink: ExtendedSource {
    fn write_string(&mut self, code: u16, text: &str) -> Result<bool, PtpUsbError>;
    fn write_byte(&mut self, code: u16, value: u8) -> Result<bool, PtpUsbError>;
    fn write_iptc(&mut self, slot: u32, data: &[u8]) -> Result<bool, PtpUsbError>;
    /// Delete the stored FTP profile at `index` (`0x90EF(index)`); the count the camera reports afterwards. The rebuild
    /// deletes one profile at a time, highest index first; there is no delete-all.
    fn delete_ftp(&mut self, index: u32) -> Result<u32, PtpUsbError>;
    /// Add one profile (it lands at the front of the list); `count_before` is the stored count.
    fn add_ftp(&mut self, blob: &[u8], count_before: u32) -> Result<bool, PtpUsbError>;
}

impl<T: crate::ptp_usb::BulkTransport> ExtendedSink for PtpUsbSession<T> {
    fn write_string(&mut self, code: u16, text: &str) -> Result<bool, PtpUsbError> {
        self.set_string_property(code, text, false).map(|r| r.verified)
    }

    fn write_byte(&mut self, code: u16, value: u8) -> Result<bool, PtpUsbError> {
        self.set_byte_property(code, value, false).map(|r| r.verified())
    }

    fn write_iptc(&mut self, slot: u32, data: &[u8]) -> Result<bool, PtpUsbError> {
        let out = self.write_iptc_slot(slot, data, false)?;
        Ok(iptc_write_verified(&out, data))
    }

    fn delete_ftp(&mut self, index: u32) -> Result<u32, PtpUsbError> {
        self.delete_ftp_profile(index)
    }

    fn add_ftp(&mut self, blob: &[u8], count_before: u32) -> Result<bool, PtpUsbError> {
        self.write_ftp_profile_blob_verified(count_before, 1, blob, false).map(|o| ftp_write_verified(&o))
    }
}

/// Whether an IPTC slot write is verified: the read after the accepted write succeeded and holds exactly what was
/// sent. A failed read-back is not verified (the write happened; nothing confirms it).
pub(crate) fn iptc_write_verified(out: &IptcWriteOutcome, sent: &[u8]) -> bool {
    matches!(&out.after, Ok(after) if after.data == sent)
}

/// Whether a guarded FTP profile write is verified: its own read-back found slot 0 holding the profile sent and the
/// count it should leave. Every other outcome (a mismatch, an undecodable read-back, a failed one) is not.
pub(crate) fn ftp_write_verified(out: &FtpWriteOutcome) -> bool {
    out.verification.is_verified()
}

fn failed(e: &PtpUsbError) -> (String, String) {
    (e.tag().to_string(), e.to_string())
}

fn prop_state(kind: PropKind, bytes: Vec<u8>) -> PropState {
    match kind {
        PropKind::Bytes => PropState::Bytes { hex: hex_encode(&bytes) },
        PropKind::Text => match decode_ptp_string_value(&bytes) {
            Ok(text) => PropState::Text { text },
            Err(why) => PropState::Undecodable { hex: hex_encode(&bytes), why },
        },
    }
}

/// Read everything this module knows how to read from `src`. Never fails as a whole: each item records
/// its own outcome, so a body that lacks a feature, or one read that fails, does not cost the other items. The
/// granularity is the item: the FTP list and the IPTC slots are each read whole, and a failed read inside one
/// (any one profile, any one slot) records `ReadFailed` for that whole section, not just the entry.
pub fn read_extended<S: ExtendedSource + ?Sized>(src: &mut S) -> Extended {
    let read_from = match src.identity() {
        Ok(x) => x,
        Err(e) => {
            let (tag, detail) = failed(&e);
            return Extended::all_failed(ReadFrom { model: String::new(), usb_serial: String::new() }, &tag, &detail);
        }
    };
    let mut props = BTreeMap::new();
    for &(code, name, kind) in &EXTENDED_PROPS {
        let state = match src.prop_bytes(code) {
            Ok(bytes) => prop_state(kind, bytes),
            Err(PtpUsbError::OperationFailed { code: PtpResponseCode(RESP_DEVICE_PROP_NOT_SUPPORTED), .. }) => {
                PropState::NotSupported
            }
            Err(e) => {
                let (tag, detail) = failed(&e);
                PropState::ReadFailed { tag, detail }
            }
        };
        props.insert(prop_key(code), ExtendedProp { code: u32::from(code), name: name.to_string(), state });
    }
    let ftp = if ftp_absent(&read_from.model) {
        FtpState::NotOnModel
    } else {
        match src.ftp_list() {
            Ok(list) => FtpState::Read(FtpStore {
                count: list.info.count,
                limit: list.info.limit,
                active_slot: list.info.has_active_profile().then_some(list.info.slot),
                device_name: list.info.device_name.text.clone(),
                truncated: list.truncated,
                profiles: list
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| FtpProfileEntry { slot: i as u32, hex: hex_encode(&e.raw) })
                    .collect(),
            }),
            Err(e) => {
                let (tag, detail) = failed(&e);
                FtpState::ReadFailed { tag, detail }
            }
        }
    };
    let iptc = if iptc_absent(&read_from.model) {
        IptcState::NotOnModel
    } else {
        match src.iptc_slots(IPTC_READ_MAX) {
            Ok((count, slots)) => IptcState::Read(IptcStore {
                count,
                truncated: count > IPTC_READ_MAX,
                slots: slots.into_iter().map(|(slot, bytes)| IptcSlotEntry { slot, hex: hex_encode(&bytes) }).collect(),
            }),
            Err(e) => {
                let (tag, detail) = failed(&e);
                IptcState::ReadFailed { tag, detail }
            }
        }
    };
    Extended { read_from, props, ftp, iptc }
}

/// A count of what an [`Extended`] holds, for a capture's result: small enough to put in a JSON result
/// and a status line without the strings (which can be private) or the FTP blobs (which hold passwords).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtendedSummary {
    /// Properties with a value (text, bytes, or undecodable bytes kept).
    pub props_read: usize,
    /// Properties the body answered `DevicePropNotSupported` for.
    pub props_not_supported: usize,
    /// Properties the body lists and the read failed on.
    pub props_failed: usize,
    /// `not_on_model`, `read_failed` or `read`.
    pub ftp: String,
    /// How many profiles were stored, when the list was read.
    pub ftp_profiles: Option<usize>,
    /// Of those, how many do not decode (kept as raw hex, which a restore refuses to write).
    #[serde(default)]
    pub ftp_profiles_undecodable: Option<usize>,
    /// The list was read only in part (`FtpStore::truncated`).
    #[serde(default)]
    pub ftp_truncated: bool,
    /// `not_read`, `not_on_model`, `read_failed` or `read`.
    pub iptc: String,
    /// How many IPTC presets hold text, when the slots were read.
    pub iptc_presets_in_use: Option<usize>,
    /// Of the slots read, how many could not be decoded at all (not counted as in use).
    #[serde(default)]
    pub iptc_slots_undecodable: Option<usize>,
    /// The slots were read only in part (`IptcStore::truncated`).
    #[serde(default)]
    pub iptc_truncated: bool,
    /// Set when the data was read but not kept, and why (a different body than the snapshot's).
    pub dropped: Option<String>,
}

impl Extended {
    pub fn summary(&self) -> ExtendedSummary {
        let count = |f: fn(&PropState) -> bool| self.props.values().filter(|p| f(&p.state)).count();
        let (ftp, ftp_profiles, ftp_profiles_undecodable, ftp_truncated) = match &self.ftp {
            FtpState::NotOnModel => ("not_on_model", None, None, false),
            FtpState::ReadFailed { .. } => ("read_failed", None, None, false),
            FtpState::Read(store) => ("read", Some(store.profiles.len()), Some(store.profiles.iter().filter(|p| p.decode().is_err()).count()), store.truncated),
        };
        let (iptc, iptc_presets_in_use, iptc_slots_undecodable, iptc_truncated) = match &self.iptc {
            IptcState::NotRead => ("not_read", None, None, false),
            IptcState::NotOnModel => ("not_on_model", None, None, false),
            IptcState::ReadFailed { .. } => ("read_failed", None, None, false),
            IptcState::Read(store) => (
                "read",
                Some(store.slots.iter().filter(|s| matches!(s.decode(), Ok(IptcRecord::Preset(p)) if !p.is_empty())).count()),
                Some(store.slots.iter().filter(|s| s.decode().is_err()).count()),
                store.truncated,
            ),
        };
        ExtendedSummary {
            props_read: count(|s| matches!(s, PropState::Text { .. } | PropState::Bytes { .. } | PropState::Undecodable { .. })),
            props_not_supported: count(|s| matches!(s, PropState::NotSupported)),
            props_failed: count(|s| matches!(s, PropState::ReadFailed { .. })),
            ftp: ftp.to_string(),
            ftp_profiles,
            ftp_profiles_undecodable,
            ftp_truncated,
            iptc: iptc.to_string(),
            iptc_presets_in_use,
            iptc_slots_undecodable,
            iptc_truncated,
            dropped: None,
        }
    }
}

/// Read the extended data of the body `serial` names (or the sole attached body when `None`): takes the
/// camera lock, opens a raw-PTP session, reads, and lets the session go, which releases the lock and the
/// interface. A failure at any step is a recorded failure, never an error: the MAID capture that follows
/// does not depend on this. Call it BEFORE the SDK session is opened; the two cannot share the interface.
pub fn read_extended_for_serial(serial: Option<&str>) -> Extended {
    match PtpUsbSession::open(serial) {
        Ok(mut session) => read_extended(&mut session),
        Err(e) => {
            let (tag, detail) = failed(&e);
            Extended::all_failed(ReadFrom { model: String::new(), usb_serial: String::new() }, &tag, &detail)
        }
    }
}

/// Open a raw-PTP session on the body `serial` names (the caller holds the camera lock and hands it
/// over; it is released when the session is dropped) and read the extended data. A session that will
/// not open is a recorded failure, not an error: the MAID capture does not depend on this.
pub fn read_extended_locked(lock: crate::camera_lock::CameraLock, serial: Option<&str>) -> Extended {
    match PtpUsbSession::open_locked(lock, serial) {
        Ok(mut session) => read_extended(&mut session),
        Err(e) => {
            let (tag, detail) = failed(&e);
            Extended::all_failed(ReadFrom { model: String::new(), usb_serial: String::new() }, &tag, &detail)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp_usb::{FTP_LIST_MAX_READ, FtpListEntry, FtpProfileInfo, PtpString};

    struct Fake {
        info: Result<ReadFrom, PtpUsbError>,
        props: BTreeMap<u16, Result<Vec<u8>, PtpUsbError>>,
        ftp: Result<FtpListState, PtpUsbError>,
        iptc: Result<(u32, Vec<(u32, Vec<u8>)>), PtpUsbError>,
        asked: Vec<String>,
        /// Answer every property read with DevicePropNotSupported.
        not_supported: bool,
    }

    fn ptp_string(text: &str) -> Vec<u8> {
        crate::ptp_usb::encode_ptp_string_value(text).unwrap()
    }

    fn list(blobs: &[&[u8]], count: u32, slot: u32) -> FtpListState {
        FtpListState {
            info: FtpProfileInfo {
                header: 200,
                count,
                limit: 20,
                slot,
                guid: [0; 16],
                device_name: PtpString { text: "Z_9_1".into(), anomalies: vec![] },
                trailer: 500,
            },
            entries: blobs.iter().map(|b| FtpListEntry { raw: b.to_vec(), decoded: None }).collect(),
            truncated: count > FTP_LIST_MAX_READ,
        }
    }

    fn z9() -> Fake {
        let mut props = BTreeMap::new();
        props.insert(0xD072, Ok(ptp_string("Brian L Wong")));
        props.insert(0xD073, Ok(ptp_string("(c)2026")));
        props.insert(0xD090, Ok(ptp_string("")));
        props.insert(0xD053, Ok(vec![1]));
        props.insert(0xD060, Ok(vec![3]));
        Fake {
            info: Ok(ReadFrom { model: "Z 9".into(), usb_serial: "0000003023668".into() }),
            props,
            ftp: Ok(list(&[&[1, 2, 3], &[4, 5]], 2, 0)),
            iptc: Ok((2, vec![(1, vec![1, 2]), (2, vec![3])])),
            asked: vec![],
            not_supported: false,
        }
    }

    impl ExtendedSource for Fake {
        fn identity(&mut self) -> Result<ReadFrom, PtpUsbError> {
            match &self.info {
                Ok(x) => Ok(x.clone()),
                Err(_) => Err(PtpUsbError::NoDataPhase { op: 0x1001 }),
            }
        }
        fn prop_bytes(&mut self, code: u16) -> Result<Vec<u8>, PtpUsbError> {
            self.asked.push(format!("prop {code:#06x}"));
            if self.not_supported {
                return Err(PtpUsbError::OperationFailed { op: 0x1015, code: PtpResponseCode(0x200A) });
            }
            match self.props.get(&code) {
                Some(Ok(b)) => Ok(b.clone()),
                _ => Err(PtpUsbError::NoDataPhase { op: 0x1015 }),
            }
        }
        fn iptc_slots(&mut self, _max: u32) -> Result<(u32, Vec<(u32, Vec<u8>)>), PtpUsbError> {
            self.asked.push("iptc".into());
            match &self.iptc {
                Ok(x) => Ok(x.clone()),
                Err(_) => Err(PtpUsbError::NoDataPhase { op: 0x9412 }),
            }
        }
        fn ftp_list(&mut self) -> Result<FtpListState, PtpUsbError> {
            self.asked.push("ftp".into());
            match &self.ftp {
                Ok(l) => Ok(l.clone()),
                Err(_) => Err(PtpUsbError::NoDataPhase { op: 0x90ED }),
            }
        }
    }

    #[test]
    fn a_body_with_everything_yields_every_item() {
        let e = read_extended(&mut z9());
        assert_eq!(e.prop(0xD072).unwrap().state, PropState::Text { text: "Brian L Wong".into() });
        assert_eq!(e.prop(0xD073).unwrap().state, PropState::Text { text: "(c)2026".into() });
        assert_eq!(e.prop(0xD090).unwrap().state, PropState::Text { text: String::new() }, "an empty string is a value");
        assert_eq!(e.prop(0xD053).unwrap().state, PropState::Bytes { hex: "01".into() });
        assert_eq!(e.prop(0xD060).unwrap().state, PropState::Bytes { hex: "03".into() });
        let FtpState::Read(store) = &e.ftp else { panic!("{:?}", e.ftp) };
        assert_eq!((store.count, store.limit, store.active_slot, store.truncated), (2, 20, Some(0), false));
        assert_eq!(store.profiles.iter().map(|p| (p.slot, p.hex.as_str())).collect::<Vec<_>>(), [(0, "010203"), (1, "0405")]);
        assert_eq!(e.read_from.usb_serial, "0000003023668");
    }

    #[test]
    fn every_known_prop_is_present_whatever_happened_to_it() {
        let mut f = z9();
        f.props.clear();
        f.not_supported = true;
        let e = read_extended(&mut f);
        assert_eq!(e.props.len(), EXTENDED_PROPS.len());
        assert!(e.props.values().all(|p| p.state == PropState::NotSupported));
    }

    #[test]
    fn the_properties_are_asked_for_whatever_the_model() {
        // A Z 9's DeviceInfo lists none of them and all five read, so nothing is skipped on a guess.
        for model in ["Z 9", "Z6_3", "Z 5", "Z_NEW"] {
            let mut f = z9();
            f.info = Ok(ReadFrom { model: model.into(), usb_serial: "1".into() });
            read_extended(&mut f);
            assert_eq!(f.asked.iter().filter(|a| a.starts_with("prop")).count(), EXTENDED_PROPS.len(), "{model}");
        }
    }

    #[test]
    fn ftp_is_skipped_for_exactly_the_models_known_to_lack_it() {
        // Driven by the capability table itself: the capture skips exactly the rows marked `Absent`, and asks every other row.
        let mut skipped = 0;
        for row in &crate::ptp_usb::MODEL_CAPS {
            let mut f = z9();
            f.info = Ok(ReadFrom { model: row.model.into(), usb_serial: "5".into() });
            if row.ftp == crate::ptp_usb::Support::Absent {
                skipped += 1;
                assert_eq!(read_extended(&mut f).ftp, FtpState::NotOnModel, "{}", row.model);
                assert!(!f.asked.iter().any(|a| a == "ftp"), "{}: {:?}", row.model, f.asked);
            } else {
                assert!(matches!(read_extended(&mut f).ftp, FtpState::Read(_)), "{}", row.model);
            }
        }
        assert!(skipped >= 1, "the table has a body without FTP profiles (the Z 5), so the skip is exercised");
        // Anything else is asked: a model the table has never heard of, and near-misses of the absent name.
        for model in ["Z_NEW", "Z 5 ", "z 5", "Z 50", ""] {
            let mut f = z9();
            f.info = Ok(ReadFrom { model: model.into(), usb_serial: "1".into() });
            assert!(matches!(read_extended(&mut f).ftp, FtpState::Read(_)), "{model:?}");
        }
    }

    #[test]
    fn iptc_slots_are_stored_raw_and_a_body_without_iptc_is_not_asked() {
        let e = read_extended(&mut z9());
        let IptcState::Read(store) = &e.iptc else { panic!("{:?}", e.iptc) };
        assert_eq!((store.count, store.truncated), (2, false));
        assert_eq!(store.slots.iter().map(|s| (s.slot, s.hex.as_str())).collect::<Vec<_>>(), [(1, "0102"), (2, "03")]);
        let mut skipped = 0;
        for row in &crate::ptp_usb::MODEL_CAPS {
            let mut f = z9();
            f.info = Ok(ReadFrom { model: row.model.into(), usb_serial: "5".into() });
            if row.iptc == crate::ptp_usb::Support::Absent {
                skipped += 1;
                assert_eq!(read_extended(&mut f).iptc, IptcState::NotOnModel, "{}", row.model);
                assert!(!f.asked.iter().any(|a| a == "iptc"));
            } else {
                assert!(matches!(read_extended(&mut f).iptc, IptcState::Read(_)), "{}", row.model);
            }
        }
        assert!(skipped >= 1);
        for model in ["Z_NEW", "Z 5 ", "z 5", "", "Z 50"] {
            let mut f = z9();
            f.info = Ok(ReadFrom { model: model.into(), usb_serial: "1".into() });
            assert!(matches!(read_extended(&mut f).iptc, IptcState::Read(_)), "an unrecognized model is asked: {model:?}");
        }
        let mut f = z9();
        f.iptc = Err(PtpUsbError::NoDataPhase { op: 0x9412 });
        assert!(matches!(read_extended(&mut f).iptc, IptcState::ReadFailed { .. }));
        // A count above the cap is a prefix, and exactly the cap is the whole list.
        let mut f = z9();
        f.iptc = Ok((IPTC_READ_MAX + 1, vec![(1, vec![1])]));
        assert!(matches!(&read_extended(&mut f).iptc, IptcState::Read(s) if s.truncated));
        f.iptc = Ok((IPTC_READ_MAX, vec![(1, vec![1])]));
        assert!(matches!(&read_extended(&mut f).iptc, IptcState::Read(s) if !s.truncated));
    }

    #[test]
    fn a_section_written_before_iptc_existed_loads_as_not_read() {
        let mut v = serde_json::to_value(read_extended(&mut z9())).unwrap();
        v.as_object_mut().unwrap().remove("iptc");
        let e: Extended = serde_json::from_value(v).unwrap();
        assert_eq!(e.iptc, IptcState::NotRead);
    }

    #[test]
    fn only_device_prop_not_supported_means_the_body_lacks_the_property() {
        // 0x200A is "not supported"; any other refusal (here GeneralError 0x2002) is a failed read.
        let mut f = z9();
        f.props.insert(0xD073, Err(PtpUsbError::NoDataPhase { op: 0x1015 }));
        let e = read_extended(&mut f);
        assert!(matches!(e.prop(0xD073).unwrap().state, PropState::ReadFailed { .. }));
    }

    #[test]
    fn one_failed_read_does_not_cost_the_others() {
        let mut f = z9();
        f.props.insert(0xD073, Err(PtpUsbError::NoDataPhase { op: 0x1015 }));
        f.ftp = Err(PtpUsbError::NoDataPhase { op: 0x90ED });
        let e = read_extended(&mut f);
        assert!(matches!(&e.prop(0xD073).unwrap().state, PropState::ReadFailed { tag, .. } if !tag.is_empty()));
        assert_eq!(e.prop(0xD072).unwrap().state, PropState::Text { text: "Brian L Wong".into() });
        assert!(matches!(&e.ftp, FtpState::ReadFailed { tag, detail } if !tag.is_empty() && !detail.is_empty()));
    }

    #[test]
    fn a_string_that_does_not_decode_keeps_its_bytes() {
        let mut f = z9();
        f.props.insert(0xD072, Ok(vec![5, b'a', 0])); // count says 5 units, 2 bytes follow
        let e = read_extended(&mut f);
        match &e.prop(0xD072).unwrap().state {
            PropState::Undecodable { hex, why } => {
                assert_eq!(hex, "056100");
                assert!(!why.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn no_active_profile_is_none_not_slot_255() {
        let mut f = z9();
        f.ftp = Ok(list(&[], 0, crate::ptp_usb::FTP_NO_ACTIVE_SLOT));
        let FtpState::Read(store) = read_extended(&mut f).ftp else { panic!() };
        assert_eq!((store.count, store.active_slot, store.profiles.len()), (0, None, 0));
    }

    #[test]
    fn a_count_above_the_read_cap_says_it_is_a_prefix() {
        let mut f = z9();
        f.ftp = Ok(list(&[&[1]], FTP_LIST_MAX_READ + 1, 0));
        let FtpState::Read(store) = read_extended(&mut f).ftp else { panic!() };
        assert!(store.truncated);
        f.ftp = Ok(list(&[&[1]], FTP_LIST_MAX_READ, 0));
        let FtpState::Read(store) = read_extended(&mut f).ftp else { panic!() };
        assert!(!store.truncated, "exactly the cap is the whole list");
    }

    #[test]
    fn a_dead_session_records_why_for_every_item() {
        let mut f = z9();
        f.info = Err(PtpUsbError::NoDataPhase { op: 0x1001 });
        let e = read_extended(&mut f);
        assert!(e.props.values().all(|p| matches!(&p.state, PropState::ReadFailed { .. })));
        assert!(matches!(e.ftp, FtpState::ReadFailed { .. }));
        assert!(f.asked.is_empty());
    }

    #[test]
    fn json_round_trips_and_names_each_state() {
        let e = read_extended(&mut z9());
        let text = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Extended>(&text).unwrap(), e);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["props"]["0xd072"]["state"], "text");
        assert_eq!(v["props"]["0xd072"]["text"], "Brian L Wong");
        assert_eq!(v["props"]["0xd053"]["hex"], "01");
        assert_eq!(v["ftp"]["state"], "read");
        assert_eq!(v["ftp"]["profiles"][1]["hex"], "0405");
        for state in [PropState::NotSupported, PropState::ReadFailed { tag: "t".into(), detail: "d".into() }] {
            let p = ExtendedProp { code: 1, name: "n".into(), state };
            let back: ExtendedProp = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
            assert_eq!(back, p);
        }
    }

    #[test]
    fn hex_decode_is_exact() {
        assert_eq!(hex_decode("").unwrap(), Vec::<u8>::new());
        assert_eq!(hex_decode("00ff7a").unwrap(), [0, 255, 122]);
        assert!(hex_decode("0").is_err(), "odd length");
        assert!(hex_decode("zz").is_err());
        assert!(hex_decode("+1").is_err(), "from_str_radix would take a sign; a byte is two hex digits");
        assert_eq!(hex_decode(&hex_encode(&[0, 1, 254, 255])).unwrap(), [0, 1, 254, 255]);
    }

    #[test]
    fn check_body_refuses_only_a_visible_mismatch() {
        let e = read_extended(&mut z9());
        assert!(e.check_body("Z 9", "0000003023668").is_ok());
        assert!(e.check_body("Z 9", "0000003023669").is_err(), "different serial");
        assert!(e.check_body("Z6_3", "0000003023668").is_err(), "different model");
        assert!(e.check_body("Z 9", "").is_ok(), "no snapshot serial is not evidence of a different body");
        let mut anon = e.clone();
        anon.read_from.usb_serial = String::new();
        assert!(anon.check_body("Z 9", "anything").is_ok(), "no USB serial on the raw side likewise");
    }

    // ── the write adapter's verdicts (the only place `verified` is decided for IPTC and FTP writes) ──

    fn read_of(data: &[u8]) -> crate::ptp_usb::VendorRead {
        crate::ptp_usb::VendorRead { data: data.to_vec(), response_params: vec![], warnings: vec![] }
    }

    fn iptc_outcome(after: Result<crate::ptp_usb::VendorRead, PtpUsbError>) -> IptcWriteOutcome {
        IptcWriteOutcome { before: read_of(&[0]), response_params: vec![], warnings: vec![], after }
    }

    #[test]
    fn an_iptc_write_is_verified_only_when_the_read_back_holds_exactly_what_was_sent() {
        let sent = [1u8, 2, 3];
        assert!(iptc_write_verified(&iptc_outcome(Ok(read_of(&sent))), &sent), "the read-back is what was sent");
        for (what, after) in [
            ("one byte different", Ok(read_of(&[1, 2, 4]))),
            ("a prefix of it", Ok(read_of(&[1, 2]))),
            ("it with a byte more", Ok(read_of(&[1, 2, 3, 0]))),
            ("empty", Ok(read_of(&[]))),
            ("the read-back failed", Err(PtpUsbError::NoDataPhase { op: 0x9412 })),
        ] {
            assert!(!iptc_write_verified(&iptc_outcome(after), &sent), "{what} must not read as verified");
        }
        // the unchanged old content is not a verification either: `before` is not consulted
        let o = IptcWriteOutcome { before: read_of(&sent), response_params: vec![], warnings: vec![], after: Err(PtpUsbError::NoDataPhase { op: 0x9412 }) };
        assert!(!iptc_write_verified(&o, &sent));
    }

    fn ftp_outcome(verification: crate::ptp_usb::FtpWriteVerification) -> FtpWriteOutcome {
        FtpWriteOutcome {
            receipt: crate::ptp_usb::FtpWriteReceipt { response_params: vec![], reply_matched_command: true },
            kind: crate::ptp_usb::FtpWriteKind::Add,
            count_before: 1,
            verification,
        }
    }

    #[test]
    fn an_ftp_write_is_verified_only_by_the_verified_read_back_and_never_by_the_other_three() {
        use crate::ptp_usb::FtpWriteVerification as V;
        assert!(ftp_write_verified(&ftp_outcome(V::Verified { count_after: 2 })));
        assert!(!ftp_write_verified(&ftp_outcome(V::Mismatch { count_after: 2, count_expected: 3, differing_fields: vec!["host"] })));
        assert!(!ftp_write_verified(&ftp_outcome(V::ReadBackUndecodable { error: PtpUsbError::NoDataPhase { op: 0x90ED } })));
        assert!(!ftp_write_verified(&ftp_outcome(V::ReadBackFailed { error: PtpUsbError::NoDataPhase { op: 0x90ED } })));
    }

    // ── what must not leak, what is one definition ──

    #[test]
    fn debug_output_of_everything_that_holds_a_profile_blob_hides_the_blob() {
        let blob_hex = "deadbeef0123456789abcdef";
        let entry = FtpProfileEntry { slot: 3, hex: blob_hex.into() };
        let store = FtpStore { count: 1, limit: 20, active_slot: Some(0), device_name: "Z_9_1".into(), truncated: false, profiles: vec![entry.clone()] };
        let ext = Extended {
            read_from: ReadFrom { model: "Z 9".into(), usb_serial: "1".into() },
            props: BTreeMap::new(),
            ftp: FtpState::Read(store.clone()),
            iptc: IptcState::NotRead,
        };
        let mut snap = crate::snapshot::Snapshot::new(
            crate::snapshot::Camera { model: "Z 9".into(), serial: "1".into(), firmware: "1".into() },
            crate::snapshot::Transport::Usb,
            "2026-11-06T00:00:00Z".into(),
        );
        snap.extended = Some(ext.clone());
        for (what, dump) in [("entry", format!("{entry:?}")), ("store", format!("{store:?}")), ("state", format!("{:?}", ext.ftp)), ("section", format!("{ext:?}")), ("snapshot", format!("{snap:?}")), ("pretty", format!("{snap:#?}"))] {
            assert!(!dump.contains(blob_hex) && !dump.contains("deadbeef"), "{what}: {dump}");
            assert!(dump.contains("12 byte(s) hidden"), "{what} says a profile is there, and how big: {dump}");
        }
        assert!(format!("{entry:?}").contains("slot: 3") && format!("{entry:?}").contains("12 byte(s) hidden"), "the slot and the size are shown");
        // the file still carries it: that is where it is kept
        assert!(serde_json::to_string(&entry).unwrap().contains(blob_hex));
    }

    #[test]
    fn device_prop_not_supported_is_the_code_ptp_usb_names_that() {
        assert_eq!(PtpResponseCode(RESP_DEVICE_PROP_NOT_SUPPORTED).to_string(), "DevicePropNotSupported (0x200a)");
    }

    #[test]
    fn a_props_key_must_be_the_one_its_code_gives() {
        let mut e = read_extended(&mut z9());
        assert!(e.check_keys().is_ok());
        let moved = e.props.remove("0xd072").unwrap();
        e.props.insert("0xd073x".into(), moved);
        assert!(e.check_keys().unwrap_err().contains("0xd073x"), "a key that is not its code's");
        let mut e = read_extended(&mut z9());
        e.props.get_mut("0xd072").unwrap().code = 0xD073;
        assert!(e.check_keys().unwrap_err().contains("0xd073"), "a code that is not its key's");
        let mut e = read_extended(&mut z9());
        e.props.get_mut("0xd072").unwrap().code = 0x1_D072;
        assert!(e.check_keys().is_err(), "a code that is not 16 bits has no key");
    }

    // ── the summary ──

    #[test]
    fn the_summary_counts_what_it_could_not_decode_and_says_when_a_list_is_partial() {
        // z9()'s FTP blobs and IPTC slots are placeholders that decode to nothing
        let s = read_extended(&mut z9()).summary();
        assert_eq!((s.ftp_profiles, s.ftp_profiles_undecodable, s.ftp_truncated), (Some(2), Some(2), false));
        assert_eq!((s.iptc_presets_in_use, s.iptc_slots_undecodable, s.iptc_truncated), (Some(0), Some(2), false));
        // a real preset slot decodes; an empty one is decoded and not in use, so neither is "undecodable"
        let preset = |name: &str| {
            crate::iptc::encode_iptc_preset(&crate::iptc::IptcPreset { name: name.into(), fields: vec![String::new(); 14] }).unwrap()
        };
        let mut f = z9();
        f.iptc = Ok((2, vec![(1, preset("Race")), (2, preset(""))]));
        let s = read_extended(&mut f).summary();
        assert_eq!((s.iptc_presets_in_use, s.iptc_slots_undecodable), (Some(1), Some(0)));
        // truncation shows in the summary too
        let mut f = z9();
        f.iptc = Ok((IPTC_READ_MAX + 1, vec![(1, preset("Race"))]));
        f.ftp = Ok(list(&[&[1]], FTP_LIST_MAX_READ + 1, 0));
        let s = read_extended(&mut f).summary();
        assert!(s.iptc_truncated && s.ftp_truncated);
        // sections that were not read have no counts
        let s = Extended::all_failed(ReadFrom { model: String::new(), usb_serial: String::new() }, "t", "d").summary();
        assert_eq!((s.ftp_profiles_undecodable, s.iptc_slots_undecodable, s.ftp_truncated, s.iptc_truncated), (None, None, false, false));
        // a summary written before these fields existed reads with them absent
        let mut v = serde_json::to_value(read_extended(&mut z9()).summary()).unwrap();
        for k in ["ftp_profiles_undecodable", "ftp_truncated", "iptc_slots_undecodable", "iptc_truncated"] {
            v.as_object_mut().unwrap().remove(k);
        }
        let old: ExtendedSummary = serde_json::from_value(v).unwrap();
        assert_eq!((old.ftp_profiles_undecodable, old.ftp_truncated), (None, false));
    }

    #[test]
    fn two_empty_identities_pass_check_body_and_a_restore_does_not_rely_on_that() {
        // Pinned, not endorsed: a section read when the raw session would not open has an empty ReadFrom (it records
        // the failure), and it is attached to whatever snapshot it was read for. The restore path decides the body
        // from the live camera (`restore_extended`'s same-body check), not from this.
        let e = Extended::all_failed(ReadFrom { model: String::new(), usb_serial: String::new() }, "t", "d");
        assert!(e.check_body("Z 9", "1").is_ok() && e.check_body("", "").is_ok());
    }
}

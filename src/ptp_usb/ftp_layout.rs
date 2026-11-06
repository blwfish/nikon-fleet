//! The `0x90EE` (write) / `0x90ED` (read) FTP-profile blob layout, stated ONCE.
//!
//! [`FTP_LAYOUT`] lists every section of the blob in wire order: the literal tag bytes, the
//! tag bytes that carry a value, and each value with its wire kind, where the encoder gets it from and
//! whether it is a secret. Three things are driven from that table and from nothing else:
//!
//! - the encoder ([`encode_ftp_profile_with`](super::encode_ftp_profile_with)): what to write, in what
//!   form, and which `FtpProfile` field, constant or [`FtpCameraNetwork`] part supplies each value;
//! - the strict decoder ([`decode_ftp_profile`](super::decode_ftp_profile) and
//!   [`decode_ftp_profile_spans`](super::decode_ftp_profile_spans)): what each section is called in
//!   an error, which bytes are accepted, and where the secrets sit in the blob;
//! - the write gate ([`DecodedFtpProfile::as_encodable`]): a decoded profile can be written back only
//!   if every value the encoder fixes to a constant has that constant, the camera network is the
//!   captured one and every text the encoder writes as ASCII is ASCII.
//!
//! A new variant of the layout (the 331-byte form, with a folder string and the option flags
//! `0x0001`, is the known next one) is an edit of this table: change a `Fixed` constant, or a value's
//! source from `Fixed` to a caller field (which also needs that field added to `FtpField` and
//! `FtpProfile`), add, drop or reorder entries. Nothing else repeats a tag byte, a section name, an
//! encoder constant or a secret flag; the tests in `ftp_layout/tests.rs` pin that by building such
//! variants from a copy of the table. **The folder**: the table's default encoder
//! ([`encode_ftp_profile`](super::encode_ftp_profile)) still fixes it to the empty string, but a profile that
//! was read and edited ([`encode_decoded_ftp_profile`](super::encode_decoded_ftp_profile)) is written with the
//! folder it holds, and snapshot editing and restore send such blobs to a camera. What a camera does with a
//! folder written by this program has **not been verified on hardware** (the 331-byte form is known from a
//! capture of NX Field's own write); reading the profile back after a restore is the check.
//!
//! The *limits* of the caller-supplied text fields (and their display names) are a separate table,
//! [`FTP_PROFILE_FIELDS`](super::FTP_PROFILE_FIELDS), because the GUIs need them without the wire
//! layout; the wire form of each field (ASCII u32-length or UTF-16 PTP string) is stated here only.

use super::*;
use std::ops::Range;

// ── slots ────────────────────────────────────────────────────────────────────────────────────

macro_rules! ftp_slots {
    ($($variant:ident => $key:literal,)*) => {
        /// Identity of one value of the blob. The key is the name of the matching
        /// [`DecodedFtpProfile`] field (an array element for the timestamps and IPv6 fields).
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum FtpSlot { $($variant),* }

        impl FtpSlot {
            /// Every slot, in declaration order.
            pub const ALL: &'static [FtpSlot] = &[$(FtpSlot::$variant),*];

            /// The machine name used for spans and in tests.
            pub fn key(self) -> &'static str {
                match self { $(FtpSlot::$variant => $key),* }
            }
        }
    };
}

ftp_slots! {
    ProfileName => "profile_name",
    Timestamp1 => "timestamp_1",
    Timestamp2 => "timestamp_2",
    CameraAddress => "camera_address",
    CameraPrefix => "camera_prefix_len",
    Ssid24Lead => "ssid24_lead",
    Ssid24Kind => "ssid24_kind",
    Ssid24 => "ssid_24ghz",
    Ssid5Kind => "ssid5_kind",
    Ssid5 => "ssid_5ghz",
    Host => "host",
    Folder => "folder",
    FtpPort => "ftp_port",
    Username => "username",
    Password => "password",
    OptionFlags => "option_flags",
    SecondPort => "second_port",
    Ipv6A => "ipv6_field_1",
    Ipv6B => "ipv6_field_2",
    Ipv6C => "ipv6_field_3",
}

/// A value read from or written to the blob, by wire kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FtpVal {
    Text(String),
    U8(u8),
    U16(u16),
    /// Dotted-quad octets in reading order.
    Ipv4([u8; 4]),
}

// ── the spec's vocabulary ────────────────────────────────────────────────────────────────────

/// One byte of a tag section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpTagByte {
    /// Must be exactly this; no known meaning beyond "copied from a real capture".
    Exact(u8),
    /// A byte the decoder keeps in `slot` (rejecting any value not in `allowed`, when given) and the
    /// encoder writes as `written`. [`DecodedFtpProfile::as_encodable`] requires the decoded byte to
    /// equal `written`.
    Var { slot: FtpSlot, allowed: Option<&'static [u8]>, written: u8 },
}

/// Where the encoder gets a text value from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpTextSource {
    /// A field of the caller's [`FtpProfile`], checked against that field's
    /// [`FtpFieldSpec`](super::FtpFieldSpec) (limit, control characters, ASCII when the wire form is).
    Profile(FtpField),
    /// The encoder always writes this; a decoded profile is encodable only if it holds exactly this.
    Fixed(&'static str),
}

/// Where the encoder gets a `u8` from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpU8Source {
    /// [`FtpCameraNetwork::prefix_len`]; the gate requires [`FtpCameraNetwork::CAPTURED`]'s.
    NetworkPrefix,
    Fixed(u8),
}

/// Where the encoder gets a `u16` from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpU16Source {
    /// [`FtpProfile::port`].
    ProfilePort,
    Fixed(u16),
}

/// How a value is laid out on the wire, and where the encoder gets it from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpValueKind {
    Text(FtpEncoding, FtpTextSource),
    U8(FtpU8Source),
    U16(FtpU16Source),
    /// The camera network's IPv4 address as a little-endian `u32` (so the octets are reversed on the
    /// wire), from [`FtpCameraNetwork::ipv4`]; the gate requires [`FtpCameraNetwork::CAPTURED`]'s.
    NetworkAddress,
}

/// What one section of the blob is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpWire {
    /// Bytes that must be exactly these (a header, padding, a separator, the trailer).
    Literal(&'static [u8]),
    /// A tag whose bytes are fixed except for the ones that carry a value.
    Pattern(&'static [FtpTagByte]),
    /// A value. `secret` marks the ones that must not reach a log, a dump or a display: the decoder
    /// records where their content sits, and `with_secrets_masked` / `secret_texts` read the flag (the
    /// hand-written `Debug` impls hide the same values; a test pins them to it).
    Value { slot: FtpSlot, kind: FtpValueKind, secret: bool },
}

/// One section of the blob: what the decoder calls it in an error, and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FtpLayoutEntry {
    /// The `what` of a [`PtpUsbError::FtpBlobMalformed`] raised at this section.
    pub name: &'static str,
    pub wire: FtpWire,
}

const fn lit(name: &'static str, bytes: &'static [u8]) -> FtpLayoutEntry {
    FtpLayoutEntry { name, wire: FtpWire::Literal(bytes) }
}

const fn pattern(name: &'static str, bytes: &'static [FtpTagByte]) -> FtpLayoutEntry {
    FtpLayoutEntry { name, wire: FtpWire::Pattern(bytes) }
}

const fn value(name: &'static str, slot: FtpSlot, kind: FtpValueKind) -> FtpLayoutEntry {
    FtpLayoutEntry { name, wire: FtpWire::Value { slot, kind, secret: false } }
}

const fn secret(name: &'static str, slot: FtpSlot, kind: FtpValueKind) -> FtpLayoutEntry {
    FtpLayoutEntry { name, wire: FtpWire::Value { slot, kind, secret: true } }
}

// ── the layout ───────────────────────────────────────────────────────────────────────────────

/// The date-time shape both timestamps of the template block carry in every capture.
pub const FTP_TEMPLATE_TIMESTAMP_PLACEHOLDER: &str = "00000000T000000";
/// What the three IPv6-shaped fields at the end carry in every capture.
pub const FTP_IPV6_PLACEHOLDER: &str = "0000:0000:0000:0000:0000:0000:0000:0000";

use FtpEncoding::{AsciiU32Len, Utf16U8Len, Utf16U8LenOptional};
use FtpSlot as S;
use FtpTagByte::{Exact, Var};
use FtpTextSource::{Fixed as FixedText, Profile};
use FtpValueKind::{NetworkAddress, Text, U8, U16};

/// The 2.4 GHz SSID tag's first byte is `03` in every July/November capture and in what the encoder
/// writes, but `00` in profiles that began life in the camera's own menu (seen 2026-07-11 on a Z 9:
/// the profiles `NxProfile` and `FtpPi`, both lineally derived from one made on the camera). The
/// decoder accepts either and keeps the byte as [`DecodedFtpProfile::ssid24_lead`].
const SSID24_LEADS: &[u8] = &[0x03, 0x00];

/// The blob, in wire order. See the module documentation.
///
/// The 82 bytes between the profile name and the SSID section (the two timestamps and the `network
/// tag` .. `network padding` entries) are the same in every real capture. Decoded: two UTF-16
/// strings `"00000000T000000"` (a date-time shape, all zeros), then tag `02 03`, `03`,
/// **`57 42 a8 c0`**, `18` and eight zero bytes. `57 42 a8 c0` read as a little-endian `u32` is
/// `c0.a8.42.57` = 192.168.66.87, the Z 9's own address on the capture rig, and `0x18` is 24: an IPv4
/// address and a /24 prefix length. So the part that varied nowhere in three captures only because
/// all three came from one rig is the address of that rig's camera; [`FtpCameraNetwork`] is that part
/// as a parameter. What the camera does with it (does it set a static address? is it ignored?) is
/// **not known**; the encoder's default stays the captured value because writing zeros or another
/// address was never tried.
///
/// What used to be written as a three-byte `[00 15 00]` "username tag" is the `folder` entry (an empty PTP string, one byte
/// `00`) followed by `ftp_port` (`15 00` = 21): the folder is the string the 331-byte capture carried
/// as `07 "Claude" 00 00`. It is [`Utf16U8LenOptional`] because the encoder writes the empty
/// form as the single count byte. Its source is `Fixed("")`: **making it a caller field is a spec
/// change, and has not been tried on a camera**.
///
/// The `option flags` word is `05 00` in what the encoder writes and `01 00` in the 331-byte capture.
/// The byte after the first IPv6 field (`0x40`) appears once, asymmetrically, in every capture:
/// meaning unknown, copied verbatim.
pub const FTP_LAYOUT: &[FtpLayoutEntry] = &[
    lit("blob header", &[0xf4, 0x01]),
    value("profile name", S::ProfileName, Text(Utf16U8Len, Profile(FtpField::ProfileName))),
    value("timestamp 1", S::Timestamp1, Text(Utf16U8Len, FixedText(FTP_TEMPLATE_TIMESTAMP_PLACEHOLDER))),
    value("timestamp 2", S::Timestamp2, Text(Utf16U8Len, FixedText(FTP_TEMPLATE_TIMESTAMP_PLACEHOLDER))),
    lit("network tag", &[0x02, 0x03, 0x03]),
    value("camera address", S::CameraAddress, NetworkAddress),
    value("camera prefix length", S::CameraPrefix, U8(FtpU8Source::NetworkPrefix)),
    lit("network padding", &[0; 8]),
    pattern(
        "2.4 GHz SSID tag",
        &[
            Var { slot: S::Ssid24Lead, allowed: Some(SSID24_LEADS), written: 0x03 },
            Exact(0x00),
            Var { slot: S::Ssid24Kind, allowed: None, written: 0x01 },
            Exact(0x00),
        ],
    ),
    value("2.4 GHz SSID", S::Ssid24, Text(AsciiU32Len, Profile(FtpField::Ssid24Ghz))),
    pattern("5 GHz SSID tag", &[Var { slot: S::Ssid5Kind, allowed: None, written: 0x04 }, Exact(0x04)]),
    secret("5 GHz SSID", S::Ssid5, Text(AsciiU32Len, Profile(FtpField::Ssid5Ghz))),
    lit("host tag", &[0x01]),
    value("host", S::Host, Text(Utf16U8Len, Profile(FtpField::Host))),
    value("folder", S::Folder, Text(Utf16U8LenOptional, FixedText(""))),
    value("FTP port", S::FtpPort, U16(FtpU16Source::Fixed(21))),
    value("username", S::Username, Text(AsciiU32Len, Profile(FtpField::Username))),
    secret("password", S::Password, Text(AsciiU32Len, Profile(FtpField::Password))),
    value("option flags", S::OptionFlags, U16(FtpU16Source::Fixed(0x0005))),
    value("second port", S::SecondPort, U16(FtpU16Source::ProfilePort)),
    lit("port-section padding", &[0; 5]),
    value("IPv6 field 1", S::Ipv6A, Text(AsciiU32Len, FixedText(FTP_IPV6_PLACEHOLDER))),
    lit("IPv6 separator", &[0x40]),
    value("IPv6 field 2", S::Ipv6B, Text(AsciiU32Len, FixedText(FTP_IPV6_PLACEHOLDER))),
    value("IPv6 field 3", S::Ipv6C, Text(AsciiU32Len, FixedText(FTP_IPV6_PLACEHOLDER))),
    lit("blob trailer", &[0x81]),
];

// ── the one projection between slots and the decoded profile ─────────────────────────────────

impl FtpVal {
    fn kind_name(&self) -> &'static str {
        match self {
            FtpVal::Text(_) => "text",
            FtpVal::U8(_) => "u8",
            FtpVal::U16(_) => "u16",
            FtpVal::Ipv4(_) => "ipv4",
        }
    }

    fn mismatch(&self, slot: FtpSlot, wanted: &str) -> ! {
        panic!("layout bug: slot {slot:?} holds {wanted}, but the layout gives it a {} value", self.kind_name())
    }

    fn into_text(self, slot: FtpSlot) -> String {
        match self {
            FtpVal::Text(t) => t,
            other => other.mismatch(slot, "text"),
        }
    }

    fn into_u8(self, slot: FtpSlot) -> u8 {
        match self {
            FtpVal::U8(v) => v,
            other => other.mismatch(slot, "a u8"),
        }
    }

    fn into_u16(self, slot: FtpSlot) -> u16 {
        match self {
            FtpVal::U16(v) => v,
            other => other.mismatch(slot, "a u16"),
        }
    }

    fn into_ipv4(self, slot: FtpSlot) -> [u8; 4] {
        match self {
            FtpVal::Ipv4(v) => v,
            other => other.mismatch(slot, "an IPv4 address"),
        }
    }
}

impl DecodedFtpProfile {
    /// All-empty, to be filled slot by slot by the decoder.
    fn blank() -> Self {
        DecodedFtpProfile {
            profile_name: String::new(),
            timestamps: [String::new(), String::new()],
            camera_network: FtpCameraNetwork { ipv4: [0; 4], prefix_len: 0 },
            ssid_24ghz: String::new(),
            ssid24_kind: 0,
            ssid24_lead: 0,
            ssid_5ghz: String::new(),
            ssid5_kind: 0,
            host: String::new(),
            folder: String::new(),
            username: String::new(),
            password: String::new(),
            ftp_port: 0,
            option_flags: 0,
            second_port: 0,
            ipv6_fields: [String::new(), String::new(), String::new()],
        }
    }

    /// The value in `slot`. Exhaustive: a new [`FtpSlot`] does not compile until it is mapped here.
    pub fn get(&self, slot: FtpSlot) -> FtpVal {
        match slot {
            S::ProfileName => FtpVal::Text(self.profile_name.clone()),
            S::Timestamp1 => FtpVal::Text(self.timestamps[0].clone()),
            S::Timestamp2 => FtpVal::Text(self.timestamps[1].clone()),
            S::CameraAddress => FtpVal::Ipv4(self.camera_network.ipv4),
            S::CameraPrefix => FtpVal::U8(self.camera_network.prefix_len),
            S::Ssid24Lead => FtpVal::U8(self.ssid24_lead),
            S::Ssid24Kind => FtpVal::U8(self.ssid24_kind),
            S::Ssid24 => FtpVal::Text(self.ssid_24ghz.clone()),
            S::Ssid5Kind => FtpVal::U8(self.ssid5_kind),
            S::Ssid5 => FtpVal::Text(self.ssid_5ghz.clone()),
            S::Host => FtpVal::Text(self.host.clone()),
            S::Folder => FtpVal::Text(self.folder.clone()),
            S::FtpPort => FtpVal::U16(self.ftp_port),
            S::Username => FtpVal::Text(self.username.clone()),
            S::Password => FtpVal::Text(self.password.clone()),
            S::OptionFlags => FtpVal::U16(self.option_flags),
            S::SecondPort => FtpVal::U16(self.second_port),
            S::Ipv6A => FtpVal::Text(self.ipv6_fields[0].clone()),
            S::Ipv6B => FtpVal::Text(self.ipv6_fields[1].clone()),
            S::Ipv6C => FtpVal::Text(self.ipv6_fields[2].clone()),
        }
    }

    /// Store `val` in `slot`. Exhaustive in the slot. The layout's value kind must match the slot's
    /// type (a test pins it for [`FTP_LAYOUT`]); a mismatch is a bug in the table, not in a blob, and
    /// panics naming both.
    fn set(&mut self, slot: FtpSlot, val: FtpVal) {
        match slot {
            S::ProfileName => self.profile_name = val.into_text(slot),
            S::Timestamp1 => self.timestamps[0] = val.into_text(slot),
            S::Timestamp2 => self.timestamps[1] = val.into_text(slot),
            S::CameraAddress => self.camera_network.ipv4 = val.into_ipv4(slot),
            S::CameraPrefix => self.camera_network.prefix_len = val.into_u8(slot),
            S::Ssid24Lead => self.ssid24_lead = val.into_u8(slot),
            S::Ssid24Kind => self.ssid24_kind = val.into_u8(slot),
            S::Ssid24 => self.ssid_24ghz = val.into_text(slot),
            S::Ssid5Kind => self.ssid5_kind = val.into_u8(slot),
            S::Ssid5 => self.ssid_5ghz = val.into_text(slot),
            S::Host => self.host = val.into_text(slot),
            S::Folder => self.folder = val.into_text(slot),
            S::FtpPort => self.ftp_port = val.into_u16(slot),
            S::Username => self.username = val.into_text(slot),
            S::Password => self.password = val.into_text(slot),
            S::OptionFlags => self.option_flags = val.into_u16(slot),
            S::SecondPort => self.second_port = val.into_u16(slot),
            S::Ipv6A => self.ipv6_fields[0] = val.into_text(slot),
            S::Ipv6B => self.ipv6_fields[1] = val.into_text(slot),
            S::Ipv6C => self.ipv6_fields[2] = val.into_text(slot),
        }
    }

    /// The text values [`FTP_LAYOUT`] marks secret, in wire order (for a content search of a dump).
    pub fn secret_texts(&self) -> Vec<String> {
        self.secret_texts_with(FTP_LAYOUT)
    }

    fn secret_texts_with(&self, layout: &[FtpLayoutEntry]) -> Vec<String> {
        secret_slots(layout)
            .filter_map(|slot| match self.get(slot) {
                FtpVal::Text(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    /// This profile with every secret text replaced by asterisks of the same byte length, for display.
    pub fn with_secrets_masked(&self) -> Self {
        self.masked_with(FTP_LAYOUT)
    }

    fn masked_with(&self, layout: &[FtpLayoutEntry]) -> Self {
        let mut masked = self.clone();
        for slot in secret_slots(layout) {
            if let FtpVal::Text(t) = self.get(slot) {
                masked.set(slot, FtpVal::Text("*".repeat(t.len())));
            }
        }
        masked
    }
}

/// The wire form `layout` gives the caller-supplied text field `field`.
pub(super) fn encoding_of(layout: &[FtpLayoutEntry], field: FtpField) -> Option<FtpEncoding> {
    layout.iter().find_map(|e| match e.wire {
        FtpWire::Value { kind: FtpValueKind::Text(encoding, FtpTextSource::Profile(f)), .. } if f == field => Some(encoding),
        _ => None,
    })
}

/// The slots `layout` marks secret, in wire order.
pub(super) fn secret_slots(layout: &[FtpLayoutEntry]) -> impl Iterator<Item = FtpSlot> + '_ {
    layout.iter().filter_map(|e| match e.wire {
        FtpWire::Value { slot, secret: true, .. } => Some(slot),
        _ => None,
    })
}

impl FtpProfile {
    /// The caller-supplied text field `field`.
    pub fn text(&self, field: FtpField) -> &str {
        match field {
            FtpField::ProfileName => &self.profile_name,
            FtpField::Ssid24Ghz => &self.ssid_24ghz,
            FtpField::Ssid5Ghz => &self.ssid_5ghz,
            FtpField::Host => &self.host,
            FtpField::Username => &self.username,
            FtpField::Password => &self.password,
        }
    }

    fn set_text(&mut self, field: FtpField, text: String) {
        match field {
            FtpField::ProfileName => self.profile_name = text,
            FtpField::Ssid24Ghz => self.ssid_24ghz = text,
            FtpField::Ssid5Ghz => self.ssid_5ghz = text,
            FtpField::Host => self.host = text,
            FtpField::Username => self.username = text,
            FtpField::Password => self.password = text,
        }
    }
}

// ── the secret spans ─────────────────────────────────────────────────────────────────────────

/// Where one secret's content sits in the blob that was decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpSecretSpan {
    /// [`FtpSlot::key`] of the secret.
    pub field: &'static str,
    /// The content bytes (not the length prefix) as offsets into the blob.
    pub range: Range<usize>,
}

/// Where the secret strings of a profile blob sit (the 5 GHz field, which is probably a Wi-Fi password,
/// and the password, at the time of writing: whatever [`FTP_LAYOUT`] marks secret), in wire order.
/// Taken from the decoder's own cursor, so a value that also appears in an earlier field cannot be
/// mistaken for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpSecretSpans {
    pub spans: Vec<FtpSecretSpan>,
}

impl FtpSecretSpans {
    /// The content ranges, in wire order.
    pub fn ranges(&self) -> impl Iterator<Item = &Range<usize>> {
        self.spans.iter().map(|s| &s.range)
    }

    /// The range of the secret named `field` ([`FtpSlot::key`]), if the layout has it.
    pub fn of(&self, field: &str) -> Option<&Range<usize>> {
        self.spans.iter().find(|s| s.field == field).map(|s| &s.range)
    }
}

// ── the encoder ──────────────────────────────────────────────────────────────────────────────

impl FtpEncoding {
    /// The unit a limit on a value of this form is expressed in.
    pub fn unit(self) -> FtpLenUnit {
        match self {
            FtpEncoding::AsciiU32Len => FtpLenUnit::Bytes,
            FtpEncoding::Utf16U8Len | FtpEncoding::Utf16U8LenOptional => FtpLenUnit::Utf16Units,
        }
    }

    /// Whether the encoder refuses non-ASCII text in this form. (The decoder reads the u32 form as
    /// UTF-8, so a camera-made profile may hold more than the encoder will write.) The write gate
    /// asks the same question, so the two cannot disagree.
    pub fn encoder_ascii_only(self) -> bool {
        matches!(self, FtpEncoding::AsciiU32Len)
    }

    /// Lay `value` out in this form without checking any limit; the `u8` count of a UTF-16 form is
    /// range-checked here so a long value cannot silently truncate it (`field` names it in the error).
    fn encode(self, field: &'static str, value: &str) -> Result<Vec<u8>, PtpUsbError> {
        match self {
            FtpEncoding::AsciiU32Len => {
                let bytes = value.as_bytes();
                let mut out = Vec::with_capacity(4 + bytes.len());
                out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                out.extend_from_slice(bytes);
                Ok(out)
            }
            FtpEncoding::Utf16U8Len | FtpEncoding::Utf16U8LenOptional => {
                if self == FtpEncoding::Utf16U8LenOptional && value.is_empty() {
                    return Ok(vec![0]);
                }
                let units: Vec<u16> = value.encode_utf16().collect();
                if u8::try_from(units.len() + 1).is_err() {
                    return Err(PtpUsbError::FtpFieldTooLong {
                        field,
                        len: units.len(),
                        max: usize::from(u8::MAX) - 1,
                        unit: self.unit().label(),
                    });
                }
                Ok(utf16_u8_len_bytes(&units))
            }
        }
    }
}

/// Check `value` against `spec` and `encoding`: ASCII-only where the wire form is, no control
/// characters, and within the limit (in the form's own unit).
fn validate_field(spec: &FtpFieldSpec, encoding: FtpEncoding, value: &str) -> Result<(), PtpUsbError> {
    validate_field_checking(spec, encoding, value, true)
}

/// [`validate_field`], with the ASCII-only rule of the form optional. A profile read from a camera may hold more than
/// the encoder writes (the decoder reads the byte-length form as UTF-8), and writing it back must not refuse it.
fn validate_field_checking(spec: &FtpFieldSpec, encoding: FtpEncoding, value: &str, require_ascii: bool) -> Result<(), PtpUsbError> {
    if require_ascii && encoding.encoder_ascii_only() && !value.is_ascii() {
        return Err(PtpUsbError::FtpFieldNotAscii { field: spec.key });
    }
    if value.chars().any(char::is_control) {
        return Err(PtpUsbError::FtpFieldControlChar { field: spec.key });
    }
    let len = match encoding.unit() {
        FtpLenUnit::Bytes => value.len(),
        FtpLenUnit::Utf16Units => value.encode_utf16().count(),
    };
    if len > spec.max_len {
        return Err(PtpUsbError::FtpFieldTooLong { field: spec.key, len, max: spec.max_len, unit: encoding.unit().label() });
    }
    Ok(())
}

/// Validate `value` against `spec` and lay it out as `encoding` says.
pub(super) fn encode_field_with(spec: &FtpFieldSpec, encoding: FtpEncoding, value: &str) -> Result<Vec<u8>, PtpUsbError> {
    validate_field(spec, encoding, value)?;
    encoding.encode(spec.key, value)
}

/// Build the data-out blob for `profile` and `network` by walking `layout`. Errors are those of the
/// first caller-supplied field, in wire order, that does not validate.
pub(super) fn encode_with(layout: &[FtpLayoutEntry], profile: &FtpProfile, network: &FtpCameraNetwork) -> Result<Vec<u8>, PtpUsbError> {
    let mut out = Vec::new();
    for entry in layout {
        match entry.wire {
            FtpWire::Literal(bytes) => out.extend_from_slice(bytes),
            FtpWire::Pattern(bytes) => out.extend(bytes.iter().map(|b| match *b {
                FtpTagByte::Exact(x) => x,
                FtpTagByte::Var { written, .. } => written,
            })),
            FtpWire::Value { kind, .. } => match kind {
                FtpValueKind::Text(encoding, FtpTextSource::Profile(field)) => {
                    out.extend_from_slice(&encode_field_with(field.spec(), encoding, profile.text(field))?)
                }
                FtpValueKind::Text(encoding, FtpTextSource::Fixed(text)) => out.extend_from_slice(&encoding.encode(entry.name, text)?),
                FtpValueKind::U8(FtpU8Source::NetworkPrefix) => out.push(network.prefix_len),
                FtpValueKind::U8(FtpU8Source::Fixed(v)) => out.push(v),
                FtpValueKind::U16(FtpU16Source::ProfilePort) => out.extend_from_slice(&profile.port.to_le_bytes()),
                FtpValueKind::U16(FtpU16Source::Fixed(v)) => out.extend_from_slice(&v.to_le_bytes()),
                FtpValueKind::NetworkAddress => {
                    let [a, b, c, d] = network.ipv4;
                    out.extend_from_slice(&[d, c, b, a]);
                }
            },
        }
    }
    Ok(out)
}

/// Build the blob for a decoded profile by walking `layout` with EVERY value taken from `d`: the tag bytes the layout
/// leaves variable, the camera network, the timestamps, the folder, the ports, the option bits and the IPv6 fields as
/// well as the text fields. So a profile that was read and not changed is written back byte for byte (pinned for the
/// real blobs by a test), and a field changed on `d` is the only difference. The caller-supplied text fields are held
/// to their limits and to "no control characters", but not to ASCII where the encoder insists on it for text a
/// person types: whether new non-ASCII text is acceptable is for the caller to decide, since a profile the camera
/// itself made can hold some.
pub(super) fn encode_decoded_with(layout: &[FtpLayoutEntry], d: &DecodedFtpProfile) -> Result<Vec<u8>, PtpUsbError> {
    let mut out = Vec::new();
    for entry in layout {
        match entry.wire {
            FtpWire::Literal(bytes) => out.extend_from_slice(bytes),
            FtpWire::Pattern(bytes) => {
                for b in bytes {
                    match *b {
                        FtpTagByte::Exact(x) => out.push(x),
                        FtpTagByte::Var { slot, .. } => out.push(d.get(slot).into_u8(slot)),
                    }
                }
            }
            FtpWire::Value { slot, kind, .. } => match kind {
                FtpValueKind::Text(encoding, source) => {
                    let text = d.get(slot).into_text(slot);
                    if let FtpTextSource::Profile(field) = source {
                        validate_field_checking(field.spec(), encoding, &text, false)?;
                    }
                    out.extend_from_slice(&encoding.encode(entry.name, &text)?);
                }
                FtpValueKind::U8(_) => out.push(d.get(slot).into_u8(slot)),
                FtpValueKind::U16(_) => out.extend_from_slice(&d.get(slot).into_u16(slot).to_le_bytes()),
                FtpValueKind::NetworkAddress => {
                    let [a, b, c, e] = d.get(slot).into_ipv4(slot);
                    out.extend_from_slice(&[e, c, b, a]);
                }
            },
        }
    }
    Ok(out)
}

// ── the decoder ──────────────────────────────────────────────────────────────────────────────

/// A tag pattern as the decoder's error shows it: `03|00 00 xx 00`.
fn describe_pattern(bytes: &[FtpTagByte]) -> String {
    bytes
        .iter()
        .map(|b| match b {
            FtpTagByte::Exact(x) => format!("{x:02x}"),
            FtpTagByte::Var { allowed: None, .. } => "xx".to_string(),
            FtpTagByte::Var { allowed: Some(set), .. } => set.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join("|"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// How many bytes of a value's wire form come before its content (the length prefix).
fn prefix_len(kind: &FtpValueKind) -> usize {
    match kind {
        FtpValueKind::Text(FtpEncoding::AsciiU32Len, _) => 4,
        FtpValueKind::Text(..) => 1,
        _ => 0,
    }
}

/// Decode `blob` by walking `layout`: any section that does not match, any truncated value and any
/// trailing byte is a [`PtpUsbError::FtpBlobMalformed`] naming the section and the offset.
pub(super) fn decode_with(layout: &[FtpLayoutEntry], blob: &[u8]) -> Result<(DecodedFtpProfile, FtpSecretSpans), PtpUsbError> {
    let mut r = FtpBlobReader { data: blob, pos: 0 };
    let mut decoded = DecodedFtpProfile::blank();
    let mut spans = Vec::new();
    for entry in layout {
        match entry.wire {
            FtpWire::Literal(expected) => r.expect(entry.name, expected)?,
            FtpWire::Pattern(bytes) => {
                let at = r.pos;
                let got = r.take(entry.name, bytes.len())?;
                let matches = bytes.iter().zip(got).all(|(b, g)| match b {
                    FtpTagByte::Exact(x) => x == g,
                    FtpTagByte::Var { allowed, .. } => allowed.is_none_or(|set| set.contains(g)),
                });
                if !matches {
                    r.pos = at;
                    return Err(r.bad(entry.name, format!("expected {}, found {}", describe_pattern(bytes), hex_bytes(got))));
                }
                for (b, g) in bytes.iter().zip(got) {
                    if let FtpTagByte::Var { slot, .. } = b {
                        decoded.set(*slot, FtpVal::U8(*g));
                    }
                }
            }
            FtpWire::Value { slot, kind, secret } => {
                let at = r.pos;
                let val = match kind {
                    FtpValueKind::Text(FtpEncoding::AsciiU32Len, _) => FtpVal::Text(r.utf8_u32_len(entry.name)?),
                    FtpValueKind::Text(..) => FtpVal::Text(r.ptp_string(entry.name)?.text),
                    FtpValueKind::U8(_) => FtpVal::U8(r.u8(entry.name)?),
                    FtpValueKind::U16(_) => FtpVal::U16(r.u16(entry.name)?),
                    FtpValueKind::NetworkAddress => {
                        let wire: [u8; 4] = r.take(entry.name, 4)?.try_into().expect("take(4) returns 4 bytes");
                        FtpVal::Ipv4([wire[3], wire[2], wire[1], wire[0]])
                    }
                };
                if secret {
                    spans.push(FtpSecretSpan { field: slot.key(), range: at + prefix_len(&kind)..r.pos });
                }
                decoded.set(slot, val);
            }
        }
    }
    if r.pos != blob.len() {
        return Err(r.bad("blob end", format!("{} unexpected byte(s) after the trailer", blob.len() - r.pos)));
    }
    Ok((decoded, FtpSecretSpans { spans }))
}

// ── the write gate ───────────────────────────────────────────────────────────────────────────

/// The [`FtpProfile`] that makes `layout`'s encoder write `d` back, when there is one: every value the
/// layout fixes (a constant, a tag byte it writes, the captured camera network) must be what `d`
/// holds, and every text the encoder writes as ASCII must be ASCII. No length or control-character
/// check: those are the encoder's, and it reports them.
pub(super) fn encodable_with(layout: &[FtpLayoutEntry], d: &DecodedFtpProfile) -> Option<FtpProfile> {
    let mut profile = FtpProfile {
        profile_name: String::new(),
        ssid_24ghz: String::new(),
        ssid_5ghz: String::new(),
        host: String::new(),
        port: 0,
        username: String::new(),
        password: String::new(),
    };
    let captured = FtpCameraNetwork::CAPTURED;
    for entry in layout {
        match entry.wire {
            FtpWire::Literal(_) => {}
            FtpWire::Pattern(bytes) => {
                for b in bytes {
                    if let FtpTagByte::Var { slot, written, .. } = *b {
                        if d.get(slot) != FtpVal::U8(written) {
                            return None;
                        }
                    }
                }
            }
            FtpWire::Value { slot, kind, .. } => {
                let got = d.get(slot);
                let fixed = match kind {
                    FtpValueKind::Text(encoding, FtpTextSource::Profile(field)) => {
                        let FtpVal::Text(text) = got else { return None };
                        if encoding.encoder_ascii_only() && !text.is_ascii() {
                            return None;
                        }
                        profile.set_text(field, text);
                        continue;
                    }
                    FtpValueKind::Text(_, FtpTextSource::Fixed(text)) => FtpVal::Text(text.to_string()),
                    FtpValueKind::U8(FtpU8Source::NetworkPrefix) => FtpVal::U8(captured.prefix_len),
                    FtpValueKind::U8(FtpU8Source::Fixed(v)) => FtpVal::U8(v),
                    FtpValueKind::U16(FtpU16Source::ProfilePort) => {
                        let FtpVal::U16(port) = got else { return None };
                        profile.port = port;
                        continue;
                    }
                    FtpValueKind::U16(FtpU16Source::Fixed(v)) => FtpVal::U16(v),
                    FtpValueKind::NetworkAddress => FtpVal::Ipv4(captured.ipv4),
                };
                if got != fixed {
                    return None;
                }
            }
        }
    }
    Some(profile)
}

#[cfg(test)]
mod tests;

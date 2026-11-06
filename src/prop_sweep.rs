//! A read-only sweep of device-property descriptors, and the comparison of two sweeps.
//!
//! Purpose: find properties nothing documents. The Nikon SDK's property list and the five properties the
//! extended snapshot reads do not include everything a body has (the AF activation custom setting, the
//! non-CPU lens data, the FTP option bits are not in either). A body answers `GetDevicePropDesc` (`0x1014`) for
//! the codes it has and `DevicePropNotSupported` for the rest, so asking for every code in a block lists the
//! block's properties with their type, get/set flag, default, current value and allowed values. Two sweeps of
//! the same body, with one menu setting changed between them, differ in exactly that setting's descriptor.
//!
//! Nothing here writes: besides opening and closing the session and reading DeviceInfo (the session's own, once),
//! the only operation sent is `GetDevicePropDesc`. See `docs/property-sweep.md`.
//!
//! Source of the data and what is done with each part (the data-capture rule):
//!
//! | the descriptor dataset (ISO 15740 §13.3.3) | disposition |
//! |---|---|
//! | the bytes as received | `raw_hex`, always: the source of truth |
//! | DevicePropertyCode | extracted; one that is not the code asked for is a fault, not silently accepted |
//! | DataType | extracted |
//! | GetSet | extracted |
//! | FactoryDefaultValue, CurrentValue | extracted: hex, plus an integer or text where the type has one. An array has neither: it is `raw-only` (its hex, which the diff compares). A string that does not decode keeps its hex and says why in `text_fault` |
//! | FormFlag, and the range or enumeration it announces | extracted |
//! | bytes after the last field | kept (`trailing_hex`) |
//! | a field the parser cannot read | the parse stops there; the default, the current value and the form flag are kept if read, and `fault` says where. A range is kept only whole; an enumeration keeps the values read before the fault |
//! | a reply too short to hold the five-byte head | no `desc`; `raw_hex` is kept, the diff says "undecodable", [`SweepCounts::bytes_only`] counts it at the sweep and [`SweepDump::undecoded`] counts it (with the faulted parses) when the dumps are compared |

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::extended::{RESP_DEVICE_PROP_NOT_SUPPORTED, hex_encode};
use crate::ptp_usb::{BulkTransport, PtpResponseCode, PtpUsbError, PtpUsbSession, decode_ptp_string_value, parse_propcode};

pub const SWEEP_FORMAT_VERSION: u32 = 1;


/// Consecutive replies that were not a clean answer (a wrong container, an out-of-order reply) after which the
/// sweep stops: a session in that state is not telling the truth about what comes next.
pub const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// The blocks swept when none is named: the standard properties (`0x5000..`) and Nikon's vendor block, which
/// holds the 0xD0xx strings and switches, 0xD100 shutter speed and the rest NX Field was seen to use.
pub fn default_ranges() -> Vec<CodeRange> {
    vec![CodeRange { lo: 0x5000, hi: 0x50FF }, CodeRange { lo: 0xD000, hi: 0xD4FF }]
}

// ── ranges ───────────────────────────────────────────────────────────────

/// An inclusive block of property codes. One representation for the distinction "which codes": the parser, the
/// merge, the coverage test and the serialised form all go through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CodeRange {
    pub lo: u16,
    pub hi: u16,
}

impl CodeRange {
    /// How many codes: `hi - lo + 1`, which is 65 536 for the whole space, so not a `u16`.
    pub fn count(self) -> u32 {
        u32::from(self.hi) - u32::from(self.lo) + 1
    }

    pub fn contains(self, code: u16) -> bool {
        self.lo <= code && code <= self.hi
    }

    /// `LO-HI` or one code, each in the CLI's number format (`0x` hex, else decimal). `LO` may equal `HI`; it may
    /// not exceed it, because a reversed range is far more likely a typo than an intent to ask for nothing.
    pub fn parse(text: &str) -> Result<CodeRange, String> {
        let one = |s: &str| -> Result<u16, String> {
            if s.trim().is_empty() {
                return Err(format!("{text:?} is not a range: an end is empty"));
            }
            parse_propcode(s)
        };
        match text.split_once('-') {
            None => {
                let c = one(text)?;
                Ok(CodeRange { lo: c, hi: c })
            }
            Some((a, b)) => {
                let (lo, hi) = (one(a)?, one(b)?);
                if lo > hi {
                    return Err(format!("{text:?} is reversed: the first code 0x{lo:04x} is above the second 0x{hi:04x}"));
                }
                Ok(CodeRange { lo, hi })
            }
        }
    }
}

impl fmt::Display for CodeRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:04x}-0x{:04x}", self.lo, self.hi)
    }
}

impl FromStr for CodeRange {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        CodeRange::parse(s)
    }
}

impl Serialize for CodeRange {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CodeRange {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        CodeRange::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// Sorted, with overlapping and touching ranges merged, so every code is asked once and `count`s add up. The
/// next code after `hi` is worked out in `u32`: `0xFFFF + 1` must not wrap into a range of its own.
pub fn normalize(ranges: &[CodeRange]) -> Vec<CodeRange> {
    let mut sorted = ranges.to_vec();
    sorted.sort();
    let mut out: Vec<CodeRange> = Vec::new();
    for r in sorted {
        match out.last_mut() {
            Some(last) if u32::from(r.lo) <= u32::from(last.hi) + 1 => last.hi = last.hi.max(r.hi),
            _ => out.push(r),
        }
    }
    out
}

fn key(code: u16) -> String {
    format!("0x{code:04x}")
}

// ── the descriptor ───────────────────────────────────────────────────────

/// A value of the property's own type, as the bytes and as much meaning as the type gives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropValue {
    pub hex: String,
    /// The integer, for a scalar integer type that fits an `i64` (a `u64` above `i64::MAX` and the 128-bit types
    /// are left to `hex`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub int: Option<i64>,
    /// The text, for a string type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Why a string type's bytes did not decode as text (`hex` still holds them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_fault: Option<String>,
}

impl PropValue {
    /// For a person: the text quoted, else the integer with its hex, else the bytes.
    pub fn show(&self) -> String {
        match (&self.text, self.int) {
            (Some(t), _) => format!("{t:?}"),
            (None, Some(i)) => format!("{i} (0x{:x})", i as u64 & mask_for(&self.hex)),
            (None, None) => format!("bytes {}", self.hex),
        }
    }
}

/// The bits an integer of this many hex digits has, so a negative `i8` shows as `0xff` and not `0xffff…`.
fn mask_for(hex: &str) -> u64 {
    let digits = hex.len();
    if digits >= 16 { u64::MAX } else { (1u64 << (digits * 4)) - 1 }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PropForm {
    /// FormFlag 0: any value of the type.
    None,
    Range { min: PropValue, max: PropValue, step: PropValue },
    Enum { values: Vec<PropValue> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropDesc {
    /// The code the dataset names; checked against the one asked for.
    pub code: u16,
    pub data_type: u16,
    /// 0 read-only, 1 read-write (the camera's answer is state-dependent).
    pub get_set: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<PropValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<PropValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<PropForm>,
    /// Bytes after the last field, kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trailing_hex: Option<String>,
    /// Where the parse stopped, when it did not reach the end of the dataset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault: Option<String>,
}

pub fn data_type_name(dt: u16) -> String {
    let scalar = |t: u16| match t {
        0x0001 => Some("i8"),
        0x0002 => Some("u8"),
        0x0003 => Some("i16"),
        0x0004 => Some("u16"),
        0x0005 => Some("i32"),
        0x0006 => Some("u32"),
        0x0007 => Some("i64"),
        0x0008 => Some("u64"),
        0x0009 => Some("i128"),
        0x000A => Some("u128"),
        _ => None,
    };
    match dt {
        0xFFFF => "string".to_string(),
        t if t & 0xFFF0 == 0x4000 => scalar(t & 0x000F).map(|s| format!("array of {s}")).unwrap_or_else(|| format!("type 0x{t:04x}")),
        t => scalar(t).map(str::to_string).unwrap_or_else(|| format!("type 0x{t:04x}")),
    }
}

pub fn get_set_name(gs: u8) -> String {
    match gs {
        0 => "read-only".to_string(),
        1 => "read-write".to_string(),
        other => format!("get/set 0x{other:02x}"),
    }
}

impl PropDesc {
    /// One line for a person.
    pub fn summary(&self) -> String {
        let mut s = format!("{}, {}", data_type_name(self.data_type), get_set_name(self.get_set));
        if let Some(c) = &self.current {
            s.push_str(&format!(", current {}", c.show()));
        }
        if let Some(f) = &self.form {
            s.push_str(&format!(", {}", form_text(f)));
        }
        if let Some(why) = &self.fault {
            s.push_str(&format!(" [undecoded past this point: {why}]"));
        }
        s
    }
}

fn form_text(f: &PropForm) -> String {
    match f {
        PropForm::None => "any value".to_string(),
        PropForm::Range { min, max, step } => format!("range {} to {} step {}", min.show(), max.show(), step.show()),
        PropForm::Enum { values } => format!("one of [{}]", values.iter().map(PropValue::show).collect::<Vec<_>>().join(", ")),
    }
}

/// A bounds-checked cursor: every read says what it was for, and a short read is a fault with its offset.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.data.len());
        match end {
            Some(end) => {
                let s = &self.data[self.pos..end];
                self.pos = end;
                Ok(s)
            }
            None => Err(format!("{what}: needs {n} byte(s) at offset {}, {} left", self.pos, self.data.len().saturating_sub(self.pos))),
        }
    }

    fn u8(&mut self, what: &str) -> Result<u8, String> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &str) -> Result<u16, String> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn left(&self) -> usize {
        self.data.len() - self.pos
    }
}

/// `(bytes, signed)` of a scalar integer type.
fn scalar_width(dt: u16) -> Option<(usize, bool)> {
    match dt {
        0x0001 => Some((1, true)),
        0x0002 => Some((1, false)),
        0x0003 => Some((2, true)),
        0x0004 => Some((2, false)),
        0x0005 => Some((4, true)),
        0x0006 => Some((4, false)),
        0x0007 => Some((8, true)),
        0x0008 => Some((8, false)),
        0x0009 => Some((16, true)),
        0x000A => Some((16, false)),
        _ => None,
    }
}

fn int_of(bytes: &[u8], signed: bool) -> Option<i64> {
    match (bytes.len(), signed) {
        (1, true) => Some(i64::from(bytes[0] as i8)),
        (1, false) => Some(i64::from(bytes[0])),
        (2, true) => Some(i64::from(i16::from_le_bytes([bytes[0], bytes[1]]))),
        (2, false) => Some(i64::from(u16::from_le_bytes([bytes[0], bytes[1]]))),
        (4, true) => Some(i64::from(i32::from_le_bytes(bytes.try_into().ok()?))),
        (4, false) => Some(i64::from(u32::from_le_bytes(bytes.try_into().ok()?))),
        (8, true) => Some(i64::from_le_bytes(bytes.try_into().ok()?)),
        (8, false) => i64::try_from(u64::from_le_bytes(bytes.try_into().ok()?)).ok(),
        _ => None,
    }
}

/// One value of type `dt`. An array is `u32 count` then the elements; a string is `u8 characters` then that many
/// UTF-16 units; both are kept whole as bytes.
fn read_value(r: &mut Reader<'_>, dt: u16, what: &str) -> Result<PropValue, String> {
    if let Some((w, signed)) = scalar_width(dt) {
        let b = r.take(w, what)?;
        return Ok(PropValue { hex: hex_encode(b), int: int_of(b, signed), text: None, text_fault: None });
    }
    if dt == 0xFFFF {
        let start = r.pos;
        let chars = usize::from(r.u8(what)?);
        let body = r.take(if chars == 0 { 0 } else { chars * 2 }, what)?;
        let _ = body;
        let whole = &r.data[start..r.pos];
        let (text, text_fault) = match decode_ptp_string_value(whole) {
            Ok(t) => (Some(t), None),
            Err(why) => (None, Some(why)),
        };
        return Ok(PropValue { hex: hex_encode(whole), int: None, text, text_fault });
    }
    if dt & 0xFFF0 == 0x4000 {
        let Some((w, _)) = scalar_width(dt & 0x000F) else {
            return Err(format!("{what}: array of an unknown element type 0x{:04x}", dt & 0x000F));
        };
        let start = r.pos;
        let count = r.take(4, what).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))? as usize;
        // The count is the camera's; the bytes must be there before anything is made of it.
        if count.checked_mul(w).is_none_or(|n| n > r.left()) {
            return Err(format!("{what}: an array of {count} element(s) of {w} byte(s) does not fit the {} byte(s) left", r.left()));
        }
        r.take(count * w, what)?;
        return Ok(PropValue { hex: hex_encode(&r.data[start..r.pos]), int: None, text: None, text_fault: None });
    }
    Err(format!("{what}: data type 0x{dt:04x} is not one this parser knows the width of"))
}

/// Parse a `GetDevicePropDesc` reply. `asked` is the code that was requested. Never fails: a dataset that cannot
/// be read to the end returns what was read, with `fault` saying where it stopped. `None` only when not even the
/// five-byte head (code, type, get/set) is there.
pub fn parse_prop_desc(data: &[u8], asked: u16) -> Option<PropDesc> {
    let mut r = Reader { data, pos: 0 };
    let code = r.u16("property code").ok()?;
    let data_type = r.u16("data type").ok()?;
    let get_set = r.u8("get/set flag").ok()?;
    let mut d = PropDesc { code, data_type, get_set, default: None, current: None, form: None, trailing_hex: None, fault: None };
    if code != asked {
        d.fault = Some(format!("the dataset is for property 0x{code:04x}, not the one asked for (0x{asked:04x})"));
        return Some(d);
    }
    if let Err(why) = read_body(&mut r, &mut d) {
        d.fault = Some(why);
        return Some(d);
    }
    if r.left() > 0 {
        d.trailing_hex = Some(hex_encode(&r.data[r.pos..]));
    }
    Some(d)
}

fn read_body(r: &mut Reader<'_>, d: &mut PropDesc) -> Result<(), String> {
    d.default = Some(read_value(r, d.data_type, "factory default")?);
    d.current = Some(read_value(r, d.data_type, "current value")?);
    let flag = r.u8("form flag")?;
    d.form = Some(match flag {
        0 => PropForm::None,
        1 => {
            let min = read_value(r, d.data_type, "range minimum")?;
            let max = read_value(r, d.data_type, "range maximum")?;
            let step = read_value(r, d.data_type, "range step")?;
            PropForm::Range { min, max, step }
        }
        2 => {
            let count = usize::from(r.u16("enumeration count")?);
            // Every value takes at least a byte, so a count past what is left is the camera's number, not ours to allocate.
            if count > r.left() {
                return Err(format!("enumeration count: {count} value(s) announced but {} byte(s) left", r.left()));
            }
            let mut values = Vec::with_capacity(count);
            for i in 0..count {
                match read_value(r, d.data_type, &format!("enumeration value {i}")) {
                    Ok(v) => values.push(v),
                    Err(why) => {
                        // Keep what was read: the fault says where the rest stopped.
                        d.form = Some(PropForm::Enum { values });
                        return Err(why);
                    }
                }
            }
            PropForm::Enum { values }
        }
        other => return Err(format!("form flag: 0x{other:02x} is neither none (0), range (1) nor enumeration (2)")),
    });
    Ok(())
}

// ── the dump ─────────────────────────────────────────────────────────────

/// Where an aborted sweep stopped.
#[derive(Clone, Copy)]
enum Stop {
    Never,
    At(u16),
    /// The file names a stop that is not a property code.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SweepEntry {
    /// The body answered with a descriptor.
    Present {
        raw_hex: String,
        /// The code is in the DeviceInfo list of supported properties. (A Z 9 serves many it does not list.)
        listed_in_device_info: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        desc: Option<PropDesc>,
    },
    /// The body answered with a response code other than `DevicePropNotSupported`.
    Refused { code: u16, name: String },
    /// No usable answer: the exchange itself went wrong.
    Failed { tag: String, detail: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepCounts {
    pub asked: u32,
    pub present: u32,
    /// Of `present`: replies whose descriptor could not be parsed at all (too short to hold the five-byte head), kept
    /// only as `raw_hex`. Not a disposition of its own (`asked` is still `present + not_supported + refused + failed`);
    /// [`SweepDump::undecoded`] also counts the replies whose parse stopped at a fault. A file written before this was
    /// counted reads it as 0.
    #[serde(default)]
    pub bytes_only: u32,
    pub not_supported: u32,
    pub refused: u32,
    pub failed: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aborted {
    /// The code being asked when the sweep stopped; it and every later code are not covered.
    pub at: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepBody {
    pub model: String,
    pub usb_serial: String,
    /// The DeviceInfo `DeviceVersion` string.
    pub firmware: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepDump {
    pub format_version: u32,
    pub captured_at: String,
    /// What state the body was in, as the person named it ("af-on-only").
    pub label: Option<String>,
    pub body: SweepBody,
    /// The blocks asked, merged. A code with no entry in one of them was answered `DevicePropNotSupported`.
    pub ranges: Vec<CodeRange>,
    pub counts: SweepCounts,
    pub aborted: Option<Aborted>,
    /// Every code DeviceInfo lists, whether or not it is in `ranges`.
    pub device_info_props: Vec<String>,
    pub entries: BTreeMap<String, SweepEntry>,
}

#[derive(Debug, Error)]
pub enum SweepError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported sweep format_version {found}; this build reads {expected}")]
    UnsupportedVersion { found: u32, expected: u32 },
}

impl SweepDump {
    /// Where an aborted sweep stopped, parsed once.
    fn stop(&self) -> Stop {
        match &self.aborted {
            None => Stop::Never,
            Some(a) => parse_propcode(&a.at).map_or(Stop::Unknown, Stop::At),
        }
    }

    /// Whether this dump says anything about `code`: asked, and (for an aborted sweep) before the code it stopped at.
    /// A dump that stopped somewhere that cannot be read says nothing about any code (it fails closed; `from_json`
    /// refuses such a file, so this is only reachable for a dump built in memory).
    pub fn covers(&self, code: u16) -> bool {
        self.coverage()(code)
    }

    /// [`Self::covers`] as a function, with the stop point parsed once (a diff asks for every code of every block).
    fn coverage(&self) -> impl Fn(u16) -> bool + '_ {
        let stop = self.stop();
        move |code| {
            self.ranges.iter().any(|r| r.contains(code))
                && match stop {
                    Stop::Never => true,
                    Stop::At(at) => code < at,
                    Stop::Unknown => false,
                }
        }
    }

    /// Entries whose answer is kept only as bytes: a present entry with no decoded descriptor, or one whose parse
    /// stopped at a fault. A diff reads these by their bytes alone.
    pub fn undecoded(&self) -> usize {
        self.entries.values().filter(|e| matches!(e, SweepEntry::Present { desc: None, .. } | SweepEntry::Present { desc: Some(PropDesc { fault: Some(_), .. }), .. })).count()
    }

    pub fn from_json(text: &str) -> Result<SweepDump, SweepError> {
        let dump: SweepDump = serde_json::from_str(text)?;
        if dump.format_version != SWEEP_FORMAT_VERSION {
            return Err(SweepError::UnsupportedVersion { found: dump.format_version, expected: SWEEP_FORMAT_VERSION });
        }
        // The sweep writes every key and the stop point with `key(code)`; anything else is a hand-edited or damaged
        // file, and a diff of it would leave entries out or cover codes it should not.
        let bad = |what: &str, text: &str| SweepError::Json(<serde_json::Error as serde::de::Error>::custom(format!("{what} {text:?} is not a property code written as 0xNNNN (lower-case hex)")));
        for k in dump.entries.keys() {
            if !parse_propcode(k).is_ok_and(|c| key(c) == *k) {
                return Err(bad("entry key", k));
            }
        }
        if let Some(a) = &dump.aborted {
            if !parse_propcode(&a.at).is_ok_and(|c| key(c) == a.at) {
                return Err(bad("aborted.at", &a.at));
            }
        }
        Ok(dump)
    }

    pub fn load(path: &Path) -> Result<SweepDump, SweepError> {
        SweepDump::from_json(&std::fs::read_to_string(path)?)
    }

    /// The file name: body, state label and time, none of which may carry a separator.
    pub fn suggested_filename(&self) -> String {
        let model = crate::snapshot::sanitize_filename_component(&crate::firmware::model_slug(&self.body.model));
        let serial = if self.body.usb_serial.is_empty() { "unknown".to_string() } else { crate::snapshot::sanitize_filename_component(&self.body.usb_serial) };
        let label = crate::snapshot::sanitize_filename_component(self.label.as_deref().unwrap_or("sweep"));
        let ts = self.captured_at.replace(':', "").replace('-', "");
        format!("{model}_{serial}_{label}_{ts}.json")
    }

    /// Write to `dir` under [`Self::suggested_filename`], creating `dir`. Never replaces a file: a name already
    /// taken is an error (two sweeps in one second with one label). The file appears whole or not at all (staged in a
    /// temporary and linked into place, the way a snapshot capture is written), so a crash cannot leave half a dump
    /// under the real name, and a dump that cannot be serialized creates nothing.
    pub fn save_new(&self, dir: &Path) -> Result<PathBuf, SweepError> {
        let text = serde_json::to_string_pretty(self)?;
        std::fs::create_dir_all(dir)?;
        let path = dir.join(self.suggested_filename());
        crate::snapshot::write_new_whole(&path, text.as_bytes())?;
        Ok(path)
    }
}

// ── the sweep ────────────────────────────────────────────────────────────

/// What the sweep needs from a raw-PTP session. A trait so it runs against a scripted body in tests.
pub trait SweepSource {
    fn identity(&mut self) -> Result<SweepIdentity, PtpUsbError>;
    /// The descriptor bytes for `code`, or the camera's refusal.
    fn prop_desc(&mut self, code: u16) -> Result<Vec<u8>, PtpUsbError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepIdentity {
    pub model: String,
    pub usb_serial: String,
    pub firmware: String,
    /// DeviceInfo's list of supported properties.
    pub listed: Vec<u16>,
}

impl<T: BulkTransport> SweepSource for PtpUsbSession<T> {
    fn identity(&mut self) -> Result<SweepIdentity, PtpUsbError> {
        let r = self.read_device_info_report()?;
        Ok(SweepIdentity {
            model: r.usb_model,
            usb_serial: r.usb_serial,
            firmware: r.dataset.device_version.text,
            listed: r.dataset.device_properties_supported,
        })
    }

    fn prop_desc(&mut self, code: u16) -> Result<Vec<u8>, PtpUsbError> {
        self.get_device_prop_desc(code).map(|r| r.data)
    }
}

#[derive(Debug, Clone)]
pub struct SweepOptions {
    pub ranges: Vec<CodeRange>,
    pub label: Option<String>,
    pub captured_at: String,
}

/// One step, for a progress line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepProgress {
    pub code: u16,
    /// Codes asked before this one.
    pub done: u32,
    pub total: u32,
}

enum Answer {
    Desc(Vec<u8>),
    NotSupported,
    Refused(PtpResponseCode),
    /// The exchange went wrong. `fatal`: the session cannot be trusted for another (a timeout can leave the pipe
    /// stalled; a poisoned session refuses everything).
    Failed { tag: &'static str, detail: String, fatal: bool },
}

fn classify(r: Result<Vec<u8>, PtpUsbError>) -> Answer {
    match r {
        Ok(data) => Answer::Desc(data),
        Err(PtpUsbError::OperationFailed { code, .. }) if code.0 == RESP_DEVICE_PROP_NOT_SUPPORTED => Answer::NotSupported,
        Err(PtpUsbError::OperationFailed { code, .. }) => Answer::Refused(code),
        Err(e) => {
            let fatal = matches!(e, PtpUsbError::TransactionTimedOut(_) | PtpUsbError::SessionPoisoned { .. } | PtpUsbError::Usb(_));
            Answer::Failed { tag: e.tag(), detail: e.to_string(), fatal }
        }
    }
}

/// Ask every code in `opts.ranges` (merged) and record what the body says. A body that stops answering ends the
/// sweep, keeping what was recorded: see [`MAX_CONSECUTIVE_FAILURES`] and `Answer::Failed`.
pub fn sweep<S: SweepSource + ?Sized>(
    src: &mut S,
    opts: &SweepOptions,
    progress: &mut dyn FnMut(SweepProgress),
) -> Result<SweepDump, PtpUsbError> {
    let ident = src.identity()?;
    let ranges = normalize(&opts.ranges);
    let total: u32 = ranges.iter().map(|r| r.count()).sum();
    let listed: BTreeSet<u16> = ident.listed.iter().copied().collect();
    let mut entries = BTreeMap::new();
    let mut counts = SweepCounts::default();
    let mut aborted = None;
    let mut consecutive_failures = 0u32;

    'blocks: for r in &ranges {
        for wide in u32::from(r.lo)..=u32::from(r.hi) {
            let code = wide as u16;
            progress(SweepProgress { code, done: counts.asked, total });
            counts.asked += 1;
            match classify(src.prop_desc(code)) {
                Answer::Desc(data) => {
                    consecutive_failures = 0;
                    counts.present += 1;
                    let desc = parse_prop_desc(&data, code);
                    if desc.is_none() {
                        counts.bytes_only += 1;
                    }
                    entries.insert(key(code), SweepEntry::Present { raw_hex: hex_encode(&data), listed_in_device_info: listed.contains(&code), desc });
                }
                Answer::NotSupported => {
                    consecutive_failures = 0;
                    counts.not_supported += 1;
                }
                Answer::Refused(c) => {
                    consecutive_failures = 0;
                    counts.refused += 1;
                    entries.insert(key(code), SweepEntry::Refused { code: c.0, name: c.to_string() });
                }
                Answer::Failed { tag, detail, fatal } => {
                    counts.failed += 1;
                    consecutive_failures += 1;
                    entries.insert(key(code), SweepEntry::Failed { tag: tag.to_string(), detail: detail.clone() });
                    if fatal || consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        let why = if fatal { format!("{detail} (the session cannot be trusted after this)") } else { format!("{consecutive_failures} replies in a row were not usable; the last: {detail}") };
                        aborted = Some(Aborted { at: key(code), reason: why });
                        break 'blocks;
                    }
                }
            }
        }
    }

    Ok(SweepDump {
        format_version: SWEEP_FORMAT_VERSION,
        captured_at: opts.captured_at.clone(),
        label: opts.label.clone(),
        body: SweepBody { model: ident.model, usb_serial: ident.usb_serial, firmware: ident.firmware },
        ranges,
        counts,
        aborted,
        device_info_props: ident.listed.iter().map(|c| key(*c)).collect(),
        entries,
    })
}

// ── comparing two sweeps ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// Not supported in the first, a descriptor in the second.
    Appeared,
    /// A descriptor in the first, not supported in the second.
    Vanished,
    /// Both have an answer and they differ.
    Changed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SweepChange {
    pub code: String,
    pub kind: ChangeKind,
    /// For a person; nothing branches on these.
    pub details: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SweepDiff {
    /// Codes both dumps cover.
    pub compared: u32,
    /// Codes only one of them covers (different blocks, or one stopped early).
    pub not_compared: u32,
    pub changes: Vec<SweepChange>,
    /// Changes that also appear between the first dump and the noise dump: things that move by themselves
    /// (a clock, a counter, a battery level). Kept out of `changes` unless asked for.
    pub volatile: Vec<SweepChange>,
    /// Entry keys that are not property codes, left out of the comparison. Empty for any dump `from_json` accepts.
    pub bad_keys: Vec<String>,
}

fn answer_text(e: Option<&SweepEntry>) -> String {
    match e {
        None => "not supported".to_string(),
        Some(SweepEntry::Present { desc: Some(d), .. }) => format!("present ({})", d.summary()),
        Some(SweepEntry::Present { desc: None, raw_hex, .. }) => format!("present (undecodable, bytes {raw_hex})"),
        Some(SweepEntry::Refused { name, .. }) => format!("refused: {name}"),
        Some(SweepEntry::Failed { tag, .. }) => format!("no usable answer ({tag})"),
    }
}

fn same_answer_class(a: &SweepEntry, b: &SweepEntry) -> bool {
    matches!(
        (a, b),
        (SweepEntry::Present { .. }, SweepEntry::Present { .. })
            | (SweepEntry::Refused { .. }, SweepEntry::Refused { .. })
            | (SweepEntry::Failed { .. }, SweepEntry::Failed { .. })
    )
}

fn value_change(label: &str, a: &Option<PropValue>, b: &Option<PropValue>, out: &mut Vec<String>) {
    if a != b {
        let show = |v: &Option<PropValue>| v.as_ref().map(PropValue::show).unwrap_or_else(|| "(unread)".to_string());
        out.push(format!("{label}: {} -> {}", show(a), show(b)));
    }
}

/// How two answers for one code differ, or `None` if they do not.
fn entry_change(a: Option<&SweepEntry>, b: Option<&SweepEntry>) -> Option<(ChangeKind, Vec<String>)> {
    match (a, b) {
        (None, None) => None,
        (None, Some(SweepEntry::Present { .. })) => Some((ChangeKind::Appeared, vec![answer_text(b)])),
        (Some(SweepEntry::Present { .. }), None) => Some((ChangeKind::Vanished, vec![format!("was {}", answer_text(a))])),
        (Some(x), Some(y)) if same_answer_class(x, y) => match (x, y) {
            (
                SweepEntry::Present { raw_hex: ra, listed_in_device_info: la, desc: da },
                SweepEntry::Present { raw_hex: rb, listed_in_device_info: lb, desc: db },
            ) => {
                let mut details = Vec::new();
                if let (Some(da), Some(db)) = (da, db) {
                    if da.data_type != db.data_type {
                        details.push(format!("data type: {} -> {}", data_type_name(da.data_type), data_type_name(db.data_type)));
                    }
                    if da.get_set != db.get_set {
                        details.push(format!("access: {} -> {}", get_set_name(da.get_set), get_set_name(db.get_set)));
                    }
                    value_change("default", &da.default, &db.default, &mut details);
                    value_change("current", &da.current, &db.current, &mut details);
                    if da.form != db.form {
                        let show = |d: &PropDesc| d.form.as_ref().map(form_text).unwrap_or_else(|| "(unread)".to_string());
                        details.push(format!("allowed values: {} -> {}", show(da), show(db)));
                    }
                }
                if la != lb {
                    details.push(format!("listed in DeviceInfo: {la} -> {lb}"));
                }
                if details.is_empty() && ra != rb {
                    details.push("the descriptor bytes differ (in a part this parser keeps only as bytes)".to_string());
                }
                (!details.is_empty()).then_some((ChangeKind::Changed, details))
            }
            (SweepEntry::Refused { code: ca, .. }, SweepEntry::Refused { code: cb, name }) => {
                (ca != cb).then(|| (ChangeKind::Changed, vec![format!("refusal: 0x{ca:04x} -> {name}")]))
            }
            (SweepEntry::Failed { tag: ta, .. }, SweepEntry::Failed { tag: tb, .. }) => {
                (ta != tb).then(|| (ChangeKind::Changed, vec![format!("failure: {ta} -> {tb}")]))
            }
            _ => None,
        },
        (x, y) => Some((ChangeKind::Changed, vec![format!("answer: {} -> {}", answer_text(x), answer_text(y))])),
    }
}

/// What differs between two dumps over the codes both cover: how many codes both cover, how many only `a` does, the
/// changes, and the entry keys that are not property codes (left out, and counted by the caller's report).
fn changes_between(a: &SweepDump, b: &SweepDump) -> (u32, u32, Vec<SweepChange>, Vec<String>) {
    let (cov_a, cov_b) = (a.coverage(), b.coverage());
    let in_a: u32 = a.ranges.iter().flat_map(|r| u32::from(r.lo)..=u32::from(r.hi)).filter(|c| cov_a(*c as u16)).count() as u32;
    let both: u32 = a.ranges.iter().flat_map(|r| u32::from(r.lo)..=u32::from(r.hi)).filter(|c| cov_a(*c as u16) && cov_b(*c as u16)).count() as u32;
    let mut codes: BTreeSet<&String> = a.entries.keys().collect();
    codes.extend(b.entries.keys());
    let mut out = Vec::new();
    let mut bad_keys = Vec::new();
    for k in codes {
        let Ok(code) = parse_propcode(k) else {
            bad_keys.push(k.clone());
            continue;
        };
        if !(cov_a(code) && cov_b(code)) {
            continue;
        }
        if let Some((kind, details)) = entry_change(a.entries.get(k), b.entries.get(k)) {
            out.push(SweepChange { code: k.clone(), kind, details });
        }
    }
    (both, in_a - both, out, bad_keys)
}

/// Compare two dumps over the codes both cover. With `noise` (a third sweep of the same body in the same state as
/// `a`), changes that `a` and `noise` also show are volatile: set aside, or kept in `changes` if `keep_volatile`.
pub fn diff_sweeps(a: &SweepDump, b: &SweepDump, noise: Option<&SweepDump>, keep_volatile: bool) -> SweepDiff {
    let (compared, only_a, mut changes, bad_keys) = changes_between(a, b);
    let (cov_a, cov_b) = (a.coverage(), b.coverage());
    let only_b: u32 = b.ranges.iter().flat_map(|r| u32::from(r.lo)..=u32::from(r.hi)).filter(|c| cov_b(*c as u16) && !cov_a(*c as u16)).count() as u32;
    let mut volatile = Vec::new();
    if let Some(n) = noise {
        let moving: BTreeSet<String> = changes_between(a, n).2.into_iter().map(|c| c.code).collect();
        if !keep_volatile {
            let (stable, moved): (Vec<_>, Vec<_>) = changes.into_iter().partition(|c| !moving.contains(&c.code));
            changes = stable;
            volatile = moved;
        }
    }
    SweepDiff { compared, not_compared: only_a + only_b, changes, volatile, bad_keys }
}

impl SweepDiff {
    pub fn to_text(&self, a: &SweepDump, b: &SweepDump) -> String {
        let name = |d: &SweepDump| {
            format!("{} {} fw {} at {}{}", d.body.model, d.body.usb_serial, d.body.firmware, d.captured_at, d.label.as_ref().map(|l| format!(", {l:?}")).unwrap_or_default())
        };
        let mut s = format!("A: {}\nB: {}\n", name(a), name(b));
        if a.body.usb_serial != b.body.usb_serial || a.body.model != b.body.model {
            s.push_str("note: these are two different bodies; differences may be the bodies, not a setting\n");
        } else if a.body.firmware != b.body.firmware {
            s.push_str("note: the firmware differs between these dumps\n");
        }
        for (label, d) in [("A", a), ("B", b)] {
            if let Some(ab) = &d.aborted {
                s.push_str(&format!("note: dump {label} stopped at {}: {}\n", ab.at, ab.reason));
            }
        }
        for (label, d) in [("A", a), ("B", b)] {
            let n = d.undecoded();
            if n > 0 {
                s.push_str(&format!("note: dump {label} holds {n} answer(s) kept only as bytes (not fully decoded); they are compared by their bytes\n"));
            }
        }
        if !self.bad_keys.is_empty() {
            s.push_str(&format!("note: {} entr(y/ies) left out because the key is not a property code: {}\n", self.bad_keys.len(), self.bad_keys.join(", ")));
        }
        s.push_str(&format!("compared {} code(s) both cover", self.compared));
        if self.not_compared > 0 {
            s.push_str(&format!("; {} covered by only one", self.not_compared));
        }
        s.push_str(&format!("; {} differ", self.changes.len()));
        if !self.volatile.is_empty() {
            s.push_str(&format!("; {} more also move on their own (noise) and are set aside", self.volatile.len()));
        }
        s.push('\n');
        let line = |c: &SweepChange| {
            let kind = match c.kind {
                ChangeKind::Appeared => "APPEARED",
                ChangeKind::Vanished => "VANISHED",
                ChangeKind::Changed => "CHANGED",
            };
            format!("{}  {kind}\n{}", c.code, c.details.iter().map(|d| format!("    {d}\n")).collect::<String>())
        };
        for c in &self.changes {
            s.push_str(&line(c));
        }
        if !self.volatile.is_empty() {
            s.push_str("set aside as noise:\n");
            for c in &self.volatile {
                s.push_str(&format!("  {}\n", line(c).trim_end().replace('\n', "\n  ")));
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    // ── ranges ──

    fn r(lo: u16, hi: u16) -> CodeRange {
        CodeRange { lo, hi }
    }

    #[test]
    fn a_range_is_one_code_or_lo_to_hi_inclusive_in_the_clis_number_format() {
        assert_eq!(CodeRange::parse("0xD000-0xD4FF").unwrap(), r(0xD000, 0xD4FF));
        assert_eq!(CodeRange::parse("0xD100").unwrap(), r(0xD100, 0xD100), "a single code is a range of one");
        assert_eq!(CodeRange::parse("0xD100-0xD100").unwrap(), r(0xD100, 0xD100), "lo == hi written out is allowed: only lo > hi is reversed");
        assert_eq!(CodeRange::parse("53248-54527").unwrap(), r(0xD000, 0xD4FF), "a bare number is decimal");
        assert_eq!(CodeRange::parse(" 0xd000 - 0xd0ff ").unwrap(), r(0xD000, 0xD0FF), "space around the ends is not part of the number");
        assert_eq!(CodeRange::parse("0xFFFF").unwrap().count(), 1, "the top code is a range of one, not an overflow");
        assert_eq!(CodeRange::parse("0-0xFFFF").unwrap().count(), 65_536, "the whole space does not fit a u16");
        assert_eq!(r(0xD000, 0xD000).count(), 1, "lo == hi: one code (the boundary)");
        assert_eq!(r(0xD000, 0xD001).count(), 2);
    }

    #[test]
    fn a_range_that_is_reversed_empty_or_out_of_range_is_refused_with_the_reason() {
        for bad in ["", " ", "-", "0xD000-", "-0xD000", "0xD100-0xD000", "0xD000-0xD100-0xD200", "0x10000", "0xD000-0x10000", "D000", "0xZZ", "1.5"] {
            assert!(CodeRange::parse(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(CodeRange::parse("0xD100-0xD000").unwrap_err().contains("reversed"));
        // One below the reversal boundary is fine.
        assert!(CodeRange::parse("0xD0FF-0xD100").is_ok());
    }

    #[test]
    fn blocks_are_merged_when_they_overlap_or_touch_and_not_when_a_code_lies_between() {
        assert_eq!(normalize(&[r(0xD100, 0xD1FF), r(0xD000, 0xD0FF)]), [r(0xD000, 0xD1FF)], "touching (0xD0FF, 0xD100) merges, in any order");
        assert_eq!(normalize(&[r(0xD000, 0xD0FE), r(0xD100, 0xD1FF)]), [r(0xD000, 0xD0FE), r(0xD100, 0xD1FF)], "one code between: apart");
        assert_eq!(normalize(&[r(0xD000, 0xD200), r(0xD100, 0xD150)]), [r(0xD000, 0xD200)], "contained");
        assert_eq!(normalize(&[r(5, 5), r(5, 5)]), [r(5, 5)], "a duplicate is asked once");
        assert_eq!(normalize(&[r(0xFFF0, 0xFFFF), r(0, 3)]), [r(0, 3), r(0xFFF0, 0xFFFF)], "the top of the space does not wrap into the bottom");
        assert!(normalize(&[]).is_empty());
    }

    #[test]
    fn a_range_serialises_as_text_and_reads_back() {
        let j = serde_json::to_string(&r(0xD000, 0xD4FF)).unwrap();
        assert_eq!(j, "\"0xd000-0xd4ff\"");
        assert_eq!(serde_json::from_str::<CodeRange>(&j).unwrap(), r(0xD000, 0xD4FF));
        assert!(serde_json::from_str::<CodeRange>("\"0xd100-0xd000\"").is_err(), "a reversed range in a file is refused too");
    }

    // ── the descriptor ──

    fn desc_bytes(code: u16, dt: u16, gs: u8, default: &[u8], current: &[u8], form: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(code.to_le_bytes());
        v.extend(dt.to_le_bytes());
        v.push(gs);
        v.extend(default);
        v.extend(current);
        v.extend(form);
        v
    }

    /// u8 enumeration [0, 1, 2], default 0, current 1, read-write.
    fn enum_u8(code: u16) -> Vec<u8> {
        desc_bytes(code, 0x0002, 1, &[0], &[1], &[2, 3, 0, 0, 1, 2])
    }

    #[test]
    fn an_enumeration_descriptor_is_read_whole() {
        let d = parse_prop_desc(&enum_u8(0xD1A0), 0xD1A0).unwrap();
        assert_eq!((d.code, d.data_type, d.get_set), (0xD1A0, 2, 1));
        assert_eq!(d.default.as_ref().unwrap().int, Some(0));
        assert_eq!(d.current.as_ref().unwrap().int, Some(1));
        let Some(PropForm::Enum { values }) = &d.form else { panic!("an enumeration: {:?}", d.form) };
        assert_eq!(values.iter().map(|v| v.int.unwrap()).collect::<Vec<_>>(), [0, 1, 2]);
        assert!(d.trailing_hex.is_none() && d.fault.is_none());
        assert_eq!(d.summary(), "u8, read-write, current 1 (0x1), one of [0 (0x0), 1 (0x1), 2 (0x2)]");
    }

    #[test]
    fn a_range_a_signed_value_a_string_and_an_array_are_read() {
        // i16 range -5..5 step 1, default 0, current -2
        let min = (-5i16).to_le_bytes();
        let form: Vec<u8> = [&[1u8][..], &min, &5i16.to_le_bytes(), &1i16.to_le_bytes()].concat();
        let d = parse_prop_desc(&desc_bytes(0x5001, 0x0003, 0, &0i16.to_le_bytes(), &(-2i16).to_le_bytes(), &form), 0x5001).unwrap();
        assert_eq!(d.current.as_ref().unwrap().int, Some(-2));
        assert_eq!(d.current.as_ref().unwrap().show(), "-2 (0xfffe)");
        let Some(PropForm::Range { min, max, step }) = &d.form else { panic!() };
        assert_eq!((min.int, max.int, step.int), (Some(-5), Some(5), Some(1)));
        // i8 -1 shows as 0xff, not 0xffff...
        let i8v = parse_prop_desc(&desc_bytes(1, 0x0001, 1, &[0], &[0xFF], &[0]), 1).unwrap();
        assert_eq!(i8v.current.unwrap().show(), "-1 (0xff)");
        // string "ab" (UTF-16, NUL included in the count), current empty
        let ab = [3u8, b'a', 0, b'b', 0, 0, 0];
        let s = parse_prop_desc(&desc_bytes(0xD072, 0xFFFF, 1, &[0], &ab, &[0]), 0xD072).unwrap();
        assert_eq!(s.current.as_ref().unwrap().text.as_deref(), Some("ab"));
        assert_eq!(s.default.as_ref().unwrap().text.as_deref(), Some(""), "a zero-length string is the empty string, not missing");
        // array of u16: count 2
        let arr = [2u8, 0, 0, 0, 1, 0, 2, 0];
        let a = parse_prop_desc(&desc_bytes(0x5002, 0x4004, 0, &arr, &arr, &[0]), 0x5002).unwrap();
        assert_eq!(a.current.as_ref().unwrap().hex, "0200000001000200");
        assert!(a.current.as_ref().unwrap().int.is_none() && a.fault.is_none());
    }

    #[test]
    fn integer_width_boundaries_keep_what_an_i64_cannot_hold_as_bytes() {
        let u64max = u64::MAX.to_le_bytes();
        let d = parse_prop_desc(&desc_bytes(1, 0x0008, 1, &u64max, &i64::MAX.to_le_bytes(), &[0]), 1).unwrap();
        assert_eq!(d.default.as_ref().unwrap().int, None, "u64::MAX does not fit an i64");
        assert_eq!(d.default.as_ref().unwrap().hex, "ffffffffffffffff", "but its bytes are kept");
        assert_eq!(d.current.as_ref().unwrap().int, Some(i64::MAX), "i64::MAX as a u64 does");
        let wide = parse_prop_desc(&desc_bytes(1, 0x000A, 1, &[1; 16], &[2; 16], &[0]), 1).unwrap();
        assert_eq!(wide.current.unwrap().int, None, "128-bit values are bytes only");
    }

    #[test]
    fn a_descriptor_for_another_code_or_with_a_bad_form_keeps_what_was_read_and_says_where_it_stopped() {
        let wrong = parse_prop_desc(&enum_u8(0xD1A1), 0xD1A0).unwrap();
        assert!(wrong.fault.as_ref().unwrap().contains("0xd1a1") && wrong.current.is_none(), "{wrong:?}");
        let bad_flag = parse_prop_desc(&desc_bytes(1, 2, 1, &[0], &[1], &[9]), 1).unwrap();
        assert!(bad_flag.fault.as_ref().unwrap().contains("form flag"), "{bad_flag:?}");
        assert!(bad_flag.current.is_some(), "the values before the bad flag are kept");
        let unknown = parse_prop_desc(&desc_bytes(1, 0x1234, 1, &[0], &[1], &[0]), 1).unwrap();
        assert!(unknown.fault.as_ref().unwrap().contains("0x1234"), "{unknown:?}");
        assert_eq!(unknown.get_set, 1, "the head is kept");
    }

    #[test]
    fn announced_counts_larger_than_the_data_are_faults_not_allocations() {
        // enumeration announcing 65535 values with 1 byte left
        let huge_enum = desc_bytes(1, 2, 1, &[0], &[1], &[2, 0xFF, 0xFF, 7]);
        let d = parse_prop_desc(&huge_enum, 1).unwrap();
        assert!(d.fault.as_ref().unwrap().contains("65535"), "{d:?}");
        // an array announcing 2^32-1 elements
        let arr = [0xFF, 0xFF, 0xFF, 0xFF, 1, 0];
        let a = parse_prop_desc(&desc_bytes(1, 0x4004, 0, &arr, &arr, &[0]), 1).unwrap();
        assert!(a.fault.as_ref().unwrap().contains("does not fit"), "{a:?}");
        // the count exactly equals what is there: fine (the boundary)
        let ok = desc_bytes(1, 2, 1, &[0], &[1], &[2, 2, 0, 0, 1]);
        assert!(parse_prop_desc(&ok, 1).unwrap().fault.is_none());
        let one_short = desc_bytes(1, 2, 1, &[0], &[1], &[2, 3, 0, 0, 1]);
        let f = parse_prop_desc(&one_short, 1).unwrap().fault.expect("one more announced than present");
        assert!(f.contains("announced"), "caught by the count guard before any value is read, not by a later short read: {f}");
    }

    #[test]
    fn every_truncation_of_a_good_descriptor_is_handled_without_panicking() {
        for full in [enum_u8(0xD1A0), desc_bytes(0xD072, 0xFFFF, 1, &[0], &[3, b'a', 0, b'b', 0, 0, 0], &[0])] {
            let n = parse_prop_desc(&full, full[0] as u16 | (full[1] as u16) << 8).map(|d| d.fault.is_none()).unwrap_or(false);
            assert!(n, "the whole descriptor parses cleanly");
            for cut in 0..full.len() {
                let code = full[0] as u16 | (full[1] as u16) << 8;
                match parse_prop_desc(&full[..cut], code) {
                    None => assert!(cut < 5, "only a head shorter than five bytes has nothing to keep (cut at {cut})"),
                    Some(d) => assert!(d.fault.is_some() || d.trailing_hex.is_some() || cut == full.len(), "a cut at {cut} must be visible: {d:?}"),
                }
            }
        }
    }

    #[test]
    fn bytes_after_the_last_field_are_kept() {
        let mut b = enum_u8(1);
        b.extend([0xAA, 0xBB]);
        let d = parse_prop_desc(&b, 1).unwrap();
        assert_eq!(d.trailing_hex.as_deref(), Some("aabb"));
        assert!(d.fault.is_none());
    }

    // ── the sweep ──

    /// A body that answers from a table. Each code can be present, refused, or fail in a chosen way.
    struct Fake {
        table: BTreeMap<u16, Result<Vec<u8>, FailKind>>,
        asked: Vec<u16>,
        identity_fails: bool,
        listed: Vec<u16>,
        script: VecDeque<(u16, FailKind)>,
    }

    #[derive(Clone, Copy)]
    enum FailKind {
        Refuse(u16),
        Timeout,
        NoData,
    }

    fn fail(k: FailKind) -> PtpUsbError {
        match k {
            FailKind::Refuse(c) => PtpUsbError::OperationFailed { op: 0x1014, code: PtpResponseCode(c) },
            FailKind::Timeout => PtpUsbError::TransactionTimedOut(std::time::Duration::from_secs(1)),
            FailKind::NoData => PtpUsbError::NoDataPhase { op: 0x1014 },
        }
    }

    impl Fake {
        fn new() -> Fake {
            Fake { table: BTreeMap::new(), asked: Vec::new(), identity_fails: false, listed: vec![0xD1A0], script: VecDeque::new() }
        }
    }

    impl SweepSource for Fake {
        fn identity(&mut self) -> Result<SweepIdentity, PtpUsbError> {
            if self.identity_fails {
                return Err(PtpUsbError::NoDataPhase { op: 0x1001 });
            }
            Ok(SweepIdentity { model: "Z 9".into(), usb_serial: "S1".into(), firmware: "C:5.31".into(), listed: self.listed.clone() })
        }
        fn prop_desc(&mut self, code: u16) -> Result<Vec<u8>, PtpUsbError> {
            self.asked.push(code);
            if let Some(pos) = self.script.iter().position(|(c, _)| *c == code) {
                let (_, k) = self.script.remove(pos).unwrap();
                return Err(fail(k));
            }
            match self.table.get(&code) {
                Some(Ok(b)) => Ok(b.clone()),
                Some(Err(k)) => Err(fail(*k)),
                None => Err(PtpUsbError::OperationFailed { op: 0x1014, code: PtpResponseCode(0x200A) }), // the real DevicePropNotSupported, not our constant
            }
        }
    }

    fn opts(ranges: &[CodeRange]) -> SweepOptions {
        SweepOptions { ranges: ranges.to_vec(), label: Some("before".into()), captured_at: "2026-11-06T10:00:00Z".into() }
    }

    fn run(f: &mut Fake, ranges: &[CodeRange]) -> SweepDump {
        sweep(f, &opts(ranges), &mut |_| {}).unwrap()
    }

    #[test]
    fn a_sweep_asks_every_code_once_in_order_and_records_each_kind_of_answer() {
        let mut f = Fake::new();
        f.table.insert(0xD1A0, Ok(enum_u8(0xD1A0)));
        f.table.insert(0xD1A2, Err(FailKind::Refuse(0x2019)));
        let d = run(&mut f, &[r(0xD1A2, 0xD1A3), r(0xD1A0, 0xD1A2)]);
        assert_eq!(f.asked, [0xD1A0, 0xD1A1, 0xD1A2, 0xD1A3], "merged, ascending, 0xD1A2 asked once though both ranges name it");
        assert_eq!(d.counts, SweepCounts { asked: 4, present: 1, bytes_only: 0, not_supported: 2, refused: 1, failed: 0 });
        assert_eq!(d.counts.asked, d.counts.present + d.counts.not_supported + d.counts.refused + d.counts.failed, "every code asked has exactly one disposition");
        assert!(matches!(d.entries["0xd1a0"], SweepEntry::Present { listed_in_device_info: true, .. }));
        assert!(matches!(&d.entries["0xd1a2"], SweepEntry::Refused { code: 0x2019, .. }));
        assert!(!d.entries.contains_key("0xd1a1"), "not supported is the absence of an entry, counted");
        assert_eq!(d.ranges, [r(0xD1A0, 0xD1A3)]);
        assert_eq!((d.body.model.as_str(), d.body.firmware.as_str()), ("Z 9", "C:5.31"));
        assert_eq!(d.device_info_props, ["0xd1a0"]);
        assert!(d.aborted.is_none());
    }

    #[test]
    fn replies_too_short_to_parse_are_counted_as_bytes_only_at_the_head_boundary() {
        let mut f = Fake::new();
        f.table.insert(0xD1A0, Ok(enum_u8(0xD1A0))); // whole: decoded
        f.table.insert(0xD1A1, Ok(Vec::new())); // empty reply: nothing to parse
        f.table.insert(0xD1A2, Ok(enum_u8(0xD1A2)[..4].to_vec())); // one byte short of the five-byte head
        f.table.insert(0xD1A3, Ok(enum_u8(0xD1A3)[..5].to_vec())); // exactly the head: parsed, then faulted at the first value
        f.table.insert(0xD1A4, Ok(desc_bytes(0xD1A4, 2, 1, &[0], &[1], &[9]))); // a bad form flag: parsed, faulted
        let d = run(&mut f, &[r(0xD1A0, 0xD1A5)]);
        assert_eq!(d.counts.present, 5);
        assert_eq!(d.counts.bytes_only, 2, "the empty and the 4-byte reply; the 5-byte head and the bad flag have a desc, with a fault");
        assert!(matches!(&d.entries["0xd1a3"], SweepEntry::Present { desc: Some(PropDesc { fault: Some(_), .. }), .. }));
        assert_eq!(d.undecoded(), 4, "the dump's own count adds the faulted parses; the counter is the narrower, at-the-sweep one");
        assert_eq!(d.counts.asked, d.counts.present + d.counts.not_supported + d.counts.refused + d.counts.failed, "bytes_only is part of present, not a disposition");
        // A file written before the counter existed reads it as 0.
        let mut v = serde_json::to_value(&d).unwrap();
        v["counts"].as_object_mut().unwrap().remove("bytes_only");
        assert_eq!(SweepDump::from_json(&v.to_string()).unwrap().counts.bytes_only, 0);
        // and a clean sweep reports none
        let clean = run(&mut Fake::new(), &[r(0xD1A0, 0xD1A1)]);
        assert_eq!(clean.counts.bytes_only, 0);
    }

    #[test]
    fn a_property_listed_in_deviceinfo_is_flagged_and_one_that_is_served_but_not_listed_is_not() {
        let mut f = Fake::new();
        f.table.insert(0xD1A0, Ok(enum_u8(0xD1A0)));
        f.table.insert(0xD1A1, Ok(enum_u8(0xD1A1)));
        let d = run(&mut f, &[r(0xD1A0, 0xD1A1)]);
        assert!(matches!(d.entries["0xd1a0"], SweepEntry::Present { listed_in_device_info: true, .. }));
        assert!(matches!(d.entries["0xd1a1"], SweepEntry::Present { listed_in_device_info: false, .. }), "a Z 9 serves many it does not list");
    }

    #[test]
    fn the_top_of_the_code_space_is_asked_and_does_not_wrap() {
        let mut f = Fake::new();
        let d = run(&mut f, &[r(0xFFFE, 0xFFFF)]);
        assert_eq!(f.asked, [0xFFFE, 0xFFFF]);
        assert_eq!(d.counts.asked, 2);
    }

    #[test]
    fn a_timeout_ends_the_sweep_at_once_and_keeps_what_was_found() {
        let mut f = Fake::new();
        f.table.insert(0xD001, Ok(enum_u8(0xD001)));
        f.script.push_back((0xD003, FailKind::Timeout));
        let d = run(&mut f, &[r(0xD000, 0xD0FF)]);
        assert_eq!(*f.asked.last().unwrap(), 0xD003, "nothing is asked after a timeout: the pipe may be stalled");
        let ab = d.aborted.as_ref().expect("aborted");
        assert_eq!(ab.at, "0xd003");
        assert!(d.entries.contains_key("0xd001") && matches!(d.entries["0xd003"], SweepEntry::Failed { .. }));
        assert_eq!(d.counts.failed, 1);
        assert!(d.covers(0xD002) && !d.covers(0xD003) && !d.covers(0xD004), "covered strictly before the code it stopped at (at/below/above)");
    }

    #[test]
    fn replies_that_are_not_usable_end_the_sweep_at_the_third_in_a_row_and_not_before() {
        // two in a row, then a clean answer: carries on (and the count restarts)
        let mut f = Fake::new();
        f.script.extend([(0xD000, FailKind::NoData), (0xD001, FailKind::NoData), (0xD003, FailKind::NoData), (0xD004, FailKind::NoData)]);
        let d = run(&mut f, &[r(0xD000, 0xD00F)]);
        assert!(d.aborted.is_none(), "{:?}", d.aborted);
        assert_eq!(d.counts.failed, 4);
        // three in a row: stops, at the third
        let mut f = Fake::new();
        f.script.extend([(0xD000, FailKind::NoData), (0xD001, FailKind::NoData), (0xD002, FailKind::NoData)]);
        let d = run(&mut f, &[r(0xD000, 0xD00F)]);
        assert_eq!(d.aborted.as_ref().unwrap().at, "0xd002");
        assert_eq!(f.asked, [0xD000, 0xD001, 0xD002]);
        assert_eq!(MAX_CONSECUTIVE_FAILURES, 3, "the boundary the two cases above pin");
    }

    #[test]
    fn only_device_prop_not_supported_means_not_there_and_its_neighbours_are_refusals() {
        // Judged by the name `ptp_usb` gives the code (an independent table), and by the Fake's own literal above,
        // not against a copy of the constant's value.
        assert_eq!(PtpResponseCode(RESP_DEVICE_PROP_NOT_SUPPORTED).to_string(), "DevicePropNotSupported (0x200a)");
        let mut f = Fake::new();
        f.table.insert(0xD000, Err(FailKind::Refuse(0x2009))); // InvalidObjectHandle
        f.table.insert(0xD001, Err(FailKind::Refuse(0x200B))); // InvalidDevicePropFormat
        let d = run(&mut f, &[r(0xD000, 0xD002)]);
        assert_eq!((d.counts.refused, d.counts.not_supported), (2, 1), "0x2009 and 0x200B are answers; 0x200A (the absent 0xD002) is the only 'not supported'");
    }

    #[test]
    fn a_camera_refusal_is_an_answer_not_a_failure() {
        let mut f = Fake::new();
        for c in 0xD000..=0xD005u16 {
            f.table.insert(c, Err(FailKind::Refuse(0x2002)));
        }
        let d = run(&mut f, &[r(0xD000, 0xD00F)]);
        assert!(d.aborted.is_none(), "six refusals in a row do not stop a sweep");
        assert_eq!((d.counts.refused, d.counts.failed), (6, 0));
    }

    #[test]
    fn a_body_that_cannot_be_identified_gives_no_dump_at_all() {
        let mut f = Fake::new();
        f.identity_fails = true;
        assert!(sweep(&mut f, &opts(&[r(0, 1)]), &mut |_| {}).is_err());
        assert!(f.asked.is_empty(), "nothing is asked of a body that did not say what it is");
    }

    #[test]
    fn no_ranges_is_an_empty_dump_and_progress_counts_up_to_the_total() {
        let mut f = Fake::new();
        let d = run(&mut f, &[]);
        assert_eq!((d.counts.asked, d.entries.len()), (0, 0));
        let mut seen = Vec::new();
        sweep(&mut f, &opts(&[r(0xD000, 0xD002)]), &mut |p| seen.push((p.code, p.done, p.total))).unwrap();
        assert_eq!(seen, [(0xD000, 0, 3), (0xD001, 1, 3), (0xD002, 2, 3)]);
    }

    #[test]
    fn the_default_blocks_are_the_standard_and_the_vendor_ones_and_do_not_overlap() {
        let d = default_ranges();
        assert_eq!(normalize(&d), d);
        assert!(d.iter().any(|b| b.contains(0xD072)) && d.iter().any(|b| b.contains(0xD100)) && d.iter().any(|b| b.contains(0x5001)));
        // docs/property-sweep.md says how many codes this asks; 256 + 1280.
        assert_eq!(d.iter().map(|b| b.count()).sum::<u32>(), 1536);
    }

    // ── the dump ──

    #[test]
    fn a_dump_round_trips_and_a_dump_of_another_version_is_refused() {
        let mut f = Fake::new();
        f.table.insert(0xD1A0, Ok(enum_u8(0xD1A0)));
        let d = run(&mut f, &[r(0xD1A0, 0xD1A1)]);
        let text = serde_json::to_string_pretty(&d).unwrap();
        assert_eq!(SweepDump::from_json(&text).unwrap(), d);
        let v2 = text.replace("\"format_version\": 1", "\"format_version\": 2");
        assert!(matches!(SweepDump::from_json(&v2), Err(SweepError::UnsupportedVersion { found: 2, .. })));
    }

    #[test]
    fn a_dump_is_saved_under_a_name_without_separators_and_never_over_another() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = Fake::new();
        let mut d = run(&mut f, &[r(0xD000, 0xD001)]);
        d.label = Some("af/on only".into());
        let p = d.save_new(dir.path()).unwrap();
        assert_eq!(p.parent().unwrap(), dir.path(), "a '/' in the label did not redirect the write: {p:?}");
        assert!(SweepDump::load(&p).unwrap() == d);
        let again = d.save_new(dir.path()).unwrap_err();
        assert!(matches!(again, SweepError::Io(ref e) if e.kind() == std::io::ErrorKind::AlreadyExists), "{again}");
        assert_eq!(SweepDump::load(&p).unwrap(), d, "the first dump is untouched");
    }

    // ── comparing ──

    fn dump_with(entries: &[(u16, SweepEntry)], ranges: &[CodeRange]) -> SweepDump {
        let mut f = Fake::new();
        let mut d = run(&mut f, ranges);
        for (c, e) in entries {
            d.entries.insert(key(*c), e.clone());
        }
        d
    }

    fn present(bytes: Vec<u8>, code: u16) -> SweepEntry {
        SweepEntry::Present { raw_hex: hex_encode(&bytes), listed_in_device_info: false, desc: parse_prop_desc(&bytes, code) }
    }

    fn enum_with_current(code: u16, current: u8) -> SweepEntry {
        present(desc_bytes(code, 2, 1, &[0], &[current], &[2, 3, 0, 0, 1, 2]), code)
    }

    #[test]
    fn two_equal_sweeps_have_no_differences() {
        let blocks = [r(0xD000, 0xD0FF)];
        let a = dump_with(&[(0xD010, enum_with_current(0xD010, 1))], &blocks);
        let b = dump_with(&[(0xD010, enum_with_current(0xD010, 1))], &blocks);
        let d = diff_sweeps(&a, &b, None, false);
        assert!(d.changes.is_empty());
        assert_eq!((d.compared, d.not_compared), (256, 0));
    }

    #[test]
    fn a_changed_current_value_is_the_setting_that_moved_and_says_from_what_to_what() {
        let blocks = [r(0xD000, 0xD0FF)];
        let a = dump_with(&[(0xD010, enum_with_current(0xD010, 1)), (0xD020, enum_with_current(0xD020, 0))], &blocks);
        let b = dump_with(&[(0xD010, enum_with_current(0xD010, 2)), (0xD020, enum_with_current(0xD020, 0))], &blocks);
        let d = diff_sweeps(&a, &b, None, false);
        assert_eq!(d.changes.len(), 1, "{:?}", d.changes);
        assert_eq!((d.changes[0].code.as_str(), d.changes[0].kind), ("0xd010", ChangeKind::Changed));
        assert_eq!(d.changes[0].details, ["current: 1 (0x1) -> 2 (0x2)"]);
        let text = d.to_text(&a, &b);
        assert!(text.contains("0xd010  CHANGED") && text.contains("1 differ"), "{text}");
    }

    #[test]
    fn appearing_vanishing_and_a_changed_list_of_allowed_values_are_each_reported() {
        let blocks = [r(0xD000, 0xD0FF)];
        let narrow = present(desc_bytes(0xD030, 2, 1, &[0], &[0], &[2, 2, 0, 0, 1]), 0xD030);
        let wide = present(desc_bytes(0xD030, 2, 1, &[0], &[0], &[2, 3, 0, 0, 1, 2]), 0xD030);
        let a = dump_with(&[(0xD030, narrow), (0xD040, enum_with_current(0xD040, 0))], &blocks);
        let b = dump_with(&[(0xD030, wide), (0xD050, enum_with_current(0xD050, 0))], &blocks);
        let d = diff_sweeps(&a, &b, None, false);
        let by = |c: &str| d.changes.iter().find(|x| x.code == c).unwrap_or_else(|| panic!("{c} in {:?}", d.changes));
        assert_eq!(by("0xd030").kind, ChangeKind::Changed);
        assert!(by("0xd030").details[0].starts_with("allowed values: one of [0 (0x0), 1 (0x1)] -> one of [0 (0x0), 1 (0x1), 2 (0x2)]"), "{:?}", by("0xd030"));
        assert_eq!(by("0xd040").kind, ChangeKind::Vanished);
        assert_eq!(by("0xd050").kind, ChangeKind::Appeared);
    }

    #[test]
    fn access_and_data_type_changes_and_answer_class_changes_are_reported() {
        let blocks = [r(0xD000, 0xD0FF)];
        let rw = present(desc_bytes(0xD060, 2, 1, &[0], &[0], &[0]), 0xD060);
        let ro = present(desc_bytes(0xD060, 2, 0, &[0], &[0], &[0]), 0xD060);
        let a = dump_with(&[(0xD060, rw.clone()), (0xD061, rw.clone())], &blocks);
        let b = dump_with(&[(0xD060, ro), (0xD061, SweepEntry::Refused { code: 0x2019, name: "DeviceBusy (0x2019)".into() })], &blocks);
        let d = diff_sweeps(&a, &b, None, false);
        assert_eq!(d.changes[0].details, ["access: read-write -> read-only"]);
        assert!(d.changes[1].details[0].starts_with("answer: present (") && d.changes[1].details[0].contains("refused: DeviceBusy"), "{:?}", d.changes[1]);
    }

    #[test]
    fn only_codes_both_dumps_cover_are_compared_and_the_rest_are_counted() {
        let a = dump_with(&[(0xD010, enum_with_current(0xD010, 1))], &[r(0xD000, 0xD0FF)]);
        let b = dump_with(&[(0xD010, enum_with_current(0xD010, 2)), (0xD110, enum_with_current(0xD110, 2))], &[r(0xD010, 0xD1FF)]);
        let d = diff_sweeps(&a, &b, None, false);
        assert_eq!(d.compared, 0xD0FF - 0xD010 + 1, "the overlap");
        assert_eq!(d.not_compared, (0xD00F - 0xD000 + 1) + (0xD1FF - 0xD100 + 1), "the parts only one covers");
        assert_eq!(d.changes.iter().map(|c| c.code.as_str()).collect::<Vec<_>>(), ["0xd010"], "0xd110 is outside A: not a difference");
    }

    #[test]
    fn a_dump_that_stopped_early_is_not_compared_past_where_it_stopped() {
        let mut a = dump_with(&[], &[r(0xD000, 0xD0FF)]);
        a.aborted = Some(Aborted { at: "0xd080".into(), reason: "timeout".into() });
        let b = dump_with(&[(0xD07F, enum_with_current(0xD07F, 1)), (0xD080, enum_with_current(0xD080, 1)), (0xD081, enum_with_current(0xD081, 1))], &[r(0xD000, 0xD0FF)]);
        let d = diff_sweeps(&a, &b, None, false);
        assert_eq!(d.changes.iter().map(|c| c.code.as_str()).collect::<Vec<_>>(), ["0xd07f"], "at/below counts; at/above the stop does not");
        assert!(d.to_text(&a, &b).contains("dump A stopped at 0xd080"));
    }

    #[test]
    fn things_that_move_on_their_own_are_set_aside_when_a_noise_dump_shows_them() {
        let blocks = [r(0xD000, 0xD0FF)];
        let a = dump_with(&[(0xD010, enum_with_current(0xD010, 1)), (0xD020, enum_with_current(0xD020, 0))], &blocks);
        // after the menu change: 0xD010 (the setting) and 0xD020 (a clock) moved
        let b = dump_with(&[(0xD010, enum_with_current(0xD010, 2)), (0xD020, enum_with_current(0xD020, 1))], &blocks);
        // a second sweep with no change: only the clock moved
        let n = dump_with(&[(0xD010, enum_with_current(0xD010, 1)), (0xD020, enum_with_current(0xD020, 2))], &blocks);
        let d = diff_sweeps(&a, &b, Some(&n), false);
        assert_eq!(d.changes.iter().map(|c| c.code.as_str()).collect::<Vec<_>>(), ["0xd010"]);
        assert_eq!(d.volatile.iter().map(|c| c.code.as_str()).collect::<Vec<_>>(), ["0xd020"]);
        let kept = diff_sweeps(&a, &b, Some(&n), true);
        assert_eq!(kept.changes.len(), 2, "asked to keep them: nothing is set aside");
        assert!(kept.volatile.is_empty());
        // and with no noise dump nothing is ever hidden
        assert_eq!(diff_sweeps(&a, &b, None, false).changes.len(), 2);
    }

    #[test]
    fn two_different_bodies_or_firmwares_are_said_to_be_so() {
        let a = dump_with(&[], &[r(0, 1)]);
        let mut b = a.clone();
        b.body.usb_serial = "S2".into();
        assert!(diff_sweeps(&a, &b, None, false).to_text(&a, &b).contains("two different bodies"));
        let mut c = a.clone();
        c.body.firmware = "C:5.40".into();
        let t = diff_sweeps(&a, &c, None, false).to_text(&a, &c);
        assert!(t.contains("firmware differs") && !t.contains("different bodies"), "{t}");
    }

    // ── findings: coverage, loading, partial forms, string faults, saving ──

    #[test]
    fn a_dump_covers_the_codes_before_where_it_stopped_and_the_boundary_is_the_stop_itself() {
        let mut d = dump_with(&[], &[r(0xD000, 0xD0FF)]);
        assert!(d.covers(0xD000) && d.covers(0xD0FF) && !d.covers(0xD100) && !d.covers(0xCFFF), "no stop: the whole block, nothing outside");
        d.aborted = Some(Aborted { at: "0xd080".into(), reason: "timeout".into() });
        assert!(d.covers(0xD07F), "just below the stop");
        assert!(!d.covers(0xD080), "at the stop: it is the code that was being asked, not covered");
        assert!(!d.covers(0xD081), "just above");
        d.aborted = Some(Aborted { at: "0xd000".into(), reason: "first code".into() });
        assert!(!d.covers(0xD000), "stopped at the very first code: nothing covered");
    }

    #[test]
    fn a_stop_that_is_not_a_property_code_covers_nothing_and_a_file_with_one_is_refused() {
        let mut d = dump_with(&[], &[r(0xD000, 0xD0FF)]);
        d.aborted = Some(Aborted { at: "somewhere".into(), reason: "x".into() });
        assert!(!(0xD000..=0xD0FFu16).any(|c| d.covers(c)), "unknown stop point: fail closed, not 'covers everything'");
        let text = serde_json::to_string(&d).unwrap();
        let e = SweepDump::from_json(&text).unwrap_err();
        assert!(matches!(e, SweepError::Json(_)) && e.to_string().contains("aborted.at"), "{e}");
        // an upper-case or decimal spelling of a real code is not the form the sweep writes either
        for at in ["0xD080", "53376"] {
            d.aborted = Some(Aborted { at: at.into(), reason: "x".into() });
            assert!(SweepDump::from_json(&serde_json::to_string(&d).unwrap()).is_err(), "{at}");
        }
        d.aborted = Some(Aborted { at: "0xd080".into(), reason: "x".into() });
        assert!(SweepDump::from_json(&serde_json::to_string(&d).unwrap()).is_ok());
    }

    #[test]
    fn an_entry_key_that_is_not_the_canonical_code_is_refused_on_load_and_counted_if_it_gets_to_a_diff() {
        let good = dump_with(&[(0xD010, enum_with_current(0xD010, 1))], &[r(0xD000, 0xD0FF)]);
        assert!(SweepDump::from_json(&serde_json::to_string(&good).unwrap()).is_ok());
        for bad in ["0xD010", "junk", "", "0xd01"] {
            let mut d = good.clone();
            let e = d.entries.remove("0xd010").unwrap();
            d.entries.insert(bad.to_string(), e);
            let err = SweepDump::from_json(&serde_json::to_string(&d).unwrap()).unwrap_err();
            assert!(err.to_string().contains("entry key"), "{bad:?}: {err}");
        }
        // built in memory it still cannot vanish silently
        let mut b = good.clone();
        b.entries.insert("junk".into(), enum_with_current(0xD011, 1));
        let diff = diff_sweeps(&good, &b, None, false);
        assert_eq!(diff.bad_keys, ["junk"]);
        assert!(diff.to_text(&good, &b).contains("junk"), "and the report says so");
        assert!(diff_sweeps(&good, &good, None, false).bad_keys.is_empty());
    }

    #[test]
    fn answers_kept_only_as_bytes_are_counted_and_the_diff_says_so() {
        let mut d = dump_with(&[(0xD010, enum_with_current(0xD010, 1))], &[r(0xD000, 0xD0FF)]);
        assert_eq!(d.undecoded(), 0);
        d.entries.insert(key(0xD011), present(vec![0x11, 0xD0, 2], 0xD011)); // shorter than the five-byte head
        d.entries.insert(key(0xD012), present(desc_bytes(0xD012, 2, 1, &[0], &[1], &[9]), 0xD012)); // a bad form flag
        d.entries.insert(key(0xD013), SweepEntry::Refused { code: 0x2002, name: "x".into() });
        assert_eq!(d.undecoded(), 2, "the short reply and the faulted one; a refusal is an answer");
        let text = diff_sweeps(&d, &d, None, false).to_text(&d, &d);
        assert!(text.contains("dump A holds 2 answer(s) kept only as bytes"), "{text}");
        let clean = dump_with(&[(0xD010, enum_with_current(0xD010, 1))], &[r(0xD000, 0xD0FF)]);
        assert!(!diff_sweeps(&clean, &clean, None, false).to_text(&clean, &clean).contains("kept only as bytes"));
    }

    #[test]
    fn an_enumeration_cut_short_keeps_the_values_read_before_the_cut_and_a_range_is_kept_only_whole() {
        // u16 enumeration announcing 3 values with 5 bytes after the count: two whole values, then one byte
        let cut = desc_bytes(1, 4, 1, &[0, 0], &[1, 0], &[2, 3, 0, 0, 0, 1, 0, 2]);
        let d = parse_prop_desc(&cut, 1).unwrap();
        assert!(d.fault.as_ref().unwrap().contains("enumeration value 2"), "{d:?}");
        let Some(PropForm::Enum { values }) = &d.form else { panic!("the values read are kept: {:?}", d.form) };
        assert_eq!(values.iter().map(|v| v.int.unwrap()).collect::<Vec<_>>(), [0, 1]);
        assert!(d.current.is_some(), "and the current value");
        // a range cut before its step has no form (stated in the module's table)
        let range = desc_bytes(1, 2, 1, &[0], &[1], &[1, 0, 9]);
        let r = parse_prop_desc(&range, 1).unwrap();
        assert!(r.fault.as_ref().unwrap().contains("range step") && r.form.is_none(), "{r:?}");
    }

    #[test]
    fn a_string_that_does_not_decode_says_why_and_a_good_one_has_no_fault() {
        // type 0xFFFF; a count of 3 units with 2 bytes: the reader refuses it before decoding
        let good = parse_prop_desc(&desc_bytes(1, 0xFFFF, 1, &[2, b'h', 0, 0, 0], &[0], &[0]), 1).unwrap();
        let cur = good.default.as_ref().unwrap();
        assert!(cur.text.is_some() && cur.text_fault.is_none(), "{good:?}");
        // one unit whose two bytes are an unpaired high surrogate: read whole, but not text
        let bad = parse_prop_desc(&desc_bytes(1, 0xFFFF, 1, &[1, 0x00, 0xD8], &[0], &[0]), 1).unwrap();
        let v = bad.default.as_ref().unwrap();
        assert!(v.text.is_none() && v.text_fault.is_some(), "{v:?}");
        assert_eq!(v.show(), format!("bytes {}", v.hex), "shown as bytes, hex kept");
    }

    #[test]
    fn a_saved_dump_leaves_no_temporary_beside_it_whether_it_was_new_or_not() {
        let dir = tempfile::tempdir().unwrap();
        let d = dump_with(&[(0xD010, enum_with_current(0xD010, 1))], &[r(0xD000, 0xD0FF)]);
        let p = d.save_new(dir.path()).unwrap();
        assert!(d.save_new(dir.path()).is_err());
        let names: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, [p.file_name().unwrap().to_string_lossy().into_owned()], "only the dump: {names:?}");
        // the directory is made when it is missing
        let nested = dir.path().join("a").join("b");
        assert!(d.save_new(&nested).unwrap().starts_with(&nested));
    }
}

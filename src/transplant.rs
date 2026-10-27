//! Cross-model settings transplant.
//!
//! `fleet restore` assumes the source snapshot and the target camera are
//! the same model: it copies each property's raw value verbatim, keyed by
//! capability code. That's correct for same-model round-trips (backup/
//! restore, firmware rollback) but wrong across models — the same
//! capability code can have a different enum option ordering, a different
//! numeric range, or simply not exist, on a different body. (This is the
//! MAID-capability-level version of the same problem raw PTP vendor
//! properties have — see docs/nx-field-session-2026-07-09.md — just with a
//! much better starting point: MAID capability IDs are a fixed SDK-wide
//! enum, confirmed by grepping sdk-runtime/MaidLayer.config for a few
//! well-known capabilities and finding exactly one numeric ID each across
//! all 38 model sections. So "the same setting" is already the same
//! capability code across bodies; only the *value* needs translating.)
//!
//! Instead of assuming "same index/value means the same setting", this
//! reads the TARGET camera's own current value for each capability and
//! translates the source's value into the target's terms:
//!
//! - **Scalar** (bool/number/string): copied literally — a copyright
//!   string or FTP host means the same thing on any body.
//! - **Packed-string enum** (`elem_type` 7 — Nikon's menu-label choices,
//!   e.g. "High"/"Normal"/"Off"): matched by the human-readable label, not
//!   index — the same label at a different index/count on the target still
//!   matches.
//! - **Other enum types** (no decoded label available yet — see
//!   docs/todo.md's "MaidLayer resource strings" backlog item): matched by
//!   raw numeric code instead. This is a weaker heuristic — nothing
//!   guarantees Nikon assigns the same code to the same meaning across
//!   bodies for these — so it's reported as a distinct strategy
//!   (`EnumByRawValue`) rather than silently treated the same as a label
//!   match.
//! - **Range**: source value clamped into the target's `[lower, upper]`,
//!   snapped to the target's own step count.
//!
//! A capability the target doesn't have, doesn't expose for writing, or
//! whose translated value has no match on the target (an enum choice that
//! simply doesn't exist there) is skipped, not guessed at.
//!
//! # Scope: Z-series only
//!
//! "The same capability code is the same setting on every body" was
//! checked against every section of `MaidLayer.config` (see
//! `tests::real_config_*`), and it holds across the Z-series bodies the
//! config covers — with the two documented exceptions in
//! `KNOWN_NAME_ALIASES` and the in-section alias at 33211. It does *not*
//! hold across the whole file: the D6 section, for one, lists
//! `WBTunePreset2` at the code every other body uses for `WBTunePreset1`.
//! And there is no section at all for the other DSLRs the fleet owns (D5),
//! so nothing could be checked for them. Rather than extend an unverified
//! assumption, [`CodeNameGuard::new`] refuses any non-Z source or target
//! model up front, before anything is written.
//!
//! Within the Z-series, [`CodeNameGuard`] also skips a capability whose
//! symbolic name differs between the source and target models' sections
//! (and isn't a declared alias), as a tripwire for a future SDK drop
//! changing what a code means.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Serialize, Serializer};
use serde_json::{Value, json};

use crate::maid_layer::MaidLayerConfig;
use crate::sdk::{CapabilityInfo, Device, OP_GET, OP_SET, SdkError};
use crate::snapshot::PropertyEntry;

const PACKED_STRING_ELEM_TYPE: u64 = 7;

/// `kNkMAIDArrayType` values `sdk::decode_enum_values` actually recognizes:
/// 0=Boolean, 1=Integer, 2=Unsigned, 3=Float, 7=PackedString, 8=String (see
/// that function's own doc comment). A present-but-unrecognized value is a
/// stronger anomaly signal than an absent one — the SDK always populates
/// this field on a live read, so an unrecognized value only reaches here via
/// a corrupt/hand-edited snapshot, same class of problem as a missing
/// `value_index`.
const KNOWN_ELEM_TYPES: &[u64] = &[0, 1, 2, 3, 7, 8];

/// What the SDK substitutes for a menu label it could not look up. Seen on a
/// Z6III (fw 2.00) for every option of `EVInterval` and all but the first of
/// `BracketingFactor`. It is a placeholder, not a label: two different options
/// carry the same text, so it can never identify one.
const UNRESOLVED_LABEL: &str = "Resource String not found";

/// The option labels of a packed-string (`elem_type` 7) enum, in index order,
/// recovered from the `values` array `sdk::decode_enum_values` stored.
///
/// The SDK hands back one buffer of NUL-separated strings with
/// `elem_bytes == 1` and `elem_count` equal to the buffer's byte length
/// (e.g. 178 for a 13-option list). `decode_enum_values` assumes `elem_bytes`
/// is a per-string stride, so for that layout it stores **one array element
/// per byte** — `"J","P","E","G"," ","B",...,""` with `""` standing for each
/// NUL — and `value_index` indexes the *strings*, not those elements.
/// Reading `values[value_index]` therefore yields one arbitrary character;
/// matching on it wrote wrong enum indices to a camera (hardware dry run,
/// 2026-10-27: 8 of 17 label-matched properties, one out of range). The
/// stored format is left as it is (existing snapshots stay valid); this
/// rebuilds the strings from it.
///
/// - `elem_bytes == 1`: byte layout, join runs between `""` elements. A final
///   run with no terminating NUL still counts as a string.
/// - `elem_bytes > 1`: the stride layout the decoder's comment describes; each
///   element is already one whole string.
/// - anything else (absent, zero, non-numeric): not interpretable. Guessing a
///   layout here would reintroduce the silent mismatch, so it is an error.
///
/// `side` ("source"/"target") only chooses which `SkipReason` carries the error.
fn packed_string_options(enum_value: &Value, side: &'static str) -> Result<Vec<String>, SkipReason> {
    let bad = |reason: &str| match side {
        "source" => SkipReason::MalformedSource,
        _ => SkipReason::EnumUndecodedBySdk { side, reason: reason.to_string() },
    };
    let elems = enum_value.get("values").and_then(Value::as_array).ok_or_else(|| bad("no values array"))?;
    let strs: Vec<&str> =
        elems.iter().map(|e| e.as_str().ok_or_else(|| bad("non-string element in a packed-string enum"))).collect::<Result<_, _>>()?;
    match enum_value.get("elem_bytes").and_then(Value::as_u64) {
        Some(1) => {
            let mut out = Vec::new();
            let mut cur = String::new();
            let mut open = false;
            for e in strs {
                if e.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    open = false;
                } else {
                    cur.push_str(e);
                    open = true;
                }
            }
            if open {
                out.push(cur);
            }
            Ok(out)
        }
        Some(n) if n > 1 => Ok(strs.into_iter().map(str::to_string).collect()),
        _ => Err(bad("packed-string enum has no usable elem_bytes")),
    }
}

/// Cap on how much of an external error message (SDK error text, captured
/// via `SdkError::to_string()`) is kept in a `SkipReason` — an unusually
/// verbose or malformed error string shouldn't be able to bloat the
/// transplant summary/log output without bound.
const MAX_ERROR_MESSAGE_LEN: usize = 500;

/// Truncate `s` to at most `max` bytes (on a char boundary, never splitting
/// a UTF-8 sequence), appending a marker with the original length so
/// truncation is visible rather than silent — never a bare cap with no
/// signal that anything was cut.
fn truncate_with_marker(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... [truncated, {} bytes total]", &s[..end], s.len())
}

/// How a property's value was translated for the target camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Plain scalar, copied as-is.
    Literal,
    /// Packed-string enum, matched by its human-readable label.
    EnumByLabel,
    /// Non-packed-string enum, matched by raw numeric code — a weaker
    /// heuristic than a label match (see module docs).
    EnumByRawValue,
    /// Range value clamped into the target's own bounds/step count.
    RangeClamped,
}

impl Strategy {
    /// Closed machine-readable tag (snake_case), for structured output. The
    /// match has no wildcard arm, so a new variant does not compile until it
    /// is given a tag. `Display` keeps the hyphenated form for the CLI text.
    pub fn tag(&self) -> &'static str {
        match self {
            Strategy::Literal => "literal",
            Strategy::EnumByLabel => "enum_by_label",
            Strategy::EnumByRawValue => "enum_by_raw_value",
            Strategy::RangeClamped => "range_clamped",
        }
    }
}

impl Serialize for Strategy {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.tag())
    }
}

impl std::fmt::Display for Strategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Strategy::Literal => "literal",
            Strategy::EnumByLabel => "enum-by-label",
            Strategy::EnumByRawValue => "enum-by-raw-value",
            Strategy::RangeClamped => "range-clamped",
        })
    }
}

/// Why a property was skipped instead of written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Capability code not present on the target camera at all.
    AbsentOnTarget,
    /// Present, but the target doesn't allow writing it (no `OP_SET` bit).
    ReadOnly,
    /// `write_capability` rejected the target's capability kind outright
    /// (`SdkError::UnsupportedWrite`) — mirrors `fleet restore`'s
    /// `skipped_type` category.
    UnsupportedWriteType,
    /// Enum value: the source's label/raw-code has no match among the
    /// target's own options.
    NoMatchingOption,
    /// The source value isn't a scalar, enum, or range shape this module
    /// knows how to translate (e.g. a datetime or raw-array capability).
    UnsupportedValueShape,
    /// Needed a live read of the target's current value (enum/range only)
    /// and that read failed. Carries the SDK's actual error when a read was
    /// attempted (distinguishing "device/USB communication failed" from a
    /// structural shape mismatch, which now has its own `EnumUndecodedBySdk`
    /// variant instead of also landing here); a fixed placeholder message in
    /// the defensive case where `translate_value` was called without a live
    /// value at all — `transplant_one` never triggers that path since it
    /// always attempts the read whenever the shape needs one.
    TargetReadFailed(String),
    /// The source snapshot's value for an enum/range capability is missing
    /// a field `sdk::decode_value` always produces — a corrupt or
    /// hand-edited snapshot file, not a normal skip case.
    MalformedSource,
    /// The live write itself failed, for a reason other than an
    /// unsupported capability kind.
    WriteFailed(String),
    /// The source or target's enum element array is one of
    /// `sdk::decode_enum_values`'s SDK-decode-failure sentinel objects
    /// (`_invalid_physical_bytes`, `_overflow_computing_packed_string_length`,
    /// `_unsupported_enum_type`) rather than a real array of options — the
    /// MAID SDK itself couldn't decode this enum's element array. Distinct
    /// from `MalformedSource` (a corrupt/hand-edited snapshot) and
    /// `NoMatchingOption` (the SDK decoded fine, the specific value just
    /// isn't among the target's valid options): this is neither — the data
    /// is well-formed JSON, the SDK's own decoder is what failed.
    EnumUndecodedBySdk { side: &'static str, reason: String },
    /// Source and target disagree on whether this enum is packed-string
    /// (`elem_type` 7, label-matched) vs. raw-code (matched by numeric
    /// value) — comparing a source label against target raw codes (or vice
    /// versa) would never match by construction, which would otherwise
    /// silently present as an ordinary `NoMatchingOption` ("this option
    /// doesn't exist on the target") rather than what it actually is: the
    /// two sides encode this capability differently.
    EnumTypeMismatch { source_elem_type: u64, target_elem_type: u64 },
    /// `MaidLayer.config` gives this capability code unrelated symbolic
    /// names on the source and target models (and the pair isn't in
    /// `KNOWN_NAME_ALIASES`), so the code may not mean the same setting on
    /// both bodies. Names are joined with `/` when a model lists several.
    CapabilityNameMismatch { source_names: String, target_names: String },
    /// The source's chosen label appears more than once in the source's or
    /// the target's option list (e.g. `ISOControlSensitivity` lists "125"
    /// twice), so matching by label cannot say which position is meant.
    /// Skipped rather than guessing the first occurrence.
    AmbiguousLabel { label: String, source_count: usize, target_count: usize },
    /// On [`NEVER_TRANSPLANT`]: reports body/session state (or locks it)
    /// rather than holding a setting. Never written, with no override.
    NeverCopied { reason: &'static str },
    /// Matched only by raw numeric code, which is not known to mean the same
    /// option on both bodies, and the user did not opt this property in.
    /// `proposed` is what it would have set (`code 5 (#2), was code 3 (#1)`).
    RawCodeUnconfirmed { proposed: String },
    /// A numeric scalar copied as-is, which is no more self-describing than a
    /// raw code (`WBAutoType` 3 on one body need not mean 3 on another, F9),
    /// would change the target, and the user did not opt this property in.
    /// `proposed` is `5, was 3`, or `5 (target's current value unknown)`.
    LiteralUnconfirmed { proposed: String },
}

/// How a skip should be read. Routine skips are expected and numerous; policy
/// skips are the tool declining on purpose (and, for opt-in gates, offering a
/// way to override); warn skips want a person's attention; an error is a write
/// that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Routine,
    Policy,
    Warn,
    Error,
}

impl SkipReason {
    /// Closed machine-readable tag (snake_case). No wildcard arm: a new
    /// variant must be given a tag before this compiles. Nothing downstream
    /// should read the `Display` text to decide anything.
    pub fn tag(&self) -> &'static str {
        match self {
            SkipReason::AbsentOnTarget => "absent_on_target",
            SkipReason::ReadOnly => "read_only",
            SkipReason::UnsupportedWriteType => "unsupported_write_type",
            SkipReason::NoMatchingOption => "no_matching_option",
            SkipReason::UnsupportedValueShape => "unsupported_value_shape",
            SkipReason::TargetReadFailed(_) => "target_read_failed",
            SkipReason::MalformedSource => "malformed_source",
            SkipReason::WriteFailed(_) => "write_failed",
            SkipReason::EnumUndecodedBySdk { .. } => "enum_undecoded_by_sdk",
            SkipReason::EnumTypeMismatch { .. } => "enum_type_mismatch",
            SkipReason::CapabilityNameMismatch { .. } => "capability_name_mismatch",
            SkipReason::AmbiguousLabel { .. } => "ambiguous_label",
            SkipReason::NeverCopied { .. } => "never_copied",
            SkipReason::RawCodeUnconfirmed { .. } => "raw_code_unconfirmed",
            SkipReason::LiteralUnconfirmed { .. } => "literal_unconfirmed",
        }
    }

    /// How this skip should be read. The CLI warns on `Warn` and `Error` only,
    /// so this is the one place that decision lives.
    pub fn severity(&self) -> Severity {
        match self {
            SkipReason::AbsentOnTarget | SkipReason::ReadOnly | SkipReason::UnsupportedWriteType => {
                Severity::Routine
            }
            SkipReason::NeverCopied { .. }
            | SkipReason::RawCodeUnconfirmed { .. }
            | SkipReason::LiteralUnconfirmed { .. } => Severity::Policy,
            SkipReason::NoMatchingOption
            | SkipReason::UnsupportedValueShape
            | SkipReason::TargetReadFailed(_)
            | SkipReason::MalformedSource
            | SkipReason::EnumUndecodedBySdk { .. }
            | SkipReason::EnumTypeMismatch { .. }
            | SkipReason::CapabilityNameMismatch { .. }
            | SkipReason::AmbiguousLabel { .. } => Severity::Warn,
            SkipReason::WriteFailed(_) => Severity::Error,
        }
    }

    /// The typed detail a variant carries, as named fields. `None` for the
    /// variants that carry nothing. SDK error text appears only as the value
    /// of a `message` field, for a human; it is never the tag.
    fn detail(&self) -> Option<Value> {
        match self {
            SkipReason::AbsentOnTarget
            | SkipReason::ReadOnly
            | SkipReason::UnsupportedWriteType
            | SkipReason::NoMatchingOption
            | SkipReason::UnsupportedValueShape
            | SkipReason::MalformedSource => None,
            SkipReason::TargetReadFailed(message) | SkipReason::WriteFailed(message) => {
                Some(json!({ "message": message }))
            }
            SkipReason::EnumUndecodedBySdk { side, reason } => {
                Some(json!({ "side": side, "reason": reason }))
            }
            SkipReason::EnumTypeMismatch { source_elem_type, target_elem_type } => {
                Some(json!({ "source_elem_type": source_elem_type, "target_elem_type": target_elem_type }))
            }
            SkipReason::CapabilityNameMismatch { source_names, target_names } => {
                Some(json!({ "source_names": source_names, "target_names": target_names }))
            }
            SkipReason::AmbiguousLabel { label, source_count, target_count } => {
                Some(json!({ "label": label, "source_count": source_count, "target_count": target_count }))
            }
            SkipReason::NeverCopied { reason } => Some(json!({ "reason": reason })),
            SkipReason::RawCodeUnconfirmed { proposed } | SkipReason::LiteralUnconfirmed { proposed } => {
                Some(json!({ "proposed": proposed }))
            }
        }
    }
}

impl Serialize for SkipReason {
    /// `{"tag": ..., "severity": ..., "detail": {...}}`; `detail` is omitted
    /// when the variant carries none. Never the `Display` text.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = serde_json::Map::new();
        m.insert("tag".into(), json!(self.tag()));
        m.insert("severity".into(), json!(self.severity()));
        if let Some(d) = self.detail() {
            m.insert("detail".into(), d);
        }
        Value::Object(m).serialize(s)
    }
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::AbsentOnTarget => f.write_str("not present on target camera"),
            SkipReason::ReadOnly => f.write_str("read-only on target camera"),
            SkipReason::UnsupportedWriteType => f.write_str("target capability kind is not writable"),
            SkipReason::NoMatchingOption => f.write_str("no matching option on target camera"),
            SkipReason::UnsupportedValueShape => f.write_str("unsupported value shape"),
            SkipReason::TargetReadFailed(msg) => write!(f, "could not read current value from target camera: {msg}"),
            SkipReason::MalformedSource => f.write_str("malformed value in source snapshot"),
            SkipReason::WriteFailed(msg) => write!(f, "write failed: {msg}"),
            SkipReason::EnumUndecodedBySdk { side, reason } => {
                write!(f, "{side} camera's enum options could not be decoded by the SDK: {reason}")
            }
            SkipReason::EnumTypeMismatch { source_elem_type, target_elem_type } => write!(
                f,
                "source and target encode this enum differently (source elem_type={source_elem_type}, \
                 target elem_type={target_elem_type})"
            ),
            SkipReason::NeverCopied { reason } => write!(f, "never copied between cameras: {reason}"),
            SkipReason::RawCodeUnconfirmed { proposed } => write!(
                f,
                "matched only by raw numeric code, which is not known to mean the same option on both bodies; \
                 would set {proposed}. Not written unless you opt this property in (--allow-raw-code)"
            ),
            SkipReason::LiteralUnconfirmed { proposed } => write!(
                f,
                "numeric value copied as-is, which is not known to mean the same on both bodies; \
                 would set {proposed}. Not written unless you opt this property in (--allow-raw-code)"
            ),
            SkipReason::AmbiguousLabel { label, source_count, target_count } => write!(
                f,
                "option {label:?} appears {source_count} time(s) in the source's list and {target_count} in the \
                 target's, so it cannot be matched to one position by label"
            ),
            SkipReason::CapabilityNameMismatch { source_names, target_names } => write!(
                f,
                "capability code has unrelated names on the two models (source: {source_names}; \
                 target: {target_names}) — it may not be the same setting"
            ),
        }
    }
}

/// Is `value` an enum shape (as produced by `sdk::decode_value` for
/// `DT_ENUM_PTR`)? Distinguished from a range shape by the absence of
/// "lower" — both are JSON objects with several similarly-named numeric
/// fields (`value_index`, `default_index`) otherwise.
fn is_enum_shape(value: &Value) -> bool {
    value.is_object() && value.get("values").is_some() && value.get("lower").is_none()
}

fn is_range_shape(value: &Value) -> bool {
    value.is_object() && value.get("lower").is_some() && value.get("upper").is_some()
}

/// If `values` is one of `sdk::decode_enum_values`'s SDK-decode-failure
/// sentinel objects (not a real element array), a short description of
/// which one. `None` means `values` is a normal array — or isn't an object
/// at all, which the caller's own `.as_array()` check handles.
fn enum_values_sdk_decode_failure(values: &Value) -> Option<String> {
    let obj = values.as_object()?;
    if let Some(v) = obj.get("_invalid_physical_bytes") {
        return Some(format!("invalid element stride ({v})"));
    }
    if obj.get("_overflow_computing_packed_string_length").is_some() {
        return Some("packed-string length overflow".to_string());
    }
    if let Some(v) = obj.get("_unsupported_enum_type") {
        return Some(format!("unsupported enum element type ({v})"));
    }
    None
}

/// `elem_type` if present and one of `KNOWN_ELEM_TYPES`; `None` if the field
/// is simply absent (defaults to the conservative raw-code strategy, same as
/// before); `Err(MalformedSource)` if it's present but not a recognized
/// value (a real anomaly, not an absent-field case).
fn parse_known_elem_type(value: &Value) -> Result<Option<u64>, SkipReason> {
    match value.get("elem_type") {
        None => Ok(None),
        Some(v) => {
            let n = v.as_u64().ok_or(SkipReason::MalformedSource)?;
            if KNOWN_ELEM_TYPES.contains(&n) { Ok(Some(n)) } else { Err(SkipReason::MalformedSource) }
        }
    }
}

/// Match a packed-string (`elem_type` 7) or raw-code enum value from
/// `source` into the target's own index space, using `target_live` (the
/// target camera's current value for the same capability).
fn translate_enum(source: &Value, target_live: &Value) -> Result<(Value, Strategy), SkipReason> {
    let src_idx = source.get("value_index").and_then(Value::as_u64).ok_or(SkipReason::MalformedSource)? as usize;
    let src_values_raw = source.get("values").ok_or(SkipReason::MalformedSource)?;
    if let Some(reason) = enum_values_sdk_decode_failure(src_values_raw) {
        return Err(SkipReason::EnumUndecodedBySdk { side: "source", reason });
    }
    let src_values = src_values_raw.as_array().ok_or(SkipReason::MalformedSource)?;
    let src_label = src_values.get(src_idx).ok_or(SkipReason::MalformedSource)?;

    let tgt_values_raw = target_live.get("values").ok_or(SkipReason::NoMatchingOption)?;
    if let Some(reason) = enum_values_sdk_decode_failure(tgt_values_raw) {
        return Err(SkipReason::EnumUndecodedBySdk { side: "target", reason });
    }
    let tgt_values = tgt_values_raw.as_array().ok_or(SkipReason::NoMatchingOption)?;

    let src_elem_type = parse_known_elem_type(source)?;
    let tgt_elem_type = parse_known_elem_type(target_live)?;
    // A source/target disagreement on packed-string vs. raw-code would
    // otherwise just fail to find a match below (a string label never
    // equals a numeric code) and get reported as an ordinary
    // NoMatchingOption — misleadingly implying the specific value is simply
    // absent, when the real cause is the two sides encoding this enum
    // differently. Only flag when BOTH sides have a known, disagreeing
    // elem_type — an absent one (None) isn't evidence of a real mismatch.
    if let (Some(s), Some(t)) = (src_elem_type, tgt_elem_type) {
        let s_is_label = s == PACKED_STRING_ELEM_TYPE;
        let t_is_label = t == PACKED_STRING_ELEM_TYPE;
        if s_is_label != t_is_label {
            return Err(SkipReason::EnumTypeMismatch { source_elem_type: s, target_elem_type: t });
        }
    }

    let (tgt_idx, strategy) = if src_elem_type == Some(PACKED_STRING_ELEM_TYPE) {
        // `src_label` above is one element of the stored array, which for the
        // real byte layout is a single character: not usable here. Match on
        // whole strings (see `packed_string_options`).
        let src_options = packed_string_options(source, "source")?;
        let tgt_options = packed_string_options(target_live, "target")?;
        let label = src_options.get(src_idx).ok_or(SkipReason::MalformedSource)?;
        if label == UNRESOLVED_LABEL {
            return Err(SkipReason::EnumUndecodedBySdk {
                side: "source",
                reason: format!("the SDK could not resolve the selected option's label ({UNRESOLVED_LABEL:?})"),
            });
        }
        let source_count = src_options.iter().filter(|o| *o == label).count();
        let target_count = tgt_options.iter().filter(|o| *o == label).count();
        if target_count == 0 {
            return Err(SkipReason::NoMatchingOption);
        }
        if source_count > 1 || target_count > 1 {
            return Err(SkipReason::AmbiguousLabel { label: label.clone(), source_count, target_count });
        }
        let idx = tgt_options.iter().position(|o| o == label).ok_or(SkipReason::NoMatchingOption)?;
        (idx, Strategy::EnumByLabel)
    } else {
        let idx = tgt_values.iter().position(|v| v == src_label).ok_or(SkipReason::NoMatchingOption)?;
        (idx, Strategy::EnumByRawValue)
    };

    // Carry the target's own elem_type/elem_count/elem_bytes/default forward
    // unchanged — only the index (the thing that actually varies per body)
    // is overwritten.
    let mut out = target_live.clone();
    out["value_index"] = json!(tgt_idx as u64);
    Ok((out, strategy))
}

/// Clamp a range value from `source` into the target's `[lower, upper]`,
/// snapping to the target's own step count if it has discrete steps.
fn translate_range(source: &Value, target_live: &Value) -> Result<(Value, Strategy), SkipReason> {
    let src_value = source.get("value").and_then(Value::as_f64).ok_or(SkipReason::MalformedSource)?;
    // A missing lower/upper here means the read itself succeeded but the
    // target's live value isn't actually range-shaped (e.g. this capability
    // is an enum on the target but a range on the source) — a structural
    // shape mismatch, not a communication failure, so the message says so
    // rather than implying the read failed.
    let shape_mismatch = || {
        SkipReason::TargetReadFailed(
            "target's live value is missing lower/upper — likely not a range capability on this camera"
                .to_string(),
        )
    };
    let lower = target_live.get("lower").and_then(Value::as_f64).ok_or_else(shape_mismatch)?;
    let upper = target_live.get("upper").and_then(Value::as_f64).ok_or_else(shape_mismatch)?;
    // Same strict treatment as lower/upper above, not a silent
    // unwrap_or(0): sdk::decode_value's DT_RANGE_PTR branch always
    // populates all three together on a real live read, so a genuinely
    // range-shaped target_live missing just `steps` would itself be a
    // signal something's wrong, not an ordinary "continuous range" case
    // (which is instead steps PRESENT with value 0 — as_u64() still
    // returns Some(0) for that, so this doesn't affect the normal
    // continuous-range path at all).
    let steps = target_live.get("steps").and_then(Value::as_u64).ok_or_else(shape_mismatch)?;

    let (lo, hi) = (lower.min(upper), lower.max(upper));
    let clamped = src_value.clamp(lo, hi);
    // steps>=2 guards against a divide-by-(steps-1)-that's-zero — but note
    // steps==1 degenerates to the same value_index=0 either way (the
    // formula's own `*(steps-1.0)` term already zeroes out at steps==1),
    // so this guard's exact boundary (>=2 vs >=1) has no observable effect
    // on output; kept at >=2 because it states the real invariant clearly
    // ("need at least 2 positions for an index to mean anything") rather
    // than relying on the formula's incidental degeneracy.
    let value_index = if steps >= 2 && upper != lower {
        (((clamped - lower) / (upper - lower)) * (steps as f64 - 1.0))
            .round()
            .clamp(0.0, (steps - 1) as f64) as u64
    } else {
        0
    };

    let mut out = target_live.clone();
    out["value"] = json!(clamped);
    out["value_index"] = json!(value_index);
    Ok((out, Strategy::RangeClamped))
}

/// Translate one property's source-camera value into the target's terms.
/// `target_live` is the target camera's current value for the *same*
/// capability code — only needed (and only read by the caller) for enum/
/// range shapes; pass `None` for a plain scalar, where it's never consulted.
pub fn translate_value(source_value: &Value, target_live: Option<&Value>) -> Result<(Value, Strategy), SkipReason> {
    if is_enum_shape(source_value) {
        let live = target_live
            .ok_or_else(|| SkipReason::TargetReadFailed("no live value provided for enum shape".to_string()))?;
        return translate_enum(source_value, live);
    }
    if is_range_shape(source_value) {
        let live = target_live
            .ok_or_else(|| SkipReason::TargetReadFailed("no live value provided for range shape".to_string()))?;
        return translate_range(source_value, live);
    }
    match source_value {
        Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok((source_value.clone(), Strategy::Literal)),
        _ => Err(SkipReason::UnsupportedValueShape),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Scope + capability-name guard
// ─────────────────────────────────────────────────────────────────────────

/// Capability codes whose symbolic name legitimately differs between Z-series
/// models while meaning the same setting. Each entry is one equivalence
/// class of names for one code.
///
/// 33200: `BracketingType` on most bodies, `AEBracketingType` on the Z 8 and
/// Z 9. Confirmed 2026-10-27 against live Z 9 (fw 5.31), Z6_3 (fw 2.00) and
/// Z 5 (fw 1.43): the Z 9 and Z6_3 offer the same option set {4..8} in
/// different orders (so a value, not an index, must be matched — which
/// `translate_enum` does), the Z 5 offers {0..8}, all three default to
/// code 4, and the config's own display label is "AEBracketing Type" on
/// the Z6_3, Z 8 and Z 9 alike. What each option code *means* in the menu
/// is not independently confirmed.
const KNOWN_NAME_ALIASES: &[(u32, &[&str])] =
    &[(33200, &["kNkMAIDCapability_BracketingType", "kNkMAIDCapability_AEBracketingType"])];

/// `transplant` was asked to use a model it has no verified support for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "cross-model transplant supports Z-series bodies only; the {role} model {model:?} is not a Z-series \
     section of the loaded MaidLayer.config (nothing was checked for it, and nothing was written)"
)]
pub struct UnsupportedModel {
    pub role: &'static str,
    pub model: String,
}

/// Is `model` a Z-series body with a section in `config`? A model with no
/// section at all (e.g. a D5) is not: there is nothing to check its codes
/// against. This is the one place the Z-series distinction lives; the
/// config's non-body sections (`DC`, `Default`, `DEMO95`, `DemoZ f`) and
/// DSLR sections (`D6`, `D780`) all fail the leading-`Z` test, and
/// `tests::real_config_z_series_set_is_pinned` pins the exact set so a
/// future config that breaks that convention fails loudly.
pub fn is_z_series_model(config: &MaidLayerConfig, model: &str) -> bool {
    model.starts_with('Z') && config.sections_for_model(model).next().is_some()
}

/// Per-transplant check that a capability code names the same setting on
/// the source and target models. Built once, before any write.
#[derive(Debug)]
pub struct CodeNameGuard {
    source: HashMap<u32, BTreeSet<String>>,
    target: HashMap<u32, BTreeSet<String>>,
}

impl CodeNameGuard {
    /// Refuses (without touching hardware) if either model isn't Z-series.
    /// The source is checked first so a bad snapshot is reported before a
    /// bad target.
    pub fn new(config: &MaidLayerConfig, source_model: &str, target_model: &str) -> Result<Self, UnsupportedModel> {
        for (role, model) in [("source", source_model), ("target", target_model)] {
            if !is_z_series_model(config, model) {
                return Err(UnsupportedModel { role, model: model.to_string() });
            }
        }
        Ok(CodeNameGuard {
            source: config.name_sets_for_model(source_model),
            target: config.name_sets_for_model(target_model),
        })
    }

    /// `Ok` if the code carries compatible names on both models — or if
    /// either model's section doesn't list the code at all, in which case
    /// the config has no verdict and the live capability list
    /// (`AbsentOnTarget`) and the value translation remain the checks, as
    /// before this guard existed. `Err(CapabilityNameMismatch)` only when
    /// both list it and the names are unrelated.
    pub fn check(&self, code: u32) -> Result<(), SkipReason> {
        let (Some(src), Some(tgt)) = (self.source.get(&code), self.target.get(&code)) else {
            return Ok(());
        };
        if names_compatible(code, src, tgt) {
            return Ok(());
        }
        Err(SkipReason::CapabilityNameMismatch { source_names: join_names(src), target_names: join_names(tgt) })
    }
}

fn join_names(names: &BTreeSet<String>) -> String {
    names.iter().map(String::as_str).collect::<Vec<_>>().join("/")
}

/// Compatible if the two models share at least one name for the code, or
/// every name either side lists is in one of `KNOWN_NAME_ALIASES`' classes
/// for that code.
fn names_compatible(code: u32, src: &BTreeSet<String>, tgt: &BTreeSet<String>) -> bool {
    if !src.is_disjoint(tgt) {
        return true;
    }
    KNOWN_NAME_ALIASES
        .iter()
        .filter(|(c, _)| *c == code)
        .any(|(_, class)| src.iter().chain(tgt).all(|n| class.contains(&n.as_str())))
}

// ─────────────────────────────────────────────────────────────────────────
// Orchestration — needs a live Device, so not unit-testable the way
// translate_value/translate_enum/translate_range are.
// ─────────────────────────────────────────────────────────────────────────

/// Capabilities `transplant` and `restore` never write, by code: they report what the
/// body or its session is doing right now (or lock it), not a setting a person
/// chose, so "make body B like body A" must not carry them across. A single
/// table is the source of truth; [`never_copied`] and the real-config test
/// both read it, so a code and its name cannot drift apart unnoticed.
///
/// Evidence is by name and by behaviour, not by anything in `MaidLayer.config`
/// (it does not classify state vs setting): `LiveViewStatus` and
/// `CurrentItemID` already failed with `SetCapability` -125 on a real Z6III
/// restore. Judgement calls are marked; add to the table rather than
/// special-casing elsewhere. (`restore` reads the same table via
/// [`never_copied`], so a full restore stops attempting these too.)
pub const NEVER_TRANSPLANT: &[(u32, &str, &str)] = &[
    (33089, "kNkMAIDCapability_LockCamera", "camera lock state; copying it could lock or unlock the target"),
    (33342, "kNkMAIDCapability_LiveViewStatus", "whether live view is running; rejected by a real Z6III write (-125)"),
    (33617, "kNkMAIDCapability_CurrentItemID", "the item currently selected in playback; rejected by a real Z6III write (-125)"),
    (33213, "kNkMAIDCapability_ExternalFlashStatus", "what is attached to the hot shoe right now"),
    (33876, "kNkMAIDCapability_RawJpegTransferStatus", "state of an in-progress transfer"),
    (34080, "kNkMAIDCapability_ChangeMonitorOffStatus", "monitor power state (judgement call: status by name)"),
    (33606, "kNkMAIDCapability_LiveViewPhotoShootingMode", "live-view session mode (judgement call: state by name)"),
    (33165, "kNkMAIDCapability_ShootingMode", "exposure mode, set by the body's mode dial (judgement call)"),
    (34147, "kNkMAIDCapability_Viewmode", "which display the body is showing (judgement call)"),
    (33108, "kNkMAIDCapability_ShootNoCard", "flipped on its own across a power cycle with nothing writing to the camera, so state-like or reset by the body (judgement call, hardware observation F10)"),
];

/// Why `code` is never transplanted, if it is on [`NEVER_TRANSPLANT`].
pub fn never_copied(code: u32) -> Option<&'static str> {
    NEVER_TRANSPLANT.iter().find(|(c, _, _)| *c == code).map(|(_, _, why)| *why)
}

/// Per-run choices that make `transplant` do more than its safe default.
#[derive(Debug, Clone, Default)]
pub struct TransplantOptions {
    /// Full property names (as keyed in the snapshot) the user has explicitly
    /// accepted: raw-code enum matches and numeric literal scalars, both of
    /// which rest on both bodies giving the same number the same meaning.
    /// Everything else of those kinds is skipped when it would change the
    /// target. (The field and flag keep their original `raw_code` name.)
    pub allow_raw_code: BTreeSet<String>,
}

/// Turn `--allow-raw-code` arguments into full property names, checked
/// against the snapshot being transplanted (a raw-code enum or a numeric
/// scalar, the two kinds the opt-in gates). Names are accepted with or without
/// the `kNkMAIDCapability_` prefix. An unknown name, or one of any other kind
/// (so the flag could do nothing for it), is an error: a typo
/// silently opting nothing in would look exactly like a safe run.
pub fn resolve_allow_raw_code(
    requested: &[String],
    properties: &BTreeMap<String, PropertyEntry>,
) -> Result<BTreeSet<String>, String> {
    const PREFIX: &str = "kNkMAIDCapability_";
    let mut out = BTreeSet::new();
    for r in requested {
        let full = if properties.contains_key(r.as_str()) { r.clone() } else { format!("{PREFIX}{r}") };
        let Some(prop) = properties.get(&full) else {
            return Err(format!("--allow-raw-code {r:?}: no property by that name in the source snapshot"));
        };
        let is_raw_enum = is_enum_shape(&prop.value)
            && prop.value.get("elem_type").and_then(Value::as_u64) != Some(PACKED_STRING_ELEM_TYPE);
        if !is_raw_enum && !prop.value.is_number() {
            return Err(format!(
                "--allow-raw-code {r:?}: {full} is not a raw-code enum or a numeric scalar (it is a label enum, \
                 a range, a string or a boolean), so the flag does not apply to it"
            ));
        }
        out.insert(full);
    }
    Ok(out)
}

/// The raw-code gate, separated from the device I/O so it is unit-testable.
///
/// A raw-code enum match rests on the unverified assumption that both bodies
/// number the same option the same way (the module docs call it a weaker
/// heuristic), so by default it is not written. Exception: a match that
/// resolves to the option the target already has is a no-op and passes without
/// opt-in. Labels are matched strictly stronger and never reach this gate.
fn raw_code_gate(
    strategy: Strategy,
    choice: Option<&ResolvedChoice>,
    allowed: bool,
) -> Result<(), SkipReason> {
    if strategy != Strategy::EnumByRawValue || allowed {
        return Ok(());
    }
    match choice {
        Some(c) if c.is_unchanged() => Ok(()),
        Some(c) => Err(SkipReason::RawCodeUnconfirmed { proposed: c.to_string() }),
        None => Err(SkipReason::RawCodeUnconfirmed { proposed: "(target's current value unknown)".to_string() }),
    }
}

/// Numeric equality that does not depend on the JSON number's spelling: `3`
/// and `3.0` are the same value, and two integers beyond 2^53 are compared
/// as integers, not rounded through `f64`.
fn numbers_equal(a: &serde_json::Number, b: &serde_json::Number) -> bool {
    if let (Some(x), Some(y)) = (a.as_i64(), b.as_i64()) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.as_u64(), b.as_u64()) {
        return x == y;
    }
    matches!((a.as_f64(), b.as_f64()), (Some(x), Some(y)) if x == y)
}

/// The numeric-literal gate, device-free like [`raw_code_gate`].
///
/// An integer is no more self-describing than a raw code (F9: `WBAutoType`,
/// `MovieRecMicrophone`, `ElectronicVR`), so a numeric scalar copied as-is is
/// not written when it would change the target, unless opted in. A value equal
/// to the target's current one is a no-op and passes. Strings and booleans
/// describe themselves and are never gated. `target_live` is `None` when the
/// current value could not be read (unreadable capability, failed read): the
/// change cannot be ruled out, so it is treated as one.
fn literal_gate(
    strategy: Strategy,
    source: &Value,
    target_live: Option<&Value>,
    opted_in: bool,
) -> Result<(), SkipReason> {
    let Some(src) = source.as_number().filter(|_| strategy == Strategy::Literal) else {
        return Ok(());
    };
    if opted_in {
        return Ok(());
    }
    match target_live {
        Some(Value::Number(cur)) if numbers_equal(src, cur) => Ok(()),
        Some(cur) => Err(SkipReason::LiteralUnconfirmed {
            proposed: format!("{src}, was {}", truncate_with_marker(cur.to_string(), 80)),
        }),
        None => Err(SkipReason::LiteralUnconfirmed {
            proposed: format!("{src} (target's current value unknown)"),
        }),
    }
}

pub struct TransplantOutcome {
    pub name: String,
    pub code: u32,
    pub result: Result<Strategy, SkipReason>,
    /// For an enum that was (or, in a dry run, would be) written: which option
    /// was picked and what the target has now. `None` for scalars, ranges and
    /// skipped properties.
    pub choice: Option<ResolvedChoice>,
}

/// The option an enum translation resolved to, in terms a person can check
/// against the camera's menu. `choice`/`current` are the option's label for
/// packed-string enums and `code N` for raw-code enums (which have no label).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedChoice {
    pub target_index: u64,
    pub choice: String,
    pub current_index: Option<u64>,
    pub current: Option<String>,
}

impl Serialize for ResolvedChoice {
    /// `unchanged` is derived so a consumer never re-implements the comparison.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        json!({
            "target_index": self.target_index,
            "choice": self.choice,
            "current_index": self.current_index,
            "current": self.current,
            "unchanged": self.is_unchanged(),
        })
        .serialize(s)
    }
}

impl Serialize for TransplantOutcome {
    /// `status` is `"copied"` (written, or would be in a dry run) or
    /// `"skipped"`. A copied outcome carries `strategy` (and `choice` for
    /// enums); a skipped one carries `reason`.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = serde_json::Map::new();
        m.insert("name".into(), json!(self.name));
        m.insert("code".into(), json!(self.code));
        match &self.result {
            Ok(strategy) => {
                m.insert("status".into(), json!("copied"));
                m.insert("strategy".into(), json!(strategy));
            }
            Err(reason) => {
                m.insert("status".into(), json!("skipped"));
                m.insert("reason".into(), json!(reason));
            }
        }
        if let Some(choice) = &self.choice {
            m.insert("choice".into(), json!(choice));
        }
        Value::Object(m).serialize(s)
    }
}

impl ResolvedChoice {
    /// Writing this would leave the target as it is.
    pub fn is_unchanged(&self) -> bool {
        self.current_index == Some(self.target_index)
    }
}

impl std::fmt::Display for ResolvedChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} (#{})", self.choice, self.target_index)?;
        match (&self.current, self.current_index) {
            _ if self.is_unchanged() => f.write_str(", unchanged"),
            (Some(cur), Some(i)) => write!(f, ", was {cur:?} (#{i})"),
            _ => Ok(()),
        }
    }
}

/// Describe the option `translated` selects, next to what `live` (the
/// target's current value) selects. `None` unless both are enum-shaped and
/// the strategy was an enum one.
pub fn resolve_choice(translated: &Value, live: &Value, strategy: Strategy) -> Option<ResolvedChoice> {
    let name_at = |v: &Value, idx: u64| -> Option<String> {
        match strategy {
            Strategy::EnumByLabel => packed_string_options(v, "target").ok()?.get(idx as usize).cloned(),
            Strategy::EnumByRawValue => {
                let code = v.get("values")?.as_array()?.get(idx as usize)?;
                Some(format!("code {code}"))
            }
            _ => None,
        }
    };
    let target_index = translated.get("value_index")?.as_u64()?;
    let current_index = live.get("value_index").and_then(Value::as_u64);
    Some(ResolvedChoice {
        target_index,
        choice: name_at(translated, target_index)?,
        current_index,
        current: current_index.and_then(|i| name_at(live, i)),
    })
}

/// Copy every property in `properties` (captured from a source camera that
/// may be a *different model* than `device`) onto `device`, translating
/// enum/range values via the target's own live equivalents.
///
/// `guard` (built with [`CodeNameGuard::new`], which also enforces the
/// Z-series-only scope) skips any capability whose code doesn't name the same
/// setting on the source and target models.
///
/// `dry_run` skips the actual write but still performs the live reads
/// needed to report what *would* happen — intentionally more work than
/// `fleet restore --dry-run`, which never touches the target beyond its
/// capability list, but that's the only way to report a real
/// `NoMatchingOption` skip before committing to anything.
pub fn transplant(
    device: &Device,
    properties: &BTreeMap<String, PropertyEntry>,
    guard: &CodeNameGuard,
    options: &TransplantOptions,
    dry_run: bool,
) -> Vec<TransplantOutcome> {
    let cap_map: HashMap<u32, &CapabilityInfo> = device.capabilities.iter().map(|c| (c.id, c)).collect();

    properties
        .iter()
        .map(|(name, prop)| {
            let opted_in = options.allow_raw_code.contains(name);
            match transplant_one(device, &cap_map, guard, prop, opted_in, dry_run) {
                Ok((strategy, choice)) => TransplantOutcome { name: name.clone(), code: prop.code, result: Ok(strategy), choice },
                Err(reason) => TransplantOutcome { name: name.clone(), code: prop.code, result: Err(reason), choice: None },
            }
        })
        .collect()
}

/// The device-free checks that must pass before any live read or write, in
/// order: the capability is not on [`NEVER_TRANSPLANT`], it exists on the target, its code names the same
/// setting on both models, and it's writable. Split out of `transplant_one`
/// so the ordering (and that the name guard is wired in at all) is
/// unit-testable without a `Device`.
fn precheck<'a>(
    cap_map: &HashMap<u32, &'a CapabilityInfo>,
    guard: &CodeNameGuard,
    prop: &PropertyEntry,
) -> Result<&'a CapabilityInfo, SkipReason> {
    if let Some(reason) = never_copied(prop.code) {
        return Err(SkipReason::NeverCopied { reason });
    }
    let cap = *cap_map.get(&prop.code).ok_or(SkipReason::AbsentOnTarget)?;
    guard.check(prop.code)?;
    if cap.operations & OP_SET == 0 {
        return Err(SkipReason::ReadOnly);
    }
    Ok(cap)
}

/// Everything `transplant_one` decides between reading the target and writing
/// it, with no device in sight: translate the source value, describe the
/// resolved option, and apply the raw-code and numeric-literal gates (one
/// opt-in, `opted_in`, covers both). Split out so that sequence (in particular
/// that the gates are applied at all) is unit-testable.
fn plan_property(
    source_value: &Value,
    target_live: Option<&Value>,
    opted_in: bool,
) -> Result<(Value, Strategy, Option<ResolvedChoice>), SkipReason> {
    let (translated, strategy) = translate_value(source_value, target_live)?;
    let choice = target_live.and_then(|l| resolve_choice(&translated, l, strategy));
    raw_code_gate(strategy, choice.as_ref(), opted_in)?;
    literal_gate(strategy, source_value, target_live, opted_in)?;
    Ok((translated, strategy, choice))
}

fn transplant_one(
    device: &Device,
    cap_map: &HashMap<u32, &CapabilityInfo>,
    guard: &CodeNameGuard,
    prop: &PropertyEntry,
    opted_in: bool,
    dry_run: bool,
) -> Result<(Strategy, Option<ResolvedChoice>), SkipReason> {
    let cap = precheck(cap_map, guard, prop)?;

    let needs_live_read = is_enum_shape(&prop.value) || is_range_shape(&prop.value);
    let live = if needs_live_read {
        Some(device.read_capability(prop.code).map_err(|e| SkipReason::TargetReadFailed(truncate_with_marker(e.to_string(), MAX_ERROR_MESSAGE_LEN)))?)
    } else if prop.value.is_number() && cap.operations & OP_GET != 0 {
        // The numeric-literal gate needs the target's current value to tell a
        // no-op from a change. A failed read is not a skip: the gate treats an
        // unknown current value as a change, and an opted-in write still goes.
        device.read_capability(prop.code).ok()
    } else {
        None
    };

    let (translated, strategy, choice) = plan_property(&prop.value, live.as_ref(), opted_in)?;

    if !dry_run {
        match device.write_capability(prop.code, cap.kind, &translated) {
            Ok(()) => {}
            Err(SdkError::UnsupportedWrite(_)) => return Err(SkipReason::UnsupportedWriteType),
            Err(e) => return Err(SkipReason::WriteFailed(truncate_with_marker(e.to_string(), MAX_ERROR_MESSAGE_LEN))),
        }
        if strategy == Strategy::RangeClamped {
            verify_range_write(device, prop.code, &translated)?;
        }
    }
    Ok((strategy, choice))
}

/// Read a just-written range capability back and confirm it landed close to
/// the intended value. `translate_range`'s discrete-step formula (mapping a
/// clamped value into the target's `value_index`) is an unverified
/// assumption about how the Nikon MAID SDK's step index actually works —
/// see the module docs. Rather than trust that assumption silently, spend
/// one extra live read per range write to catch it empirically: if the
/// camera reports back something meaningfully different from what was
/// intended, that's surfaced as a write failure instead of a silent,
/// possibly-wrong success.
fn verify_range_write(device: &Device, code: u32, translated: &Value) -> Result<(), SkipReason> {
    let intended = translated.get("value").and_then(Value::as_f64);
    let Some(intended) = intended else { return Ok(()) }; // translate_range always sets this; defensive only
    let readback = device.read_capability(code).map_err(|e| {
        SkipReason::WriteFailed(format!("write succeeded but read-back verification failed: {e}"))
    })?;
    let Some(actual) = readback.get("value").and_then(Value::as_f64) else {
        return Err(SkipReason::WriteFailed(
            "write succeeded but read-back value was not the expected range shape".to_string(),
        ));
    };
    // Generous tolerance: this only needs to catch the step-formula being
    // grossly wrong (e.g. landing on the wrong step entirely), not chase
    // float rounding noise.
    const TOLERANCE: f64 = 1e-6;
    if (actual - intended).abs() > TOLERANCE.max(intended.abs() * 1e-3) {
        return Err(SkipReason::WriteFailed(format!(
            "write succeeded but camera reports {actual} instead of the intended {intended} — \
             the range step-index formula may not match this camera's actual behavior"
        )));
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── shape detection ──────────────────────────────────────────────────

    #[test]
    fn enum_shape_detected() {
        let v = json!({"value_index": 0, "elem_type": 7, "elem_count": 2, "elem_bytes": 4, "values": ["Off", "On"]});
        assert!(is_enum_shape(&v));
        assert!(!is_range_shape(&v));
    }

    #[test]
    fn range_shape_detected() {
        let v = json!({"value": 1.0, "default": 0.0, "value_index": 5, "default_index": 0, "lower": -3.0, "upper": 3.0, "steps": 7});
        assert!(is_range_shape(&v));
        assert!(!is_enum_shape(&v));
    }

    #[test]
    fn scalar_is_neither_shape() {
        assert!(!is_enum_shape(&json!(true)));
        assert!(!is_range_shape(&json!(42)));
        assert!(!is_enum_shape(&json!("Standard")));
    }

    // ── translate_value: literal scalars ─────────────────────────────────

    #[test]
    fn literal_bool_copied_as_is() {
        let (v, s) = translate_value(&json!(true), None).unwrap();
        assert_eq!(v, json!(true));
        assert_eq!(s, Strategy::Literal);
    }

    #[test]
    fn literal_string_copied_as_is() {
        // The "copy copyright text literally" case from the motivating example.
        let (v, s) = translate_value(&json!("© Brian Wong"), None).unwrap();
        assert_eq!(v, json!("© Brian Wong"));
        assert_eq!(s, Strategy::Literal);
    }

    #[test]
    fn literal_number_copied_as_is() {
        let (v, s) = translate_value(&json!(56), None).unwrap();
        assert_eq!(v, json!(56));
        assert_eq!(s, Strategy::Literal);
    }

    #[test]
    fn null_is_unsupported_shape() {
        assert_eq!(translate_value(&Value::Null, None), Err(SkipReason::UnsupportedValueShape));
    }

    #[test]
    fn unrecognized_object_is_unsupported_shape() {
        // e.g. decode_value's {"_type": "datetime"} / {"_type": "array"} fallbacks.
        let v = json!({"_type": "datetime"});
        assert_eq!(translate_value(&v, None), Err(SkipReason::UnsupportedValueShape));
    }

    // ── translate_value: packed-string enum (elem_type 7) ────────────────

    fn packed_string_enum(value_index: u64, values: &[&str]) -> Value {
        json!({
            "value_index": value_index,
            "default_index": 0,
            "elem_count": values.len(),
            "elem_type": 7,
            "elem_bytes": 8,
            "values": values,
        })
    }

    #[test]
    fn enum_by_label_matches_at_different_index() {
        // The motivating example: source picked "High" at index 2 (out of a
        // 3-option list); target has the same label at index 1 (a 2-option
        // list) -- must match by label, not index.
        let source = packed_string_enum(2, &["Off", "Low", "High"]);
        let target_live = packed_string_enum(0, &["Off", "High"]);
        let (translated, strategy) = translate_value(&source, Some(&target_live)).unwrap();
        assert_eq!(strategy, Strategy::EnumByLabel);
        assert_eq!(translated["value_index"], json!(1));
        // Target's own elem_count/values must be preserved, not source's.
        assert_eq!(translated["elem_count"], json!(2));
        assert_eq!(translated["values"], json!(["Off", "High"]));
    }

    #[test]
    fn enum_by_label_same_index_when_lists_match() {
        let source = packed_string_enum(1, &["Off", "On"]);
        let target_live = packed_string_enum(0, &["Off", "On"]);
        let (translated, strategy) = translate_value(&source, Some(&target_live)).unwrap();
        assert_eq!(strategy, Strategy::EnumByLabel);
        assert_eq!(translated["value_index"], json!(1));
    }

    #[test]
    fn enum_no_matching_label_is_skipped() {
        // Target genuinely lacks this option (a body-specific choice).
        let source = packed_string_enum(0, &["Extra High"]);
        let target_live = packed_string_enum(0, &["Off", "Low", "High"]);
        assert_eq!(
            translate_value(&source, Some(&target_live)),
            Err(SkipReason::NoMatchingOption)
        );
    }

    #[test]
    fn enum_requires_target_live_value() {
        let source = packed_string_enum(0, &["Off"]);
        assert!(matches!(translate_value(&source, None), Err(SkipReason::TargetReadFailed(_))));
    }

    // ── packed-string enums: the real byte layout ────────────────────────
    //
    // `fleet snapshot` stores these as one array element per BYTE, "" for each
    // NUL, with elem_bytes == 1 and value_index counting STRINGS. The tests
    // above use whole-string elements with elem_bytes 8 (the stride layout),
    // which never occurs on a real camera and so could not catch the
    // single-character matching bug these tests pin.

    /// Build a value the way the SDK decode stores a real packed-string enum.
    fn byte_layout(value_index: u64, labels: &[&str]) -> Value {
        let mut values: Vec<String> = Vec::new();
        for l in labels {
            values.extend(l.chars().map(|c| c.to_string()));
            values.push(String::new());
        }
        json!({
            "value_index": value_index, "default_index": 0, "elem_count": values.len(),
            "elem_type": 7, "elem_bytes": 1, "values": values,
        })
    }

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/packed_string_enums_z9_z6iii.json")).unwrap()
    }

    fn fx(body: &str, name: &str) -> Value {
        fixture()[body][format!("kNkMAIDCapability_{name}")].clone()
    }

    #[test]
    fn packed_string_options_rebuilds_real_capture_strings() {
        let opts = packed_string_options(&fx("z9", "CompressionLevel"), "source").unwrap();
        assert_eq!(opts.len(), 13, "178 bytes hold 13 NUL-terminated labels: {opts:?}");
        assert_eq!(opts[6], "RAW");
        // value_index counts strings, not bytes: the old bug read values[6] == "a".
        assert_eq!(fx("z9", "CompressionLevel")["values"][6], json!("a"));
    }

    #[test]
    fn real_z9_to_z6iii_label_enums_resolve_to_the_right_option() {
        // Expected target index per property, worked out by reading both
        // bodies' labels (None = the Z6III has no such option). The old code
        // wrote a different index for CompressionLevel (48, beyond its 13
        // options), FlashISOAutoHighLimit, ISOAutoHiLimit, ISOAutoShutterTime,
        // MovieISOAutoHiLimit, MovieSensitivity, MovieWBMode and Sensitivity.
        let expected: &[(&str, Option<u64>)] = &[
            ("AFAreaSelector", Some(0)),
            ("AFcPriority", Some(0)),
            ("AFsPriority", Some(0)),
            ("BracketingFactor", Some(0)),
            ("BracketingOrder", Some(0)),
            ("BracketingVary", Some(1)),
            ("CWMeteringDiameter", Some(1)),
            ("CompressionLevel", Some(6)),
            ("FlashISOAutoHighLimit", Some(30)),
            ("ISOAutoHiLimit", Some(21)),
            ("ISOAutoShutterTime", Some(46)),
            ("MovieISOAutoHiLimit", Some(21)),
            ("MovieSensitivity", Some(0)),
            ("MovieWBMode", Some(14)),
            ("Sensitivity", None), // Z 9 ISO 64 does not exist on a Z6III
            ("Slot2JpegImageSize", Some(1)),
            ("WBMode", Some(0)),
        ];
        for (name, want) in expected {
            let got = translate_value(&fx("z9", name), Some(&fx("z6iii", name)));
            match want {
                Some(idx) => {
                    let (v, strategy) = got.unwrap_or_else(|e| panic!("{name}: {e:?}"));
                    assert_eq!(strategy, Strategy::EnumByLabel, "{name}");
                    assert_eq!(v["value_index"], json!(idx), "{name}");
                    // Target's own list, count and layout are carried forward.
                    assert_eq!(v["values"], fx("z6iii", name)["values"], "{name}");
                    assert_eq!(v["elem_count"], fx("z6iii", name)["elem_count"], "{name}");
                    assert_eq!(v["elem_bytes"], json!(1), "{name}");
                }
                None => assert_eq!(got, Err(SkipReason::NoMatchingOption), "{name}"),
            }
        }
    }

    #[test]
    fn real_capture_movie_sensitivity_stays_iso_100_not_200() {
        // Both bodies are on "100" (Z 9 index 2, Z6III index 0). Single-char
        // matching picked the first NUL separator on the target = index 3 =
        // "200".
        let (v, _) = translate_value(&fx("z9", "MovieSensitivity"), Some(&fx("z6iii", "MovieSensitivity"))).unwrap();
        let opts = packed_string_options(&v, "target").unwrap();
        assert_eq!(opts[v["value_index"].as_u64().unwrap() as usize], "100");
    }

    #[test]
    fn real_capture_unique_label_in_a_list_with_duplicates_still_matches() {
        // ISOControlSensitivity lists many ISO values twice, but "25600" is
        // unique on both bodies, so it resolves (Z 9 index 57 -> Z6III 53).
        let (v, _) =
            translate_value(&fx("z9", "ISOControlSensitivity"), Some(&fx("z6iii", "ISOControlSensitivity"))).unwrap();
        assert_eq!(v["value_index"], json!(53));
    }

    #[test]
    fn real_capture_duplicated_label_is_ambiguous_not_first_match() {
        // Z 9 index 10 is "125", which appears twice in both lists.
        let mut src = fx("z9", "ISOControlSensitivity");
        src["value_index"] = json!(10);
        assert_eq!(
            translate_value(&src, Some(&fx("z6iii", "ISOControlSensitivity"))),
            Err(SkipReason::AmbiguousLabel { label: "125".into(), source_count: 2, target_count: 2 })
        );
    }

    #[test]
    fn real_capture_unresolved_label_placeholders_never_match() {
        // Z6III EVInterval's three options are all "Resource String not found".
        // Target side: the Z 9's real "1/3 Step" has no match (not "first
        // placeholder").
        assert_eq!(
            translate_value(&fx("z9", "EVInterval"), Some(&fx("z6iii", "EVInterval"))),
            Err(SkipReason::NoMatchingOption)
        );
        // Source side: a placeholder label identifies nothing, so it is
        // reported as the SDK's failure, not matched against the target's
        // own first placeholder-or-label.
        assert!(matches!(
            translate_value(&fx("z6iii", "EVInterval"), Some(&fx("z9", "EVInterval"))),
            Err(SkipReason::EnumUndecodedBySdk { side: "source", .. })
        ));
        // A list that is mostly placeholders still matches its one real label.
        let (v, _) = translate_value(&fx("z9", "BracketingFactor"), Some(&fx("z6iii", "BracketingFactor"))).unwrap();
        assert_eq!(v["value_index"], json!(0));
        let mut other = fx("z9", "BracketingFactor");
        other["value_index"] = json!(1); // a real Z 9 label the Z6III cannot show
        assert_eq!(translate_value(&other, Some(&fx("z6iii", "BracketingFactor"))), Err(SkipReason::NoMatchingOption));
    }

    #[test]
    fn byte_layout_index_boundaries() {
        let target = byte_layout(0, &["Off", "Low", "High"]);
        // last string resolves; one past it is malformed (never a panic or an
        // arbitrary byte).
        assert_eq!(translate_value(&byte_layout(2, &["Off", "Low", "High"]), Some(&target)).unwrap().0["value_index"], json!(2));
        assert_eq!(
            translate_value(&byte_layout(3, &["Off", "Low", "High"]), Some(&target)),
            Err(SkipReason::MalformedSource)
        );
        // index 0 of a one-option list.
        assert_eq!(translate_value(&byte_layout(0, &["Off"]), Some(&target)).unwrap().0["value_index"], json!(0));
    }

    #[test]
    fn byte_layout_different_position_and_missing_label() {
        let target = byte_layout(0, &["Off", "High"]);
        assert_eq!(translate_value(&byte_layout(2, &["Off", "Low", "High"]), Some(&target)).unwrap().0["value_index"], json!(1));
        assert_eq!(translate_value(&byte_layout(1, &["Off", "Low", "High"]), Some(&target)), Err(SkipReason::NoMatchingOption));
    }

    #[test]
    fn byte_layout_label_that_is_a_prefix_of_another_does_not_match_it() {
        // "On" must not match "Online" or a stray "O"/"n" byte.
        let target = byte_layout(0, &["Online", "O", "n"]);
        assert_eq!(translate_value(&byte_layout(0, &["On"]), Some(&target)), Err(SkipReason::NoMatchingOption));
    }

    #[test]
    fn byte_layout_final_string_without_terminating_nul_still_counts() {
        let mut v = byte_layout(1, &["Off", "On"]);
        v["values"].as_array_mut().unwrap().pop(); // drop the final NUL
        let target = byte_layout(0, &["On", "Off"]);
        assert_eq!(translate_value(&v, Some(&target)).unwrap().0["value_index"], json!(0));
    }

    #[test]
    fn byte_layout_empty_label_is_pinned_as_an_ordinary_label() {
        // Consecutive NULs make an empty string. None exist in the real
        // captures; pinned so a change is deliberate. One empty option on each
        // side matches; two on a side would be ambiguous like any duplicate.
        let src = byte_layout(1, &["A", "", "B"]);
        assert_eq!(translate_value(&src, Some(&byte_layout(0, &["B", "", "A"]))).unwrap().0["value_index"], json!(1));
        assert!(matches!(
            translate_value(&src, Some(&byte_layout(0, &["", "", "A"]))),
            Err(SkipReason::AmbiguousLabel { .. })
        ));
    }

    #[test]
    fn byte_layout_label_duplicated_only_on_the_source_side_is_ambiguous() {
        // Which "A" the source meant is unknowable even though the target has
        // exactly one, so it must not be matched on the strength of the target
        // list alone.
        let src = byte_layout(0, &["A", "B", "A"]);
        let target = byte_layout(0, &["C", "A"]);
        assert_eq!(
            translate_value(&src, Some(&target)),
            Err(SkipReason::AmbiguousLabel { label: "A".into(), source_count: 2, target_count: 1 })
        );
    }

    #[test]
    fn packed_string_enum_without_usable_elem_bytes_is_rejected_not_guessed() {
        let good = byte_layout(0, &["Off"]);
        for bad_bytes in [json!(null), json!(0), json!("1")] {
            let mut src = good.clone();
            src["elem_bytes"] = bad_bytes.clone();
            assert_eq!(translate_value(&src, Some(&good)), Err(SkipReason::MalformedSource), "source {bad_bytes}");
            let mut tgt = good.clone();
            tgt["elem_bytes"] = bad_bytes.clone();
            assert!(
                matches!(translate_value(&good, Some(&tgt)), Err(SkipReason::EnumUndecodedBySdk { side: "target", .. })),
                "target {bad_bytes}"
            );
        }
        let mut absent = good.clone();
        absent.as_object_mut().unwrap().remove("elem_bytes");
        assert_eq!(translate_value(&absent, Some(&good)), Err(SkipReason::MalformedSource));
    }

    #[test]
    fn old_snapshot_with_empty_label_array_is_malformed_not_matched() {
        // June 2026 snapshots stored `values: []` for every packed-string enum.
        let mut old = byte_layout(0, &["Off"]);
        old["values"] = json!([]);
        assert_eq!(translate_value(&old, Some(&byte_layout(0, &["Off"]))), Err(SkipReason::MalformedSource));
    }

    // ── never-copied state properties ────────────────────────────────────

    #[test]
    fn never_copied_covers_every_table_entry_and_nothing_adjacent() {
        for (code, name, why) in NEVER_TRANSPLANT {
            assert_eq!(never_copied(*code), Some(*why), "{name}");
            // Boundaries either side of each code are not caught by accident.
            for neighbour in [code - 1, code + 1] {
                if !NEVER_TRANSPLANT.iter().any(|(c, _, _)| *c == neighbour) {
                    assert_eq!(never_copied(neighbour), None, "{neighbour} next to {name}");
                }
            }
        }
        assert_eq!(never_copied(0), None);
        assert_eq!(never_copied(u32::MAX), None);
    }

    #[test]
    fn never_transplant_table_has_unique_codes_and_names_and_a_reason() {
        let codes: BTreeSet<u32> = NEVER_TRANSPLANT.iter().map(|(c, _, _)| *c).collect();
        let names: BTreeSet<&str> = NEVER_TRANSPLANT.iter().map(|(_, n, _)| *n).collect();
        assert_eq!(codes.len(), NEVER_TRANSPLANT.len(), "duplicate code");
        assert_eq!(names.len(), NEVER_TRANSPLANT.len(), "duplicate name");
        assert!(NEVER_TRANSPLANT.iter().all(|(_, n, w)| n.starts_with("kNkMAIDCapability_") && !w.is_empty()));
    }

    #[test]
    fn real_config_never_transplant_names_match_their_codes() {
        // Each (code, name) pair must exist as written in at least one Z-series
        // section, so a mistyped code cannot leave the intended property
        // unprotected while an unrelated one is blocked.
        let Some(c) = real_config() else { return };
        let z: Vec<String> = c.known_models().into_iter().filter(|m| is_z_series_model(&c, m)).collect();
        for (code, name, _) in NEVER_TRANSPLANT {
            let found = z.iter().any(|m| c.name_sets_for_model(m).get(code).is_some_and(|names| names.contains(*name)));
            assert!(found, "{name} is not named at code {code} in any Z-series section");
        }
    }

    #[test]
    fn precheck_never_copied_wins_over_absent_and_over_name_mismatch() {
        let denied = NEVER_TRANSPLANT[0].0;
        let c = parse_config(&[("Z 9", &[("Aperture", denied)]), ("Z 8", &[("ShutterSpeed", denied)])]);
        let g = CodeNameGuard::new(&c, "Z 9", "Z 8").unwrap();
        // Absent from the target AND unrelated names: the policy skip is what is reported.
        let caps: HashMap<u32, &CapabilityInfo> = HashMap::new();
        assert!(matches!(precheck(&caps, &g, &prop_for(denied)), Err(SkipReason::NeverCopied { .. })));
        // Even present and writable on the target, it is still refused.
        let ok = cap_info(denied, OP_SET);
        let caps: HashMap<u32, &CapabilityInfo> = HashMap::from([(denied, &ok)]);
        assert!(matches!(precheck(&caps, &g, &prop_for(denied)), Err(SkipReason::NeverCopied { .. })));
    }

    #[test]
    fn shoot_no_card_is_never_copied_and_its_neighbours_are_not() {
        // 33108 flipped across a power cycle with nothing writing (F10). Pin
        // the code and both sides of it: a typo'd code would protect the wrong
        // property while this one stayed writable.
        assert!(never_copied(33108).is_some_and(|why| why.contains("power cycle")));
        assert_eq!(never_copied(33107), None);
        assert_eq!(never_copied(33109), None);
        assert!(NEVER_TRANSPLANT.iter().any(|(c, n, _)| *c == 33108 && *n == "kNkMAIDCapability_ShootNoCard"));
    }

    #[test]
    fn precheck_refuses_shoot_no_card_even_when_present_and_writable() {
        let c = parse_config(&[("Z 9", &[("ShootNoCard", 33108)]), ("Z 8", &[("ShootNoCard", 33108)])]);
        let g = CodeNameGuard::new(&c, "Z 9", "Z 8").unwrap();
        let ok = cap_info(33108, OP_SET);
        let caps: HashMap<u32, &CapabilityInfo> = HashMap::from([(33108, &ok)]);
        assert!(matches!(precheck(&caps, &g, &prop_for(33108)), Err(SkipReason::NeverCopied { .. })));
    }

    #[test]
    fn precheck_does_not_block_an_unlisted_code() {
        let c = parse_config(&[("Z 9", &[("Aperture", 7)]), ("Z 8", &[("Aperture", 7)])]);
        let g = CodeNameGuard::new(&c, "Z 9", "Z 8").unwrap();
        let ok = cap_info(7, OP_SET);
        let caps: HashMap<u32, &CapabilityInfo> = HashMap::from([(7, &ok)]);
        assert_eq!(precheck(&caps, &g, &prop_for(7)).unwrap().id, 7);
    }

    #[test]
    fn never_copied_display_states_the_reason() {
        let msg = SkipReason::NeverCopied { reason: "camera lock state" }.to_string();
        assert!(msg.contains("never copied") && msg.contains("camera lock state"), "{msg}");
    }

    // ── raw-code opt-in gate ─────────────────────────────────────────────

    fn choice(target: u64, current: Option<u64>) -> ResolvedChoice {
        ResolvedChoice {
            target_index: target,
            choice: format!("code {target}"),
            current_index: current,
            current: current.map(|c| format!("code {c}")),
        }
    }

    #[test]
    fn raw_code_gate_blocks_a_changing_match_unless_allowed() {
        let changing = choice(2, Some(1));
        let err = raw_code_gate(Strategy::EnumByRawValue, Some(&changing), false).unwrap_err();
        assert_eq!(err, SkipReason::RawCodeUnconfirmed { proposed: "\"code 2\" (#2), was \"code 1\" (#1)".into() });
        assert_eq!(raw_code_gate(Strategy::EnumByRawValue, Some(&changing), true), Ok(()));
    }

    #[test]
    fn raw_code_gate_lets_a_no_op_through_without_opt_in() {
        // Boundary: same index as the target already has => writing it back
        // changes nothing, so no confirmation is needed.
        assert_eq!(raw_code_gate(Strategy::EnumByRawValue, Some(&choice(3, Some(3))), false), Ok(()));
        // One index off is a change.
        assert!(raw_code_gate(Strategy::EnumByRawValue, Some(&choice(3, Some(4))), false).is_err());
    }

    #[test]
    fn raw_code_gate_with_unknown_current_value_is_blocked_not_assumed_unchanged() {
        assert!(raw_code_gate(Strategy::EnumByRawValue, Some(&choice(3, None)), false).is_err());
        assert!(raw_code_gate(Strategy::EnumByRawValue, None, false).is_err());
    }

    #[test]
    fn raw_code_gate_never_applies_to_other_strategies() {
        for st in [Strategy::EnumByLabel, Strategy::Literal, Strategy::RangeClamped] {
            assert_eq!(raw_code_gate(st, Some(&choice(2, Some(1))), false), Ok(()), "{st:?}");
            assert_eq!(raw_code_gate(st, None, false), Ok(()), "{st:?}");
        }
    }

    #[test]
    fn raw_code_gate_end_to_end_through_translate_and_resolve() {
        let src = raw_code_enum(1, &[3, 5]); // source picked code 5
        let live = raw_code_enum(0, &[5, 3]); // target currently on index 0 (code 5): already equal
        let (t, st) = translate_value(&src, Some(&live)).unwrap();
        let c = resolve_choice(&t, &live, st).unwrap();
        assert_eq!(raw_code_gate(st, Some(&c), false), Ok(()));

        let live2 = raw_code_enum(1, &[5, 3]); // target on code 3: would change
        let (t, st) = translate_value(&src, Some(&live2)).unwrap();
        let c = resolve_choice(&t, &live2, st).unwrap();
        assert!(matches!(raw_code_gate(st, Some(&c), false), Err(SkipReason::RawCodeUnconfirmed { .. })));
    }

    #[test]
    fn raw_code_unconfirmed_display_names_the_flag_and_the_proposal() {
        let msg = SkipReason::RawCodeUnconfirmed { proposed: "code 5 (#0)".into() }.to_string();
        assert!(msg.contains("--allow-raw-code") && msg.contains("code 5 (#0)"), "{msg}");
    }

    #[test]
    fn plan_property_applies_the_raw_code_gate_and_returns_what_would_be_written() {
        let src = raw_code_enum(1, &[3, 5]);
        let live = raw_code_enum(1, &[5, 3]); // on code 3; source wants code 5 => changes
        let blocked = plan_property(&src, Some(&live), false).unwrap_err();
        assert!(matches!(blocked, SkipReason::RawCodeUnconfirmed { .. }), "{blocked:?}");

        let (translated, strategy, choice) = plan_property(&src, Some(&live), true).unwrap();
        assert_eq!(strategy, Strategy::EnumByRawValue);
        assert_eq!(translated["value_index"], json!(0));
        assert_eq!(choice.unwrap().target_index, 0);
    }

    #[test]
    fn plan_property_leaves_labels_scalars_and_ranges_ungated() {
        let (_, st, ch) =
            plan_property(&fx("z9", "BracketingVary"), Some(&fx("z6iii", "BracketingVary")), false).unwrap();
        assert_eq!(st, Strategy::EnumByLabel);
        assert!(!ch.unwrap().is_unchanged(), "a label match that changes the target needs no opt-in");
        assert_eq!(plan_property(&json!("© me"), None, false).unwrap().1, Strategy::Literal);
        let range = json!({"value": 1.0, "default": 0.0, "value_index": 1, "default_index": 0, "lower": 0.0, "upper": 2.0, "steps": 3});
        assert_eq!(plan_property(&range, Some(&range), false).unwrap().1, Strategy::RangeClamped);
    }

    // ── structured output: closed tags, typed detail ─────────────────────

    fn every_skip_reason() -> Vec<SkipReason> {
        vec![
            SkipReason::AbsentOnTarget,
            SkipReason::ReadOnly,
            SkipReason::UnsupportedWriteType,
            SkipReason::NoMatchingOption,
            SkipReason::UnsupportedValueShape,
            SkipReason::TargetReadFailed("usb timeout".into()),
            SkipReason::MalformedSource,
            SkipReason::WriteFailed("-125".into()),
            SkipReason::EnumUndecodedBySdk { side: "source", reason: "_invalid_physical_bytes".into() },
            SkipReason::EnumTypeMismatch { source_elem_type: 7, target_elem_type: 2 },
            SkipReason::CapabilityNameMismatch { source_names: "A".into(), target_names: "B/C".into() },
            SkipReason::AmbiguousLabel { label: "125".into(), source_count: 2, target_count: 1 },
            SkipReason::NeverCopied { reason: "lock state" },
            SkipReason::RawCodeUnconfirmed { proposed: "code 5 (#2)".into() },
            SkipReason::LiteralUnconfirmed { proposed: "5, was 3".into() },
        ]
    }

    #[test]
    fn skip_reason_tags_are_the_documented_closed_set() {
        // The doc table in docs/mcp-plan.md §5.1 is this list; the exhaustive
        // match in `tag()` is what forces a new variant to appear here.
        let tags: Vec<&str> = every_skip_reason().iter().map(SkipReason::tag).collect();
        assert_eq!(
            tags,
            [
                "absent_on_target", "read_only", "unsupported_write_type", "no_matching_option",
                "unsupported_value_shape", "target_read_failed", "malformed_source", "write_failed",
                "enum_undecoded_by_sdk", "enum_type_mismatch", "capability_name_mismatch",
                "ambiguous_label", "never_copied", "raw_code_unconfirmed", "literal_unconfirmed",
            ]
        );
        let distinct: BTreeSet<&str> = tags.iter().copied().collect();
        assert_eq!(distinct.len(), tags.len(), "duplicate tag");
        assert!(tags.iter().all(|t| t.chars().all(|c| c.is_ascii_lowercase() || c == '_')), "snake_case only");
    }

    #[test]
    fn strategy_tags_are_snake_case_and_distinct_from_the_cli_text() {
        let all = [Strategy::Literal, Strategy::EnumByLabel, Strategy::EnumByRawValue, Strategy::RangeClamped];
        let tags: Vec<&str> = all.iter().map(Strategy::tag).collect();
        assert_eq!(tags, ["literal", "enum_by_label", "enum_by_raw_value", "range_clamped"]);
        for s in all {
            assert_eq!(serde_json::to_value(s).unwrap(), json!(s.tag()));
        }
        // The hyphenated CLI form is unchanged.
        assert_eq!(Strategy::EnumByLabel.to_string(), "enum-by-label");
    }

    #[test]
    fn skip_reason_severity_matches_the_plan_table() {
        use Severity::*;
        let got: Vec<(&str, Severity)> = every_skip_reason().iter().map(|r| (r.tag(), r.severity())).collect();
        assert_eq!(
            got,
            [
                ("absent_on_target", Routine), ("read_only", Routine), ("unsupported_write_type", Routine),
                ("no_matching_option", Warn), ("unsupported_value_shape", Warn), ("target_read_failed", Warn),
                ("malformed_source", Warn), ("write_failed", Error), ("enum_undecoded_by_sdk", Warn),
                ("enum_type_mismatch", Warn), ("capability_name_mismatch", Warn), ("ambiguous_label", Warn),
                ("never_copied", Policy), ("raw_code_unconfirmed", Policy), ("literal_unconfirmed", Policy),
            ]
        );
    }

    #[test]
    fn skip_reason_json_has_tag_severity_and_typed_detail_only_where_one_exists() {
        let j = |r: SkipReason| serde_json::to_value(r).unwrap();
        // No detail field at all (not null) for the variants that carry none.
        assert_eq!(j(SkipReason::ReadOnly), json!({"tag": "read_only", "severity": "routine"}));
        assert_eq!(
            j(SkipReason::WriteFailed("-125".into())),
            json!({"tag": "write_failed", "severity": "error", "detail": {"message": "-125"}})
        );
        assert_eq!(
            j(SkipReason::EnumTypeMismatch { source_elem_type: 7, target_elem_type: 2 }),
            json!({"tag": "enum_type_mismatch", "severity": "warn",
                   "detail": {"source_elem_type": 7, "target_elem_type": 2}})
        );
        assert_eq!(
            j(SkipReason::AmbiguousLabel { label: "125".into(), source_count: 2, target_count: 1 }),
            json!({"tag": "ambiguous_label", "severity": "warn",
                   "detail": {"label": "125", "source_count": 2, "target_count": 1}})
        );
        assert_eq!(
            j(SkipReason::LiteralUnconfirmed { proposed: "5, was 3".into() }),
            json!({"tag": "literal_unconfirmed", "severity": "policy", "detail": {"proposed": "5, was 3"}})
        );
    }

    #[test]
    fn skip_reason_json_never_contains_the_display_text() {
        for r in every_skip_reason() {
            let json_text = serde_json::to_string(&r).unwrap();
            let display = r.to_string();
            assert!(!json_text.contains(&display), "{}: Display leaked into JSON", r.tag());
        }
    }

    #[test]
    fn skip_reason_detail_passes_awkward_text_through_unmangled() {
        // Quotes, newlines and non-ASCII in SDK error text must survive as data.
        let msg = "bad \"value\"\nline 2 – café";
        let v = serde_json::to_value(SkipReason::TargetReadFailed(msg.into())).unwrap();
        assert_eq!(v["detail"]["message"], json!(msg));
        assert_eq!(v["tag"], json!("target_read_failed"));
    }

    #[test]
    fn resolved_choice_json_derives_unchanged_at_the_boundary() {
        let c = |target, current: Option<u64>| ResolvedChoice {
            target_index: target,
            choice: "AE Only".into(),
            current_index: current,
            current: current.map(|i| format!("opt{i}")),
        };
        assert_eq!(serde_json::to_value(c(1, Some(1))).unwrap()["unchanged"], json!(true));
        assert_eq!(serde_json::to_value(c(1, Some(0))).unwrap()["unchanged"], json!(false));
        // Unknown current value is not "unchanged".
        let unknown = serde_json::to_value(c(1, None)).unwrap();
        assert_eq!(unknown["unchanged"], json!(false));
        assert_eq!(unknown["current_index"], json!(null));
        assert_eq!(unknown["current"], json!(null));
    }

    #[test]
    fn outcome_json_for_copied_and_skipped() {
        let copied = TransplantOutcome {
            name: "kNkMAIDCapability_BracketingVary".into(),
            code: 0x8029,
            result: Ok(Strategy::EnumByLabel),
            choice: Some(ResolvedChoice {
                target_index: 1,
                choice: "AE Only".into(),
                current_index: Some(0),
                current: Some("AE & Flash".into()),
            }),
        };
        assert_eq!(
            serde_json::to_value(&copied).unwrap(),
            json!({
                "name": "kNkMAIDCapability_BracketingVary", "code": 0x8029, "status": "copied",
                "strategy": "enum_by_label",
                "choice": {"target_index": 1, "choice": "AE Only", "current_index": 0,
                           "current": "AE & Flash", "unchanged": false}
            })
        );
        let skipped = TransplantOutcome {
            name: "kNkMAIDCapability_LockCamera".into(),
            code: 33089,
            result: Err(SkipReason::NeverCopied { reason: "lock state" }),
            choice: None,
        };
        let v = serde_json::to_value(&skipped).unwrap();
        assert_eq!(v["status"], json!("skipped"));
        assert_eq!(v["reason"]["tag"], json!("never_copied"));
        assert!(v.get("strategy").is_none() && v.get("choice").is_none(), "{v}");
    }

    // ── numeric-literal gate ─────────────────────────────────────────────

    fn lit_gate(src: Value, live: Option<Value>, opted_in: bool) -> Result<(), SkipReason> {
        literal_gate(Strategy::Literal, &src, live.as_ref(), opted_in)
    }

    fn proposed(r: Result<(), SkipReason>) -> String {
        match r {
            Err(SkipReason::LiteralUnconfirmed { proposed }) => proposed,
            other => panic!("expected LiteralUnconfirmed, got {other:?}"),
        }
    }

    #[test]
    fn literal_gate_blocks_a_changing_number_and_names_both_values() {
        assert_eq!(proposed(lit_gate(json!(5), Some(json!(3)), false)), "5, was 3");
    }

    #[test]
    fn literal_gate_passes_a_number_equal_to_the_targets_current_value() {
        assert_eq!(lit_gate(json!(3), Some(json!(3)), false), Ok(()));
    }

    #[test]
    fn literal_gate_equality_ignores_how_the_number_is_spelled() {
        // 3 vs 3.0, and zero vs negative zero, are the same value.
        assert_eq!(lit_gate(json!(3), Some(json!(3.0)), false), Ok(()));
        assert_eq!(lit_gate(json!(3.0), Some(json!(3)), false), Ok(()));
        assert_eq!(lit_gate(json!(0), Some(json!(-0.0)), false), Ok(()));
        assert_eq!(lit_gate(json!(0.5), Some(json!(0.5)), false), Ok(()));
    }

    #[test]
    fn literal_gate_neighbouring_values_are_a_change() {
        // Just below / above the target's value: both change it.
        assert!(lit_gate(json!(2), Some(json!(3)), false).is_err());
        assert!(lit_gate(json!(4), Some(json!(3)), false).is_err());
        assert!(lit_gate(json!(-1), Some(json!(1)), false).is_err());
        assert_eq!(lit_gate(json!(-1), Some(json!(-1)), false), Ok(()));
        assert!(lit_gate(json!(0.5), Some(json!(0.25)), false).is_err());
    }

    #[test]
    fn literal_gate_compares_big_integers_exactly_not_through_f64() {
        // 2^53 + 1 and 2^53 collapse to the same f64.
        let (a, b) = (9007199254740993u64, 9007199254740992u64);
        assert!(lit_gate(json!(a), Some(json!(b)), false).is_err());
        assert_eq!(lit_gate(json!(a), Some(json!(a)), false), Ok(()));
        assert!(lit_gate(json!(u64::MAX), Some(json!(u64::MAX - 1)), false).is_err());
        assert_eq!(lit_gate(json!(i64::MIN), Some(json!(i64::MIN)), false), Ok(()));
    }

    #[test]
    fn literal_gate_opt_in_passes_a_change_and_an_unknown_target() {
        assert_eq!(lit_gate(json!(5), Some(json!(3)), true), Ok(()));
        assert_eq!(lit_gate(json!(5), None, true), Ok(()));
    }

    #[test]
    fn literal_gate_unknown_current_value_counts_as_a_change() {
        assert_eq!(proposed(lit_gate(json!(5), None, false)), "5 (target's current value unknown)");
    }

    #[test]
    fn literal_gate_a_non_number_current_value_is_a_change_not_a_match() {
        // "3" is not 3; null and an object are not numbers at all.
        assert!(proposed(lit_gate(json!(3), Some(json!("3")), false)).contains("\"3\""));
        assert!(lit_gate(json!(3), Some(json!(null)), false).is_err());
        assert!(lit_gate(json!(3), Some(json!({"value": 3})), false).is_err());
    }

    #[test]
    fn literal_gate_never_gates_strings_or_booleans() {
        // They describe themselves; only the numeric kind is ambiguous.
        for (src, live) in [(json!("a"), Some(json!("b"))), (json!(true), Some(json!(false))), (json!(false), None)] {
            assert_eq!(lit_gate(src.clone(), live, false), Ok(()), "{src}");
        }
    }

    #[test]
    fn literal_gate_applies_only_to_the_literal_strategy() {
        // A number in an enum/range translation is governed by its own gate.
        for st in [Strategy::EnumByLabel, Strategy::EnumByRawValue, Strategy::RangeClamped] {
            assert_eq!(literal_gate(st, &json!(5), Some(&json!(3)), false), Ok(()), "{st}");
        }
    }

    #[test]
    fn literal_gate_bounds_the_length_of_a_huge_current_value() {
        let big = json!({"values": vec![1234567; 500]});
        let msg = proposed(lit_gate(json!(5), Some(big), false));
        assert!(msg.len() < 200, "{} bytes", msg.len());
    }

    #[test]
    fn literal_unconfirmed_display_names_the_flag_and_the_proposal() {
        let msg = SkipReason::LiteralUnconfirmed { proposed: "5, was 3".into() }.to_string();
        assert!(msg.contains("--allow-raw-code") && msg.contains("5, was 3"), "{msg}");
    }

    #[test]
    fn plan_property_gates_a_changing_numeric_literal_and_passes_the_rest() {
        let blocked = plan_property(&json!(5), Some(&json!(3)), false).unwrap_err();
        assert!(matches!(blocked, SkipReason::LiteralUnconfirmed { .. }), "{blocked:?}");
        // Opted in: written as-is.
        let (v, st, ch) = plan_property(&json!(5), Some(&json!(3)), true).unwrap();
        assert_eq!((v, st, ch), (json!(5), Strategy::Literal, None));
        // Unchanged: no opt-in needed.
        assert_eq!(plan_property(&json!(5), Some(&json!(5)), false).unwrap().1, Strategy::Literal);
        // Unreadable target, not opted in: blocked; opted in: still written.
        assert!(plan_property(&json!(5), None, false).is_err());
        assert!(plan_property(&json!(5), None, true).is_ok());
        // Strings and booleans stay ungated.
        assert!(plan_property(&json!("x"), Some(&json!("y")), false).is_ok());
        assert!(plan_property(&json!(true), None, false).is_ok());
    }

    #[test]
    fn one_opt_in_covers_a_raw_code_enum_and_a_numeric_literal_independently() {
        let src = raw_code_enum(1, &[3, 5]);
        let live = raw_code_enum(1, &[5, 3]);
        assert!(plan_property(&src, Some(&live), false).is_err());
        assert!(plan_property(&json!(5), Some(&json!(3)), false).is_err());
        assert!(plan_property(&src, Some(&live), true).is_ok());
        assert!(plan_property(&json!(5), Some(&json!(3)), true).is_ok());
    }

    #[test]
    fn allow_raw_code_accepts_a_numeric_scalar_by_short_or_full_name() {
        let p = props(&[("kNkMAIDCapability_WBAutoType", json!(3))]);
        for name in ["WBAutoType", "kNkMAIDCapability_WBAutoType"] {
            let got = resolve_allow_raw_code(&[name.to_string()], &p).unwrap();
            assert_eq!(got, BTreeSet::from(["kNkMAIDCapability_WBAutoType".to_string()]), "{name}");
        }
        // Floats and negatives are numbers too.
        let p = props(&[("kNkMAIDCapability_A", json!(0.5)), ("kNkMAIDCapability_B", json!(-2))]);
        assert!(resolve_allow_raw_code(&["A".into(), "B".into()], &p).is_ok());
    }

    #[test]
    fn allow_raw_code_still_rejects_every_other_kind() {
        let range = json!({"value": 1.0, "default": 0.0, "value_index": 1, "default_index": 0, "lower": 0.0, "upper": 2.0, "steps": 3});
        let p = props(&[
            ("kNkMAIDCapability_Str", json!("x")),
            ("kNkMAIDCapability_Bool", json!(true)),
            ("kNkMAIDCapability_Null", json!(null)),
            ("kNkMAIDCapability_Arr", json!([1, 2])),
            ("kNkMAIDCapability_Range", range),
            ("kNkMAIDCapability_Label", fx("z9", "BracketingVary")),
        ]);
        for n in ["Str", "Bool", "Null", "Arr", "Range", "Label"] {
            let e = resolve_allow_raw_code(&[n.to_string()], &p).unwrap_err();
            assert!(e.contains("not a raw-code enum or a numeric scalar"), "{n}: {e}");
        }
    }

    // ── --allow-raw-code resolution ──────────────────────────────────────

    fn props(entries: &[(&str, Value)]) -> BTreeMap<String, PropertyEntry> {
        entries.iter().enumerate().map(|(i, (n, v))| (n.to_string(), PropertyEntry { code: i as u32, value: v.clone() })).collect()
    }

    #[test]
    fn allow_raw_code_accepts_short_and_full_names_and_dedups() {
        let p = props(&[("kNkMAIDCapability_MeteringMode", raw_code_enum(0, &[1, 2]))]);
        let got = resolve_allow_raw_code(
            &["MeteringMode".into(), "kNkMAIDCapability_MeteringMode".into()],
            &p,
        )
        .unwrap();
        assert_eq!(got, BTreeSet::from(["kNkMAIDCapability_MeteringMode".to_string()]));
        assert!(resolve_allow_raw_code(&[], &p).unwrap().is_empty());
    }

    #[test]
    fn allow_raw_code_rejects_unknown_and_inapplicable_names_loudly() {
        let p = props(&[
            ("kNkMAIDCapability_MeteringMode", raw_code_enum(0, &[1, 2])),
            ("kNkMAIDCapability_WBMode", byte_layout(0, &["Auto"])),
            ("kNkMAIDCapability_ExposureComp", json!({"value": 1.0, "lower": -3.0, "upper": 3.0, "steps": 7, "value_index": 0, "default_index": 0})),
            ("kNkMAIDCapability_Copyright", json!("me")),
        ]);
        // A typo must not silently opt nothing in.
        let e = resolve_allow_raw_code(&["MeteringMod".into()], &p).unwrap_err();
        assert!(e.contains("no property by that name"), "{e}");
        for name in ["WBMode", "ExposureComp", "Copyright"] {
            let e = resolve_allow_raw_code(&[name.into()], &p).unwrap_err();
            assert!(e.contains("not a raw-code enum"), "{name}: {e}");
        }
        // One bad name fails the whole request, even after a good one.
        assert!(resolve_allow_raw_code(&["MeteringMode".into(), "Nope".into()], &p).is_err());
    }

    // ── resolved choice (what the dry run prints) ────────────────────────

    #[test]
    fn resolve_choice_label_enum_changed_and_unchanged() {
        let src = byte_layout(1, &["Off", "On"]);
        let live = byte_layout(0, &["Off", "On"]);
        let (translated, strategy) = translate_value(&src, Some(&live)).unwrap();
        let c = resolve_choice(&translated, &live, strategy).unwrap();
        assert_eq!(c, ResolvedChoice { target_index: 1, choice: "On".into(), current_index: Some(0), current: Some("Off".into()) });
        assert!(!c.is_unchanged());
        assert_eq!(c.to_string(), "\"On\" (#1), was \"Off\" (#0)");

        let same_live = byte_layout(1, &["Off", "On"]);
        let (translated, strategy) = translate_value(&src, Some(&same_live)).unwrap();
        let c = resolve_choice(&translated, &same_live, strategy).unwrap();
        assert!(c.is_unchanged());
        assert_eq!(c.to_string(), "\"On\" (#1), unchanged");
    }

    #[test]
    fn resolve_choice_real_capture_shows_the_label_a_person_can_check() {
        let live = fx("z6iii", "ISOAutoHiLimit");
        let (translated, strategy) = translate_value(&fx("z9", "ISOAutoHiLimit"), Some(&live)).unwrap();
        let c = resolve_choice(&translated, &live, strategy).unwrap();
        assert_eq!(c.choice, "25600");
        assert_eq!(c.target_index, 21);
        assert!(c.is_unchanged(), "both bodies are already on 25600");
    }

    #[test]
    fn resolve_choice_raw_code_enum_and_non_enum() {
        let src = raw_code_enum(1, &[3, 5]);
        let live = raw_code_enum(0, &[5, 3]);
        let (translated, strategy) = translate_value(&src, Some(&live)).unwrap();
        let c = resolve_choice(&translated, &live, strategy).unwrap();
        assert_eq!(c.choice, "code 5");
        assert_eq!(c.target_index, 0);
        assert!(resolve_choice(&json!(1), &json!(1), Strategy::Literal).is_none());
    }

    // ── translate_value: raw-code enum (not elem_type 7) ─────────────────

    fn raw_code_enum(value_index: u64, values: &[u64]) -> Value {
        json!({
            "value_index": value_index,
            "default_index": 0,
            "elem_count": values.len(),
            "elem_type": 2,
            "elem_bytes": 4,
            "values": values,
        })
    }

    #[test]
    fn enum_by_raw_value_matches_same_code() {
        let source = raw_code_enum(1, &[10, 20, 30]);
        let target_live = raw_code_enum(0, &[20, 30]);
        let (translated, strategy) = translate_value(&source, Some(&target_live)).unwrap();
        assert_eq!(strategy, Strategy::EnumByRawValue);
        assert_eq!(translated["value_index"], json!(0)); // 20 is at index 0 on target
    }

    #[test]
    fn enum_by_raw_value_no_match_is_skipped() {
        let source = raw_code_enum(0, &[99]);
        let target_live = raw_code_enum(0, &[1, 2, 3]);
        assert_eq!(
            translate_value(&source, Some(&target_live)),
            Err(SkipReason::NoMatchingOption)
        );
    }

    // ── translate_value: malformed enum source ───────────────────────────

    #[test]
    fn enum_missing_value_index_is_malformed() {
        let source = json!({"values": ["Off", "On"]});
        let target_live = packed_string_enum(0, &["Off", "On"]);
        assert_eq!(
            translate_value(&source, Some(&target_live)),
            Err(SkipReason::MalformedSource)
        );
    }

    #[test]
    fn enum_index_out_of_bounds_is_malformed() {
        let source = packed_string_enum(5, &["Off", "On"]); // index 5, only 2 values
        let target_live = packed_string_enum(0, &["Off", "On"]);
        assert_eq!(
            translate_value(&source, Some(&target_live)),
            Err(SkipReason::MalformedSource)
        );
    }

    // ── translate_value: SDK-decode-failure sentinels (not a real values array) ──

    #[test]
    fn source_enum_undecoded_by_sdk_is_reported_distinctly() {
        let source = json!({
            "value_index": 0, "elem_type": 2, "elem_count": 3, "elem_bytes": 4,
            "values": {"_invalid_physical_bytes": -1},
        });
        let target_live = raw_code_enum(0, &[1, 2, 3]);
        assert_eq!(
            translate_value(&source, Some(&target_live)),
            Err(SkipReason::EnumUndecodedBySdk { side: "source", reason: "invalid element stride (-1)".to_string() })
        );
    }

    #[test]
    fn target_enum_undecoded_by_sdk_is_reported_distinctly() {
        let source = raw_code_enum(0, &[1, 2, 3]);
        let target_live = json!({
            "value_index": 0, "elem_type": 2, "elem_count": 3, "elem_bytes": 4,
            "values": {"_unsupported_enum_type": 99},
        });
        assert_eq!(
            translate_value(&source, Some(&target_live)),
            Err(SkipReason::EnumUndecodedBySdk {
                side: "target",
                reason: "unsupported enum element type (99)".to_string()
            })
        );
    }

    #[test]
    fn packed_string_length_overflow_sentinel_is_reported_distinctly() {
        let source = json!({
            "value_index": 0, "elem_type": 7, "elem_count": 3, "elem_bytes": 8,
            "values": {"_overflow_computing_packed_string_length": true},
        });
        let target_live = packed_string_enum(0, &["Off", "On"]);
        assert_eq!(
            translate_value(&source, Some(&target_live)),
            Err(SkipReason::EnumUndecodedBySdk {
                side: "source",
                reason: "packed-string length overflow".to_string()
            })
        );
    }

    // ── translate_value: source/target elem_type disagreement ─────────────

    #[test]
    fn packed_string_source_vs_raw_code_target_is_type_mismatch_not_no_match() {
        // Source is packed-string (elem_type 7, string labels); target's
        // live read is raw-code (elem_type != 7, numeric values). Comparing
        // a string label against numeric codes could never match by
        // construction — must be reported as a type mismatch, not the
        // generic "this option doesn't exist" NoMatchingOption.
        let source = packed_string_enum(0, &["High"]);
        let target_live = raw_code_enum(0, &[1, 2, 3]);
        assert_eq!(
            translate_value(&source, Some(&target_live)),
            Err(SkipReason::EnumTypeMismatch { source_elem_type: 7, target_elem_type: 2 })
        );
    }

    #[test]
    fn raw_code_source_vs_packed_string_target_is_type_mismatch() {
        let source = raw_code_enum(0, &[1, 2, 3]);
        let target_live = packed_string_enum(0, &["High"]);
        assert_eq!(
            translate_value(&source, Some(&target_live)),
            Err(SkipReason::EnumTypeMismatch { source_elem_type: 2, target_elem_type: 7 })
        );
    }

    #[test]
    fn matching_elem_types_are_not_reported_as_mismatch() {
        // Both packed-string — must fall through to normal label matching,
        // not get flagged as a type mismatch.
        let source = packed_string_enum(2, &["Off", "Low", "High"]);
        let target_live = packed_string_enum(0, &["Off", "High"]);
        assert!(translate_value(&source, Some(&target_live)).is_ok());
    }

    #[test]
    fn unrecognized_elem_type_value_is_malformed_not_silently_defaulted() {
        // Present but not in KNOWN_ELEM_TYPES (0,1,2,3,7,8) — a real
        // anomaly, distinct from the field simply being absent.
        let source = json!({
            "value_index": 0, "elem_type": 99, "elem_count": 1, "elem_bytes": 4,
            "values": [1],
        });
        let target_live = raw_code_enum(0, &[1]);
        assert_eq!(translate_value(&source, Some(&target_live)), Err(SkipReason::MalformedSource));
    }

    #[test]
    fn absent_elem_type_still_defaults_to_raw_code_not_malformed() {
        // Missing entirely (not present-but-bad) keeps the old lenient
        // behavior — defaults to the conservative raw-code strategy.
        let source = json!({"value_index": 0, "values": [1, 2, 3]});
        let target_live = raw_code_enum(0, &[1, 2, 3]);
        let (_, strategy) = translate_value(&source, Some(&target_live)).unwrap();
        assert_eq!(strategy, Strategy::EnumByRawValue);
    }

    // ── translate_value: range ────────────────────────────────────────────

    fn range_value(value: f64, lower: f64, upper: f64, steps: u64) -> Value {
        json!({"value": value, "default": 0.0, "value_index": 0, "default_index": 0, "lower": lower, "upper": upper, "steps": steps})
    }

    #[test]
    fn range_within_target_bounds_passes_through() {
        let source = range_value(2.0, -5.0, 5.0, 0);
        let target_live = range_value(0.0, -5.0, 5.0, 0);
        let (translated, strategy) = translate_value(&source, Some(&target_live)).unwrap();
        assert_eq!(strategy, Strategy::RangeClamped);
        assert_eq!(translated["value"], json!(2.0));
    }

    #[test]
    fn range_above_target_upper_is_clamped() {
        // Source body allows up to +5 EV; target only goes to +3 EV.
        let source = range_value(5.0, -5.0, 5.0, 0);
        let target_live = range_value(0.0, -3.0, 3.0, 0);
        let (translated, _) = translate_value(&source, Some(&target_live)).unwrap();
        assert_eq!(translated["value"], json!(3.0));
    }

    #[test]
    fn range_below_target_lower_is_clamped() {
        let source = range_value(-5.0, -5.0, 5.0, 0);
        let target_live = range_value(0.0, -3.0, 3.0, 0);
        let (translated, _) = translate_value(&source, Some(&target_live)).unwrap();
        assert_eq!(translated["value"], json!(-3.0));
    }

    #[test]
    fn range_value_index_snapped_to_target_steps() {
        // Target has 7 discrete steps across [-3, 3] (step size 1.0);
        // clamped value 2.0 should land on index 5 (-3 + 5*1.0 = 2.0).
        let source = range_value(2.0, -5.0, 5.0, 0);
        let target_live = range_value(0.0, -3.0, 3.0, 7);
        let (translated, _) = translate_value(&source, Some(&target_live)).unwrap();
        assert_eq!(translated["value_index"], json!(5));
    }

    #[test]
    fn range_steps_at_zero_is_continuous_index_zero() {
        let source = range_value(2.0, -5.0, 5.0, 0);
        let target_live = range_value(0.0, -3.0, 3.0, 0);
        let (translated, _) = translate_value(&source, Some(&target_live)).unwrap();
        assert_eq!(translated["value_index"], json!(0));
    }

    #[test]
    fn range_steps_at_exactly_one_is_index_zero() {
        // Threshold-boundary pin for the steps>=2 guard: at exactly one
        // step there's only one possible position, index 0 — verified to
        // be the same result the >=2 guard's formula would itself compute
        // if it ran (multiplying by (steps-1)=(1-1)=0), so this also pins
        // that the guard's exact boundary doesn't change observable output.
        let source = range_value(2.0, -5.0, 5.0, 0);
        let target_live = range_value(0.0, -3.0, 3.0, 1);
        let (translated, _) = translate_value(&source, Some(&target_live)).unwrap();
        assert_eq!(translated["value_index"], json!(0));
    }

    #[test]
    fn range_steps_at_exactly_two_snaps_between_the_two_positions() {
        // Threshold-boundary pin just above the guard: 2 steps means
        // exactly 2 valid positions (0 and 1), unlike steps=1 above.
        let source = range_value(5.0, -5.0, 5.0, 0); // clamps to upper (3.0)
        let target_live = range_value(0.0, -3.0, 3.0, 2);
        let (translated, _) = translate_value(&source, Some(&target_live)).unwrap();
        assert_eq!(translated["value_index"], json!(1)); // clamped to upper -> last index
    }

    #[test]
    fn range_missing_steps_on_target_is_treated_as_shape_mismatch() {
        // steps ABSENT (not present-with-value-0) on an otherwise
        // range-shaped target_live — same strict treatment as missing
        // lower/upper, not silently defaulted to continuous.
        let source = range_value(1.0, -5.0, 5.0, 0);
        let target_live = json!({"value": 0.0, "lower": -3.0, "upper": 3.0});
        assert!(matches!(translate_value(&source, Some(&target_live)), Err(SkipReason::TargetReadFailed(_))));
    }

    #[test]
    fn range_preserves_target_bounds_not_sources() {
        let source = range_value(1.0, -5.0, 5.0, 0);
        let target_live = range_value(0.0, -3.0, 3.0, 4);
        let (translated, _) = translate_value(&source, Some(&target_live)).unwrap();
        assert_eq!(translated["lower"], json!(-3.0));
        assert_eq!(translated["upper"], json!(3.0));
        assert_eq!(translated["steps"], json!(4));
    }

    #[test]
    fn range_requires_target_live_value() {
        let source = range_value(1.0, -5.0, 5.0, 0);
        assert!(matches!(translate_value(&source, None), Err(SkipReason::TargetReadFailed(_))));
    }

    #[test]
    fn range_missing_value_is_malformed() {
        let source = json!({"lower": -5.0, "upper": 5.0});
        let target_live = range_value(0.0, -3.0, 3.0, 0);
        assert_eq!(
            translate_value(&source, Some(&target_live)),
            Err(SkipReason::MalformedSource)
        );
    }

    // ── SkipReason / Strategy Display ────────────────────────────────────

    #[test]
    fn strategy_display_matches_expected_names() {
        assert_eq!(Strategy::Literal.to_string(), "literal");
        assert_eq!(Strategy::EnumByLabel.to_string(), "enum-by-label");
        assert_eq!(Strategy::EnumByRawValue.to_string(), "enum-by-raw-value");
        assert_eq!(Strategy::RangeClamped.to_string(), "range-clamped");
    }

    #[test]
    fn skip_reason_write_failed_includes_message() {
        let reason = SkipReason::WriteFailed("SDK call `SetCapability` returned error code -1".to_string());
        assert!(reason.to_string().contains("SetCapability"));
    }

    // ── truncate_with_marker ─────────────────────────────────────────────

    #[test]
    fn truncate_with_marker_leaves_short_strings_alone() {
        assert_eq!(truncate_with_marker("short".to_string(), 500), "short");
    }

    #[test]
    fn truncate_with_marker_at_exactly_the_limit_is_unchanged() {
        let s = "x".repeat(500);
        assert_eq!(truncate_with_marker(s.clone(), 500), s);
    }

    #[test]
    fn truncate_with_marker_truncates_and_marks_over_the_limit() {
        let s = "x".repeat(501);
        let out = truncate_with_marker(s, 500);
        assert!(out.starts_with(&"x".repeat(500)));
        assert!(out.contains("truncated"));
        assert!(out.contains("501 bytes total"));
    }

    #[test]
    fn truncate_with_marker_never_splits_a_utf8_char() {
        // A multi-byte character sitting right at the cut point must not
        // panic (byte-slicing mid-character) or produce invalid UTF-8.
        let s = format!("{}€", "x".repeat(499)); // € is 3 bytes, cut point lands inside it
        let out = truncate_with_marker(s, 500);
        assert!(out.starts_with(&"x".repeat(499)));
    }

    // ── Z-series scope + capability-name guard ───────────────────────────

    /// Build a MaidLayer.config text: one section per `(model, caps)`, each
    /// cap `(symbolic name without prefix, code)`.
    fn config_text(models: &[(&str, &[(&str, u32)])]) -> String {
        let mut out = String::from("<ProfileVersion>9.0.3000</ProfileVersion>\n");
        for (model, caps) in models {
            out.push_str(&format!("<model:{model}>\n<version>common</version>\n<caplist>\n"));
            for (name, code) in *caps {
                out.push_str(&format!(
                    "<capability:kNkMAIDCapability_{name}-{code}>\n<description>0,0,\"{name}\"</description>\n\
                     <allowedoperation:14></allowedoperation>\n<DeviceCommand>1,0,0,0,0</DeviceCommand>\n</capability>\n"
                ));
            }
            out.push_str("</caplist>\n</model>\n");
        }
        out
    }

    fn parse_config(models: &[(&str, &[(&str, u32)])]) -> MaidLayerConfig {
        MaidLayerConfig::parse(&config_text(models)).expect("fixture config parses")
    }

    fn names(ns: &[&str]) -> BTreeSet<String> {
        ns.iter().map(|n| format!("kNkMAIDCapability_{n}")).collect()
    }

    #[test]
    fn z_series_model_with_section_is_supported() {
        let c = parse_config(&[("Z 9", &[("Aperture", 1)])]);
        assert!(is_z_series_model(&c, "Z 9"));
    }

    #[test]
    fn dslr_section_is_not_z_series() {
        // D6 has a section in the real config but is outside the verified scope.
        let c = parse_config(&[("D6", &[("Aperture", 1)])]);
        assert!(!is_z_series_model(&c, "D6"));
    }

    #[test]
    fn model_with_no_section_is_not_z_series() {
        // The D5 case: no section at all, so nothing to check codes against —
        // and a Z-looking name that simply isn't in the config ("Z 99")
        // must not pass on its leading letter alone.
        let c = parse_config(&[("Z 9", &[("Aperture", 1)])]);
        assert!(!is_z_series_model(&c, "D5"));
        assert!(!is_z_series_model(&c, "Z 99"));
        assert!(!is_z_series_model(&c, ""));
    }

    #[test]
    fn leading_z_alone_is_not_enough_for_non_body_sections() {
        // "DemoZ f" contains Z but doesn't start with it.
        let c = parse_config(&[("DemoZ f", &[("Aperture", 1)]), ("Default", &[("Aperture", 1)])]);
        assert!(!is_z_series_model(&c, "DemoZ f"));
        assert!(!is_z_series_model(&c, "Default"));
    }

    #[test]
    fn guard_new_rejects_non_z_source_with_role() {
        let c = parse_config(&[("Z 9", &[("Aperture", 1)]), ("D6", &[("Aperture", 1)])]);
        let err = CodeNameGuard::new(&c, "D6", "Z 9").unwrap_err();
        assert_eq!(err, UnsupportedModel { role: "source", model: "D6".to_string() });
    }

    #[test]
    fn guard_new_rejects_non_z_target_with_role() {
        let c = parse_config(&[("Z 9", &[("Aperture", 1)]), ("D6", &[("Aperture", 1)])]);
        let err = CodeNameGuard::new(&c, "Z 9", "D5").unwrap_err();
        assert_eq!(err, UnsupportedModel { role: "target", model: "D5".to_string() });
    }

    #[test]
    fn guard_new_reports_source_first_when_both_are_unsupported() {
        let c = parse_config(&[("Z 9", &[("Aperture", 1)])]);
        let err = CodeNameGuard::new(&c, "D5", "D6").unwrap_err();
        assert_eq!(err.role, "source");
    }

    #[test]
    fn guard_new_accepts_same_model_both_sides() {
        let c = parse_config(&[("Z 9", &[("Aperture", 1)])]);
        assert!(CodeNameGuard::new(&c, "Z 9", "Z 9").is_ok());
    }

    #[test]
    fn unsupported_model_message_names_role_and_model() {
        let e = UnsupportedModel { role: "target", model: "D5".to_string() };
        let msg = e.to_string();
        assert!(msg.contains("target") && msg.contains("\"D5\"") && msg.contains("Z-series"), "{msg}");
    }

    #[test]
    fn check_passes_when_names_match() {
        let c = parse_config(&[("Z 9", &[("Aperture", 1)]), ("Z 8", &[("Aperture", 1)])]);
        let g = CodeNameGuard::new(&c, "Z 9", "Z 8").unwrap();
        assert_eq!(g.check(1), Ok(()));
    }

    #[test]
    fn check_skips_unrelated_names_on_the_same_code() {
        let c = parse_config(&[("Z 9", &[("Aperture", 7)]), ("Z 8", &[("ShutterSpeed", 7)])]);
        let g = CodeNameGuard::new(&c, "Z 9", "Z 8").unwrap();
        assert_eq!(
            g.check(7),
            Err(SkipReason::CapabilityNameMismatch {
                source_names: "kNkMAIDCapability_Aperture".to_string(),
                target_names: "kNkMAIDCapability_ShutterSpeed".to_string(),
            })
        );
        // Symmetric.
        let g = CodeNameGuard::new(&c, "Z 8", "Z 9").unwrap();
        assert!(matches!(g.check(7), Err(SkipReason::CapabilityNameMismatch { .. })));
    }

    #[test]
    fn check_allows_the_declared_33200_alias_in_both_directions() {
        let c = parse_config(&[("Z 9", &[("AEBracketingType", 33200)]), ("Z6_3", &[("BracketingType", 33200)])]);
        assert_eq!(CodeNameGuard::new(&c, "Z 9", "Z6_3").unwrap().check(33200), Ok(()));
        assert_eq!(CodeNameGuard::new(&c, "Z6_3", "Z 9").unwrap().check(33200), Ok(()));
    }

    #[test]
    fn check_alias_does_not_cover_a_third_name() {
        // Boundary of the alias rule: EVERY name on both sides must be in the
        // class, not just one pair of them.
        let c = parse_config(&[
            ("Z 9", &[("AEBracketingType", 33200)]),
            ("Z6_3", &[("BracketingType", 33200), ("Mystery", 33200)]),
        ]);
        let g = CodeNameGuard::new(&c, "Z 9", "Z6_3").unwrap();
        assert!(matches!(g.check(33200), Err(SkipReason::CapabilityNameMismatch { .. })));
    }

    #[test]
    fn check_alias_is_scoped_to_its_own_code() {
        // The same two names, disjoint, at a code that is NOT 33200: no alias.
        let c = parse_config(&[("Z 9", &[("AEBracketingType", 5)]), ("Z6_3", &[("BracketingType", 5)])]);
        let g = CodeNameGuard::new(&c, "Z 9", "Z6_3").unwrap();
        assert!(matches!(g.check(5), Err(SkipReason::CapabilityNameMismatch { .. })));
    }

    #[test]
    fn check_passes_when_sides_share_one_of_several_names() {
        // 33211-style: the newer body lists an extra in-section alias.
        let c = parse_config(&[
            ("Z 5", &[("ExternalNewTypeFlashMode", 33211)]),
            ("Z 8", &[("ExternalNewTypeFlashMode", 33211), ("ExternalFlashStatus", 33211)]),
        ]);
        assert_eq!(CodeNameGuard::new(&c, "Z 5", "Z 8").unwrap().check(33211), Ok(()));
        assert_eq!(CodeNameGuard::new(&c, "Z 8", "Z 5").unwrap().check(33211), Ok(()));
    }

    #[test]
    fn check_has_no_verdict_when_either_model_omits_the_code() {
        // Pins current behavior: absence in the config is not a mismatch (the
        // live capability list and value translation remain the checks).
        let c = parse_config(&[("Z 9", &[("Aperture", 1)]), ("Z 8", &[("Other", 2)])]);
        let g = CodeNameGuard::new(&c, "Z 9", "Z 8").unwrap();
        assert_eq!(g.check(1), Ok(())); // only on source
        assert_eq!(g.check(2), Ok(())); // only on target
        assert_eq!(g.check(999), Ok(())); // on neither
    }

    #[test]
    fn check_unions_names_across_firmware_sections_of_one_model() {
        // name_map_for_model is first-seen-wins; the guard must see both.
        let text = "<model:Z 9>\n<version>1.0</version>\n<caplist>\n\
                    <capability:kNkMAIDCapability_Aperture-3>\n<allowedoperation:14></allowedoperation>\n</capability>\n</caplist>\n</model>\n\
                    <model:Z 9>\n<version>2.0</version>\n<caplist>\n\
                    <capability:kNkMAIDCapability_ApertureNew-3>\n<allowedoperation:14></allowedoperation>\n</capability>\n</caplist>\n</model>\n";
        let c = MaidLayerConfig::parse(text).expect("parses");
        assert_eq!(c.name_sets_for_model("Z 9").get(&3), Some(&names(&["Aperture", "ApertureNew"])));
    }

    #[test]
    fn capability_name_mismatch_display_names_both_sides() {
        let r = SkipReason::CapabilityNameMismatch { source_names: "A".to_string(), target_names: "B/C".to_string() };
        let msg = r.to_string();
        assert!(msg.contains("source: A") && msg.contains("target: B/C"), "{msg}");
    }

    // ── precheck ordering (the wiring transplant_one relies on) ──────────

    fn cap_info(id: u32, operations: u32) -> CapabilityInfo {
        CapabilityInfo { id, kind: 0, visibility: 0, operations, description: String::new() }
    }

    fn prop_for(code: u32) -> PropertyEntry {
        PropertyEntry { code, value: json!(1) }
    }

    #[test]
    fn precheck_absent_on_target_wins_over_name_mismatch() {
        // Unrelated names at code 7, but the target doesn't even have the
        // capability: the routine skip is reported, not the name warning.
        let c = parse_config(&[("Z 9", &[("Aperture", 7)]), ("Z 8", &[("ShutterSpeed", 7)])]);
        let g = CodeNameGuard::new(&c, "Z 9", "Z 8").unwrap();
        let caps: HashMap<u32, &CapabilityInfo> = HashMap::new();
        assert_eq!(precheck(&caps, &g, &prop_for(7)).unwrap_err(), SkipReason::AbsentOnTarget);
    }

    #[test]
    fn precheck_name_mismatch_wins_over_read_only() {
        let c = parse_config(&[("Z 9", &[("Aperture", 7)]), ("Z 8", &[("ShutterSpeed", 7)])]);
        let g = CodeNameGuard::new(&c, "Z 9", "Z 8").unwrap();
        let ro = cap_info(7, 0); // no OP_SET bit
        let caps: HashMap<u32, &CapabilityInfo> = HashMap::from([(7, &ro)]);
        assert!(matches!(precheck(&caps, &g, &prop_for(7)), Err(SkipReason::CapabilityNameMismatch { .. })));
    }

    #[test]
    fn precheck_read_only_reported_when_names_agree() {
        let c = parse_config(&[("Z 9", &[("Aperture", 7)]), ("Z 8", &[("Aperture", 7)])]);
        let g = CodeNameGuard::new(&c, "Z 9", "Z 8").unwrap();
        let ro = cap_info(7, 0);
        let caps: HashMap<u32, &CapabilityInfo> = HashMap::from([(7, &ro)]);
        assert_eq!(precheck(&caps, &g, &prop_for(7)).unwrap_err(), SkipReason::ReadOnly);
    }

    #[test]
    fn precheck_passes_writable_capability_with_matching_names() {
        let c = parse_config(&[("Z 9", &[("Aperture", 7)]), ("Z 8", &[("Aperture", 7)])]);
        let g = CodeNameGuard::new(&c, "Z 9", "Z 8").unwrap();
        let rw = cap_info(7, OP_SET);
        let caps: HashMap<u32, &CapabilityInfo> = HashMap::from([(7, &rw)]);
        assert_eq!(precheck(&caps, &g, &prop_for(7)).unwrap().id, 7);
    }

    // ── real MaidLayer.config sweep ──────────────────────────────────────
    //
    // `sdk-runtime/` is gitignored (Nikon's SDK), so these only run where the
    // SDK has been set up (scripts/setup-sdk-runtime.sh). Absence is loud on
    // stderr but not a failure, since a clean checkout legitimately has no
    // config; set REQUIRE_SDK_RUNTIME=1 to make absence fail (e.g. on the
    // machine that is supposed to have it).

    const EXPECTED_Z_MODELS: &[&str] = &[
        "Z 30", "Z 5", "Z 50", "Z 6", "Z 6_2", "Z 7", "Z 7_2", "Z 8", "Z 9", "Z f", "Z fc", "Z50_2", "Z5_2", "Z6_3", "ZR",
    ];

    fn real_config() -> Option<MaidLayerConfig> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/sdk-runtime/MaidLayer.config");
        if !std::path::Path::new(path).exists() {
            assert!(
                std::env::var_os("REQUIRE_SDK_RUNTIME").is_none(),
                "REQUIRE_SDK_RUNTIME is set but {path} does not exist"
            );
            eprintln!("SKIPPED real-config sweep: {path} not present (run scripts/setup-sdk-runtime.sh)");
            return None;
        }
        Some(MaidLayerConfig::parse_file(path).expect("real MaidLayer.config parses"))
    }

    #[test]
    fn real_config_z_series_set_is_pinned() {
        let Some(c) = real_config() else { return };
        let mut z: Vec<String> = c.known_models().into_iter().filter(|m| is_z_series_model(&c, m)).collect();
        z.sort();
        let mut expected: Vec<String> = EXPECTED_Z_MODELS.iter().map(|m| m.to_string()).collect();
        expected.sort();
        assert_eq!(z, expected);
        for non_z in ["D6", "D780", "DC", "Default", "DEMO95", "DemoZ f"] {
            assert!(!is_z_series_model(&c, non_z), "{non_z} must not count as Z-series");
        }
    }

    #[test]
    fn real_config_cross_model_name_exceptions_are_exactly_the_declared_ones() {
        let Some(c) = real_config() else { return };
        let sets: Vec<(&str, HashMap<u32, BTreeSet<String>>)> =
            EXPECTED_Z_MODELS.iter().map(|m| (*m, c.name_sets_for_model(m))).collect();

        // (a) Every Z-model pair, every code both list: the guard passes
        // (declared aliases included).
        let mut guard_failures = Vec::new();
        // (b) Same sweep with the alias table ignored: only plain
        // name-set disjointness. Pins that the table covers exactly what
        // needs covering and nothing stale.
        let mut raw_disjoint_codes = BTreeSet::new();
        for (sm, ss) in &sets {
            for (tm, ts) in &sets {
                for (code, sn) in ss {
                    let Some(tn) = ts.get(code) else { continue };
                    if sn.is_disjoint(tn) {
                        raw_disjoint_codes.insert(*code);
                    }
                    if !names_compatible(*code, sn, tn) {
                        guard_failures.push((*sm, *tm, *code));
                    }
                }
            }
        }
        assert_eq!(guard_failures, vec![], "unexplained cross-model name divergence");
        assert_eq!(raw_disjoint_codes.into_iter().collect::<Vec<_>>(), vec![33200]);
        assert_eq!(KNOWN_NAME_ALIASES.iter().map(|(c, _)| *c).collect::<Vec<_>>(), vec![33200]);
    }

    #[test]
    fn real_config_in_section_alias_codes_are_exactly_33211() {
        let Some(c) = real_config() else { return };
        let mut multi_name_codes = BTreeSet::new();
        for m in EXPECTED_Z_MODELS {
            for (code, ns) in c.name_sets_for_model(m) {
                if ns.len() > 1 {
                    multi_name_codes.insert(code);
                }
            }
        }
        assert_eq!(multi_name_codes.into_iter().collect::<Vec<_>>(), vec![33211]);
    }
}

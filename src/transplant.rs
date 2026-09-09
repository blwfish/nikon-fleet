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

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

use crate::sdk::{CapabilityInfo, Device, OP_SET, SdkError};
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

    let tgt_idx = tgt_values.iter().position(|v| v == src_label).ok_or(SkipReason::NoMatchingOption)?;

    let strategy =
        if src_elem_type == Some(PACKED_STRING_ELEM_TYPE) { Strategy::EnumByLabel } else { Strategy::EnumByRawValue };

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
    let steps = target_live.get("steps").and_then(Value::as_u64).unwrap_or(0);

    let (lo, hi) = (lower.min(upper), lower.max(upper));
    let clamped = src_value.clamp(lo, hi);
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
// Orchestration — needs a live Device, so not unit-testable the way
// translate_value/translate_enum/translate_range are.
// ─────────────────────────────────────────────────────────────────────────

pub struct TransplantOutcome {
    pub name: String,
    pub code: u32,
    pub result: Result<Strategy, SkipReason>,
}

/// Copy every property in `properties` (captured from a source camera that
/// may be a *different model* than `device`) onto `device`, translating
/// enum/range values via the target's own live equivalents.
///
/// `dry_run` skips the actual write but still performs the live reads
/// needed to report what *would* happen — intentionally more work than
/// `fleet restore --dry-run`, which never touches the target beyond its
/// capability list, but that's the only way to report a real
/// `NoMatchingOption` skip before committing to anything.
pub fn transplant(device: &Device, properties: &BTreeMap<String, PropertyEntry>, dry_run: bool) -> Vec<TransplantOutcome> {
    let cap_map: HashMap<u32, &CapabilityInfo> = device.capabilities.iter().map(|c| (c.id, c)).collect();

    properties
        .iter()
        .map(|(name, prop)| {
            let result = transplant_one(device, &cap_map, prop, dry_run);
            TransplantOutcome { name: name.clone(), code: prop.code, result }
        })
        .collect()
}

fn transplant_one(
    device: &Device,
    cap_map: &HashMap<u32, &CapabilityInfo>,
    prop: &PropertyEntry,
    dry_run: bool,
) -> Result<Strategy, SkipReason> {
    let cap = cap_map.get(&prop.code).ok_or(SkipReason::AbsentOnTarget)?;
    if cap.operations & OP_SET == 0 {
        return Err(SkipReason::ReadOnly);
    }

    let needs_live_read = is_enum_shape(&prop.value) || is_range_shape(&prop.value);
    let live = if needs_live_read {
        Some(device.read_capability(prop.code).map_err(|e| SkipReason::TargetReadFailed(e.to_string()))?)
    } else {
        None
    };

    let (translated, strategy) = translate_value(&prop.value, live.as_ref())?;

    if !dry_run {
        match device.write_capability(prop.code, cap.kind, &translated) {
            Ok(()) => {}
            Err(SdkError::UnsupportedWrite(_)) => return Err(SkipReason::UnsupportedWriteType),
            Err(e) => return Err(SkipReason::WriteFailed(e.to_string())),
        }
        if strategy == Strategy::RangeClamped {
            verify_range_write(device, prop.code, &translated)?;
        }
    }
    Ok(strategy)
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
}

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
    /// and that read failed.
    TargetReadFailed,
    /// The source snapshot's value for an enum/range capability is missing
    /// a field `sdk::decode_value` always produces — a corrupt or
    /// hand-edited snapshot file, not a normal skip case.
    MalformedSource,
    /// The live write itself failed, for a reason other than an
    /// unsupported capability kind.
    WriteFailed(String),
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::AbsentOnTarget => f.write_str("not present on target camera"),
            SkipReason::ReadOnly => f.write_str("read-only on target camera"),
            SkipReason::UnsupportedWriteType => f.write_str("target capability kind is not writable"),
            SkipReason::NoMatchingOption => f.write_str("no matching option on target camera"),
            SkipReason::UnsupportedValueShape => f.write_str("unsupported value shape"),
            SkipReason::TargetReadFailed => f.write_str("could not read current value from target camera"),
            SkipReason::MalformedSource => f.write_str("malformed value in source snapshot"),
            SkipReason::WriteFailed(msg) => write!(f, "write failed: {msg}"),
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

/// Match a packed-string (`elem_type` 7) or raw-code enum value from
/// `source` into the target's own index space, using `target_live` (the
/// target camera's current value for the same capability).
fn translate_enum(source: &Value, target_live: &Value) -> Result<(Value, Strategy), SkipReason> {
    let src_idx = source.get("value_index").and_then(Value::as_u64).ok_or(SkipReason::MalformedSource)? as usize;
    let src_values = source.get("values").and_then(Value::as_array).ok_or(SkipReason::MalformedSource)?;
    let src_label = src_values.get(src_idx).ok_or(SkipReason::MalformedSource)?;

    let tgt_values = target_live.get("values").and_then(Value::as_array).ok_or(SkipReason::NoMatchingOption)?;
    let tgt_idx = tgt_values.iter().position(|v| v == src_label).ok_or(SkipReason::NoMatchingOption)?;

    let elem_type = source.get("elem_type").and_then(Value::as_u64).unwrap_or(0);
    let strategy = if elem_type == PACKED_STRING_ELEM_TYPE { Strategy::EnumByLabel } else { Strategy::EnumByRawValue };

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
    let lower = target_live.get("lower").and_then(Value::as_f64).ok_or(SkipReason::TargetReadFailed)?;
    let upper = target_live.get("upper").and_then(Value::as_f64).ok_or(SkipReason::TargetReadFailed)?;
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
        let live = target_live.ok_or(SkipReason::TargetReadFailed)?;
        return translate_enum(source_value, live);
    }
    if is_range_shape(source_value) {
        let live = target_live.ok_or(SkipReason::TargetReadFailed)?;
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
        Some(device.read_capability(prop.code).map_err(|_| SkipReason::TargetReadFailed)?)
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
    }
    Ok(strategy)
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
        assert_eq!(translate_value(&source, None), Err(SkipReason::TargetReadFailed));
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
        assert_eq!(translate_value(&source, None), Err(SkipReason::TargetReadFailed));
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

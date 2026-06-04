//! Same-model snapshot diffing.
//!
//! Cross-model diffing (e.g. Z 9 vs Z 6III) is deliberately out of scope
//! for v1 — semantic alignment of properties between bodies needs the
//! per-model availability data from MaidLayer, and we'd want a richer
//! "missing on one side because the body doesn't have it" UX than the
//! simple symmetric-difference output below.

use std::collections::BTreeSet;

use serde::Serialize;
use thiserror::Error;

use crate::firmware::{capability_map_for_fw, schema_major};
use crate::maid_layer::MaidLayerConfig;
use crate::snapshot::Snapshot;

/// A single property whose value differs between two snapshots.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Change {
    pub name: String,
    pub code: u32,
    pub value_a: serde_json::Value,
    pub value_b: serde_json::Value,
}

/// Schema-driven explanation of diff entries that cross a firmware version
/// boundary. Present only when both snapshots have non-empty, distinct firmware
/// versions and the model has per-version schema sections.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FirmwareAnnotation {
    pub firmware_a: String,
    pub firmware_b: String,
    /// Subset of `only_in_a` that the schema explains as removed in newer firmware.
    pub schema_only_in_a: Vec<String>,
    /// Subset of `only_in_b` that the schema explains as added in newer firmware.
    pub schema_only_in_b: Vec<String>,
    /// Subset of `changed[].name` where `allowed_ops` differs between the two
    /// firmware versions' schemas.
    pub schema_changed: Vec<String>,
}

/// Result of comparing two snapshots. `only_in_a` and `only_in_b` should
/// typically be empty when comparing snapshots of the same body unless one
/// was captured before a firmware update that added or removed properties.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Diff {
    pub changed: Vec<Change>,
    pub only_in_a: Vec<String>,
    pub only_in_b: Vec<String>,
    /// Set when both snapshots have differing, non-empty firmware versions and
    /// the model has per-version schema sections. `None` for legacy snapshots
    /// or models with only a `common` section (e.g. Z 5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware_annotation: Option<FirmwareAnnotation>,
}

impl Diff {
    /// True when there are no differences.
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.only_in_a.is_empty() && self.only_in_b.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum DiffError {
    #[error(
        "cross-model diff not supported (yet): {model_a:?} vs {model_b:?}"
    )]
    ModelMismatch { model_a: String, model_b: String },
}

/// Properties to skip by default — values that change just from the camera
/// being on for a few seconds. The user can override with `include_volatile`.
///
/// This list is conservative; we add to it as real-world diffs show noise.
/// Names are matched as substrings of the capability name, case-sensitive,
/// against the full `kNkMAIDCapability_...` symbol.
const VOLATILE_SUBSTRINGS: &[&str] = &[
    "BatteryLevel",
    "ChargeStatus",
    "Temperature",
    "SensorTemp",
    "ExposureMeter",      // live meter reading
    "ShutterCount",       // monotonically increasing
    "DeviceUptime",
    "DateTime",           // clock ticks
    "FocusPosition",
    "FocusDistance",
    "AngleLevel",         // accelerometer-derived tilt
];

/// Options for `diff`.
#[derive(Debug, Clone, Default)]
pub struct DiffOptions {
    /// Include properties from the volatile list. Default: false.
    pub include_volatile: bool,
}

pub fn diff(a: &Snapshot, b: &Snapshot, opts: &DiffOptions) -> Result<Diff, DiffError> {
    if a.camera.model != b.camera.model {
        return Err(DiffError::ModelMismatch {
            model_a: a.camera.model.clone(),
            model_b: b.camera.model.clone(),
        });
    }

    let keys_a: BTreeSet<&String> = a.properties.keys().collect();
    let keys_b: BTreeSet<&String> = b.properties.keys().collect();

    let mut result = Diff::default();

    // Names in both — compare values.
    for key in keys_a.intersection(&keys_b) {
        if !opts.include_volatile && is_volatile(key) {
            continue;
        }
        let pa = &a.properties[*key];
        let pb = &b.properties[*key];
        if pa.value != pb.value {
            result.changed.push(Change {
                name: (*key).clone(),
                code: pa.code,
                value_a: pa.value.clone(),
                value_b: pb.value.clone(),
            });
        }
    }

    // Names only on one side.
    for key in keys_a.difference(&keys_b) {
        if !opts.include_volatile && is_volatile(key) {
            continue;
        }
        result.only_in_a.push((*key).clone());
    }
    for key in keys_b.difference(&keys_a) {
        if !opts.include_volatile && is_volatile(key) {
            continue;
        }
        result.only_in_b.push((*key).clone());
    }

    Ok(result)
}

pub fn is_volatile(name: &str) -> bool {
    VOLATILE_SUBSTRINGS.iter().any(|s| name.contains(s))
}

/// Like `diff`, but adds firmware-version annotation when the two snapshots
/// were captured on different firmware versions and the schema knows about both.
pub fn diff_with_schema(
    a: &Snapshot,
    b: &Snapshot,
    opts: &DiffOptions,
    schema: &MaidLayerConfig,
) -> Result<Diff, DiffError> {
    let mut d = diff(a, b, opts)?;

    let fw_a = &a.camera.firmware;
    let fw_b = &b.camera.firmware;
    if fw_a.is_empty() || fw_b.is_empty() || fw_a == fw_b {
        return Ok(d);
    }
    let (Some(major_a), Some(major_b)) = (schema_major(fw_a), schema_major(fw_b)) else {
        return Ok(d);
    };

    // Skip if the model has no per-firmware sections (e.g. Z 5 with only "common").
    let has_versioned = schema.sections_for_model(&a.camera.model)
        .any(|s| schema_major(&s.version).is_some());
    if !has_versioned {
        return Ok(d);
    }

    // Warn and skip for any firmware version not covered by the schema.
    let known_majors: BTreeSet<u32> = schema.sections_for_model(&a.camera.model)
        .filter_map(|s| schema_major(&s.version))
        .collect();
    if !known_majors.contains(&major_a) {
        eprintln!(
            "Warning: no schema section found for {} firmware {}; cross-firmware annotation skipped.",
            a.camera.model, fw_a
        );
        return Ok(d);
    }
    if !known_majors.contains(&major_b) {
        eprintln!(
            "Warning: no schema section found for {} firmware {}; cross-firmware annotation skipped.",
            a.camera.model, fw_b
        );
        return Ok(d);
    }

    let caps_a = capability_map_for_fw(schema, &a.camera.model, major_a);
    let caps_b = capability_map_for_fw(schema, &a.camera.model, major_b);

    let schema_only_in_a: Vec<String> = d.only_in_a.iter()
        .filter(|n| caps_a.contains_key(*n) && !caps_b.contains_key(*n))
        .cloned().collect();
    let schema_only_in_b: Vec<String> = d.only_in_b.iter()
        .filter(|n| caps_b.contains_key(*n) && !caps_a.contains_key(*n))
        .cloned().collect();
    // A capability is "schema-changed" only when BOTH firmware schemas define it
    // but with different allowed_ops.  When one schema omits it entirely the
    // capability falls into schema_only_in_a/b, not here.
    let schema_changed: Vec<String> = d.changed.iter()
        .filter(|c| {
            let a_ops = caps_a.get(&c.name);
            let b_ops = caps_b.get(&c.name);
            a_ops.is_some() && b_ops.is_some() && a_ops != b_ops
        })
        .map(|c| c.name.clone()).collect();

    d.firmware_annotation = Some(FirmwareAnnotation {
        firmware_a: fw_a.clone(),
        firmware_b: fw_b.clone(),
        schema_only_in_a,
        schema_only_in_b,
        schema_changed,
    });
    Ok(d)
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maid_layer::MaidLayerConfig;
    use crate::snapshot::{Camera, Snapshot, Transport};
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn snap(props: &[(&str, u32, serde_json::Value)]) -> Snapshot {
        let mut s = Snapshot::new(
            Camera { model: "Z 9".into(), serial: "X".into(), firmware: "5.00".into() },
            Transport::Usb,
            "2026-05-24T17:30:00Z".into(),
        );
        for (n, c, v) in props {
            s.insert(*n, *c, v.clone());
        }
        s
    }

    fn snap_fw(fw: &str, props: &[(&str, u32, serde_json::Value)]) -> Snapshot {
        let mut s = snap(props);
        s.camera.firmware = fw.into();
        s
    }

    /// Minimal schema with two firmware sections for Z 9.
    fn test_schema() -> MaidLayerConfig {
        MaidLayerConfig::parse(r#"
<model:Z 9>
  <version>4.0</version>
  <caplist>
    <capability:OldCap-100>
      <description>0,0,"OldCap"</description>
      <allowedoperation:14></allowedoperation>
    </capability>
    <capability:SharedCap-200>
      <description>0,0,"SharedCap"</description>
      <allowedoperation:14></allowedoperation>
    </capability>
  </caplist>
</model>
<model:Z 9>
  <version>5.0</version>
  <caplist>
    <capability:NewCap-300>
      <description>0,0,"NewCap"</description>
      <allowedoperation:14></allowedoperation>
    </capability>
    <capability:SharedCap-200>
      <description>0,0,"SharedCap"</description>
      <allowedoperation:10></allowedoperation>
    </capability>
  </caplist>
</model>
"#).unwrap()
    }

    #[test]
    fn empty_diff_when_identical() {
        let a = snap(&[
            ("kNkMAIDCapability_Aperture", 33285, json!(56)),
            ("kNkMAIDCapability_WhiteBalance", 33172, json!(2)),
        ]);
        let b = a.clone();
        let d = diff(&a, &b, &DiffOptions::default()).unwrap();
        assert!(d.is_empty());
    }

    #[test]
    fn detects_changed_values() {
        let a = snap(&[("kNkMAIDCapability_Aperture", 33285, json!(56))]);
        let b = snap(&[("kNkMAIDCapability_Aperture", 33285, json!(80))]);
        let d = diff(&a, &b, &DiffOptions::default()).unwrap();
        assert_eq!(d.changed.len(), 1);
        assert_eq!(d.changed[0].name, "kNkMAIDCapability_Aperture");
        assert_eq!(d.changed[0].value_a, json!(56));
        assert_eq!(d.changed[0].value_b, json!(80));
        assert!(d.only_in_a.is_empty());
        assert!(d.only_in_b.is_empty());
    }

    #[test]
    fn detects_one_sided_keys() {
        let a = snap(&[
            ("kNkMAIDCapability_Aperture", 33285, json!(56)),
            ("kNkMAIDCapability_OnlyOnA", 99999, json!(1)),
        ]);
        let b = snap(&[
            ("kNkMAIDCapability_Aperture", 33285, json!(56)),
            ("kNkMAIDCapability_OnlyOnB", 99998, json!(2)),
        ]);
        let d = diff(&a, &b, &DiffOptions::default()).unwrap();
        assert!(d.changed.is_empty());
        assert_eq!(d.only_in_a, vec!["kNkMAIDCapability_OnlyOnA"]);
        assert_eq!(d.only_in_b, vec!["kNkMAIDCapability_OnlyOnB"]);
    }

    #[test]
    fn skips_volatile_properties_by_default() {
        let a = snap(&[
            ("kNkMAIDCapability_BatteryLevel", 1, json!(80)),
            ("kNkMAIDCapability_Aperture", 33285, json!(56)),
        ]);
        let b = snap(&[
            ("kNkMAIDCapability_BatteryLevel", 1, json!(45)),
            ("kNkMAIDCapability_Aperture", 33285, json!(56)),
        ]);
        let d = diff(&a, &b, &DiffOptions::default()).unwrap();
        assert!(d.is_empty(), "battery-level change should be hidden by default");
    }

    #[test]
    fn includes_volatile_when_asked() {
        let a = snap(&[("kNkMAIDCapability_BatteryLevel", 1, json!(80))]);
        let b = snap(&[("kNkMAIDCapability_BatteryLevel", 1, json!(45))]);
        let opts = DiffOptions { include_volatile: true };
        let d = diff(&a, &b, &opts).unwrap();
        assert_eq!(d.changed.len(), 1);
    }

    #[test]
    fn rejects_cross_model_diff() {
        let a = snap(&[]);
        let mut b = snap(&[]);
        b.camera.model = "Z 6III".into();
        let err = diff(&a, &b, &DiffOptions::default()).unwrap_err();
        match err {
            DiffError::ModelMismatch { model_a, model_b } => {
                assert_eq!(model_a, "Z 9");
                assert_eq!(model_b, "Z 6III");
            }
        }
    }

    // ── diff_with_schema ────────────────────────────────────────────────

    #[test]
    fn schema_no_annotation_when_same_firmware() {
        let schema = test_schema();
        let a = snap_fw("5.31", &[("SharedCap", 200, json!(1))]);
        let b = snap_fw("5.31", &[("SharedCap", 200, json!(2))]);
        let d = diff_with_schema(&a, &b, &DiffOptions::default(), &schema).unwrap();
        assert!(d.firmware_annotation.is_none(), "same firmware → no annotation");
    }

    #[test]
    fn schema_no_annotation_when_firmware_empty() {
        let schema = test_schema();
        let a = snap_fw("", &[("SharedCap", 200, json!(1))]);
        let b = snap_fw("5.31", &[("SharedCap", 200, json!(2))]);
        let d = diff_with_schema(&a, &b, &DiffOptions::default(), &schema).unwrap();
        assert!(d.firmware_annotation.is_none(), "empty firmware → no annotation");
    }

    #[test]
    fn schema_annotates_capability_added_in_newer_fw() {
        let schema = test_schema();
        // A is fw 4.31 (older), B is fw 5.00 (newer).
        // caps_a (fw4) = {OldCap, SharedCap}; caps_b (fw5) = {OldCap, SharedCap, NewCap}.
        // NewCap is only in caps_b → if it appears in d.only_in_b, it's schema_only_in_b.
        let a = snap_fw("4.31", &[("SharedCap", 200, json!(7))]);
        let b = snap_fw("5.00", &[("NewCap", 300, json!(1)), ("SharedCap", 200, json!(7))]);
        let d = diff_with_schema(&a, &b, &DiffOptions::default(), &schema).unwrap();
        let ann = d.firmware_annotation.as_ref().expect("should have annotation");
        assert_eq!(ann.firmware_a, "4.31");
        assert_eq!(ann.firmware_b, "5.00");
        assert!(ann.schema_only_in_b.contains(&"NewCap".to_string()),
            "NewCap (added in 5.x schema) should appear in schema_only_in_b");
    }

    #[test]
    fn schema_annotates_capability_only_in_older_fw() {
        let schema = test_schema();
        // A is fw 5.00 (newer), B is fw 4.31 (older).
        // caps_a (fw5) = {OldCap, SharedCap, NewCap}; caps_b (fw4) = {OldCap, SharedCap}.
        // NewCap is only in caps_a → if it appears in d.only_in_a, it's schema_only_in_a.
        let a = snap_fw("5.00", &[("NewCap", 300, json!(1)), ("SharedCap", 200, json!(7))]);
        let b = snap_fw("4.31", &[("SharedCap", 200, json!(7))]);
        let d = diff_with_schema(&a, &b, &DiffOptions::default(), &schema).unwrap();
        let ann = d.firmware_annotation.as_ref().expect("should have annotation");
        assert!(ann.schema_only_in_a.contains(&"NewCap".to_string()),
            "NewCap (only in 5.x schema) should appear in schema_only_in_a when A is the newer fw");
    }

    #[test]
    fn schema_changed_allowed_ops_detected() {
        let schema = test_schema();
        // SharedCap has allowed_ops=14 in 4.0, allowed_ops=10 in 5.0 override
        // But capability_map_for_fw takes the last-inserted value; for fw5 the 5.0
        // section overrides the 4.0 section's value → allowed_ops=10.
        // For fw4 only section 4.0 applies → allowed_ops=14.
        // diff: SharedCap value differs between A and B → should be in schema_changed.
        let a = snap_fw("4.31", &[("SharedCap", 200, json!(1))]);
        let b = snap_fw("5.00", &[("SharedCap", 200, json!(9))]);
        let d = diff_with_schema(&a, &b, &DiffOptions::default(), &schema).unwrap();
        let ann = d.firmware_annotation.as_ref().expect("should have annotation");
        assert!(ann.schema_changed.contains(&"SharedCap".to_string()),
            "SharedCap has different allowed_ops across firmware versions → schema_changed");
    }

    #[test]
    fn schema_changed_no_double_count_with_only_in_a() {
        // A capability that is in d.changed AND present in caps_a but absent
        // from caps_b (unusual: both cameras have it but 5.x schema omits it)
        // should NOT appear in schema_changed — only allowed_ops difference counts.
        // Here OldCap is absent from 5.0 schema so caps_b won't have it.
        // We contrive snapshots where both cameras happen to expose OldCap.
        let schema = test_schema();
        let a = snap_fw("4.31", &[("OldCap", 100, json!(1))]);
        let b = snap_fw("5.00", &[("OldCap", 100, json!(99))]);  // also has OldCap
        let d = diff_with_schema(&a, &b, &DiffOptions::default(), &schema).unwrap();
        let ann = d.firmware_annotation.as_ref().expect("should have annotation");
        // OldCap value differs → in d.changed
        assert!(d.changed.iter().any(|c| c.name == "OldCap"), "OldCap should be in changed");
        // OldCap absent from caps_b → NOT in schema_changed (one side is None)
        assert!(!ann.schema_changed.contains(&"OldCap".to_string()),
            "OldCap absent from 5.x schema should NOT be in schema_changed");
    }
}

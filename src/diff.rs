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
    let schema_changed: Vec<String> = d.changed.iter()
        .filter(|c| caps_a.get(&c.name) != caps_b.get(&c.name))
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
}

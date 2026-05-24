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

use crate::snapshot::Snapshot;

/// A single property whose value differs between two snapshots.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Change {
    pub name: String,
    pub code: u32,
    pub value_a: serde_json::Value,
    pub value_b: serde_json::Value,
}

/// Result of comparing two snapshots. `only_in_a` and `only_in_b` should
/// typically be empty when comparing snapshots of the same body unless one
/// was captured before a firmware update that added or removed properties.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Diff {
    pub changed: Vec<Change>,
    pub only_in_a: Vec<String>,
    pub only_in_b: Vec<String>,
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

fn is_volatile(name: &str) -> bool {
    VOLATILE_SUBSTRINGS.iter().any(|s| name.contains(s))
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

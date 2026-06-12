//! Snapshot data model: one file = one capture of one camera's settings.
//!
//! ## Design choices
//!
//! - **Keyed by capability name, not code.** Names are stable, human-readable,
//!   and survive small changes to the underlying numeric codes between
//!   firmware versions. We carry the code as a field too, so we never lose
//!   the protocol-level information.
//!
//! - **`serde_json::Value` as the property value type.** Property values are
//!   heterogeneous (ints, strings, byte arrays, nested arrays). A fully
//!   typed enum would be brittle as new types appear; storing whatever the
//!   camera reports as JSON keeps round-trip fidelity perfect, and diffing
//!   two `Value`s for equality is trivial. We can always add a typed view
//!   later if the diff display wants it.
//!
//! - **`BTreeMap` for `properties`.** Sorted output means git diffs on the
//!   JSON file show real changes, not key-order churn.
//!
//! - **`format_version` from day one.** Cheap insurance against having to
//!   re-snapshot when the schema evolves.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Schema version of the snapshot file. Bump when the format changes.
pub const CURRENT_FORMAT_VERSION: u32 = 1;

/// Camera identity. Together (model, serial) uniquely identifies a body.
/// `firmware` is informational — useful when diffing reveals a value change
/// that turns out to be a firmware-update artifact rather than user action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Camera {
    pub model: String,
    pub serial: String,
    pub firmware: String,
}

/// How we got to the camera. Mostly informational; useful for the future
/// "snapshot's protocol-set differs from what was readable today" case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// Real USB connection via the Nikon SDK / PTP.
    Usb,
    /// Real PTP-IP over Ethernet or WiFi.
    Ptpip,
    /// Loaded from a file (a snapshot of a snapshot, e.g. for tests).
    File,
}

/// One captured property: the value as serialized by the camera plus its
/// numeric code. Storing the code lets the diff tool surface protocol-level
/// info when a name changes between firmware versions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropertyEntry {
    pub code: u32,
    pub value: serde_json::Value,
}

/// A full settings snapshot from one camera at one moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub format_version: u32,
    pub camera: Camera,
    /// RFC 3339 / ISO 8601 timestamp string, in UTC.
    /// Stored as a string so the file format stays trivially human-editable
    /// without pulling typed-time-with-serde plumbing into the public schema.
    pub captured_at: String,
    /// Optional user label, e.g. "wedding-baseline" or "post-firmware-update".
    pub label: Option<String>,
    pub transport: Transport,
    /// Properties keyed by symbolic capability name (e.g. "kNkMAIDCapability_Aperture").
    pub properties: BTreeMap<String, PropertyEntry>,
}

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported format_version {found}; this build expects {expected}")]
    UnsupportedVersion { found: u32, expected: u32 },
}

impl Snapshot {
    /// Construct an empty snapshot for a given camera + transport. Caller
    /// fills in properties before saving.
    pub fn new(camera: Camera, transport: Transport, captured_at: String) -> Self {
        Snapshot {
            format_version: CURRENT_FORMAT_VERSION,
            camera,
            captured_at,
            label: None,
            transport,
            properties: BTreeMap::new(),
        }
    }

    /// Convenience: insert one property.
    pub fn insert(&mut self, name: impl Into<String>, code: u32, value: serde_json::Value) {
        self.properties.insert(name.into(), PropertyEntry { code, value });
    }

    /// Pretty-printed JSON. We use pretty (not compact) because these
    /// files are meant to be read and git-diffed by humans.
    pub fn to_pretty_json(&self) -> Result<String, SnapshotError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), SnapshotError> {
        fs::write(path, self.to_pretty_json()?)?;
        Ok(())
    }

    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, SnapshotError> {
        let text = fs::read_to_string(path)?;
        Self::from_json(&text)
    }

    pub fn from_json(text: &str) -> Result<Self, SnapshotError> {
        let snap: Snapshot = serde_json::from_str(text)?;
        if snap.format_version != CURRENT_FORMAT_VERSION {
            return Err(SnapshotError::UnsupportedVersion {
                found: snap.format_version,
                expected: CURRENT_FORMAT_VERSION,
            });
        }
        Ok(snap)
    }

    /// Suggested filename: `{model_slug}_{serial}_{label}_{timestamp}.json`.
    /// Spaces in model names become underscores. Timestamps lose colons.
    pub fn suggested_filename(&self) -> String {
        let model = self.camera.model.replace(' ', "_");
        let serial = if self.camera.serial.is_empty() {
            "unknown".to_string()
        } else {
            self.camera.serial.clone()
        };
        let label = self.label.as_deref().unwrap_or("snap");
        let ts = self.captured_at.replace(':', "").replace('-', "");
        format!("{model}_{serial}_{label}_{ts}.json")
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn sample_snapshot() -> Snapshot {
        let mut s = Snapshot::new(
            Camera {
                model: "Z 9".into(),
                serial: "ABC123".into(),
                firmware: "5.00".into(),
            },
            Transport::Usb,
            "2026-05-24T17:30:00Z".into(),
        );
        s.label = Some("baseline".into());
        s.insert("kNkMAIDCapability_Aperture", 33285, json!(56));
        s.insert("kNkMAIDCapability_WhiteBalance", 33172, json!(2));
        s.insert("kNkMAIDCapability_PictureControlName", 33614, json!("Standard"));
        s
    }

    #[test]
    fn round_trip_json() {
        let original = sample_snapshot();
        let text = original.to_pretty_json().unwrap();
        let parsed = Snapshot::from_json(&text).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn unsupported_version_rejected() {
        let mut bad = sample_snapshot();
        bad.format_version = 999;
        let text = serde_json::to_string(&bad).unwrap();
        let err = Snapshot::from_json(&text).unwrap_err();
        match err {
            SnapshotError::UnsupportedVersion { found, expected } => {
                assert_eq!(found, 999);
                assert_eq!(expected, CURRENT_FORMAT_VERSION);
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn deterministic_property_ordering() {
        // Two snapshots with the same properties inserted in different
        // orders must serialize identically. (BTreeMap, not HashMap.)
        let mut a = Snapshot::new(
            Camera { model: "Z 9".into(), serial: "X".into(), firmware: "5.00".into() },
            Transport::Usb,
            "2026-01-01T00:00:00Z".into(),
        );
        let mut b = a.clone();
        a.insert("kNkMAIDCapability_Aperture", 33285, json!(56));
        a.insert("kNkMAIDCapability_WhiteBalance", 33172, json!(2));
        b.insert("kNkMAIDCapability_WhiteBalance", 33172, json!(2));
        b.insert("kNkMAIDCapability_Aperture", 33285, json!(56));
        assert_eq!(a.to_pretty_json().unwrap(), b.to_pretty_json().unwrap());
    }

    #[test]
    fn filename_shape() {
        let s = sample_snapshot();
        let name = s.suggested_filename();
        // Exact contract: spaces→_, no colons, no dashes in timestamp.
        // captured_at="2026-05-24T17:30:00Z" → "20260524T173000Z"
        assert_eq!(name, "Z_9_ABC123_baseline_20260524T173000Z.json");
    }

    #[test]
    fn filename_empty_serial_uses_unknown() {
        // Surviving mutant: removing the empty-serial branch makes this return
        // "Z_9__baseline_..." which doesn't match reference filename convention.
        let mut s = sample_snapshot();
        s.camera.serial = String::new();
        let name = s.suggested_filename();
        assert!(name.starts_with("Z_9_unknown_"), "empty serial must map to 'unknown'");
    }
}

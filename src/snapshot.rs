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

use std::collections::{BTreeMap, BTreeSet};
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

impl SnapshotError {
    /// Closed machine-readable tag, for structured output. Exhaustive match.
    pub fn tag(&self) -> &'static str {
        match self {
            SnapshotError::Io(_) => "io",
            SnapshotError::Json(_) => "json",
            SnapshotError::UnsupportedVersion { .. } => "unsupported_version",
        }
    }
}

/// Fill the staged file: all of `bytes`, then to disk. A seam so a test can fail the write half way.
type StageWrite<'a> = &'a dyn Fn(&mut fs::File, &[u8]) -> std::io::Result<()>;
/// Give the staged file its final name without replacing anything. A seam so a test can see the
/// directory mid-publish.
type Publish<'a> = &'a dyn Fn(&Path, &Path) -> std::io::Result<()>;
/// Hard-link the first path to the second. A seam so a test can make links unsupported.
type Link<'a> = &'a dyn Fn(&Path, &Path) -> std::io::Result<()>;

fn write_synced(file: &mut fs::File, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    file.write_all(bytes)?;
    file.sync_all()
}

/// `errno`s that mean "this filesystem has no hard links" (macOS FAT/exFAT/SMB give `EPERM`), as
/// opposed to a real failure. The same set `gui/fleet_archive.py` falls back on.
fn hard_links_unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EPERM | libc::ENOTSUP | libc::ENOSYS | libc::EMLINK | libc::EOPNOTSUPP)
    )
}

/// Hard-link `tmp` to `dest`, which fails with `AlreadyExists` if `dest` is taken (a symlink, dangling
/// or not, included). Where links are unsupported: a checked rename.
fn link_no_replace(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    link_or_rename(&|a, b| fs::hard_link(a, b), tmp, dest)
}

fn link_or_rename(link: Link<'_>, tmp: &Path, dest: &Path) -> std::io::Result<()> {
    match link(tmp, dest) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists && hard_links_unsupported(&e) => {
            if fs::symlink_metadata(dest).is_ok() {
                return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
            }
            fs::rename(tmp, dest)
        }
        other => other,
    }
}

/// Create `dest` holding all of `bytes`, or not at all, never replacing anything. See
/// [`Snapshot::save_new`]. The temporary file is gone when this returns, success or not.
fn publish_new(dest: &Path, bytes: &[u8], write: StageWrite<'_>, publish: Publish<'_>) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let dir = dest.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // `create_new`: never reuse a stale temporary left by a process that died with the same pid.
    let (mut file, tmp) = loop {
        let tmp = dir.join(format!(".{name}.{}.{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        match fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => break (f, tmp),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let result = write(&mut file, bytes).and_then(|()| {
        drop(file);
        publish(&tmp, dest)
    });
    // The temporary name is always removed: after a link it is the second name of the published
    // file, after a rename it is already gone (hence ignoring NotFound), after a failure it is debris.
    // A failure here is not the caller's: `result` is.
    if let Err(e) = fs::remove_file(&tmp) {
        if e.kind() != std::io::ErrorKind::NotFound && result.is_ok() {
            // The file is published whole; only a stray second name remains.
            eprintln!("warning: could not remove the temporary file {}: {e}", tmp.display());
        }
    }
    result
}

/// Replace path-separator characters in a free-text value bound for a
/// filename component. `/` is the dangerous one on any platform this runs
/// on (macOS/Linux path separator; also treated as a separator by Rust's
/// `Path` on Windows); `\` is included too since it's the Windows separator
/// and this codebase ships a Windows SDK bundle path.
fn sanitize_filename_component(s: &str) -> String {
    s.replace(['/', '\\'], "_")
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

    /// Capability codes present in this snapshot. A capture inserts a property
    /// only when the read succeeded, so this is the readable set.
    pub fn readable_codes(&self) -> BTreeSet<u32> {
        self.properties.values().map(|p| p.code).collect()
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

    /// Write to `path`, replacing whatever is there, whole or not at all: staged in a temporary file
    /// beside `path` and renamed over it ([`crate::ops::paths::write_atomic`]), so a reader or a crash
    /// sees the old file or the complete new one. Use [`Snapshot::save_new`] for captures, which must
    /// never overwrite an existing file.
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), SnapshotError> {
        crate::ops::paths::write_atomic(path.as_ref(), self.to_pretty_json()?.as_bytes())?;
        Ok(())
    }

    /// Write to `path` only if nothing exists there: an existing file, or a symlink at `path` (even
    /// a dangling one), fails with an `AlreadyExists` I/O error and is left untouched.
    ///
    /// The file appears whole or not at all. The bytes are staged in a temporary file in the same
    /// directory (a `.tmp` name, never a `.json`, so no lister or retention count sees it), synced,
    /// then given the final name with a hard link, which fails if the name is taken; the temporary
    /// name is removed afterwards. Where the filesystem has no hard links the publish is a checked
    /// rename instead, with a small window between the existence check and the rename. A failure at
    /// any step leaves neither the file nor the temporary.
    pub fn save_new<P: AsRef<Path>>(&self, path: P) -> Result<(), SnapshotError> {
        publish_new(path.as_ref(), self.to_pretty_json()?.as_bytes(), &write_synced, &link_no_replace)?;
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
        // model and serial come from the camera (USB descriptors) or, in a restore point, from
        // a snapshot file: neither may carry a separator into the join below.
        let model = sanitize_filename_component(&crate::firmware::model_slug(&self.camera.model));
        let serial = if self.camera.serial.is_empty() {
            "unknown".to_string()
        } else {
            sanitize_filename_component(&self.camera.serial)
        };
        // label is free-text (CLI --label or a GUI text field) — a "/" in it
        // would otherwise be re-split into path components by Path::join,
        // redirecting the write target into a different directory.
        let label = sanitize_filename_component(self.label.as_deref().unwrap_or("snap"));
        let ts = self.captured_at.replace(':', "").replace('-', "");
        format!("{model}_{serial}_{label}_{ts}.json")
    }
}

/// The subset of a snapshot's fields needed to list it (a GUI's snapshot
/// list, `fleet ls`).
///
/// It accepts exactly the files [`Snapshot::from_json`] accepts: [`SnapshotSummary::from_json`]
/// parses the whole file (`format_version`, `properties` and `transport` included) and keeps the
/// summary fields, so a listing never shows a file that loading it for use would refuse. There is
/// one set of validity rules. Parsing the `properties` map costs nothing a listing of a few
/// hundred small files would notice. The derived `Deserialize` is lenient (it reads these four
/// fields and ignores the rest); use `from_json` / `load_from_file`, not `serde_json::from_str`.
#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotSummary {
    pub format_version: u32,
    pub camera: Camera,
    pub captured_at: String,
    pub label: Option<String>,
}

impl SnapshotSummary {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, SnapshotError> {
        let text = fs::read_to_string(path)?;
        Self::from_json(&text)
    }

    pub fn from_json(text: &str) -> Result<Self, SnapshotError> {
        let snap = Snapshot::from_json(text)?;
        Ok(SnapshotSummary {
            format_version: snap.format_version,
            camera: snap.camera,
            captured_at: snap.captured_at,
            label: snap.label,
        })
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
    fn malformed_json_rejected_as_json_error() {
        // Distinct from unsupported_version_rejected below (valid JSON,
        // wrong format_version) — this pins genuinely truncated/malformed
        // input, which the SnapshotError::Json variant exists for but
        // nothing previously exercised.
        let err = Snapshot::from_json(r#"{"camera": {"model": "Z 9""#).unwrap_err();
        assert!(matches!(err, SnapshotError::Json(_)), "expected Json error, got {err:?}");
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
    fn filename_label_with_slash_is_sanitized() {
        // Regression: a "/" in a free-text label (CLI --label or a GUI text
        // field) used to be embedded unsanitized, so Path::join would
        // re-split it into path components and redirect the write target.
        let mut s = sample_snapshot();
        s.label = Some("wedding/../../escape".into());
        let name = s.suggested_filename();
        assert!(!name.contains('/'), "sanitized filename must not contain a path separator: {name}");
        assert!(name.contains("wedding_.._.._escape"));
    }

    #[test]
    fn summary_reads_full_snapshot_json_ignoring_properties() {
        // A full Snapshot's JSON must also parse as a SnapshotSummary
        // (listing code reads whatever real snapshot files already exist
        // on disk) — this pins that SnapshotSummary accepts the same shape
        // Snapshot::from_json does, just ignoring "properties" and
        // "format_version"/"transport".
        let original = sample_snapshot();
        let text = original.to_pretty_json().unwrap();
        let summary: SnapshotSummary = serde_json::from_str(&text).unwrap();
        assert_eq!(summary.camera, original.camera);
        assert_eq!(summary.captured_at, original.captured_at);
        assert_eq!(summary.label, original.label);
    }

    #[test]
    fn filename_model_and_serial_with_separators_are_sanitized_too() {
        let mut s = sample_snapshot();
        s.camera.serial = "../x/y".into();
        s.camera.model = "Z/9\\".into();
        let name = s.suggested_filename();
        assert!(!name.contains(['/', '\\']), "{name}");
        assert_eq!(name, "Z_9__.._x_y_baseline_20260524T173000Z.json");
        // A plain serial is unchanged: the name every existing snapshot has.
        assert_eq!(sample_snapshot().suggested_filename(), "Z_9_ABC123_baseline_20260524T173000Z.json");
    }

    /// Every way a snapshot file can be wrong or right, and what each reader must say.
    fn validity_cases() -> Vec<(&'static str, String, bool)> {
        let good = serde_json::to_value(sample_snapshot()).unwrap();
        let without = |key: &str| {
            let mut v = good.clone();
            v.as_object_mut().unwrap().remove(key);
            v.to_string()
        };
        let with = |key: &str, value: serde_json::Value| {
            let mut v = good.clone();
            v[key] = value;
            v.to_string()
        };
        vec![
            ("valid", good.to_string(), true),
            ("wrong format_version", with("format_version", json!(999)), false),
            ("format_version 0", with("format_version", json!(0)), false),
            ("no format_version", without("format_version"), false),
            ("no properties", without("properties"), false),
            ("properties not a map", with("properties", json!([1])), false),
            ("property without a code", with("properties", json!({"A": {"value": 1}})), false),
            ("no transport", without("transport"), false),
            ("unknown transport", with("transport", json!("carrier-pigeon")), false),
            ("no camera", without("camera"), false),
            ("no captured_at", without("captured_at"), false),
            ("null label is fine", with("label", json!(null)), true),
            ("extra unknown key is fine", with("extra", json!({"x": 1})), true),
            ("not json", "{ nope".to_string(), false),
        ]
    }

    #[test]
    fn the_summary_accepts_exactly_the_files_the_snapshot_reader_accepts() {
        for (what, text, ok) in validity_cases() {
            assert_eq!(Snapshot::from_json(&text).is_ok(), ok, "Snapshot: {what}");
            assert_eq!(SnapshotSummary::from_json(&text).is_ok(), ok, "SnapshotSummary: {what}");
        }
    }

    #[test]
    fn summary_rejects_unsupported_format_version() {
        // Same contract as Snapshot::from_json — a version-incompatible
        // file must be rejected by the summary loader too, not silently
        // listed as if it were readable.
        let mut bad = sample_snapshot();
        bad.format_version = 999;
        let text = serde_json::to_string(&bad).unwrap();
        let err = SnapshotSummary::from_json(&text).unwrap_err();
        assert!(matches!(err, SnapshotError::UnsupportedVersion { found: 999, .. }));
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
    // ── save_new (create-new) ────────────────────────────────────────────

    fn io_kind(e: SnapshotError) -> std::io::ErrorKind {
        match e {
            SnapshotError::Io(e) => e.kind(),
            other => panic!("expected an I/O error, got {other:?}"),
        }
    }

    #[test]
    fn save_new_writes_a_file_that_loads_back_equal() {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("s.json");
        let s = sample_snapshot();
        s.save_new(&p).unwrap();
        assert_eq!(Snapshot::load_from_file(&p).unwrap(), s);
    }

    #[test]
    fn save_new_refuses_an_existing_file_and_leaves_its_bytes_alone() {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("s.json");
        std::fs::write(&p, "precious").unwrap();
        let e = sample_snapshot().save_new(&p).unwrap_err();
        assert_eq!(io_kind(e), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "precious");
    }

    #[test]
    fn save_new_refuses_an_existing_empty_file() {
        // Boundary: zero bytes still counts as "exists".
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("s.json");
        std::fs::write(&p, "").unwrap();
        let e = sample_snapshot().save_new(&p).unwrap_err();
        assert_eq!(io_kind(e), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&p).unwrap().len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn save_new_does_not_follow_a_symlink_to_an_existing_file() {
        let d = tempfile::TempDir::new().unwrap();
        let target = d.path().join("target.json");
        std::fs::write(&target, "precious").unwrap();
        let link = d.path().join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let e = sample_snapshot().save_new(&link).unwrap_err();
        assert_eq!(io_kind(e), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "precious");
    }

    #[cfg(unix)]
    #[test]
    fn save_new_does_not_create_through_a_dangling_symlink() {
        let d = tempfile::TempDir::new().unwrap();
        let target = d.path().join("not-yet.json");
        let link = d.path().join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let e = sample_snapshot().save_new(&link).unwrap_err();
        assert_eq!(io_kind(e), std::io::ErrorKind::AlreadyExists);
        assert!(!target.exists(), "must not create the symlink's target");
    }

    #[test]
    fn save_new_with_missing_parent_is_not_found_and_creates_nothing() {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("no-such-dir/s.json");
        let e = sample_snapshot().save_new(&p).unwrap_err();
        assert_eq!(io_kind(e), std::io::ErrorKind::NotFound);
        assert!(!d.path().join("no-such-dir").exists());
    }

    #[test]
    fn save_new_writes_an_empty_properties_snapshot() {
        // Whether an empty capture is a failure is the caller's gate, not the
        // writer's; the writer must not silently skip it.
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("empty.json");
        let mut s = sample_snapshot();
        s.properties.clear();
        s.save_new(&p).unwrap();
        assert!(Snapshot::load_from_file(&p).unwrap().properties.is_empty());
    }

    #[test]
    fn save_to_file_still_overwrites() {
        // Pin the contrast: only captures use save_new.
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("s.json");
        std::fs::write(&p, "old").unwrap();
        sample_snapshot().save_to_file(&p).unwrap();
        assert_eq!(Snapshot::load_from_file(&p).unwrap(), sample_snapshot());
    }

    // ── whole-or-nothing writes ──────────────────────────────────────────

    use std::io::Write as _;
    use std::path::PathBuf;

    fn dir_names(d: &Path) -> Vec<String> {
        let mut v: Vec<String> =
            fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    fn sample_bytes() -> Vec<u8> {
        sample_snapshot().to_pretty_json().unwrap().into_bytes()
    }

    /// A write that stores the first half of `bytes` and then fails (a full disk, a yanked volume).
    fn half_then_fail(file: &mut fs::File, bytes: &[u8]) -> std::io::Result<()> {
        file.write_all(&bytes[..bytes.len() / 2])?;
        Err(std::io::Error::other("disk full"))
    }

    fn link(a: &Path, b: &Path) -> std::io::Result<()> {
        fs::hard_link(a, b)
    }

    #[test]
    fn save_new_leaves_only_the_snapshot_behind() {
        let d = tempfile::TempDir::new().unwrap();
        sample_snapshot().save_new(d.path().join("s.json")).unwrap();
        assert_eq!(dir_names(d.path()), ["s.json"], "no staging file may remain after a successful save");
    }

    #[test]
    fn a_write_that_fails_half_way_leaves_no_file_and_no_temporary() {
        let d = tempfile::TempDir::new().unwrap();
        let dest = d.path().join("s.json");
        let e = publish_new(&dest, &sample_bytes(), &half_then_fail, &link_no_replace).unwrap_err();
        assert_eq!(e.to_string(), "disk full");
        assert_eq!(dir_names(d.path()), Vec::<String>::new(), "neither a truncated snapshot nor its temporary");
    }

    #[test]
    fn the_final_name_does_not_exist_until_the_staged_file_is_complete() {
        // While the bytes are being written, and at the instant of publishing, no `.json` is
        // visible, so a lister or the retention count cannot meet a partial capture.
        let d = tempfile::TempDir::new().unwrap();
        let dest = d.path().join("s.json");
        let seen_during_write = std::cell::RefCell::new(Vec::new());
        let seen_at_publish = std::cell::RefCell::new(Vec::new());
        let bytes = sample_bytes();
        let counts = std::cell::RefCell::new(None);
        publish_new(
            &dest,
            &bytes,
            &|f, b| {
                f.write_all(&b[..b.len() / 2])?;
                f.flush()?;
                *seen_during_write.borrow_mut() = dir_names(d.path());
                *counts.borrow_mut() =
                    Some(crate::ops::retention::count_for_camera(d.path(), "Z 9", "ABC123").unwrap());
                f.write_all(&b[b.len() / 2..])?;
                f.sync_all()
            },
            &|tmp, dest| {
                *seen_at_publish.borrow_mut() = dir_names(d.path());
                assert_eq!(fs::read(tmp).unwrap(), bytes, "the staged file is complete when it is published");
                link(tmp, dest)
            },
        )
        .unwrap();
        for seen in [seen_during_write.borrow(), seen_at_publish.borrow()] {
            assert_eq!(seen.len(), 1, "exactly the staging file: {seen:?}");
            assert!(seen[0].starts_with(".s.json.") && seen[0].ends_with(".tmp"), "{seen:?}");
        }
        let c = counts.borrow().unwrap();
        assert_eq!((c.captures, c.restore_points, c.unreadable), (0, 0, 0), "the retention count ignores the staging file");
        assert_eq!(fs::read(&dest).unwrap(), bytes);
        assert_eq!(dir_names(d.path()), ["s.json"]);
    }

    #[test]
    fn a_refused_publish_removes_the_temporary_and_keeps_the_existing_file() {
        let d = tempfile::TempDir::new().unwrap();
        let dest = d.path().join("s.json");
        fs::write(&dest, "precious").unwrap();
        let e = sample_snapshot().save_new(&dest).unwrap_err();
        assert_eq!(io_kind(e), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "precious");
        assert_eq!(dir_names(d.path()), ["s.json"], "no staging file may remain after a refused save");
    }

    #[test]
    fn a_publish_error_other_than_exists_is_returned_and_cleans_up() {
        let d = tempfile::TempDir::new().unwrap();
        let dest = d.path().join("s.json");
        let e = publish_new(&dest, &sample_bytes(), &write_synced, &|_, _| {
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        })
        .unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EIO));
        assert_eq!(dir_names(d.path()), Vec::<String>::new());
    }

    fn unsupported_link(_: &Path, _: &Path) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(libc::EPERM))
    }

    #[test]
    fn where_hard_links_are_unsupported_the_publish_is_a_checked_rename() {
        let d = tempfile::TempDir::new().unwrap();
        let dest = d.path().join("s.json");
        let bytes = sample_bytes();
        publish_new(&dest, &bytes, &write_synced, &|t, p| link_or_rename(&unsupported_link, t, p)).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), bytes);
        assert_eq!(dir_names(d.path()), ["s.json"], "the rename leaves no second name");
    }

    #[test]
    fn the_rename_fallback_still_refuses_to_replace() {
        let d = tempfile::TempDir::new().unwrap();
        let dest = d.path().join("s.json");
        fs::write(&dest, "precious").unwrap();
        let e = publish_new(&dest, &sample_bytes(), &write_synced, &|t, p| link_or_rename(&unsupported_link, t, p))
            .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "precious");
        assert_eq!(dir_names(d.path()), ["s.json"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_rename_fallback_refuses_a_dangling_symlink() {
        let d = tempfile::TempDir::new().unwrap();
        let target = d.path().join("not-yet.json");
        let dest = d.path().join("s.json");
        std::os::unix::fs::symlink(&target, &dest).unwrap();
        let e = publish_new(&dest, &sample_bytes(), &write_synced, &|t, p| link_or_rename(&unsupported_link, t, p))
            .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(!target.exists(), "must not create through the link");
    }

    #[test]
    fn a_link_error_that_is_not_unsupported_does_not_fall_back_to_a_rename() {
        // EIO is a real failure, and EEXIST is the answer: neither may be turned into a rename.
        for errno in [libc::EIO, libc::EEXIST, libc::EACCES] {
            let d = tempfile::TempDir::new().unwrap();
            let dest = d.path().join("s.json");
            let failing = move |_: &Path, _: &Path| Err(std::io::Error::from_raw_os_error(errno));
            let e = publish_new(&dest, &sample_bytes(), &write_synced, &|t, p| link_or_rename(&failing, t, p))
                .unwrap_err();
            assert_eq!(e.raw_os_error(), Some(errno));
            assert!(!dest.exists(), "errno {errno}");
        }
    }

    #[test]
    fn every_unsupported_errno_takes_the_fallback_and_its_neighbours_do_not() {
        for errno in [libc::EPERM, libc::ENOTSUP, libc::ENOSYS, libc::EMLINK, libc::EOPNOTSUPP] {
            assert!(hard_links_unsupported(&std::io::Error::from_raw_os_error(errno)), "errno {errno}");
        }
        for errno in [libc::EIO, libc::EEXIST, libc::EACCES, libc::ENOSPC, libc::EXDEV, libc::ENOENT] {
            assert!(!hard_links_unsupported(&std::io::Error::from_raw_os_error(errno)), "errno {errno}");
        }
        assert!(!hard_links_unsupported(&std::io::Error::other("no os error")));
    }

    /// A read-only directory, or `None` when this process can write into one anyway (root).
    #[cfg(unix)]
    fn read_only_dir() -> Option<(tempfile::TempDir, PathBuf)> {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::TempDir::new().unwrap();
        let ro = d.path().join("ro");
        fs::create_dir(&ro).unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
        if fs::File::create(ro.join("probe")).is_ok() {
            return None;
        }
        Some((d, ro))
    }

    #[cfg(unix)]
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }
    /// Lets `TempDir` delete a directory this test made read-only.
    #[cfg(unix)]
    struct RestoreMode(PathBuf);

    #[cfg(unix)]
    #[test]
    fn saving_into_a_read_only_directory_fails_and_creates_nothing() {
        let Some((_keep, ro)) = read_only_dir() else { return };
        let _restore = RestoreMode(ro.clone());
        let e = sample_snapshot().save_new(ro.join("s.json")).unwrap_err();
        assert_eq!(io_kind(e), std::io::ErrorKind::PermissionDenied);
        let e = sample_snapshot().save_to_file(ro.join("s.json")).unwrap_err();
        assert_eq!(io_kind(e), std::io::ErrorKind::PermissionDenied);
        assert_eq!(dir_names(&ro), Vec::<String>::new());
    }

    #[test]
    fn save_to_file_onto_a_directory_fails_and_leaves_nothing_behind() {
        let d = tempfile::TempDir::new().unwrap();
        let target = d.path().join("s.json");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("inside"), "x").unwrap();
        assert!(sample_snapshot().save_to_file(&target).is_err());
        assert_eq!(dir_names(d.path()), ["s.json"], "no staging file may remain after a failed save");
        assert_eq!(dir_names(&target), ["inside"]);
    }

    #[test]
    fn save_to_file_replaces_whole_so_an_open_reader_keeps_the_old_complete_file() {
        // `fs::write` truncates in place: a reader that already has the file open sees it emptied and
        // refilled. A staged file renamed over it leaves that reader the old bytes, intact.
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("s.json");
        let mut old = sample_snapshot();
        old.label = Some("old".into());
        old.save_to_file(&p).unwrap();
        let mut reader = fs::File::open(&p).unwrap();
        let mut new = sample_snapshot();
        new.label = Some("new".into());
        new.save_to_file(&p).unwrap();
        let mut seen = String::new();
        std::io::Read::read_to_string(&mut reader, &mut seen).unwrap();
        assert_eq!(Snapshot::from_json(&seen).unwrap().label.as_deref(), Some("old"));
        assert_eq!(Snapshot::load_from_file(&p).unwrap().label.as_deref(), Some("new"));
        assert_eq!(dir_names(d.path()), ["s.json"]);
    }
}

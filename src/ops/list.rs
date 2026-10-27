//! Listing stored snapshots and references: a structured result, no printing.
//!
//! The CLI prints it as text (`ls`, `ref list`) or as JSON (`--json`); the
//! MCP will return it as is. Every entry in the directory that looks like a
//! snapshot is accounted for: an unreadable file is reported as such, not
//! dropped, and directory entries the OS refused to yield are counted.

use std::fs;
use std::path::Path;

use serde::Serialize;
use thiserror::Error;

use crate::firmware::model_slug;
use crate::snapshot::Snapshot;

/// Whether a snapshot `name` (a filename) belongs to the camera identified
/// by `filter` (a "{model}_{serial}" string, per `--camera`'s documented
/// form). Matches the whole leading `{model_slug}_{serial}` component, not
/// just a string prefix — without the trailing "_", a filter of "…_id-1"
/// would also match "…_id-10_..." (the "id-{id}" fallback serial makes this
/// a real, not just theoretical, collision: "id-1" is a proper string
/// prefix of "id-10").
pub fn camera_filename_matches(name: &str, filter: &str) -> bool {
    name.starts_with(&format!("{}_", model_slug(filter)))
}

#[derive(Debug, Error)]
pub enum ListError {
    #[error("listing {dir}: {source}")]
    Io {
        dir: String,
        #[source]
        source: std::io::Error,
    },
}

/// One `*.json` file in the listed directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListEntry {
    /// The file name, lossily converted if it is not valid UTF-8.
    pub file: String,
    #[serde(flatten)]
    pub state: EntryState,
}

/// What loading the file showed. Serialized with a `status` tag: `"ok"` with
/// the summary fields, or `"unreadable"` with a closed `error_kind` (see
/// `SnapshotError::tag`) and the error text for a human.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EntryState {
    Ok {
        model: String,
        serial: String,
        firmware: String,
        label: Option<String>,
        captured_at: String,
        property_count: usize,
    },
    Unreadable {
        error_kind: &'static str,
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listing {
    pub dir: String,
    /// False when the directory does not exist (not an error: nothing stored yet).
    pub dir_exists: bool,
    /// `*.json` entries after the camera filter, sorted by path.
    pub entries: Vec<ListEntry>,
    /// Directory entries the OS failed to yield. Counted, never silently dropped.
    pub skipped_dir_entries: usize,
}

/// Split directory-iteration results into the paths and a count of the
/// entries the OS failed to yield (never dropped silently).
fn collect_paths(
    entries: impl Iterator<Item = std::io::Result<std::path::PathBuf>>,
) -> (Vec<std::path::PathBuf>, usize) {
    let mut paths = Vec::new();
    let mut skipped = 0;
    for entry in entries {
        match entry {
            Ok(p) => paths.push(p),
            Err(_) => skipped += 1,
        }
    }
    (paths, skipped)
}

/// List `dir`, keeping only entries for the camera in `camera_filter`
/// (`"{model}_{serial}"`) when given. A missing directory is an empty listing
/// with `dir_exists: false`; a directory that cannot be read is an error.
pub fn list_dir(dir: &Path, camera_filter: Option<&str>) -> Result<Listing, ListError> {
    let mut listing = Listing {
        dir: dir.display().to_string(),
        dir_exists: dir.exists(),
        entries: Vec::new(),
        skipped_dir_entries: 0,
    };
    if !listing.dir_exists {
        return Ok(listing);
    }
    let read = fs::read_dir(dir).map_err(|source| ListError::Io { dir: listing.dir.clone(), source })?;
    let (mut paths, skipped) = collect_paths(read.map(|r| r.map(|e| e.path())));
    listing.skipped_dir_entries = skipped;
    paths.retain(|p| p.extension().is_some_and(|e| e == "json"));
    paths.sort();

    for path in paths {
        let file = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(f) = camera_filter {
            if !camera_filename_matches(&file, f) {
                continue;
            }
        }
        let state = match Snapshot::load_from_file(&path) {
            Ok(s) => EntryState::Ok {
                property_count: s.properties.len(),
                model: s.camera.model,
                serial: s.camera.serial,
                firmware: s.camera.firmware,
                label: s.label,
                captured_at: s.captured_at,
            },
            Err(e) => EntryState::Unreadable { error_kind: e.tag(), error: e.to_string() },
        };
        listing.entries.push(ListEntry { file, state });
    }
    Ok(listing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Camera, Transport};
    use serde_json::json;
    use tempfile::TempDir;

    fn snap(model: &str, serial: &str, label: Option<&str>) -> Snapshot {
        let mut s = Snapshot::new(
            Camera { model: model.into(), serial: serial.into(), firmware: "5.31".into() },
            Transport::Usb,
            "2026-10-27T12:00:00Z".into(),
        );
        s.label = label.map(String::from);
        s.insert("kNkMAIDCapability_A", 1, json!(1));
        s.insert("kNkMAIDCapability_B", 2, json!(2));
        s
    }

    fn put(dir: &Path, name: &str, s: &Snapshot) {
        s.save_to_file(dir.join(name)).unwrap();
    }

    // ── camera_filename_matches (moved from main.rs with its tests) ──────

    #[test]
    fn camera_filename_matches_exact_camera() {
        assert!(camera_filename_matches("Z_9_ABC123_baseline_20260101T000000Z.json", "Z 9_ABC123"));
    }

    #[test]
    fn camera_filename_matches_rejects_prefix_collision() {
        // Regression: "id-1" is a string prefix of "id-10" — without
        // anchoring on the trailing "_", filtering by the id-1 fallback
        // serial would also match id-10's snapshots.
        assert!(!camera_filename_matches("Z_9_id-10_baseline_20260101T000000Z.json", "Z 9_id-1"));
        assert!(camera_filename_matches("Z_9_id-1_baseline_20260101T000000Z.json", "Z 9_id-1"));
    }

    #[test]
    fn camera_filename_matches_rejects_other_camera() {
        assert!(!camera_filename_matches("Z_9_OTHER456_baseline_20260101T000000Z.json", "Z 9_ABC123"));
    }

    // ── list_dir ─────────────────────────────────────────────────────────

    #[test]
    fn missing_directory_is_an_empty_listing_not_an_error() {
        let d = TempDir::new().unwrap();
        let l = list_dir(&d.path().join("snapshots"), None).unwrap();
        assert!(!l.dir_exists && l.entries.is_empty() && l.skipped_dir_entries == 0);
    }

    #[test]
    fn existing_empty_directory_is_distinguishable_from_a_missing_one() {
        let d = TempDir::new().unwrap();
        let l = list_dir(d.path(), None).unwrap();
        assert!(l.dir_exists && l.entries.is_empty());
    }

    #[test]
    fn path_that_is_a_file_is_an_error_not_an_empty_listing() {
        let d = TempDir::new().unwrap();
        let f = d.path().join("not-a-dir");
        fs::write(&f, "x").unwrap();
        assert!(matches!(list_dir(&f, None), Err(ListError::Io { .. })));
    }

    #[test]
    fn entries_are_sorted_json_only_and_summarised() {
        let d = TempDir::new().unwrap();
        put(d.path(), "b.json", &snap("Z 9", "2", Some("two")));
        put(d.path(), "a.json", &snap("Z 9", "1", None));
        fs::write(d.path().join("notes.txt"), "ignore me").unwrap();
        fs::write(d.path().join("c.json.bak"), "ignore me").unwrap();
        let l = list_dir(d.path(), None).unwrap();
        let files: Vec<&str> = l.entries.iter().map(|e| e.file.as_str()).collect();
        assert_eq!(files, ["a.json", "b.json"]);
        assert_eq!(
            l.entries[1].state,
            EntryState::Ok {
                model: "Z 9".into(), serial: "2".into(), firmware: "5.31".into(),
                label: Some("two".into()), captured_at: "2026-10-27T12:00:00Z".into(), property_count: 2,
            }
        );
        match &l.entries[0].state {
            EntryState::Ok { label, .. } => assert_eq!(label, &None),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unreadable_files_are_reported_with_a_closed_kind_not_dropped() {
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("broken.json"), "{ not json").unwrap();
        let mut future = snap("Z 9", "1", None);
        future.format_version = 999;
        put(d.path(), "future.json", &future);
        fs::create_dir(d.path().join("dir.json")).unwrap(); // a directory with a .json name
        let l = list_dir(d.path(), None).unwrap();
        let kinds: Vec<(&str, &str)> = l
            .entries
            .iter()
            .map(|e| match &e.state {
                EntryState::Unreadable { error_kind, .. } => (e.file.as_str(), *error_kind),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(kinds, [("broken.json", "json"), ("dir.json", "io"), ("future.json", "unsupported_version")]);
    }

    #[test]
    fn camera_filter_keeps_only_that_camera_and_matches_on_the_whole_component() {
        let d = TempDir::new().unwrap();
        put(d.path(), "Z_9_id-1_x_1.json", &snap("Z 9", "id-1", None));
        put(d.path(), "Z_9_id-10_x_1.json", &snap("Z 9", "id-10", None));
        put(d.path(), "Z6_3_300_x_1.json", &snap("Z6_3", "300", None));
        let only = |f| -> Vec<String> {
            list_dir(d.path(), Some(f)).unwrap().entries.into_iter().map(|e| e.file).collect()
        };
        assert_eq!(only("Z 9_id-1"), ["Z_9_id-1_x_1.json"]);
        assert_eq!(only("Z6_3_300"), ["Z6_3_300_x_1.json"]);
        assert!(only("Z 9_nobody").is_empty());
        // A filter applies to the filename, so an unreadable file is filtered the same way.
        fs::write(d.path().join("Z_9_nobody_x_1.json"), "{").unwrap();
        assert_eq!(only("Z 9_nobody"), ["Z_9_nobody_x_1.json"]);
    }

    #[test]
    fn failed_directory_entries_are_counted_not_dropped() {
        let io_err = || std::io::Error::other("stale handle");
        let items = vec![Ok("a.json".into()), Err(io_err()), Ok("b.json".into()), Err(io_err()), Err(io_err())];
        let (paths, skipped) = collect_paths(items.into_iter());
        assert_eq!((paths.len(), skipped), (2, 3));
        assert_eq!(collect_paths(std::iter::empty()), (vec![], 0));
    }

    #[test]
    fn listing_json_shape() {
        let d = TempDir::new().unwrap();
        put(d.path(), "a.json", &snap("Z 9", "1", Some("base")));
        fs::write(d.path().join("b.json"), "{").unwrap();
        let v = serde_json::to_value(list_dir(d.path(), None).unwrap()).unwrap();
        assert_eq!(v["dir_exists"], json!(true));
        assert_eq!(v["skipped_dir_entries"], json!(0));
        assert_eq!(
            v["entries"][0],
            json!({"file": "a.json", "status": "ok", "model": "Z 9", "serial": "1", "firmware": "5.31",
                   "label": "base", "captured_at": "2026-10-27T12:00:00Z", "property_count": 2})
        );
        assert_eq!(v["entries"][1]["status"], json!("unreadable"));
        assert_eq!(v["entries"][1]["error_kind"], json!("json"));
        assert!(v["entries"][1]["error"].is_string());
        assert!(v["entries"][1].get("model").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_file_name_is_not_blanked() {
        use std::os::unix::ffi::OsStrExt;
        let d = TempDir::new().unwrap();
        let name = std::ffi::OsStr::from_bytes(b"bad\xffname.json");
        // Some filesystems refuse invalid UTF-8 names; skip rather than fail there.
        if fs::write(d.path().join(name), "{").is_err() {
            return;
        }
        let l = list_dir(d.path(), None).unwrap();
        assert_eq!(l.entries.len(), 1);
        assert!(l.entries[0].file.starts_with("bad") && l.entries[0].file.ends_with("name.json"));
    }
}

//! Listing stored snapshots and references: a structured result, no printing.
//!
//! The CLI prints it as text (`ls`, `ref list`) or as JSON (`--json`); the
//! MCP will return it as is. Every entry in the directory that looks like a
//! snapshot is accounted for: an unreadable file is reported as such, not
//! dropped, and directory entries the OS refused to yield are counted.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::Serialize;
use thiserror::Error;

use crate::firmware::model_slug;
use crate::ops::paths::checked_reference_filename;
use crate::snapshot::{Snapshot, SnapshotError};

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

/// File name of the reference for a camera: `{model_slug}_{serial}.json`.
/// The CLI's `ref set`, the Rust GUI and the Python GUI all write this name.
/// For a model and serial read from a data file, use
/// `ops::paths::checked_reference_filename`, which refuses a non-plain value.
pub fn reference_filename(model: &str, serial: &str) -> String {
    format!("{}_{serial}.json", model_slug(model))
}

/// A value that is safe to splice into a file name. Snapshot files are data, so
/// the model and serial inside them must not steer a lookup out of
/// `references/`. The rule is `ops::paths::is_safe_path_component`'s, kept under
/// this name for the GUIs that already import it; there is no second copy.
pub use crate::ops::paths::is_safe_path_component as is_plain_name_component;

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
        /// Whether this file is byte-identical to its camera's reference.
        /// Present only when a references directory was given.
        #[serde(skip_serializing_if = "Option::is_none")]
        is_reference: Option<bool>,
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
    /// Reference files that exist but could not be read (so their snapshots
    /// are reported `is_reference: false` without being able to tell).
    /// Counted once per distinct reference file.
    pub reference_read_failures: usize,
}

/// Read a snapshot file's bytes and parse them. The bytes are kept for the
/// byte-identity reference check. Invalid UTF-8 is an I/O-kind error, as it is
/// in `Snapshot::load_from_file`.
fn read_snapshot(path: &Path) -> Result<(Vec<u8>, Snapshot), SnapshotError> {
    let bytes = fs::read(path)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let snap = Snapshot::from_json(text)?;
    Ok((bytes, snap))
}

/// Looks up each snapshot's reference file once per (model, serial).
struct References<'a> {
    dir: &'a Path,
    cache: HashMap<(String, String), Option<Vec<u8>>>,
    read_failures: usize,
}

impl<'a> References<'a> {
    fn new(dir: &'a Path) -> Self {
        References { dir, cache: HashMap::new(), read_failures: 0 }
    }

    /// The single rule: a snapshot is the reference exactly when its file is
    /// byte-identical to `references/{model_slug}_{serial}.json`, with model
    /// and serial taken from the snapshot itself. A timestamp comparison
    /// mis-flags snapshots taken in the same second; a filename comparison
    /// never matches, because promoting copies the file under a canonical name.
    fn is_reference(&mut self, model: &str, serial: &str, snapshot_bytes: &[u8]) -> bool {
        let Ok(file_name) = checked_reference_filename(model, serial) else {
            return false;
        };
        let key = (model.to_string(), serial.to_string());
        if !self.cache.contains_key(&key) {
            let path = self.dir.join(file_name);
            let bytes = match fs::read(&path) {
                Ok(b) => Some(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => {
                    self.read_failures += 1;
                    None
                }
            };
            self.cache.insert(key.clone(), bytes);
        }
        self.cache[&key].as_deref() == Some(snapshot_bytes)
    }
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
/// (`"{model}_{serial}"`) when given. With `references` (a `references/`
/// directory) each readable entry also carries `is_reference`. A missing directory is an empty listing
/// with `dir_exists: false`; a directory that cannot be read is an error.
pub fn list_dir(
    dir: &Path,
    camera_filter: Option<&str>,
    references: Option<&Path>,
) -> Result<Listing, ListError> {
    let mut listing = Listing {
        dir: dir.display().to_string(),
        dir_exists: dir.exists(),
        entries: Vec::new(),
        skipped_dir_entries: 0,
        reference_read_failures: 0,
    };
    let mut refs = references.map(References::new);
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
        let state = match read_snapshot(&path) {
            Ok((bytes, s)) => EntryState::Ok {
                is_reference: refs
                    .as_mut()
                    .map(|r| r.is_reference(&s.camera.model, &s.camera.serial, &bytes)),
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
    listing.reference_read_failures = refs.map_or(0, |r| r.read_failures);
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
        let l = list_dir(&d.path().join("snapshots"), None, None).unwrap();
        assert!(!l.dir_exists && l.entries.is_empty() && l.skipped_dir_entries == 0);
    }

    #[test]
    fn existing_empty_directory_is_distinguishable_from_a_missing_one() {
        let d = TempDir::new().unwrap();
        let l = list_dir(d.path(), None, None).unwrap();
        assert!(l.dir_exists && l.entries.is_empty());
    }

    #[test]
    fn path_that_is_a_file_is_an_error_not_an_empty_listing() {
        let d = TempDir::new().unwrap();
        let f = d.path().join("not-a-dir");
        fs::write(&f, "x").unwrap();
        assert!(matches!(list_dir(&f, None, None), Err(ListError::Io { .. })));
    }

    #[test]
    fn entries_are_sorted_json_only_and_summarised() {
        let d = TempDir::new().unwrap();
        put(d.path(), "b.json", &snap("Z 9", "2", Some("two")));
        put(d.path(), "a.json", &snap("Z 9", "1", None));
        fs::write(d.path().join("notes.txt"), "ignore me").unwrap();
        fs::write(d.path().join("c.json.bak"), "ignore me").unwrap();
        let l = list_dir(d.path(), None, None).unwrap();
        let files: Vec<&str> = l.entries.iter().map(|e| e.file.as_str()).collect();
        assert_eq!(files, ["a.json", "b.json"]);
        assert_eq!(
            l.entries[1].state,
            EntryState::Ok {
                model: "Z 9".into(), serial: "2".into(), firmware: "5.31".into(),
                label: Some("two".into()), captured_at: "2026-10-27T12:00:00Z".into(), property_count: 2,
                is_reference: None,
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
        let l = list_dir(d.path(), None, None).unwrap();
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
            list_dir(d.path(), Some(f), None).unwrap().entries.into_iter().map(|e| e.file).collect()
        };
        assert_eq!(only("Z 9_id-1"), ["Z_9_id-1_x_1.json"]);
        assert_eq!(only("Z6_3_300"), ["Z6_3_300_x_1.json"]);
        assert!(only("Z 9_nobody").is_empty());
        // A filter applies to the filename, so an unreadable file is filtered the same way.
        fs::write(d.path().join("Z_9_nobody_x_1.json"), "{").unwrap();
        assert_eq!(only("Z 9_nobody"), ["Z_9_nobody_x_1.json"]);
    }

    // ── reference marking (byte identity) ────────────────────────────────

    fn fixture_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/reference_parity")
    }

    fn ok_fields(e: &ListEntry) -> (&str, &str, Option<bool>) {
        match &e.state {
            EntryState::Ok { serial, is_reference, .. } => (e.file.as_str(), serial.as_str(), *is_reference),
            other => panic!("{}: {other:?}", e.file),
        }
    }

    /// Parity: the Rust GUI (`gui/src/main.rs`) and the Python GUI
    /// (`gui/test_fleet_lib.py`) check the same expectations against the same
    /// directory. If this test and one of those disagree, the rule has drifted.
    #[test]
    fn reference_rule_matches_the_shared_parity_fixture() {
        let fx = fixture_dir();
        let expected: std::collections::BTreeMap<String, std::collections::BTreeMap<String, bool>> =
            serde_json::from_str(&fs::read_to_string(fx.join("expected.json")).unwrap()).unwrap();
        let l = list_dir(&fx.join("snapshots"), None, Some(&fx.join("references"))).unwrap();
        assert_eq!(l.reference_read_failures, 0);

        let mut seen = 0;
        for e in &l.entries {
            if e.file == "broken.json" {
                assert!(matches!(e.state, EntryState::Unreadable { .. }));
                continue;
            }
            let (file, serial, is_ref) = ok_fields(e);
            let want = expected
                .get(serial)
                .and_then(|m| m.get(file))
                .unwrap_or_else(|| panic!("{file} (serial {serial}) is not in expected.json"));
            assert_eq!(is_ref, Some(*want), "{file}");
            seen += 1;
        }
        let expected_total: usize = expected.values().map(|m| m.len()).sum();
        assert_eq!(seen, expected_total, "every expected file was listed");
    }

    /// The validity fixture (`tests/fixtures/snapshot_validity`): the same files, the same verdicts
    /// for every reader of snapshot files. `format_version`, `properties` and `transport` are all
    /// required, so an incomplete file is listed unreadable (`json`) and a future one
    /// `unsupported_version`, never as a normal entry.
    #[test]
    fn listing_agrees_with_the_shared_validity_fixture() {
        let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/snapshot_validity");
        let expected: std::collections::BTreeMap<String, String> =
            serde_json::from_str(&fs::read_to_string(fx.join("expected.json")).unwrap()).unwrap();
        let l = list_dir(&fx.join("snapshots"), None, None).unwrap();
        let got: std::collections::BTreeMap<String, String> = l
            .entries
            .iter()
            .map(|e| {
                let verdict = match &e.state {
                    EntryState::Ok { .. } => "ok".to_string(),
                    EntryState::Unreadable { error_kind, .. } => error_kind.to_string(),
                };
                (e.file.clone(), verdict)
            })
            .collect();
        assert_eq!(got, expected);
        // And the lenient reader the GUIs used to carry would have listed more of them.
        let lenient_ok = expected
            .keys()
            .filter(|f| {
                let text = fs::read_to_string(fx.join("snapshots").join(f)).unwrap();
                serde_json::from_str::<crate::snapshot::SnapshotSummary>(&text).is_ok()
            })
            .count();
        assert!(lenient_ok > expected.values().filter(|v| *v == "ok").count(), "the fixture must tell the two apart");
    }

    fn two_snapshots_one_reference() -> TempDir {
        let d = TempDir::new().unwrap();
        fs::create_dir_all(d.path().join("snapshots")).unwrap();
        fs::create_dir_all(d.path().join("references")).unwrap();
        put(&d.path().join("snapshots"), "a.json", &snap("Z 9", "1", Some("a")));
        put(&d.path().join("snapshots"), "b.json", &snap("Z 9", "1", Some("b")));
        fs::copy(d.path().join("snapshots/b.json"), d.path().join("references").join(reference_filename("Z 9", "1")))
            .unwrap();
        d
    }

    fn flags(l: &Listing) -> Vec<(&str, Option<bool>)> {
        l.entries.iter().map(|e| (e.file.as_str(), ok_fields(e).2)).collect()
    }

    #[test]
    fn promoted_snapshot_is_the_only_one_marked() {
        let d = two_snapshots_one_reference();
        let l = list_dir(&d.path().join("snapshots"), None, Some(&d.path().join("references"))).unwrap();
        assert_eq!(flags(&l), [("a.json", Some(false)), ("b.json", Some(true))]);
    }

    #[test]
    fn without_a_references_dir_the_field_is_absent_not_false() {
        let d = two_snapshots_one_reference();
        let l = list_dir(&d.path().join("snapshots"), None, None).unwrap();
        assert_eq!(flags(&l), [("a.json", None), ("b.json", None)]);
        let v = serde_json::to_value(&l).unwrap();
        assert!(v["entries"][0].get("is_reference").is_none());
    }

    #[test]
    fn a_missing_references_dir_marks_nothing_and_is_not_a_failure() {
        let d = two_snapshots_one_reference();
        fs::remove_dir_all(d.path().join("references")).unwrap();
        let l = list_dir(&d.path().join("snapshots"), None, Some(&d.path().join("references"))).unwrap();
        assert_eq!(flags(&l), [("a.json", Some(false)), ("b.json", Some(false))]);
        assert_eq!(l.reference_read_failures, 0);
    }

    #[test]
    fn an_appended_byte_on_the_reference_matches_nothing() {
        // The boundary of byte identity: one extra byte.
        let d = two_snapshots_one_reference();
        let r = d.path().join("references").join(reference_filename("Z 9", "1"));
        let mut bytes = fs::read(&r).unwrap();
        bytes.push(b'\n');
        fs::write(&r, bytes).unwrap();
        let l = list_dir(&d.path().join("snapshots"), None, Some(&d.path().join("references"))).unwrap();
        assert_eq!(flags(&l), [("a.json", Some(false)), ("b.json", Some(false))]);
    }

    #[test]
    fn an_unreadable_reference_is_counted_once_not_silently_treated_as_absent() {
        // A directory where the reference file should be: exists, cannot be read.
        let d = two_snapshots_one_reference();
        let r = d.path().join("references").join(reference_filename("Z 9", "1"));
        fs::remove_file(&r).unwrap();
        fs::create_dir(&r).unwrap();
        let l = list_dir(&d.path().join("snapshots"), None, Some(&d.path().join("references"))).unwrap();
        assert_eq!(flags(&l), [("a.json", Some(false)), ("b.json", Some(false))]);
        assert_eq!(l.reference_read_failures, 1, "once per reference file, not per snapshot");
    }

    #[test]
    fn reference_is_looked_up_by_the_snapshots_own_model_and_serial() {
        let d = TempDir::new().unwrap();
        let (sd, rd) = (d.path().join("snapshots"), d.path().join("references"));
        fs::create_dir_all(&sd).unwrap();
        fs::create_dir_all(&rd).unwrap();
        put(&sd, "z9.json", &snap("Z 9", "id-1", None));
        put(&sd, "z6.json", &snap("Z6_3", "id-1", None));
        fs::copy(sd.join("z6.json"), rd.join(reference_filename("Z6_3", "id-1"))).unwrap();
        let l = list_dir(&sd, None, Some(&rd)).unwrap();
        assert_eq!(flags(&l), [("z6.json", Some(true)), ("z9.json", Some(false))]);
    }

    #[test]
    fn a_hostile_serial_cannot_steer_the_lookup_into_a_subdirectory() {
        let d = TempDir::new().unwrap();
        let (sd, rd) = (d.path().join("snapshots"), d.path().join("references"));
        fs::create_dir_all(&sd).unwrap();
        fs::create_dir_all(rd.join("Z_9_x")).unwrap();
        put(&sd, "hostile.json", &snap("Z 9", "x/y", None));
        // The file the naive path "references/Z_9_x/y.json" would find, byte-identical.
        fs::copy(sd.join("hostile.json"), rd.join("Z_9_x").join("y.json")).unwrap();
        let l = list_dir(&sd, None, Some(&rd)).unwrap();
        assert_eq!(flags(&l), [("hostile.json", Some(false))]);
    }

    #[test]
    fn plain_name_component_boundaries() {
        // The shared table: this alias and the predicate it names are one rule.
        for (value, plain) in crate::ops::paths::PLAIN_COMPONENT_TABLE {
            assert_eq!(is_plain_name_component(value), *plain, "{value:?}");
        }
    }

    #[test]
    fn the_reference_mark_refuses_exactly_what_the_shared_table_refuses() {
        // A snapshot whose serial is `value` is marked only when `value` is plain
        // AND a byte-identical reference sits at the checked name.
        for (value, plain) in crate::ops::paths::PLAIN_COMPONENT_TABLE {
            let d = TempDir::new().unwrap();
            let (sd, rd) = (d.path().join("snapshots"), d.path().join("references"));
            fs::create_dir_all(&sd).unwrap();
            fs::create_dir_all(&rd).unwrap();
            let s = snap("Z 9", value, None);
            put(&sd, "s.json", &s);
            if *plain {
                fs::copy(sd.join("s.json"), rd.join(reference_filename("Z 9", value))).unwrap();
            }
            let l = list_dir(&sd, None, Some(&rd)).unwrap();
            assert_eq!(flags(&l), [("s.json", Some(*plain))], "serial {value:?}");
        }
    }

    #[test]
    fn reference_filename_is_slug_underscore_serial() {
        assert_eq!(reference_filename("Z 9", "30012345"), "Z_9_30012345.json");
        assert_eq!(reference_filename("Z6_3", "id-1"), "Z6_3_id-1.json");
    }

    #[test]
    fn a_snapshot_that_is_not_valid_utf8_is_unreadable_io_not_a_panic_or_a_match() {
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("bin.json"), b"\xff\xfe{").unwrap();
        let l = list_dir(d.path(), None, Some(&d.path().join("refs"))).unwrap();
        match &l.entries[0].state {
            EntryState::Unreadable { error_kind, .. } => assert_eq!(*error_kind, "io"),
            other => panic!("{other:?}"),
        }
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
        let v = serde_json::to_value(list_dir(d.path(), None, None).unwrap()).unwrap();
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
        let l = list_dir(d.path(), None, None).unwrap();
        assert_eq!(l.entries.len(), 1);
        assert!(l.entries[0].file.starts_with("bad") && l.entries[0].file.ends_with("name.json"));
    }
}

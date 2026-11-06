//! Edited copies of stored snapshots, as files: make one, change one in place.
//!
//! The rules live in `crate::snapshot_edit` and are applied to a value; this is the part that touches the disk:
//! a copy is a new file in `snapshots/` (never replacing one), and an in-place edit replaces a file whole or not at
//! all. A file is edited only if it is itself an edited copy, and only if it is directly inside the snapshots
//! directory: the references directory is outside it, so a reference cannot be reached through here.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::ops::paths::{PathError, resolve_existing};
use crate::ops::retention::kind_of_label;
use crate::snapshot::{Snapshot, SnapshotError};
use crate::snapshot_edit::{EditRefusal, copy_for_edit, ensure_editable};

#[derive(Debug, Error)]
pub enum EditFileError {
    #[error("{0}")]
    Path(#[from] PathError),
    #[error("{0}")]
    Snapshot(#[from] SnapshotError),
    #[error("{0}")]
    Refused(#[from] EditRefusal),
    /// Another writer replaced the file while this edit was being made, so the edit (made on the old content)
    /// was not written: writing it would have undone theirs.
    #[error("{file} changed on disk while it was being edited (another editor or process wrote it); nothing was written. Edit it again")]
    ChangedSinceRead { file: String },
    #[error("the label {label:?} starts a name the MCP server's captures and restore points use and are counted by; choose another")]
    ReservedLabel { label: String },
}

impl EditFileError {
    pub fn tag(&self) -> &'static str {
        match self {
            EditFileError::Path(e) => e.tag(),
            EditFileError::Snapshot(e) => e.tag(),
            EditFileError::Refused(e) => e.tag(),
            EditFileError::ChangedSinceRead { .. } => "changed_since_read",
            EditFileError::ReservedLabel { .. } => "reserved_label",
        }
    }
}

/// A copy that was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditCopy {
    pub file: String,
    pub path: PathBuf,
}

/// Copy the snapshot at `source` (any snapshot, a reference included) into `snapshots_dir` as an editable copy. The
/// original is only read. `now` is an RFC 3339 time.
pub fn create_edit_copy(snapshots_dir: &Path, source: &Path, label: Option<&str>, now: &str) -> Result<EditCopy, EditFileError> {
    if let Some(l) = label.map(str::trim) {
        if kind_of_label(l).is_some() {
            return Err(EditFileError::ReservedLabel { label: l.to_string() });
        }
    }
    let snap = Snapshot::load_from_file(source)?;
    let source_file = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let copy = copy_for_edit(&snap, &source_file, label, now);
    std::fs::create_dir_all(snapshots_dir).map_err(SnapshotError::Io)?;
    let file = copy.suggested_filename();
    let path = snapshots_dir.join(&file);
    copy.save_new(&path)?;
    Ok(EditCopy { file, path })
}

/// Apply `edit` to the edited copy `name` in `snapshots_dir` and keep the result, whole or not at all. Nothing is
/// written when the edit is refused or changes nothing.
///
/// Compare-before-replace, not a lock: the file's text is read once, and just before the replace it is read again
/// and must be identical, else [`EditFileError::ChangedSinceRead`] and nothing is written. That catches a second
/// editor (the CLI and the GUI can both be open on a copy) that finished writing between this edit's read and its
/// replace, which would otherwise be silently undone. It does not close the window between that second read and the
/// rename (microseconds; there is no OS lock here), so two writers racing inside it can still lose one edit.
pub fn edit_in_place<T>(
    snapshots_dir: &Path,
    name: &str,
    edit: impl FnOnce(&mut Snapshot) -> Result<T, EditRefusal>,
) -> Result<T, EditFileError> {
    let path = resolve_existing(snapshots_dir, name)?;
    let text = std::fs::read_to_string(&path).map_err(SnapshotError::Io)?;
    let mut snap = Snapshot::from_json(&text)?;
    ensure_editable(&snap)?;
    let before = snap.clone();
    let out = edit(&mut snap)?;
    if snap != before {
        if std::fs::read_to_string(&path).map_err(SnapshotError::Io)? != text {
            return Err(EditFileError::ChangedSinceRead { file: name.to_string() });
        }
        snap.save_to_file(&path)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Camera, Transport};
    use crate::snapshot_edit::set_enum_value;
    use serde_json::json;

    const NOW: &str = "2026-11-06T12:00:00Z";
    const VR: &str = "kNkMAIDCapability_VibrationReduction";

    fn captured_in(dir: &Path) -> PathBuf {
        let mut s = Snapshot::new(Camera { model: "Z 9".into(), serial: "S1".into(), firmware: "5.31".into() }, Transport::Usb, "2026-11-05T10:00:00Z".into());
        s.label = Some("base".into());
        s.insert(VR, 34053, json!({"default_index": 2, "elem_bytes": 4, "elem_count": 3, "elem_type": 2, "value_index": 2, "values": [0, 1, 2]}));
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("captured.json");
        s.save_to_file(&p).unwrap();
        p
    }

    #[test]
    fn a_copy_is_a_new_file_beside_the_others_and_the_original_is_untouched() {
        let d = tempfile::tempdir().unwrap();
        let snaps = d.path().join("snapshots");
        let src = captured_in(&snaps);
        let before = std::fs::read(&src).unwrap();
        let c = create_edit_copy(&snaps, &src, Some("no-vr"), NOW).unwrap();
        assert_ne!(c.path, src);
        assert_eq!(c.path.parent().unwrap(), snaps);
        assert_eq!(std::fs::read(&src).unwrap(), before, "the source is byte for byte what it was");
        let copy = Snapshot::load_from_file(&c.path).unwrap();
        assert!(copy.is_edited() && copy.label.as_deref() == Some("no-vr"));
        assert_eq!(copy.derived_from.unwrap().source_file, "captured.json");
    }

    #[test]
    fn a_copy_of_a_file_outside_the_snapshots_directory_such_as_a_reference_is_allowed_and_leaves_it_alone() {
        let d = tempfile::tempdir().unwrap();
        let refs = d.path().join("references");
        let reference = captured_in(&refs);
        let before = std::fs::read(&reference).unwrap();
        let c = create_edit_copy(&d.path().join("snapshots"), &reference, None, NOW).unwrap();
        assert!(c.path.starts_with(d.path().join("snapshots")));
        assert_eq!(std::fs::read(&reference).unwrap(), before);
    }

    #[test]
    fn two_copies_of_one_snapshot_get_two_files_and_the_same_second_with_the_same_label_does_not_overwrite() {
        let d = tempfile::tempdir().unwrap();
        let snaps = d.path().join("snapshots");
        let src = captured_in(&snaps);
        let a = create_edit_copy(&snaps, &src, Some("x"), "2026-11-06T12:00:00Z").unwrap();
        let b = create_edit_copy(&snaps, &src, Some("x"), "2026-11-06T12:00:01Z").unwrap();
        assert_ne!(a.file, b.file);
        let again = create_edit_copy(&snaps, &src, Some("x"), "2026-11-06T12:00:00Z").unwrap_err();
        assert_eq!(again.tag(), "io", "{again}");
        assert!(a.path.exists(), "the first copy is still there");
    }

    #[test]
    fn a_label_the_mcp_counts_its_files_by_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let snaps = d.path().join("snapshots");
        let src = captured_in(&snaps);
        for bad in ["mcp-x", "mcp-restore-x", " mcp-x"] {
            assert_eq!(create_edit_copy(&snaps, &src, Some(bad), NOW).unwrap_err().tag(), "reserved_label", "{bad:?}");
        }
        assert!(create_edit_copy(&snaps, &src, Some("mcpx"), NOW).is_ok(), "only the prefixes, not a name that merely begins with those letters");
        assert!(create_edit_copy(&snaps, &src, Some("my-mcp-note"), NOW).is_ok());
    }

    #[test]
    fn a_missing_or_unreadable_source_is_an_error_and_makes_no_file() {
        let d = tempfile::tempdir().unwrap();
        let snaps = d.path().join("snapshots");
        assert!(create_edit_copy(&snaps, &d.path().join("nope.json"), None, NOW).is_err());
        let junk = d.path().join("junk.json");
        std::fs::write(&junk, "{ not json").unwrap();
        assert_eq!(create_edit_copy(&snaps, &junk, None, NOW).unwrap_err().tag(), "json");
        assert!(!snaps.exists() || std::fs::read_dir(&snaps).unwrap().next().is_none());
    }

    #[test]
    fn an_edit_in_place_is_kept_and_a_refused_or_empty_one_writes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let snaps = d.path().join("snapshots");
        let src = captured_in(&snaps);
        let c = create_edit_copy(&snaps, &src, None, NOW).unwrap();
        edit_in_place(&snaps, &c.file, |s| set_enum_value(s, VR, 0, "2026-11-06T12:10:00Z")).unwrap();
        let after = Snapshot::load_from_file(&c.path).unwrap();
        assert_eq!(after.properties[VR].value["value_index"], 0);
        assert_eq!(after.derived_from.as_ref().unwrap().edits.len(), 1);
        let bytes = std::fs::read(&c.path).unwrap();
        let modified = std::fs::metadata(&c.path).unwrap().modified().unwrap();
        // a refused edit
        let e = edit_in_place(&snaps, &c.file, |s| set_enum_value(s, VR, 9, NOW)).unwrap_err();
        assert_eq!(e.tag(), "value_not_allowed");
        // an edit to what is already there
        edit_in_place(&snaps, &c.file, |s| set_enum_value(s, VR, 0, NOW)).unwrap();
        assert_eq!(std::fs::read(&c.path).unwrap(), bytes);
        assert_eq!(std::fs::metadata(&c.path).unwrap().modified().unwrap(), modified, "the file is not rewritten for either, not even with the same bytes");
    }

    #[test]
    fn a_snapshot_read_from_a_camera_is_not_edited_through_the_file_either() {
        let d = tempfile::tempdir().unwrap();
        let snaps = d.path().join("snapshots");
        let src = captured_in(&snaps);
        let before = std::fs::read(&src).unwrap();
        // even a closure that would do nothing is refused up front
        let e = edit_in_place(&snaps, "captured.json", |_| Ok(())).unwrap_err();
        assert_eq!(e.tag(), "not_a_copy");
        assert_eq!(std::fs::read(&src).unwrap(), before);
    }

    #[test]
    fn only_a_plain_name_inside_the_snapshots_directory_can_be_edited() {
        let d = tempfile::tempdir().unwrap();
        let snaps = d.path().join("snapshots");
        let src = captured_in(&snaps);
        let c = create_edit_copy(&snaps, &src, None, NOW).unwrap();
        // a copy that lives in the references directory is out of reach by name
        std::fs::create_dir_all(d.path().join("references")).unwrap();
        std::fs::copy(&c.path, d.path().join("references").join("ref.json")).unwrap();
        for bad in ["../references/ref.json", "/etc/passwd", "", "..", "a/b.json"] {
            assert_eq!(edit_in_place(&snaps, bad, |_| Ok(())).unwrap_err().tag(), "unsafe_name", "{bad:?}");
        }
        assert_eq!(edit_in_place(&snaps, "ref.json", |_| Ok(())).unwrap_err().tag(), "not_found");
    }

    #[test]
    fn an_edit_is_not_written_over_a_file_another_writer_replaced_while_it_was_being_made() {
        let d = tempfile::tempdir().unwrap();
        let snaps = d.path().join("snapshots");
        let src = captured_in(&snaps);
        let c = create_edit_copy(&snaps, &src, None, NOW).unwrap();
        // The other writer's edit lands between this edit's read and its replace (the closure runs in that window).
        let theirs = {
            let mut s = Snapshot::load_from_file(&c.path).unwrap();
            set_enum_value(&mut s, VR, 1, "2026-11-06T12:05:00Z").unwrap();
            s
        };
        let e = edit_in_place(&snaps, &c.file, |s| {
            let r = set_enum_value(s, VR, 0, "2026-11-06T12:10:00Z");
            theirs.save_to_file(&c.path).unwrap();
            r
        })
        .unwrap_err();
        assert_eq!(e.tag(), "changed_since_read", "{e}");
        assert!(e.to_string().contains(&c.file) && e.to_string().contains("nothing was written"), "{e}");
        let on_disk = Snapshot::load_from_file(&c.path).unwrap();
        assert_eq!(on_disk, theirs, "the other writer's edit is what is on disk, not undone");
        assert_eq!(on_disk.properties[VR].value["value_index"], 1);
        // Run again on the current content and it goes through.
        edit_in_place(&snaps, &c.file, |s| set_enum_value(s, VR, 0, "2026-11-06T12:10:00Z")).unwrap();
        assert_eq!(Snapshot::load_from_file(&c.path).unwrap().properties[VR].value["value_index"], 0);
    }

    #[test]
    fn a_concurrent_write_does_not_fail_an_edit_that_changes_nothing() {
        // Nothing is going to be written, so there is nothing to protect: the "no change" result stands.
        let d = tempfile::tempdir().unwrap();
        let snaps = d.path().join("snapshots");
        let src = captured_in(&snaps);
        let c = create_edit_copy(&snaps, &src, None, NOW).unwrap();
        let mut other = Snapshot::load_from_file(&c.path).unwrap();
        set_enum_value(&mut other, VR, 1, NOW).unwrap();
        edit_in_place(&snaps, &c.file, |s| {
            other.save_to_file(&c.path).unwrap();
            set_enum_value(s, VR, 2, NOW) // already 2: no change
        })
        .unwrap();
        assert_eq!(Snapshot::load_from_file(&c.path).unwrap(), other);
    }
}

//! Labels and per-camera caps for the files the MCP creates.
//!
//! There is no `rm` tool, so captures and restore points only accumulate
//! (docs/mcp-plan.md §3, "Retention"). Files the MCP creates are marked by a
//! label prefix added server-side, so an agent-supplied label cannot dodge it:
//!
//! - captures: `mcp-<label>`
//! - restore points: `mcp-restore-<op>-<timestamp>`
//!
//! Each kind has its own per-camera cap. Past the capture cap `capture_snapshot`
//! refuses; past the restore-point cap an apply refuses before any write.
//! Nothing here deletes anything.

use std::path::Path;

use serde::Serialize;

use crate::ops::list::{EntryState, ListError, list_dir};

/// Marks a capture made through the MCP.
pub const CAPTURE_PREFIX: &str = "mcp-";
/// Marks a restore point made through the MCP. Checked before [`CAPTURE_PREFIX`].
pub const RESTORE_PREFIX: &str = "mcp-restore-";

/// Placeholder cap on MCP captures per camera, chosen at build time
/// (docs/mcp-plan.md §9, still open).
pub const MAX_CAPTURES_PER_CAMERA: usize = 200;
/// Placeholder cap on MCP restore points per camera.
pub const MAX_RESTORE_POINTS_PER_CAMERA: usize = 50;

/// Longest label an agent may supply (before the prefix).
pub const MAX_LABEL_LEN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Capture,
    RestorePoint,
}

/// Which kind of MCP file a label marks, if any. `mcp-restore-` is tested
/// first, because every restore-point label also starts with `mcp-`.
pub fn kind_of_label(label: &str) -> Option<Kind> {
    if label.starts_with(RESTORE_PREFIX) {
        Some(Kind::RestorePoint)
    } else if label.starts_with(CAPTURE_PREFIX) {
        Some(Kind::Capture)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelError {
    Empty,
    TooLong { len: usize },
    /// Only ASCII letters, digits and `-`, `_`, `.` are accepted.
    BadCharacter { found: char },
    /// A capture label starting with `restore-` would be counted as a restore point.
    Reserved,
}

impl LabelError {
    pub fn tag(&self) -> &'static str {
        match self {
            LabelError::Empty => "label_empty",
            LabelError::TooLong { .. } => "label_too_long",
            LabelError::BadCharacter { .. } => "label_bad_character",
            LabelError::Reserved => "label_reserved",
        }
    }
}

impl std::fmt::Display for LabelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LabelError::Empty => f.write_str("label is empty"),
            LabelError::TooLong { len } => write!(f, "label is {len} characters; the limit is {MAX_LABEL_LEN}"),
            LabelError::BadCharacter { found } => {
                write!(f, "label contains {found:?}; use ASCII letters, digits, '-', '_' and '.'")
            }
            LabelError::Reserved => f.write_str("a capture label may not start with \"restore-\""),
        }
    }
}

impl std::error::Error for LabelError {}

/// The stored label for a capture: `mcp-` + the agent's label (`snap` if none).
pub fn capture_label(user: Option<&str>) -> Result<String, LabelError> {
    let user = match user {
        None => return Ok(format!("{CAPTURE_PREFIX}snap")),
        Some(u) => u,
    };
    if user.is_empty() {
        return Err(LabelError::Empty);
    }
    let len = user.chars().count();
    if len > MAX_LABEL_LEN {
        return Err(LabelError::TooLong { len });
    }
    if let Some(found) = user.chars().find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))) {
        return Err(LabelError::BadCharacter { found });
    }
    if user.starts_with("restore-") {
        return Err(LabelError::Reserved);
    }
    Ok(format!("{CAPTURE_PREFIX}{user}"))
}

/// What a restore point was taken before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreOp {
    PreTransplant,
    PreRestore,
}

impl RestoreOp {
    fn slug(&self) -> &'static str {
        match self {
            RestoreOp::PreTransplant => "pre-transplant",
            RestoreOp::PreRestore => "pre-restore",
        }
    }
}

/// `mcp-restore-<op>-<timestamp>`. The timestamp is the caller's compact
/// form (e.g. `20261027T172600Z`); it must be a plain label fragment.
pub fn restore_point_label(op: RestoreOp, timestamp: &str) -> Result<String, LabelError> {
    if timestamp.is_empty() {
        return Err(LabelError::Empty);
    }
    if let Some(found) = timestamp.chars().find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))) {
        return Err(LabelError::BadCharacter { found });
    }
    Ok(format!("{RESTORE_PREFIX}{}-{timestamp}", op.slug()))
}

/// How many MCP files exist for one camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Counts {
    pub captures: usize,
    pub restore_points: usize,
    /// `.json` files in the directory that could not be read, so could not be
    /// attributed to any camera. Reported, never silently ignored.
    pub unreadable: usize,
}

/// Count the MCP files for (`model`, `serial`) in `snapshots_dir`, by the
/// content of each file (model, serial, label), not by file name.
pub fn count_for_camera(snapshots_dir: &Path, model: &str, serial: &str) -> Result<Counts, ListError> {
    let listing = list_dir(snapshots_dir, None, None)?;
    let mut c = Counts::default();
    for e in &listing.entries {
        match &e.state {
            EntryState::Unreadable { .. } => c.unreadable += 1,
            EntryState::Ok { model: m, serial: s, label, .. } if m == model && s == serial => {
                match label.as_deref().and_then(kind_of_label) {
                    Some(Kind::Capture) => c.captures += 1,
                    Some(Kind::RestorePoint) => c.restore_points += 1,
                    None => {}
                }
            }
            EntryState::Ok { .. } => {}
        }
    }
    Ok(c)
}

/// The cap for `kind` is reached: one more file would exceed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DataDirFull {
    pub kind: Kind,
    pub count: usize,
    pub cap: usize,
}

impl DataDirFull {
    pub fn tag(&self) -> &'static str {
        "data_dir_full"
    }
}

impl std::fmt::Display for DataDirFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = match self.kind {
            Kind::Capture => "captures",
            Kind::RestorePoint => "restore points",
        };
        write!(
            f,
            "{} MCP {what} are stored for this camera (cap {}); nothing is deleted automatically, \
             remove some by hand",
            self.count, self.cap
        )
    }
}

impl std::error::Error for DataDirFull {}

fn check(kind: Kind, count: usize, cap: usize) -> Result<(), DataDirFull> {
    if count >= cap {
        Err(DataDirFull { kind, count, cap })
    } else {
        Ok(())
    }
}

/// May one more capture be written?
pub fn check_capture_cap(counts: &Counts) -> Result<(), DataDirFull> {
    check(Kind::Capture, counts.captures, MAX_CAPTURES_PER_CAMERA)
}

/// May one more restore point be written?
pub fn check_restore_point_cap(counts: &Counts) -> Result<(), DataDirFull> {
    check(Kind::RestorePoint, counts.restore_points, MAX_RESTORE_POINTS_PER_CAMERA)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Camera, Snapshot, Transport};
    use serde_json::json;
    use tempfile::TempDir;

    // ── labels ───────────────────────────────────────────────────────────

    #[test]
    fn capture_label_adds_the_prefix_and_defaults_to_snap() {
        assert_eq!(capture_label(Some("wedding-1")).unwrap(), "mcp-wedding-1");
        assert_eq!(capture_label(None).unwrap(), "mcp-snap");
        assert_eq!(capture_label(Some("a.b_c-D9")).unwrap(), "mcp-a.b_c-D9");
    }

    #[test]
    fn capture_label_length_boundary() {
        let at = "x".repeat(MAX_LABEL_LEN);
        assert_eq!(capture_label(Some(&at)).unwrap().len(), MAX_LABEL_LEN + CAPTURE_PREFIX.len());
        let over = "x".repeat(MAX_LABEL_LEN + 1);
        assert_eq!(capture_label(Some(&over)), Err(LabelError::TooLong { len: MAX_LABEL_LEN + 1 }));
        // Counted in characters, not bytes: 64 multi-byte characters are still refused for their bytes' sake.
        assert!(matches!(capture_label(Some(&"é".repeat(MAX_LABEL_LEN))), Err(LabelError::BadCharacter { .. })));
    }

    #[test]
    fn capture_label_refuses_empty_and_awkward_characters() {
        assert_eq!(capture_label(Some("")), Err(LabelError::Empty));
        for (bad, ch) in [("a/b", '/'), ("a\\b", '\\'), ("a b", ' '), ("a\nb", '\n'), ("a\0b", '\0'), ("é", 'é'), ("a:b", ':')] {
            assert_eq!(capture_label(Some(bad)), Err(LabelError::BadCharacter { found: ch }), "{bad:?}");
        }
        // Dots alone are legal: the label is only ever one file-name fragment after "mcp-",
        // never a path component of its own. A separator is what would matter, and is refused.
        assert_eq!(capture_label(Some("..")).unwrap(), "mcp-..");
        assert_eq!(capture_label(Some("../x")), Err(LabelError::BadCharacter { found: '/' }));
    }

    #[test]
    fn a_capture_label_cannot_pose_as_a_restore_point() {
        assert_eq!(capture_label(Some("restore-x")), Err(LabelError::Reserved));
        assert_eq!(capture_label(Some("restore-")), Err(LabelError::Reserved));
        // "restore" without the dash, and "my-restore-x", are ordinary.
        assert_eq!(kind_of_label(&capture_label(Some("restore")).unwrap()), Some(Kind::Capture));
        assert_eq!(kind_of_label(&capture_label(Some("my-restore-x")).unwrap()), Some(Kind::Capture));
    }

    #[test]
    fn restore_point_labels() {
        assert_eq!(
            restore_point_label(RestoreOp::PreTransplant, "20261027T172600Z").unwrap(),
            "mcp-restore-pre-transplant-20261027T172600Z"
        );
        assert_eq!(restore_point_label(RestoreOp::PreRestore, "t").unwrap(), "mcp-restore-pre-restore-t");
        assert_eq!(restore_point_label(RestoreOp::PreRestore, ""), Err(LabelError::Empty));
        for (bad, ch) in [("a/b", '/'), ("a b", ' '), ("a:b", ':'), ("a\\b", '\\')] {
            assert_eq!(restore_point_label(RestoreOp::PreRestore, bad), Err(LabelError::BadCharacter { found: ch }), "{bad:?}");
        }
    }

    #[test]
    fn classification_checks_restore_before_capture() {
        assert_eq!(kind_of_label("mcp-restore-pre-transplant-x"), Some(Kind::RestorePoint));
        assert_eq!(kind_of_label("mcp-wedding"), Some(Kind::Capture));
        assert_eq!(kind_of_label("mcp-"), Some(Kind::Capture), "boundary: bare prefix");
        assert_eq!(kind_of_label("mcp-restore-"), Some(Kind::RestorePoint), "boundary: bare restore prefix");
        for plain in ["", "mcp", "mcpx", "wedding", "pre-transplant-x", "MCP-x", " mcp-x"] {
            assert_eq!(kind_of_label(plain), None, "{plain:?}");
        }
    }

    // ── counting ─────────────────────────────────────────────────────────

    fn put(dir: &Path, file: &str, model: &str, serial: &str, label: Option<&str>) {
        let mut s = Snapshot::new(
            Camera { model: model.into(), serial: serial.into(), firmware: "5.31".into() },
            Transport::Usb,
            "2026-10-27T12:00:00Z".into(),
        );
        s.label = label.map(String::from);
        s.insert("kNkMAIDCapability_A", 1, json!(1));
        s.save_to_file(dir.join(file)).unwrap();
    }

    #[test]
    fn counts_are_per_camera_and_per_kind_and_ignore_other_labels() {
        let d = TempDir::new().unwrap();
        put(d.path(), "1.json", "Z 9", "A", Some("mcp-one"));
        put(d.path(), "2.json", "Z 9", "A", Some("mcp-two"));
        put(d.path(), "3.json", "Z 9", "A", Some("mcp-restore-pre-transplant-t"));
        put(d.path(), "4.json", "Z 9", "A", Some("hand-made"));
        put(d.path(), "5.json", "Z 9", "A", None);
        put(d.path(), "6.json", "Z 9", "B", Some("mcp-other-serial"));
        put(d.path(), "7.json", "Z6_3", "A", Some("mcp-other-model"));
        let c = count_for_camera(d.path(), "Z 9", "A").unwrap();
        assert_eq!(c, Counts { captures: 2, restore_points: 1, unreadable: 0 });
    }

    #[test]
    fn counting_goes_by_content_not_file_name() {
        let d = TempDir::new().unwrap();
        // A renamed file, and a file named like an MCP capture of another camera.
        put(d.path(), "totally-unrelated-name.json", "Z 9", "A", Some("mcp-x"));
        put(d.path(), "Z_9_A_mcp-looks-like-mine_1.json", "Z 9", "B", Some("mcp-y"));
        assert_eq!(count_for_camera(d.path(), "Z 9", "A").unwrap().captures, 1);
    }

    #[test]
    fn unreadable_files_are_counted_not_ignored() {
        let d = TempDir::new().unwrap();
        std::fs::write(d.path().join("broken.json"), "{ nope").unwrap();
        put(d.path(), "ok.json", "Z 9", "A", Some("mcp-x"));
        let c = count_for_camera(d.path(), "Z 9", "A").unwrap();
        assert_eq!((c.captures, c.unreadable), (1, 1));
    }

    #[test]
    fn a_missing_directory_counts_zero() {
        let d = TempDir::new().unwrap();
        assert_eq!(count_for_camera(&d.path().join("nope"), "Z 9", "A").unwrap(), Counts::default());
    }

    // ── caps ─────────────────────────────────────────────────────────────

    #[test]
    fn capture_cap_below_at_above() {
        let n = |captures| Counts { captures, ..Counts::default() };
        assert!(check_capture_cap(&n(MAX_CAPTURES_PER_CAMERA - 1)).is_ok(), "room for exactly one more");
        let at = check_capture_cap(&n(MAX_CAPTURES_PER_CAMERA)).unwrap_err();
        assert_eq!((at.kind, at.count, at.cap), (Kind::Capture, MAX_CAPTURES_PER_CAMERA, MAX_CAPTURES_PER_CAMERA));
        assert!(check_capture_cap(&n(MAX_CAPTURES_PER_CAMERA + 1)).is_err());
        assert!(check_capture_cap(&n(0)).is_ok());
    }

    #[test]
    fn restore_point_cap_below_at_above() {
        let n = |restore_points| Counts { restore_points, ..Counts::default() };
        assert!(check_restore_point_cap(&n(MAX_RESTORE_POINTS_PER_CAMERA - 1)).is_ok());
        assert_eq!(check_restore_point_cap(&n(MAX_RESTORE_POINTS_PER_CAMERA)).unwrap_err().kind, Kind::RestorePoint);
        assert!(check_restore_point_cap(&n(MAX_RESTORE_POINTS_PER_CAMERA + 1)).is_err());
    }

    #[test]
    fn the_two_caps_are_independent() {
        // Full of captures does not block a restore point, and vice versa: an apply must stay possible.
        let full_captures = Counts { captures: MAX_CAPTURES_PER_CAMERA, ..Counts::default() };
        assert!(check_restore_point_cap(&full_captures).is_ok());
        let full_points = Counts { restore_points: MAX_RESTORE_POINTS_PER_CAMERA, ..Counts::default() };
        assert!(check_capture_cap(&full_points).is_ok());
    }

    #[test]
    fn the_error_is_typed_and_says_nothing_is_deleted() {
        let e = DataDirFull { kind: Kind::Capture, count: 200, cap: 200 };
        assert_eq!(e.tag(), "data_dir_full");
        let s = e.to_string();
        assert!(s.contains("200") && s.contains("captures") && s.contains("nothing is deleted"), "{s}");
        assert!(DataDirFull { kind: Kind::RestorePoint, count: 1, cap: 1 }.to_string().contains("restore points"));
    }

    #[test]
    fn checking_and_counting_never_delete_anything() {
        let d = TempDir::new().unwrap();
        for i in 0..5 {
            put(d.path(), &format!("{i}.json"), "Z 9", "A", Some("mcp-x"));
        }
        let before: Vec<_> = std::fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        let c = count_for_camera(d.path(), "Z 9", "A").unwrap();
        let _ = check_capture_cap(&c);
        let after: Vec<_> = std::fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(before.len(), after.len());
    }
}

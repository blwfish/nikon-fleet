//! The capture workflow, from "I have a body" to "a snapshot file exists".
//!
//! Everything the MCP's `capture_snapshot` does once a session is open and the
//! target body has been chosen, on a [`CameraPort`] so it runs without
//! hardware (docs/mcp-plan.md §4, Phase 2):
//!
//! 1. the label is validated and prefixed (`ops::retention`) and the
//!    per-camera capture cap is checked **before the camera is read**;
//! 2. the body is captured (`ops::capture`);
//! 3. an empty capture, and a capture that fails the baseline gate
//!    (`ops::baseline`), are refused **in memory**, so a failed capture leaves no
//!    file;
//! 4. the snapshot is written create-new, never over an existing file.
//!
//! Session handling (lock, SDK, choosing the body by USB serial, the
//! same-model refusal) stays with the caller.

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;

use crate::ops::baseline::{GateOutcome, MissingCapability, gate};
use crate::ops::capture::{CaptureStats, capture};
use crate::ops::list::ListError;
use crate::ops::port::CameraPort;
use crate::ops::retention::{self, DataDirFull, LabelError};
use crate::snapshot::{Camera, Snapshot, SnapshotError, Transport};

pub struct CaptureRequest<'a> {
    pub snapshots_dir: &'a Path,
    /// Identity of the body being captured, from its USB descriptors.
    pub camera: Camera,
    /// The agent's label, before the `mcp-` prefix. `None` means the default.
    pub label: Option<&'a str>,
    pub name_lookup: &'a HashMap<u32, String>,
    /// RFC 3339 capture time, supplied so the file name is deterministic in tests.
    pub captured_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaptureOutcome {
    /// File name inside the snapshots directory.
    pub file: String,
    /// The label stored in the file. The MCP always has one (`mcp-...`); the CLI's is optional.
    pub label: Option<String>,
    pub camera: Camera,
    pub stats: CaptureStats,
    /// `Passed`, or `NoBaseline` (the capture was written but not judged).
    pub gate: GateOutcome,
    pub property_count: usize,
}

impl CaptureOutcome {
    /// The one place this value is built, so the CLI's `snapshot --json` and the worker's
    /// `capture` result cannot drift apart.
    pub fn new(
        file: String,
        label: Option<String>,
        camera: Camera,
        property_count: usize,
        stats: CaptureStats,
        gate: GateOutcome,
    ) -> Self {
        CaptureOutcome { file, label, camera, stats, gate, property_count }
    }
}

#[derive(Debug)]
pub enum CaptureError {
    Label(LabelError),
    List(ListError),
    DataDirFull(DataDirFull),
    /// Nothing at all could be read, so there is nothing worth keeping.
    NothingReadable { read_err: usize },
    /// The baseline gate failed: an incomplete capture. Not written.
    Incomplete { missing: Vec<MissingCapability>, stats: CaptureStats },
    /// Creating the file failed, including "already exists".
    Write(SnapshotError),
}

impl CaptureError {
    /// Closed vocabulary for structured output.
    pub fn tag(&self) -> &'static str {
        match self {
            CaptureError::Label(e) => e.tag(),
            CaptureError::List(_) => "list_failed",
            CaptureError::DataDirFull(e) => e.tag(),
            CaptureError::NothingReadable { .. } => "nothing_readable",
            CaptureError::Incomplete { .. } => "capture_incomplete",
            CaptureError::Write(_) => "write_failed",
        }
    }
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::Label(e) => write!(f, "{e}"),
            CaptureError::List(e) => write!(f, "{e}"),
            CaptureError::DataDirFull(e) => write!(f, "{e}"),
            CaptureError::NothingReadable { read_err } => {
                write!(f, "no capability could be read ({read_err} read error(s)); nothing was written")
            }
            CaptureError::Incomplete { missing, .. } => {
                write!(f, "capture is incomplete: {} baseline capabilit{} could not be read; nothing was written",
                    missing.len(), if missing.len() == 1 { "y" } else { "ies" })
            }
            CaptureError::Write(e) => write!(f, "could not create the snapshot file: {e}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Run the capture workflow. `progress` is told `"captured"` after the read
/// and `"written"` after the file exists.
pub fn capture_snapshot<P: CameraPort + ?Sized>(
    port: &P,
    req: &CaptureRequest<'_>,
    progress: &mut dyn FnMut(&str),
) -> Result<CaptureOutcome, CaptureError> {
    let label = retention::capture_label(req.label).map_err(CaptureError::Label)?;
    let counts = retention::count_for_camera(req.snapshots_dir, &req.camera.model, &req.camera.serial)
        .map_err(CaptureError::List)?;
    retention::check_capture_cap(&counts).map_err(CaptureError::DataDirFull)?;

    let mut snap = Snapshot::new(req.camera.clone(), Transport::Usb, req.captured_at.clone());
    snap.label = Some(label.clone());
    let stats = capture(port, req.name_lookup, &mut snap);
    progress("captured");

    if stats.read_ok == 0 {
        return Err(CaptureError::NothingReadable { read_err: stats.read_err });
    }
    let verdict = gate(&snap);
    if let GateOutcome::Failed { missing } = verdict {
        return Err(CaptureError::Incomplete { missing, stats });
    }

    std::fs::create_dir_all(req.snapshots_dir)
        .map_err(|e| CaptureError::Write(SnapshotError::Io(e)))?;
    let file = snap.suggested_filename();
    snap.save_new(req.snapshots_dir.join(&file)).map_err(CaptureError::Write)?;
    progress("written");

    Ok(CaptureOutcome::new(file, Some(label), req.camera.clone(), snap.properties.len(), stats, verdict))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::baseline::{NoBaselineReason, baseline_for};
    use crate::ops::port::FakeCamera;
    use crate::sdk::OP_GET;
    use serde_json::json;
    use tempfile::TempDir;

    const AT: &str = "2026-10-27T12:00:00Z";

    /// A fake Z 9 that can read exactly the baseline's capabilities, minus the first `drop`.
    fn z9(drop: usize) -> (FakeCamera, HashMap<u32, String>) {
        let b = baseline_for("Z 9").unwrap();
        let mut cam = FakeCamera::new();
        let mut names = HashMap::new();
        for c in b.readable.iter().skip(drop) {
            cam = cam.with(c.code, OP_GET, json!(1));
            names.insert(c.code, c.name.clone());
        }
        (cam, names)
    }

    fn camera(model: &str) -> Camera {
        Camera { model: model.into(), serial: "0000003023668".into(), firmware: "5.31".into() }
    }

    fn req<'a>(dir: &'a Path, model: &str, label: Option<&'a str>, names: &'a HashMap<u32, String>) -> CaptureRequest<'a> {
        CaptureRequest { snapshots_dir: dir, camera: camera(model), label, name_lookup: names, captured_at: AT.into() }
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .map(|r| r.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn a_complete_capture_is_written_with_the_mcp_prefix_and_passes_the_gate() {
        let d = TempDir::new().unwrap();
        let (cam, names) = z9(0);
        let mut phases = Vec::new();
        let out = capture_snapshot(&cam, &req(d.path(), "Z 9", Some("wedding"), &names), &mut |p| phases.push(p.to_string())).unwrap();
        assert_eq!(out.label.as_deref(), Some("mcp-wedding"));
        assert_eq!(out.gate, GateOutcome::Passed);
        assert_eq!(out.property_count, 196);
        assert_eq!(phases, ["captured", "written"]);
        assert_eq!(files(d.path()), [out.file.clone()]);
        let back = Snapshot::load_from_file(d.path().join(&out.file)).unwrap();
        assert_eq!(back.label.as_deref(), Some("mcp-wedding"));
        assert_eq!(back.properties.len(), 196);
        assert!(out.file.contains("_mcp-wedding_"), "{}", out.file);
    }

    #[test]
    fn no_label_means_mcp_snap() {
        let d = TempDir::new().unwrap();
        let (cam, names) = z9(0);
        let out = capture_snapshot(&cam, &req(d.path(), "Z 9", None, &names), &mut |_| {}).unwrap();
        assert_eq!(out.label.as_deref(), Some("mcp-snap"));
    }

    #[test]
    fn an_incomplete_capture_is_refused_in_memory_and_leaves_no_file() {
        let d = TempDir::new().unwrap();
        for dropped in [1usize, 5, 178, 195] {
            let (cam, names) = z9(dropped);
            let err = capture_snapshot(&cam, &req(d.path(), "Z 9", None, &names), &mut |_| {}).unwrap_err();
            match &err {
                CaptureError::Incomplete { missing, stats } => {
                    assert_eq!(missing.len(), dropped);
                    assert_eq!(stats.read_ok, 196 - dropped);
                }
                other => panic!("{dropped}: {other:?}"),
            }
            assert_eq!(err.tag(), "capture_incomplete");
            assert!(files(d.path()).is_empty(), "{dropped}: a failed gate must leave no file");
        }
    }

    #[test]
    fn a_body_that_reads_nothing_is_refused_even_without_a_baseline() {
        let d = TempDir::new().unwrap();
        let cam = FakeCamera::new().with_unreadable(1, OP_GET).with_unreadable(2, OP_GET);
        let err = capture_snapshot(&cam, &req(d.path(), "Z 5", None, &HashMap::new()), &mut |_| {}).unwrap_err();
        assert!(matches!(err, CaptureError::NothingReadable { read_err: 2 }), "{err:?}");
        assert!(files(d.path()).is_empty());
        let err = capture_snapshot(&FakeCamera::new(), &req(d.path(), "Z 5", None, &HashMap::new()), &mut |_| {}).unwrap_err();
        assert!(matches!(err, CaptureError::NothingReadable { read_err: 0 }));
    }

    #[test]
    fn a_model_without_a_baseline_is_written_and_marked_unjudged() {
        let d = TempDir::new().unwrap();
        let cam = FakeCamera::new().with(1, OP_GET, json!(1));
        let out = capture_snapshot(&cam, &req(d.path(), "Z 5", None, &HashMap::new()), &mut |_| {}).unwrap();
        assert_eq!(out.gate, GateOutcome::NoBaseline(NoBaselineReason::UnknownModel));
        assert_eq!(files(d.path()).len(), 1);
    }

    #[test]
    fn a_bad_label_is_refused_before_the_camera_is_read() {
        let d = TempDir::new().unwrap();
        let (cam, names) = z9(0);
        for bad in ["", "a/b", "restore-x", &"x".repeat(65)] {
            let err = capture_snapshot(&cam, &req(d.path(), "Z 9", Some(bad), &names), &mut |_| {}).unwrap_err();
            assert!(matches!(err, CaptureError::Label(_)), "{bad:?}: {err:?}");
        }
        assert!(cam.reads().is_empty(), "no read happened for a refused label");
        assert!(files(d.path()).is_empty());
    }

    #[test]
    fn the_cap_is_checked_before_the_camera_is_read_and_nothing_is_deleted() {
        let d = TempDir::new().unwrap();
        // Fill the cap with this camera's MCP captures.
        for i in 0..retention::MAX_CAPTURES_PER_CAMERA {
            let mut s = Snapshot::new(camera("Z 9"), Transport::Usb, format!("2026-01-01T00:00:{:02}.{:03}Z", i % 60, i));
            s.label = Some("mcp-old".into());
            s.insert("kNkMAIDCapability_A", 1, json!(1));
            s.save_to_file(d.path().join(format!("old-{i}.json"))).unwrap();
        }
        let before = files(d.path());
        let (cam, names) = z9(0);
        let err = capture_snapshot(&cam, &req(d.path(), "Z 9", None, &names), &mut |_| {}).unwrap_err();
        assert!(matches!(err, CaptureError::DataDirFull(_)), "{err:?}");
        assert_eq!(err.tag(), "data_dir_full");
        assert!(cam.reads().is_empty(), "refused before touching the camera");
        assert_eq!(files(d.path()), before, "nothing deleted, nothing added");
        // A different camera's captures do not count against this one.
        let mut other = req(d.path(), "Z 9", None, &names);
        other.camera.serial = "OTHER".into();
        assert!(capture_snapshot(&cam, &other, &mut |_| {}).is_ok());
    }

    #[test]
    fn the_cap_has_room_for_exactly_one_more_just_below_it() {
        let d = TempDir::new().unwrap();
        for i in 0..retention::MAX_CAPTURES_PER_CAMERA - 1 {
            let mut s = Snapshot::new(camera("Z 9"), Transport::Usb, format!("2026-01-01T00:00:00.{i:03}Z"));
            s.label = Some("mcp-old".into());
            s.insert("kNkMAIDCapability_A", 1, json!(1));
            s.save_to_file(d.path().join(format!("old-{i}.json"))).unwrap();
        }
        let (cam, names) = z9(0);
        assert!(capture_snapshot(&cam, &req(d.path(), "Z 9", None, &names), &mut |_| {}).is_ok());
        assert!(matches!(
            capture_snapshot(&cam, &req(d.path(), "Z 9", Some("again"), &names), &mut |_| {}),
            Err(CaptureError::DataDirFull(_))
        ));
    }

    #[test]
    fn an_existing_file_of_the_same_name_is_never_overwritten() {
        let d = TempDir::new().unwrap();
        let (cam, names) = z9(0);
        let first = capture_snapshot(&cam, &req(d.path(), "Z 9", None, &names), &mut |_| {}).unwrap();
        let path = d.path().join(&first.file);
        std::fs::write(&path, "precious").unwrap();
        // Same camera, label and captured_at: the same file name.
        let err = capture_snapshot(&cam, &req(d.path(), "Z 9", None, &names), &mut |_| {}).unwrap_err();
        assert!(matches!(err, CaptureError::Write(_)), "{err:?}");
        assert_eq!(err.tag(), "write_failed");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "precious");
    }

    #[test]
    fn the_snapshots_directory_is_created_if_missing() {
        let d = TempDir::new().unwrap();
        let sub = d.path().join("snapshots");
        let (cam, names) = z9(0);
        capture_snapshot(&cam, &req(&sub, "Z 9", None, &names), &mut |_| {}).unwrap();
        assert_eq!(files(&sub).len(), 1);
    }

    #[test]
    fn the_outcome_serialises_with_the_gate_and_stats() {
        let d = TempDir::new().unwrap();
        let (cam, names) = z9(0);
        let out = capture_snapshot(&cam, &req(d.path(), "Z 9", None, &names), &mut |_| {}).unwrap();
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["gate"], json!({"outcome": "passed"}));
        assert_eq!(v["stats"]["read_ok"], json!(196));
        assert_eq!(v["camera"]["serial"], json!("0000003023668"));
        assert_eq!(v["property_count"], json!(196));
    }

    #[test]
    fn the_cli_and_the_worker_build_the_same_value_and_an_absent_label_is_null() {
        let cam = Camera { model: "Z 9".into(), serial: "S".into(), firmware: "5.31".into() };
        let with = CaptureOutcome::new("f.json".into(), Some("mcp-x".into()), cam.clone(), 3, CaptureStats::default(), GateOutcome::Passed);
        let without = CaptureOutcome::new("f.json".into(), None, cam, 3, CaptureStats::default(), GateOutcome::Passed);
        let (a, b) = (serde_json::to_value(&with).unwrap(), serde_json::to_value(&without).unwrap());
        assert_eq!(a["label"], serde_json::json!("mcp-x"));
        assert_eq!(b["label"], serde_json::Value::Null);
        let keys = |v: &serde_json::Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys(&a), keys(&b));
    }
}

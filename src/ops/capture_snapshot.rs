//! The capture workflow, from "I have a body" to "a snapshot file exists".
//!
//! Everything the MCP's `capture_snapshot` does once a session is open and the
//! target body has been chosen, on a [`CameraPort`] so it runs without
//! hardware (docs/mcp-plan.md §4, Phase 2):
//!
//! 1. the label is validated and prefixed (`ops::retention`) and the
//!    per-camera capture cap is checked **before the camera is read**;
//! 2. the body is captured (`ops::capture`);
//! 3. an empty capture, a capture that fails the baseline gate
//!    (`ops::baseline`) and a capture of a different firmware that lacks
//!    capabilities the baseline reads ([`firmware_shortfall`]) are refused **in
//!    memory**, so a failed capture leaves no file;
//! 4. the snapshot is written create-new, never over an existing file.
//!
//! Session handling (lock, SDK, choosing the body by USB serial, the
//! same-model refusal) stays with the caller.

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;

use crate::ops::baseline::{
    GateOutcome, MissingCapability, NoBaselineReason, baseline_for, capabilities_phrase, describe_missing_list, gate,
    missing_capabilities,
};
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
    /// Of `stats.failed`, how many the model's baseline also does not read, so a
    /// healthy body of this model fails them too (200 read / 12 failed is normal
    /// on the Z6III). `None` when the capture was not judged (`gate` is
    /// `no_baseline`), because then nothing says which failures are normal.
    pub expected_unreadable: Option<usize>,
    /// `stats.failed` minus `expected_unreadable`: reads that failed although the
    /// baseline reads them. Always `Some(0)` after a passed gate (a baseline
    /// capability that failed would have failed the gate); kept so a non-zero
    /// value is visible rather than assumed away. `None` when not judged.
    pub unexpected_unreadable: Option<usize>,
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
        let (expected_unreadable, unexpected_unreadable) = match (&gate, crate::ops::baseline::baseline_for(&camera.model)) {
            (GateOutcome::Passed, Some(b)) => {
                let unexpected =
                    stats.failed.iter().filter(|f| b.readable.iter().any(|c| c.code == f.code)).count();
                (Some(stats.failed.len() - unexpected), Some(unexpected))
            }
            _ => (None, None),
        };
        CaptureOutcome { file, label, camera, stats, gate, property_count, expected_unreadable, unexpected_unreadable }
    }
}

/// A capture the gate could not judge because its firmware differs from the one the baseline
/// was seeded from, that also lacks capabilities the baseline reads. A firmware update can drop
/// capabilities, but an interrupted capture or a USB reset looks exactly the same, so the
/// front ends refuse it unless told the shortfall is expected (the CLI's `--allow-partial`; the
/// MCP has no such opt-in).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareShortfall {
    pub model: String,
    /// The firmware the baseline was seeded from.
    pub baseline_firmware: String,
    /// The firmware the capture records (never empty: an unknown firmware is not evidence of a
    /// change, and the gate then says `Failed`, not `FirmwareChanged`).
    pub capture_firmware: String,
    pub missing: Vec<MissingCapability>,
}

/// The shortfall behind a `NoBaseline(FirmwareChanged)` verdict, or `None` for any other verdict
/// and for a firmware change whose readable set differs only by *added* capabilities.
pub fn firmware_shortfall(snap: &Snapshot, verdict: &GateOutcome) -> Option<FirmwareShortfall> {
    let GateOutcome::NoBaseline(NoBaselineReason::FirmwareChanged { baseline, capture }) = verdict else {
        return None;
    };
    let missing = baseline_for(&snap.camera.model).map(|b| missing_capabilities(b, snap)).unwrap_or_default();
    (!missing.is_empty()).then(|| FirmwareShortfall {
        model: snap.camera.model.clone(),
        baseline_firmware: baseline.clone(),
        capture_firmware: capture.clone(),
        missing,
    })
}

/// The `detail` of a `capture_incomplete` error result: what could not be read.
pub fn incomplete_detail(missing: &[MissingCapability], stats: &CaptureStats) -> serde_json::Value {
    serde_json::json!({"missing": missing, "read_ok": stats.read_ok, "read_err": stats.read_err})
}

/// The `detail` of a `firmware_shortfall` error result: the same shape plus the two firmwares.
pub fn shortfall_detail(shortfall: &FirmwareShortfall, stats: &CaptureStats) -> serde_json::Value {
    let mut v = incomplete_detail(&shortfall.missing, stats);
    v["baseline_firmware"] = shortfall.baseline_firmware.clone().into();
    v["capture_firmware"] = shortfall.capture_firmware.clone().into();
    v
}

impl std::fmt::Display for FirmwareShortfall {
    /// The CLI's and the MCP's one wording for this refusal (without the "what to do next" tail,
    /// which differs by front end).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this capture is of firmware {}, but the {:?} baseline was seeded from firmware {}, \
             and {} that baseline reads could not be read: {}. A firmware update can remove capabilities, but \
             an interrupted capture or a USB reset looks exactly the same.",
            self.capture_firmware,
            self.model,
            self.baseline_firmware,
            capabilities_phrase(self.missing.len()),
            describe_missing_list(&self.missing),
        )
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
    /// A different firmware than the baseline's, lacking capabilities the baseline reads. Not written.
    FirmwareShortfall { shortfall: FirmwareShortfall, stats: CaptureStats },
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
            CaptureError::FirmwareShortfall { .. } => "firmware_shortfall",
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
                write!(f, "capture is incomplete: {} in the baseline could not be read; nothing was written",
                    capabilities_phrase(missing.len()))
            }
            CaptureError::FirmwareShortfall { shortfall, .. } => write!(
                f,
                "{shortfall}\nNothing was written. Re-capture; if the same capabilities are missing again, tell the user: \
                 the baseline needs re-seeding for the new firmware (`nikon-fleet baseline seed`)."
            ),
            CaptureError::Write(e) => write!(f, "could not create the snapshot file: {e}"),
        }
    }
}

impl std::error::Error for CaptureError {}

impl CaptureError {
    /// Structured detail for the error result, for the errors that have any.
    pub fn detail(&self) -> Option<serde_json::Value> {
        match self {
            CaptureError::Incomplete { missing, stats } => Some(incomplete_detail(missing, stats)),
            CaptureError::FirmwareShortfall { shortfall, stats } => Some(shortfall_detail(shortfall, stats)),
            _ => None,
        }
    }
}

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
    if let Some(shortfall) = firmware_shortfall(&snap, &verdict) {
        return Err(CaptureError::FirmwareShortfall { shortfall, stats });
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

    /// `req` for a Z 9 running `firmware` (the baseline was seeded from 5.31).
    fn req_fw<'a>(dir: &'a Path, firmware: &str, names: &'a HashMap<u32, String>) -> CaptureRequest<'a> {
        let mut r = req(dir, "Z 9", None, names);
        r.camera.firmware = firmware.into();
        r
    }

    #[test]
    fn a_capture_of_a_different_firmware_that_lacks_baseline_capabilities_is_refused_and_leaves_no_file() {
        // G19 / review #33: the baseline gate calls this `NoBaseline(FirmwareChanged)` ("cannot judge"),
        // which used to be written. An interrupted capture looks exactly like it, so it is refused, as
        // the CLI refuses it unless `--allow-partial`.
        let d = TempDir::new().unwrap();
        for dropped in [1usize, 2, 5, 178, 195] {
            let (cam, names) = z9(dropped);
            let err = capture_snapshot(&cam, &req_fw(d.path(), "5.40", &names), &mut |_| {}).unwrap_err();
            let CaptureError::FirmwareShortfall { shortfall, stats } = &err else { panic!("{dropped}: {err:?}") };
            assert_eq!(shortfall.missing.len(), dropped);
            assert_eq!((shortfall.baseline_firmware.as_str(), shortfall.capture_firmware.as_str()), ("5.31", "5.40"));
            assert_eq!(stats.read_ok, 196 - dropped);
            assert_eq!(err.tag(), "firmware_shortfall");
            assert!(files(d.path()).is_empty(), "{dropped}: a refused capture must leave no file");
        }
    }

    #[test]
    fn the_refusal_uses_the_clis_wording_and_counts_in_the_clis_plural() {
        let d = TempDir::new().unwrap();
        let (cam, names) = z9(1);
        let one = capture_snapshot(&cam, &req_fw(d.path(), "5.40", &names), &mut |_| {}).unwrap_err().to_string();
        let first = baseline_for("Z 9").unwrap().readable[0].clone();
        assert!(
            one.starts_with(&format!(
                "this capture is of firmware 5.40, but the \"Z 9\" baseline was seeded from firmware 5.31, and 1 capability that baseline \
                 reads could not be read: {} ({:#x}). A firmware update can remove capabilities, but an interrupted capture or a USB reset \
                 looks exactly the same.",
                first.name, first.code
            )),
            "{one}"
        );
        assert!(one.contains("Nothing was written"), "{one}");
        let (cam, names) = z9(16);
        let many = capture_snapshot(&cam, &req_fw(d.path(), "5.40", &names), &mut |_| {}).unwrap_err().to_string();
        assert!(many.contains("and 16 capabilities that baseline reads"), "{many}");
        assert!(many.contains(", and 1 more"), "the list shows 15 names, then how many more: {many}");
    }

    #[test]
    fn a_different_firmware_that_only_adds_capabilities_is_still_written_unjudged() {
        let d = TempDir::new().unwrap();
        let (cam, names) = z9(0);
        let extra = (0u32..).find(|c| !baseline_for("Z 9").unwrap().readable.iter().any(|r| r.code == *c)).unwrap();
        let cam = cam.with(extra, OP_GET, json!(1));
        let out = capture_snapshot(&cam, &req_fw(d.path(), "5.40", &names), &mut |_| {}).unwrap();
        assert_eq!(
            out.gate,
            GateOutcome::NoBaseline(NoBaselineReason::FirmwareChanged { baseline: "5.31".into(), capture: "5.40".into() })
        );
        assert_eq!((out.expected_unreadable, out.unexpected_unreadable), (None, None), "not judged");
        assert_eq!(files(d.path()), [out.file]);
    }

    #[test]
    fn a_complete_capture_of_a_different_firmware_passes() {
        let d = TempDir::new().unwrap();
        let (cam, names) = z9(0);
        let out = capture_snapshot(&cam, &req_fw(d.path(), "5.40", &names), &mut |_| {}).unwrap();
        assert_eq!(out.gate, GateOutcome::Passed, "exactly the baseline's set on another firmware is a pass");
    }

    #[test]
    fn an_unreadable_firmware_is_not_evidence_of_a_change() {
        // `""` is what the body reports when its USB record was not read; the gate must then say
        // "incomplete" (Failed), not "firmware changed" (which is only a shortfall for a *known* firmware).
        let d = TempDir::new().unwrap();
        let (cam, names) = z9(3);
        let err = capture_snapshot(&cam, &req_fw(d.path(), "", &names), &mut |_| {}).unwrap_err();
        assert!(matches!(&err, CaptureError::Incomplete { missing, .. } if missing.len() == 3), "{err:?}");
        assert_eq!(err.tag(), "capture_incomplete");
        assert!(files(d.path()).is_empty());
        // ... and a complete capture with no firmware at all is fine.
        let (cam, names) = z9(0);
        assert_eq!(capture_snapshot(&cam, &req_fw(d.path(), "", &names), &mut |_| {}).unwrap().gate, GateOutcome::Passed);
    }

    fn snap_of(model: &str, firmware: &str, drop: usize) -> Snapshot {
        let b = baseline_for("Z 9").unwrap();
        let mut s = Snapshot::new(Camera { model: model.into(), serial: "S".into(), firmware: firmware.into() }, Transport::Usb, AT.into());
        for c in b.readable.iter().skip(drop) {
            s.insert(c.name.clone(), c.code, json!(1));
        }
        s
    }

    #[test]
    fn firmware_shortfall_is_only_for_a_changed_firmware_verdict_with_something_missing() {
        // The one case: a known other firmware and capabilities missing (below, at and above one).
        let nothing_missing = snap_of("Z 9", "5.40", 0);
        assert_eq!(firmware_shortfall(&nothing_missing, &gate(&nothing_missing)), None, "nothing missing");
        for dropped in [1usize, 2] {
            let s = snap_of("Z 9", "5.40", dropped);
            let f = firmware_shortfall(&s, &gate(&s)).expect("a shortfall");
            assert_eq!((f.missing.len(), f.model.as_str()), (dropped, "Z 9"));
        }
        // Every other verdict is not a shortfall, whatever is missing.
        let same_fw = snap_of("Z 9", "5.31", 4);
        assert!(matches!(gate(&same_fw), GateOutcome::Failed { .. }));
        assert_eq!(firmware_shortfall(&same_fw, &gate(&same_fw)), None, "same firmware is `Failed`, the other refusal");
        let unknown_fw = snap_of("Z 9", "", 4);
        assert_eq!(firmware_shortfall(&unknown_fw, &gate(&unknown_fw)), None, "unknown firmware is `Failed` too");
        let z5 = snap_of("Z 5", "1.43", 4);
        assert_eq!(gate(&z5), GateOutcome::NoBaseline(NoBaselineReason::UnknownModel));
        assert_eq!(firmware_shortfall(&z5, &gate(&z5)), None, "no baseline: nothing to fall short of");
        let passed = snap_of("Z 9", "5.31", 0);
        assert_eq!(firmware_shortfall(&passed, &GateOutcome::Passed), None);
    }

    #[test]
    fn the_error_results_carry_what_was_missing_and_both_firmwares() {
        let d = TempDir::new().unwrap();
        let (cam, names) = z9(3);
        let inc = capture_snapshot(&cam, &req_fw(d.path(), "5.31", &names), &mut |_| {}).unwrap_err();
        let v = inc.detail().expect("an incomplete capture has detail");
        assert_eq!((v["read_ok"].clone(), v["read_err"].clone(), v["missing"].as_array().map(Vec::len)), (json!(193), json!(0), Some(3)));
        assert!(v.get("baseline_firmware").is_none(), "only the shortfall names two firmwares");
        let short = capture_snapshot(&cam, &req_fw(d.path(), "5.40", &names), &mut |_| {}).unwrap_err();
        let v = short.detail().expect("a shortfall has detail");
        assert_eq!((v["baseline_firmware"].clone(), v["capture_firmware"].clone()), (json!("5.31"), json!("5.40")));
        assert_eq!((v["read_ok"].clone(), v["missing"].as_array().map(Vec::len)), (json!(193), Some(3)));
        assert_eq!(v["missing"][0]["name"], json!(baseline_for("Z 9").unwrap().readable[0].name));
        // The errors that have nothing structured to say say nothing.
        assert!(CaptureError::NothingReadable { read_err: 1 }.detail().is_none());
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
    fn failed(code: u32) -> crate::ops::capture::FailedRead {
        crate::ops::capture::FailedRead { code, name: format!("P{code}"), error: "e".into() }
    }

    #[test]
    fn failures_outside_the_baseline_are_expected_and_inside_it_are_not() {
        let cam = Camera { model: "Z 9".into(), serial: "S".into(), firmware: "5.31".into() };
        let inside = crate::ops::baseline::baseline_for("Z 9").unwrap().readable[0].code;
        let outside = (0u32..).find(|c| !crate::ops::baseline::baseline_for("Z 9").unwrap().readable.iter().any(|r| r.code == *c)).unwrap();
        let mk = |failed_codes: &[u32], gate| {
            let stats = CaptureStats { failed: failed_codes.iter().map(|c| failed(*c)).collect(), ..Default::default() };
            let o = CaptureOutcome::new("f".into(), None, cam.clone(), 1, stats, gate);
            (o.expected_unreadable, o.unexpected_unreadable)
        };
        assert_eq!(mk(&[], GateOutcome::Passed), (Some(0), Some(0)), "no failures is a judged zero, not unknown");
        assert_eq!(mk(&[outside], GateOutcome::Passed), (Some(1), Some(0)));
        assert_eq!(mk(&[outside, inside], GateOutcome::Passed), (Some(1), Some(1)), "a baseline code that failed is surfaced");
        let unjudged = GateOutcome::NoBaseline(crate::ops::baseline::NoBaselineReason::UnknownModel);
        assert_eq!(mk(&[outside], unjudged), (None, None));
    }

    #[test]
    fn a_model_with_no_baseline_is_not_judged_even_when_the_gate_is_passed_by_hand() {
        let cam = Camera { model: "Nonexistent".into(), serial: "S".into(), firmware: "1".into() };
        let o = CaptureOutcome::new("f".into(), None, cam, 1, CaptureStats::default(), GateOutcome::Passed);
        assert_eq!((o.expected_unreadable, o.unexpected_unreadable), (None, None));
    }
}

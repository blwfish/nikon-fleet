//! `check`: capture a body live and diff it against its reference.
//!
//! Writes no file and no camera setting. The live capture is judged by the
//! baseline gate first: an incomplete capture would diff as "removed
//! properties", which is a lie about the camera, so it is an error instead
//! (docs/mcp-plan.md §4). Same shape as the capture workflow, minus the file.

use std::collections::HashMap;

use serde::Serialize;

use crate::diff::{Diff, DiffError, DiffOptions, diff_with_schema};
use crate::maid_layer::MaidLayerConfig;
use crate::ops::baseline::{GateOutcome, MissingCapability, gate};
use crate::ops::capture::{CaptureStats, capture};
use crate::ops::port::CameraPort;
use crate::snapshot::{Camera, Snapshot, Transport};

pub struct CheckRequest<'a> {
    pub live_camera: Camera,
    pub reference: &'a Snapshot,
    pub name_lookup: &'a HashMap<u32, String>,
    pub schema: &'a MaidLayerConfig,
    pub captured_at: String,
}

#[derive(Debug, Serialize)]
pub struct CheckReport {
    pub camera: Camera,
    pub reference_label: Option<String>,
    pub reference_captured_at: String,
    /// `Passed`, or `NoBaseline` (compared, but the capture was not judged for completeness).
    pub gate: GateOutcome,
    pub stats: CaptureStats,
    pub diff: Diff,
    pub differences: usize,
}

#[derive(Debug)]
pub enum CheckError {
    NothingReadable { read_err: usize },
    Incomplete { missing: Vec<MissingCapability>, stats: CaptureStats },
    Diff(DiffError),
}

impl CheckError {
    pub fn tag(&self) -> &'static str {
        match self {
            CheckError::NothingReadable { .. } => "nothing_readable",
            CheckError::Incomplete { .. } => "capture_incomplete",
            CheckError::Diff(DiffError::ModelMismatch { .. }) => "model_mismatch",
        }
    }
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckError::NothingReadable { read_err } => {
                write!(f, "no capability could be read ({read_err} read error(s)); nothing to compare")
            }
            CheckError::Incomplete { missing, .. } => write!(
                f,
                "the live capture is incomplete ({} baseline capabilit{} could not be read), so a comparison would show them as removed",
                missing.len(),
                if missing.len() == 1 { "y" } else { "ies" }
            ),
            CheckError::Diff(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CheckError {}

pub fn check<P: CameraPort + ?Sized>(port: &P, req: &CheckRequest<'_>) -> Result<CheckReport, CheckError> {
    let mut live = Snapshot::new(req.live_camera.clone(), Transport::Usb, req.captured_at.clone());
    live.label = Some("live".into());
    let stats = capture(port, req.name_lookup, &mut live);
    if stats.read_ok == 0 {
        return Err(CheckError::NothingReadable { read_err: stats.read_err });
    }
    let verdict = gate(&live);
    if let GateOutcome::Failed { missing } = verdict {
        return Err(CheckError::Incomplete { missing, stats });
    }
    let d = diff_with_schema(req.reference, &live, &DiffOptions::default(), req.schema).map_err(CheckError::Diff)?;
    let differences = d.changed.len() + d.only_in_a.len() + d.only_in_b.len();
    Ok(CheckReport {
        camera: req.live_camera.clone(),
        reference_label: req.reference.label.clone(),
        reference_captured_at: req.reference.captured_at.clone(),
        gate: verdict,
        stats,
        diff: d,
        differences,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::baseline::baseline_for;
    use crate::ops::port::FakeCamera;
    use crate::sdk::OP_GET;
    use crate::test_support::parse_config;
    use serde_json::json;

    fn z9_camera() -> Camera {
        Camera { model: "Z 9".into(), serial: "S".into(), firmware: "5.31".into() }
    }

    /// A fake Z 9 with the baseline's capabilities, value 1 each, and the same names.
    fn z9() -> (FakeCamera, HashMap<u32, String>, Snapshot) {
        let b = baseline_for("Z 9").unwrap();
        let mut cam = FakeCamera::new();
        let mut names = HashMap::new();
        let mut reference = Snapshot::new(z9_camera(), Transport::Usb, "2026-01-01T00:00:00Z".into());
        reference.label = Some("ref".into());
        for c in &b.readable {
            cam = cam.with(c.code, OP_GET, json!(1));
            names.insert(c.code, c.name.clone());
            reference.insert(c.name.clone(), c.code, json!(1));
        }
        (cam, names, reference)
    }

    fn req<'a>(reference: &'a Snapshot, names: &'a HashMap<u32, String>, schema: &'a MaidLayerConfig) -> CheckRequest<'a> {
        CheckRequest {
            live_camera: z9_camera(),
            reference,
            name_lookup: names,
            schema,
            captured_at: "2026-10-27T12:00:00Z".into(),
        }
    }

    fn schema() -> MaidLayerConfig {
        parse_config(&[("Z 9", &[("X", 1)])])
    }

    #[test]
    fn an_unchanged_camera_has_no_differences() {
        let (cam, names, reference) = z9();
        let sc = schema();
        let r = check(&cam, &req(&reference, &names, &sc)).unwrap();
        assert_eq!(r.differences, 0);
        assert!(r.diff.is_empty());
        assert_eq!(r.gate, GateOutcome::Passed);
        assert_eq!(r.stats.read_ok, 196);
        assert_eq!(r.reference_label.as_deref(), Some("ref"));
    }

    #[test]
    fn a_changed_property_is_reported_with_both_values() {
        let (_, names, reference) = z9();
        let b = baseline_for("Z 9").unwrap();
        let changed_code = b.readable[10].code;
        let mut cam = FakeCamera::new();
        for c in &b.readable {
            cam = cam.with(c.code, OP_GET, json!(if c.code == changed_code { 2 } else { 1 }));
        }
        let sc = schema();
        let r = check(&cam, &req(&reference, &names, &sc)).unwrap();
        assert_eq!(r.differences, 1);
        assert_eq!(r.diff.changed[0].code, changed_code);
        assert_eq!((r.diff.changed[0].value_a.clone(), r.diff.changed[0].value_b.clone()), (json!(1), json!(2)));
    }

    #[test]
    fn an_incomplete_live_capture_is_an_error_not_a_pile_of_removals() {
        let (_, names, reference) = z9();
        let b = baseline_for("Z 9").unwrap();
        let mut cam = FakeCamera::new();
        for c in b.readable.iter().skip(5) {
            cam = cam.with(c.code, OP_GET, json!(1));
        }
        let sc = schema();
        let err = check(&cam, &req(&reference, &names, &sc)).unwrap_err();
        match &err {
            CheckError::Incomplete { missing, stats } => assert_eq!((missing.len(), stats.read_ok), (5, 191)),
            other => panic!("{other:?}"),
        }
        assert_eq!(err.tag(), "capture_incomplete");
    }

    #[test]
    fn a_body_that_reads_nothing_is_an_error() {
        let (_, names, reference) = z9();
        let sc = schema();
        let err = check(&FakeCamera::new(), &req(&reference, &names, &sc)).unwrap_err();
        assert!(matches!(err, CheckError::NothingReadable { read_err: 0 }), "{err:?}");
        assert_eq!(err.tag(), "nothing_readable");
    }

    #[test]
    fn a_reference_of_another_model_is_refused_not_compared() {
        let (cam, names, mut reference) = z9();
        reference.camera.model = "Z6_3".into();
        let sc = schema();
        let err = check(&cam, &req(&reference, &names, &sc)).unwrap_err();
        assert_eq!(err.tag(), "model_mismatch");
    }

    #[test]
    fn a_model_without_a_baseline_is_compared_and_marked_unjudged() {
        let mut cam = FakeCamera::new();
        let mut reference = Snapshot::new(
            Camera { model: "Z 5".into(), serial: "S".into(), firmware: "1.43".into() },
            Transport::Usb,
            "2026-01-01T00:00:00Z".into(),
        );
        let mut names = HashMap::new();
        for code in 1..=3u32 {
            cam = cam.with(code, OP_GET, json!(code));
            names.insert(code, format!("kNkMAIDCapability_P{code}"));
            reference.insert(format!("kNkMAIDCapability_P{code}"), code, json!(code));
        }
        let sc = parse_config(&[("Z 5", &[("P1", 1)])]);
        let r = check(
            &cam,
            &CheckRequest {
                live_camera: reference.camera.clone(),
                reference: &reference,
                name_lookup: &names,
                schema: &sc,
                captured_at: "2026-10-27T12:00:00Z".into(),
            },
        )
        .unwrap();
        assert!(matches!(r.gate, GateOutcome::NoBaseline(_)));
        assert_eq!(r.differences, 0);
    }

    #[test]
    fn check_writes_nothing_to_the_camera() {
        let (cam, names, reference) = z9();
        let sc = schema();
        let _ = check(&cam, &req(&reference, &names, &sc)).unwrap();
        assert!(cam.writes().is_empty());
    }

    #[test]
    fn the_report_serialises_with_gate_stats_and_diff() {
        let (cam, names, reference) = z9();
        let sc = schema();
        let v = serde_json::to_value(check(&cam, &req(&reference, &names, &sc)).unwrap()).unwrap();
        assert_eq!(v["gate"], json!({"outcome": "passed"}));
        assert_eq!(v["differences"], json!(0));
        assert_eq!(v["diff"]["changed"], json!([]));
        assert_eq!(v["camera"]["serial"], json!("S"));
    }
    #[test]
    fn properties_present_on_only_one_side_count_as_differences() {
        let (cam, names, mut reference) = z9();
        // Reference has a property the live body does not; live has one the reference lacks.
        reference.insert("kNkMAIDCapability_OnlyInReference", 0xFFFF_0001, json!(1));
        let some = reference.properties.keys().find(|k| k.contains("Aperture") || !k.starts_with("cap_")).cloned().unwrap();
        reference.properties.remove(&some);
        let sc = schema();
        let r = check(&cam, &req(&reference, &names, &sc)).unwrap();
        assert_eq!((r.diff.only_in_a.len(), r.diff.only_in_b.len(), r.diff.changed.len()), (1, 1, 0));
        assert_eq!(r.differences, 2);
    }

    #[test]
    fn a_volatile_property_changing_is_not_a_difference() {
        let (_, names, reference) = z9();
        let b = baseline_for("Z 9").unwrap();
        let level = b.readable.iter().find(|c| c.name.contains("AngleLevel")).expect("AngleLevel is readable on a Z 9");
        let mut cam = FakeCamera::new();
        for c in &b.readable {
            cam = cam.with(c.code, OP_GET, json!(if c.code == level.code { 99 } else { 1 }));
        }
        let sc = schema();
        assert_eq!(check(&cam, &req(&reference, &names, &sc)).unwrap().differences, 0);
    }
}

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
use crate::ops::baseline::{GateOutcome, MissingCapability, capabilities_phrase, gate};
use crate::ops::capture::{CaptureStats, capture};
use crate::ops::capture_snapshot::{FirmwareShortfall, firmware_shortfall, incomplete_detail, shortfall_detail};
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
    /// The body runs a different firmware than the baseline's and lacks capabilities the baseline
    /// reads: indistinguishable from an interrupted capture, so not compared.
    FirmwareShortfall { shortfall: FirmwareShortfall, stats: CaptureStats },
    Diff(DiffError),
    /// The reference is an edited copy, not a reading from a camera (see [`require_captured_reference`]).
    EditedReference,
}

impl CheckError {
    pub fn tag(&self) -> &'static str {
        match self {
            CheckError::NothingReadable { .. } => "nothing_readable",
            CheckError::Incomplete { .. } => "capture_incomplete",
            CheckError::FirmwareShortfall { .. } => "firmware_shortfall",
            CheckError::Diff(DiffError::ModelMismatch { .. }) => "model_mismatch",
            CheckError::EditedReference => "edited_reference",
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
                "the live capture is incomplete ({} in the baseline could not be read), so a comparison would show them as removed",
                capabilities_phrase(missing.len())
            ),
            CheckError::FirmwareShortfall { shortfall, .. } => write!(
                f,
                "{shortfall}\nNothing was compared, because the missing capabilities would show as removed. Re-run the check; \
                 if the same capabilities are missing again, tell the user: the baseline needs re-seeding for the new firmware."
            ),
            CheckError::Diff(e) => write!(f, "{e}"),
            CheckError::EditedReference => write!(f, "the reference {}", crate::gui_argv::EditedCopyRefused),
        }
    }
}

impl std::error::Error for CheckError {}

impl CheckError {
    /// Structured detail for the error result: the same shapes as the capture's.
    pub fn detail(&self) -> Option<serde_json::Value> {
        match self {
            CheckError::Incomplete { missing, stats } => Some(incomplete_detail(missing, stats)),
            CheckError::FirmwareShortfall { shortfall, stats } => Some(shortfall_detail(shortfall, stats)),
            CheckError::NothingReadable { .. } | CheckError::Diff(_) | CheckError::EditedReference => None,
        }
    }
}

/// A reference must be a reading from a camera: an edited copy is a hand-made file, and a body "matching" it
/// (or not) says nothing about the camera. `ref set` refuses to promote one; this is the same rule
/// (`gui_argv::refuse_edited_reference`) applied where a reference is USED, since a file can be placed in
/// `references/` by hand. The CLI's `check` and the worker's both call it.
pub fn require_captured_reference(reference: &Snapshot) -> Result<(), CheckError> {
    crate::gui_argv::refuse_edited_reference(reference).map_err(|_| CheckError::EditedReference)
}

/// Diff a reference against a live MAID capture. The live capture cannot carry an extended section, so the
/// reference's own section is left out of the comparison (otherwise every reference captured with one would
/// show it as an `only_in_a` difference and the gate would fail permanently). The CLI's `check` and the
/// worker's both compare through this one function, so the decision cannot be made twice.
pub fn compare_to_live(reference: &Snapshot, live: &Snapshot, schema: &MaidLayerConfig) -> Result<Diff, DiffError> {
    diff_with_schema(reference, live, &DiffOptions::for_live_maid_capture(), schema)
}

pub fn check<P: CameraPort + ?Sized>(port: &P, req: &CheckRequest<'_>) -> Result<CheckReport, CheckError> {
    crate::diag::scoped(crate::diag::Subsystem::Snapshot, "check", crate::diag::Class::Top, &[], |_span| {
        require_captured_reference(req.reference)?;
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
        if let Some(shortfall) = firmware_shortfall(&live, &verdict) {
            return Err(CheckError::FirmwareShortfall { shortfall, stats });
        }
        let d = compare_to_live(req.reference, &live, req.schema).map_err(CheckError::Diff)?;
        Ok(CheckReport::new(req.live_camera.clone(), req.reference, verdict, stats, d))
    })
}

impl CheckReport {
    /// The one place this value is built, so the CLI's `check --json` and the worker's `check`
    /// result cannot drift apart.
    pub fn new(camera: Camera, reference: &Snapshot, gate: GateOutcome, stats: CaptureStats, diff: Diff) -> Self {
        let differences = diff.difference_count();
        CheckReport {
            camera,
            reference_label: reference.label.clone(),
            reference_captured_at: reference.captured_at.clone(),
            gate,
            stats,
            diff,
            differences,
        }
    }
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
    fn a_live_capture_of_a_different_firmware_that_lacks_baseline_capabilities_is_not_compared() {
        // Same hole as the capture's (G19): the gate cannot judge another firmware, so the shortfall
        // used to be diffed as a pile of removals. The CLI's `check` refuses it; so does this.
        let (_, names, reference) = z9();
        let b = baseline_for("Z 9").unwrap();
        let sc = schema();
        for dropped in [1usize, 5] {
            let mut cam = FakeCamera::new();
            for c in b.readable.iter().skip(dropped) {
                cam = cam.with(c.code, OP_GET, json!(1));
            }
            let mut r = req(&reference, &names, &sc);
            r.live_camera.firmware = "5.40".into();
            let err = check(&cam, &r).unwrap_err();
            let CheckError::FirmwareShortfall { shortfall, stats } = &err else { panic!("{dropped}: {err:?}") };
            assert_eq!((shortfall.missing.len(), stats.read_ok), (dropped, 196 - dropped));
            assert_eq!(err.tag(), "firmware_shortfall");
            assert!(err.to_string().contains("firmware 5.40"), "{err}");
        }
    }

    #[test]
    fn a_different_firmware_with_nothing_missing_is_compared() {
        let (cam, names, reference) = z9();
        let sc = schema();
        let mut r = req(&reference, &names, &sc);
        r.live_camera.firmware = "5.40".into();
        let report = check(&cam, &r).unwrap();
        assert_eq!(report.gate, GateOutcome::Passed);
        assert_eq!(report.differences, 0);
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

    fn with_extended(mut reference: Snapshot) -> Snapshot {
        use crate::extended::{Extended, FtpState, ReadFrom};
        reference.extended = Some(Extended {
            read_from: ReadFrom { model: "Z 9".into(), usb_serial: "1".into() },
            props: Default::default(),
            iptc: Default::default(),
            ftp: FtpState::NotOnModel,
        });
        reference
    }

    #[test]
    fn a_reference_with_an_extended_section_is_not_different_from_a_maid_only_live_capture() {
        // Every new capture carries an extended section; the live `check` capture never does. If the
        // section were compared, it would show as an `only_in_a` difference and the gate would fail forever.
        let (cam, names, reference) = z9();
        let reference = with_extended(reference);
        let sc = schema();
        let r = check(&cam, &req(&reference, &names, &sc)).unwrap();
        assert_eq!(r.differences, 0, "{:?}", r.diff.extended);
        assert!(r.diff.extended.is_empty());
        assert!(r.diff.is_empty());
    }

    #[test]
    fn compare_to_live_leaves_the_reference_extended_section_out_and_the_default_options_do_not() {
        // The CLI's `check` calls compare_to_live (same function as the worker's); pin that it skips the
        // section, and that the unrestricted diff of the same pair does report it (so the pin is not vacuous).
        let (_, _, reference) = z9();
        let reference = with_extended(reference);
        let mut live = Snapshot::new(z9_camera(), Transport::Usb, "2026-10-27T12:00:00Z".into());
        for (k, p) in &reference.properties {
            live.insert(k.clone(), p.code, p.value.clone());
        }
        let sc = schema();
        assert!(compare_to_live(&reference, &live, &sc).unwrap().is_empty());
        let full = crate::diff::diff(&reference, &live, &DiffOptions::default()).unwrap();
        assert_eq!(full.extended.len(), 1);
        assert_eq!(full.difference_count(), 1);
    }

    #[test]
    fn an_edited_copy_is_refused_as_a_reference_before_the_camera_is_read() {
        let (_, names, reference) = z9();
        let copy = crate::snapshot_edit::copy_for_edit(&reference, "ref.json", Some("copy"), "2026-02-02T00:00:00Z");
        assert!(copy.is_edited());
        let sc = schema();
        // A camera that has nothing: if the capture ran first this would be `NothingReadable`.
        let err = check(&FakeCamera::new(), &req(&copy, &names, &sc)).unwrap_err();
        assert!(matches!(err, CheckError::EditedReference), "{err:?}");
        assert_eq!(err.tag(), "edited_reference");
        assert!(err.to_string().contains(&crate::gui_argv::EditedCopyRefused.to_string()), "{err}");
        assert!(err.detail().is_none());
        // The captured reference it was copied from is accepted by the same call.
        assert!(require_captured_reference(&reference).is_ok());
        let (cam, ..) = z9();
        assert!(check(&cam, &req(&reference, &names, &sc)).is_ok());
    }
}

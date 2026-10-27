//! `transplant_apply`: write a plan to the body, with a restore point first and
//! a read-back after (docs/mcp-plan.md §5.4-5.5).
//!
//! The sequence, on a [`CameraPort`]:
//!
//! 1. the caller must echo the plan's **digest and summary**; either missing is a
//!    refusal. The plan is recomputed on the live body and both are compared; a
//!    mismatch is `stale_plan`. Nothing has been written yet.
//! 2. a plan that copies nothing is a no-op and says so.
//! 3. a **restore point** (`mcp-restore-pre-transplant-<ts>`) is captured and
//!    written create-new. The apply is refused, before any camera write, if the
//!    restore-point cap is reached, the model has no baseline, the capture
//!    reads nothing or fails the baseline gate (the `ok=18 err=194` of F2), or
//!    the file cannot be created (including: it already exists).
//! 4. the real transplant runs. `writing` is the last cancellable moment.
//! 5. the body is **read back**, held to the same baseline gate, and compared
//!    with the restore point. The exit status of anything cannot be trusted
//!    (F3, F10): the result comes from this read-back.
//!
//! Every refusal above happens before the first write, so it is an `Err`. Once
//! writing has started the answer is always an `Ok(ApplyReport)` whose status
//! says what is known:
//!
//! - `applied_verified`: no write failed, the read-back is complete, and every
//!   written property reads back as planned;
//! - `applied_unverified`: writes reported success but the read-back was
//!   incomplete, or a written property does not read back as planned;
//! - `failed`: at least one write failed;
//! - `indeterminate`: never produced here. The parent reports it when the worker
//!   died or was killed before sending a result, so nothing is known.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::diff::{Diff, DiffOptions, diff};
use crate::ops::baseline::{GateOutcome, MissingCapability, NoBaselineReason, gate};
use crate::ops::capture::{CaptureStats, capture};
use crate::ops::list::ListError;
use crate::ops::plan::{PlanInput, digest, summary_text, would_change};
use crate::ops::port::CameraPort;
use crate::ops::readback::{SideEffects, side_effects};
use crate::ops::retention::{self, DataDirFull, LabelError, RestoreOp};
use crate::snapshot::{Camera, Snapshot, SnapshotError, Transport};
use crate::transplant::{CodeNameGuard, SkipReason, TransplantOutcome, numbers_equal, transplant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyStatus {
    AppliedVerified,
    AppliedUnverified,
    Failed,
    /// Produced by the caller of the worker, never by this module.
    Indeterminate,
}

pub struct ApplyRequest<'a> {
    pub plan: PlanInput<'a>,
    pub guard: &'a CodeNameGuard,
    pub snapshots_dir: &'a Path,
    pub name_lookup: &'a HashMap<u32, String>,
    /// The digest and summary the caller copied from the plan result.
    pub expected_digest: &'a str,
    pub expected_summary: &'a str,
    /// Used for the restore point's label and file name.
    pub captured_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestorePoint {
    pub file: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WriteError {
    pub name: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct Readback {
    /// The read-back held to the baseline gate. `None` if nothing at all could be read.
    pub gate: Option<GateOutcome>,
    pub stats: CaptureStats,
    /// The read-back compared with the restore point. Absent if the read-back
    /// was not complete enough to compare.
    pub diff: Option<Diff>,
    /// What changed without being written. Absent if the read-back was not complete.
    pub side_effects: Option<SideEffects>,
    /// Written properties that do not read back as planned.
    pub not_taken: Vec<String>,
    /// Written properties that could not be read back.
    pub unverifiable: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ApplyReport {
    pub status: ApplyStatus,
    /// The plan copied nothing: no restore point, no writes.
    pub no_op: bool,
    pub plan_digest: String,
    pub restore_point: Option<RestorePoint>,
    /// Properties actually written, in name order.
    pub written: Vec<String>,
    pub write_errors: Vec<WriteError>,
    pub outcomes: Vec<TransplantOutcome>,
    pub readback: Option<Readback>,
    /// What `restore_apply(restore_point, only=...)` needs to undo this:
    /// the written properties plus the side effects.
    pub undo_set: Vec<String>,
}

#[derive(Debug)]
pub enum ApplyError {
    /// The digest or summary was not supplied.
    MissingConfirmation { which: &'static str },
    /// The recomputed plan no longer matches what the caller echoed.
    StalePlan { what: &'static str },
    List(ListError),
    RestorePointCap(DataDirFull),
    Label(LabelError),
    /// Apply needs a baseline to trust the restore point and the read-back.
    NoBaseline(NoBaselineReason),
    /// The restore point could not be read at all.
    NothingReadable { read_err: usize },
    /// The restore point capture was incomplete.
    RestorePointIncomplete { missing: Vec<MissingCapability>, stats: CaptureStats },
    /// The restore point file could not be created (including "already exists").
    RestorePointWrite(SnapshotError),
}

impl ApplyError {
    pub fn tag(&self) -> &'static str {
        match self {
            ApplyError::MissingConfirmation { .. } => "missing_confirmation",
            ApplyError::StalePlan { .. } => "stale_plan",
            ApplyError::List(_) => "list_failed",
            ApplyError::RestorePointCap(e) => e.tag(),
            ApplyError::Label(e) => e.tag(),
            ApplyError::NoBaseline(_) => "no_baseline",
            ApplyError::NothingReadable { .. } => "nothing_readable",
            ApplyError::RestorePointIncomplete { .. } => "restore_point_incomplete",
            ApplyError::RestorePointWrite(_) => "restore_point_write_failed",
        }
    }
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::MissingConfirmation { which } => {
                write!(f, "the plan's {which} must be echoed back to apply it; plan first and pass it on")
            }
            ApplyError::StalePlan { what } => write!(
                f,
                "the plan is stale: the {what} no longer matches what the camera and snapshot give now; plan again"
            ),
            ApplyError::List(e) => write!(f, "{e}"),
            ApplyError::RestorePointCap(e) => write!(f, "{e}"),
            ApplyError::Label(e) => write!(f, "{e}"),
            ApplyError::NoBaseline(NoBaselineReason::UnknownModel) => {
                f.write_str("this model has no capture baseline, so a restore point and read-back could not be trusted; nothing was written")
            }
            ApplyError::NoBaseline(NoBaselineReason::FirmwareChanged { baseline, capture }) => write!(
                f,
                "firmware {capture} differs from the baseline's {baseline} and the readable set differs; re-seed the baseline first; nothing was written"
            ),
            ApplyError::NothingReadable { read_err } => {
                write!(f, "the restore point could not be read ({read_err} read error(s)); nothing was written")
            }
            ApplyError::RestorePointIncomplete { missing, .. } => write!(
                f,
                "the restore point capture is incomplete ({} baseline capabilit{} unreadable); nothing was written",
                missing.len(),
                if missing.len() == 1 { "y" } else { "ies" }
            ),
            ApplyError::RestorePointWrite(e) => write!(f, "could not create the restore point file: {e}; nothing was written"),
        }
    }
}

impl std::error::Error for ApplyError {}

/// Both the digest and the summary must have been echoed. Callers check this
/// before opening a camera session; `apply_transplant` checks it again.
pub fn require_confirmation(digest: &str, summary: &str) -> Result<(), ApplyError> {
    if digest.trim().is_empty() {
        return Err(ApplyError::MissingConfirmation { which: "digest" });
    }
    if summary.trim().is_empty() {
        return Err(ApplyError::MissingConfirmation { which: "summary" });
    }
    Ok(())
}

/// Does `got` (read back) show the effect of writing `planned`?
fn took_effect(planned: &Value, got: &Value) -> bool {
    if planned.get("lower").is_some() {
        // A range: the value, within the tolerance the per-write check uses.
        return match (planned.get("value").and_then(Value::as_f64), got.get("value").and_then(Value::as_f64)) {
            (Some(a), Some(b)) => (a - b).abs() <= 1e-6_f64.max(a.abs() * 1e-3),
            _ => false,
        };
    }
    if let (Some(a), Some(b)) = (planned.get("value_index"), got.get("value_index")) {
        // An enum: which option is selected.
        return a == b;
    }
    match (planned, got) {
        (Value::Number(a), Value::Number(b)) => numbers_equal(a, b),
        _ => planned == got,
    }
}

fn compact_timestamp(captured_at: &str) -> String {
    captured_at.replace([':', '-'], "")
}

/// Run the apply. `progress` is told `plan_verified`, `restore_point_written`,
/// `writing` (the last point at which cancelling is safe) and `read_back`.
pub fn apply_transplant<P: CameraPort + ?Sized>(
    port: &P,
    req: &ApplyRequest<'_>,
    progress: &mut dyn FnMut(&str),
) -> Result<ApplyReport, ApplyError> {
    require_confirmation(req.expected_digest, req.expected_summary)?;

    // 1. Recompute the plan on the live body and compare.
    let props = &req.plan.source.properties;
    let planned = transplant(port, props, req.guard, req.plan.options, true);
    let allow = &req.plan.options.allow_raw_code;
    let now_digest = digest(req.plan.source_bytes, &req.plan.target.serial, allow, &planned);
    if now_digest != req.expected_digest.trim() {
        return Err(ApplyError::StalePlan { what: "digest" });
    }
    let n_change = planned.iter().filter(|o| would_change(o)).count();
    if summary_text(req.plan.target, n_change, allow) != req.expected_summary.trim() {
        return Err(ApplyError::StalePlan { what: "summary" });
    }
    progress("plan_verified");

    // 2. Nothing to copy: say so, touch nothing.
    if !planned.iter().any(|o| o.result.is_ok()) {
        return Ok(ApplyReport {
            status: ApplyStatus::AppliedVerified,
            no_op: true,
            plan_digest: now_digest,
            restore_point: None,
            written: Vec::new(),
            write_errors: Vec::new(),
            outcomes: planned,
            readback: None,
            undo_set: Vec::new(),
        });
    }

    // 3. The restore point, before any write.
    let stamp = compact_timestamp(&req.captured_at);
    let label = retention::restore_point_label(RestoreOp::PreTransplant, &stamp).map_err(ApplyError::Label)?;
    let counts = retention::count_for_camera(req.snapshots_dir, &req.plan.target.model, &req.plan.target.serial)
        .map_err(ApplyError::List)?;
    retention::check_restore_point_cap(&counts).map_err(ApplyError::RestorePointCap)?;

    let mut restore_point = Snapshot::new(req.plan.target.clone(), Transport::Usb, req.captured_at.clone());
    restore_point.label = Some(label.clone());
    let rp_stats = capture(port, req.name_lookup, &mut restore_point);
    if rp_stats.read_ok == 0 {
        return Err(ApplyError::NothingReadable { read_err: rp_stats.read_err });
    }
    match gate(&restore_point) {
        GateOutcome::Passed => {}
        GateOutcome::Failed { missing } => {
            return Err(ApplyError::RestorePointIncomplete { missing, stats: rp_stats });
        }
        GateOutcome::NoBaseline(reason) => return Err(ApplyError::NoBaseline(reason)),
    }
    std::fs::create_dir_all(req.snapshots_dir).map_err(|e| ApplyError::RestorePointWrite(SnapshotError::Io(e)))?;
    let rp_file = restore_point.suggested_filename();
    restore_point.save_new(req.snapshots_dir.join(&rp_file)).map_err(ApplyError::RestorePointWrite)?;
    progress("restore_point_written");

    // 4. The writes. From here on there is always a report.
    progress("writing");
    let outcomes = transplant(port, props, req.guard, req.plan.options, false);
    let mut written: BTreeSet<String> = BTreeSet::new();
    let mut write_errors = Vec::new();
    for o in &outcomes {
        match &o.result {
            Ok(_) => {
                written.insert(o.name.clone());
            }
            Err(SkipReason::WriteFailed(msg)) => {
                write_errors.push(WriteError { name: o.name.clone(), message: msg.clone() });
            }
            Err(_) => {}
        }
    }

    // 5. Read back, with the same gate.
    progress("read_back");
    let mut after = Snapshot::new(req.plan.target.clone(), Transport::Usb, req.captured_at.clone());
    after.label = Some("readback".into());
    let after_stats = capture(port, req.name_lookup, &mut after);
    let after_gate = if after_stats.read_ok == 0 {
        None
    } else {
        Some(gate(&after))
    };
    let complete = matches!(after_gate, Some(GateOutcome::Passed));

    let (diff_vs_rp, effects) = if complete {
        let d = diff(&restore_point, &after, &DiffOptions::default()).ok();
        let se = side_effects(&restore_point, &after, &written).ok();
        (d, se)
    } else {
        (None, None)
    };

    let mut not_taken = Vec::new();
    let mut unverifiable = Vec::new();
    for o in &outcomes {
        let (Ok(_), Some(p)) = (&o.result, &o.planned) else { continue };
        match after.properties.get(&o.name) {
            Some(got) if complete => {
                if !took_effect(&p.value, &got.value) {
                    not_taken.push(o.name.clone());
                }
            }
            _ => unverifiable.push(o.name.clone()),
        }
    }

    let status = if !write_errors.is_empty() {
        ApplyStatus::Failed
    } else if !complete || !not_taken.is_empty() || !unverifiable.is_empty() {
        ApplyStatus::AppliedUnverified
    } else {
        ApplyStatus::AppliedVerified
    };

    let mut undo: BTreeSet<String> = written.clone();
    if let Some(se) = &effects {
        undo.extend(se.names());
    }

    Ok(ApplyReport {
        status,
        no_op: false,
        plan_digest: now_digest,
        restore_point: Some(RestorePoint { file: rp_file, label }),
        written: written.into_iter().collect(),
        write_errors,
        outcomes,
        readback: Some(Readback {
            gate: after_gate,
            stats: after_stats,
            diff: diff_vs_rp,
            side_effects: effects,
            not_taken,
            unverifiable,
        }),
        undo_set: undo.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::baseline::baseline_for;
    use crate::ops::plan::{PlanReport, build_plan};
    use crate::ops::port::{FakeCamera, Failure};
    use crate::sdk::{OP_GET, OP_SET};
    use crate::test_support::parse_config;
    use crate::transplant::TransplantOptions;
    use serde_json::json;
    use tempfile::TempDir;

    const RW: u32 = OP_GET | OP_SET;
    const AT: &str = "2026-10-27T17:26:00.688812Z";

    /// A fake Z 9 that reads and writes exactly the baseline's capabilities (value 1 each).
    struct World {
        cam: FakeCamera,
        names: HashMap<u32, String>,
        codes: Vec<(String, u32)>, // the first few baseline capabilities, used as plan targets
        dir: TempDir,
    }

    /// `drop_tail` baseline capabilities at the end are missing from the body. Plan targets are
    /// baseline entries 6..10, which survive any drop_tail up to 186.
    fn world_with(cam_builder: impl Fn(FakeCamera, &[(String, u32)]) -> FakeCamera, drop_tail: usize) -> World {
        let b = baseline_for("Z 9").unwrap();
        let mut cam = FakeCamera::new();
        let mut names = HashMap::new();
        for c in &b.readable[..b.readable.len() - drop_tail] {
            cam = cam.with(c.code, RW, json!(1));
            names.insert(c.code, c.name.clone());
        }
        // Plan targets: three string properties and one numeric, from among the readable ones.
        let codes: Vec<(String, u32)> = b.readable.iter().skip(6).take(4).map(|c| (c.name.clone(), c.code)).collect();
        World { cam: cam_builder(cam, &codes), names, codes, dir: TempDir::new().unwrap() }
    }

    fn world() -> World {
        world_with(|c, _| c, 0)
    }

    struct Fixture {
        source: Snapshot,
        bytes: Vec<u8>,
        target: Camera,
        options: TransplantOptions,
        guard: CodeNameGuard,
    }

    fn fixture(w: &World, allow_numeric: bool) -> Fixture {
        let mut source = Snapshot::new(
            Camera { model: "Z6_3".into(), serial: "SRC".into(), firmware: "2.00".into() },
            Transport::Usb,
            "2026-10-27T10:00:00Z".into(),
        );
        for (i, (name, code)) in w.codes.iter().enumerate() {
            let v = if i == 3 { json!(55) } else { json!(format!("text {i}")) };
            source.insert(name.clone(), *code, v);
        }
        let bytes = source.to_pretty_json().unwrap().into_bytes();
        let caps: Vec<(&str, u32)> = w.codes.iter().map(|(n, c)| (n.trim_start_matches("kNkMAIDCapability_"), *c)).collect();
        let cfg = parse_config(&[("Z6_3", &caps), ("Z 9", &caps)]);
        let guard = CodeNameGuard::new(&cfg, "Z6_3", "Z 9").unwrap();
        let allow = if allow_numeric { vec![w.codes[3].0.clone()] } else { vec![] };
        Fixture {
            source,
            bytes,
            target: Camera { model: "Z 9".into(), serial: "T1".into(), firmware: "5.31".into() },
            options: TransplantOptions { allow_raw_code: allow.into_iter().collect() },
            guard,
        }
    }

    fn plan(w: &World, f: &Fixture) -> PlanReport {
        build_plan(&w.cam, &f.guard, &PlanInput { source: &f.source, source_bytes: &f.bytes, target: &f.target, options: &f.options })
    }

    fn run(w: &World, f: &Fixture, digest: &str, summary: &str, phases: &mut Vec<String>) -> Result<ApplyReport, ApplyError> {
        apply_transplant(
            &w.cam,
            &ApplyRequest {
                plan: PlanInput { source: &f.source, source_bytes: &f.bytes, target: &f.target, options: &f.options },
                guard: &f.guard,
                snapshots_dir: w.dir.path(),
                name_lookup: &w.names,
                expected_digest: digest,
                expected_summary: summary,
                captured_at: AT.into(),
            },
            &mut |p| phases.push(p.to_string()),
        )
    }

    fn apply_planned(w: &World, f: &Fixture) -> Result<ApplyReport, ApplyError> {
        let p = plan(w, f);
        run(w, f, &p.digest, &p.summary, &mut Vec::new())
    }

    fn dir_files(w: &World) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(w.dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    // ── the happy path ───────────────────────────────────────────────────

    #[test]
    fn a_planned_apply_writes_reads_back_and_is_verified() {
        let w = world();
        let f = fixture(&w, true);
        let p = plan(&w, &f);
        let mut phases = Vec::new();
        let r = run(&w, &f, &p.digest, &p.summary, &mut phases).unwrap();
        assert_eq!(r.status, ApplyStatus::AppliedVerified);
        assert!(!r.no_op);
        assert_eq!(phases, ["plan_verified", "restore_point_written", "writing", "read_back"]);
        assert_eq!(r.written.len(), 4);
        assert!(r.write_errors.is_empty());
        for (i, (_, code)) in w.codes.iter().enumerate() {
            let want = if i == 3 { json!(55) } else { json!(format!("text {i}")) };
            assert_eq!(w.cam.value(*code), Some(want));
        }
        let rb = r.readback.as_ref().unwrap();
        assert_eq!(rb.gate, Some(GateOutcome::Passed));
        assert!(rb.not_taken.is_empty() && rb.unverifiable.is_empty());
        assert!(rb.side_effects.as_ref().unwrap().is_empty());
        assert_eq!(rb.diff.as_ref().unwrap().changed.len(), 4, "the diff against the restore point is exactly the writes");
        assert_eq!(r.undo_set, r.written, "no side effects: the undo set is the written set");
        assert_eq!(r.plan_digest, p.digest);
    }

    #[test]
    fn the_restore_point_holds_the_pre_write_state_and_exists_before_the_first_write() {
        let w = world();
        let f = fixture(&w, true);
        let r = apply_planned(&w, &f).unwrap();
        let rp = r.restore_point.unwrap();
        assert_eq!(rp.label, "mcp-restore-pre-transplant-20261027T172600.688812Z");
        assert!(rp.file.contains("_mcp-restore-pre-transplant-"), "{}", rp.file);
        let saved = Snapshot::load_from_file(w.dir.path().join(&rp.file)).unwrap();
        assert_eq!(saved.label.as_deref(), Some(rp.label.as_str()));
        assert_eq!(saved.properties.len(), 196);
        for (_, code) in &w.codes {
            let p = saved.properties.values().find(|p| p.code == *code).unwrap();
            assert_eq!(p.value, json!(1), "the value before the apply, not after");
        }
        assert_eq!(dir_files(&w), [rp.file]);
    }

    // ── confirmation and staleness ───────────────────────────────────────

    #[test]
    fn a_missing_digest_or_summary_is_refused_before_the_camera_is_touched() {
        let w = world();
        let f = fixture(&w, true);
        let p = plan(&w, &f);
        let reads_after_plan = w.cam.reads().len();
        for (d, s, which) in [("", p.summary.as_str(), "digest"), ("  ", p.summary.as_str(), "digest"), (p.digest.as_str(), "", "summary"), (p.digest.as_str(), " \n", "summary")] {
            let e = run(&w, &f, d, s, &mut Vec::new()).unwrap_err();
            assert!(matches!(e, ApplyError::MissingConfirmation { which: w2 } if w2 == which), "{which}: {e:?}");
            assert_eq!(e.tag(), "missing_confirmation");
        }
        assert_eq!(w.cam.reads().len(), reads_after_plan, "no read happened for a refused call");
        assert!(w.cam.writes().is_empty() && dir_files(&w).is_empty());
    }

    #[test]
    fn a_digest_that_differs_by_one_outcome_is_stale_and_nothing_happens() {
        let w = world();
        let f = fixture(&w, true);
        let p = plan(&w, &f);
        // The body changes after the plan: the numeric property now equals the source's value.
        w.cam.write_capability(w.codes[3].1, 0, &json!(55)).unwrap();
        let before = w.cam.writes().len();
        let e = run(&w, &f, &p.digest, &p.summary, &mut Vec::new()).unwrap_err();
        assert!(matches!(e, ApplyError::StalePlan { what: "digest" }), "{e:?}");
        assert_eq!(e.tag(), "stale_plan");
        assert_eq!(w.cam.writes().len(), before, "no write from the apply");
        assert!(dir_files(&w).is_empty(), "no restore point either");
    }

    #[test]
    fn a_summary_that_was_edited_is_stale_even_with_the_right_digest() {
        let w = world();
        let f = fixture(&w, true);
        let p = plan(&w, &f);
        let e = run(&w, &f, &p.digest, "Z 9 T1: 0 properties would change; opted in: none", &mut Vec::new()).unwrap_err();
        assert!(matches!(e, ApplyError::StalePlan { what: "summary" }), "{e:?}");
        assert!(w.cam.writes().is_empty() && dir_files(&w).is_empty());
    }

    #[test]
    fn opt_ins_that_differ_between_plan_and_apply_are_stale() {
        let w = world();
        let planned_with = fixture(&w, true);
        let p = plan(&w, &planned_with);
        let applied_without = fixture(&w, false);
        let e = run(&w, &applied_without, &p.digest, &p.summary, &mut Vec::new()).unwrap_err();
        assert!(matches!(e, ApplyError::StalePlan { .. }), "{e:?}");
        assert!(w.cam.writes().is_empty());
    }

    #[test]
    fn a_changed_source_file_is_stale() {
        let w = world();
        let f = fixture(&w, true);
        let p = plan(&w, &f);
        let mut other = fixture(&w, true);
        other.bytes.push(b'\n');
        let e = run(&w, &other, &p.digest, &p.summary, &mut Vec::new()).unwrap_err();
        assert!(matches!(e, ApplyError::StalePlan { what: "digest" }));
    }

    #[test]
    fn applying_the_same_plan_twice_cannot_write_twice() {
        // Retry safety after a client timeout: the second call finds every outcome already
        // unchanged, so its digest differs and it is refused as stale.
        let w = world();
        let f = fixture(&w, true);
        let p = plan(&w, &f);
        run(&w, &f, &p.digest, &p.summary, &mut Vec::new()).unwrap();
        let writes = w.cam.writes().len();
        let e = run(&w, &f, &p.digest, &p.summary, &mut Vec::new()).unwrap_err();
        assert!(matches!(e, ApplyError::StalePlan { .. }), "{e:?}");
        assert_eq!(w.cam.writes().len(), writes);
        assert_eq!(dir_files(&w).len(), 1, "one restore point, not two");
    }

    // ── a plan that copies nothing ───────────────────────────────────────

    #[test]
    fn an_all_skipped_plan_is_a_no_op_with_no_restore_point() {
        let w = world();
        let mut f = fixture(&w, true);
        // Nothing in the source exists on the target.
        f.source.properties.clear();
        f.source.insert("kNkMAIDCapability_Nowhere", 0xFFFF_0001, json!("x"));
        f.bytes = f.source.to_pretty_json().unwrap().into_bytes();
        let r = apply_planned(&w, &f).unwrap();
        assert!(r.no_op);
        assert_eq!(r.status, ApplyStatus::AppliedVerified);
        assert!(r.written.is_empty() && r.restore_point.is_none() && r.readback.is_none() && r.undo_set.is_empty());
        assert!(w.cam.writes().is_empty() && dir_files(&w).is_empty());
    }

    // ── refusals before any write ────────────────────────────────────────

    #[test]
    fn an_incomplete_restore_point_capture_stops_everything() {
        // 178 of 196 capabilities missing: the ok=18 shape from F2.
        let w = world_with(|c, _| c, 178);
        let f = fixture(&w, true);
        let p = plan(&w, &f);
        let e = run(&w, &f, &p.digest, &p.summary, &mut Vec::new()).unwrap_err();
        match &e {
            ApplyError::RestorePointIncomplete { missing, stats } => assert_eq!((missing.len(), stats.read_ok), (178, 18)),
            other => panic!("{other:?}"),
        }
        assert_eq!(e.tag(), "restore_point_incomplete");
        assert!(w.cam.writes().is_empty() && dir_files(&w).is_empty());
    }

    #[test]
    fn a_restore_point_that_reads_nothing_stops_everything() {
        // Write-only capabilities: the plan can still be computed (strings are written without a read,
        // the numeric target is opted in), but the restore point capture reads nothing at all.
        let b = baseline_for("Z 9").unwrap();
        let mut cam = FakeCamera::new();
        let mut names = HashMap::new();
        for c in &b.readable {
            cam = cam.with(c.code, OP_SET, json!(1));
            names.insert(c.code, c.name.clone());
        }
        let codes: Vec<(String, u32)> = b.readable.iter().skip(6).take(4).map(|c| (c.name.clone(), c.code)).collect();
        let w = World { cam, names, codes, dir: TempDir::new().unwrap() };
        let f = fixture(&w, true);
        let e = apply_planned(&w, &f).unwrap_err();
        assert!(matches!(e, ApplyError::NothingReadable { read_err: 0 }), "{e:?}");
        assert_eq!(e.tag(), "nothing_readable");
        assert!(w.cam.writes().is_empty() && dir_files(&w).is_empty());
    }

    #[test]
    fn an_existing_restore_point_file_is_never_overwritten_and_no_write_happens() {
        let w = world();
        let f = fixture(&w, true);
        let p = plan(&w, &f);
        let first = run(&w, &f, &p.digest, &p.summary, &mut Vec::new()).unwrap();
        let rp_path = w.dir.path().join(&first.restore_point.unwrap().file);
        std::fs::write(&rp_path, "precious").unwrap();
        // Rewind the body so the same plan is valid again, then apply at the same timestamp.
        for (i, (_, code)) in w.codes.iter().enumerate() {
            let _ = i;
            w.cam.write_capability(*code, 0, &json!(1)).unwrap();
        }
        let writes = w.cam.writes().len();
        let p2 = plan(&w, &f);
        let e = run(&w, &f, &p2.digest, &p2.summary, &mut Vec::new()).unwrap_err();
        assert!(matches!(e, ApplyError::RestorePointWrite(_)), "{e:?}");
        assert_eq!(e.tag(), "restore_point_write_failed");
        assert_eq!(std::fs::read_to_string(&rp_path).unwrap(), "precious");
        assert_eq!(w.cam.writes().len(), writes, "no write after the refusal");
    }

    #[test]
    fn the_restore_point_cap_refuses_before_any_write_and_deletes_nothing() {
        let w = world();
        let f = fixture(&w, true);
        for i in 0..retention::MAX_RESTORE_POINTS_PER_CAMERA {
            let mut s = Snapshot::new(f.target.clone(), Transport::Usb, format!("2026-01-01T00:00:00.{i:03}Z"));
            s.label = Some("mcp-restore-pre-transplant-old".into());
            s.insert("kNkMAIDCapability_A", 1, json!(1));
            s.save_to_file(w.dir.path().join(format!("rp-{i}.json"))).unwrap();
        }
        let before = dir_files(&w);
        let e = apply_planned(&w, &f).unwrap_err();
        assert!(matches!(e, ApplyError::RestorePointCap(_)), "{e:?}");
        assert_eq!(e.tag(), "data_dir_full");
        assert!(w.cam.writes().is_empty());
        assert_eq!(dir_files(&w), before);
    }

    #[test]
    fn a_model_without_a_baseline_cannot_be_applied_to() {
        let mut cam = FakeCamera::new();
        let mut names = HashMap::new();
        for code in 1..=3u32 {
            cam = cam.with(code, RW, json!("old"));
            names.insert(code, format!("kNkMAIDCapability_P{code}"));
        }
        let w = World { cam, names, codes: vec![("kNkMAIDCapability_P1".into(), 1), ("kNkMAIDCapability_P2".into(), 2), ("kNkMAIDCapability_P3".into(), 3), ("kNkMAIDCapability_P3".into(), 3)], dir: TempDir::new().unwrap() };
        let mut f = fixture(&w, false);
        f.target.model = "Z 5".into();
        let caps: Vec<(&str, u32)> = vec![("P1", 1), ("P2", 2), ("P3", 3)];
        let cfg = parse_config(&[("Z6_3", &caps), ("Z 5", &caps)]);
        f.guard = CodeNameGuard::new(&cfg, "Z6_3", "Z 5").unwrap();
        let e = apply_planned(&w, &f).unwrap_err();
        assert!(matches!(e, ApplyError::NoBaseline(NoBaselineReason::UnknownModel)), "{e:?}");
        assert_eq!(e.tag(), "no_baseline");
        assert!(w.cam.writes().is_empty() && dir_files(&w).is_empty());
    }

    // ── after the writes start ───────────────────────────────────────────

    #[test]
    fn a_failed_write_is_status_failed_with_the_others_written_and_a_read_back() {
        let w = world_with(|c, codes| c.failing_write(codes[1].1, Failure::Call(-125)), 0);
        let f = fixture(&w, true);
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.status, ApplyStatus::Failed);
        assert_eq!(r.written.len(), 3);
        assert_eq!(r.write_errors.len(), 1);
        assert!(r.write_errors[0].message.contains("-125"));
        assert!(r.restore_point.is_some(), "the restore point exists whatever happened after");
        assert!(r.readback.is_some(), "the body is still read back");
        assert_eq!(w.cam.value(w.codes[1].1), Some(json!(1)), "the failed property kept its old value");
    }

    #[test]
    fn a_write_that_reports_success_but_does_not_take_is_unverified_not_verified() {
        // Writing the first property is accepted, then the body puts it straight back (F4's shape).
        let w = world_with(|c, codes| c.with_dependent(codes[0].1, codes[0].1, json!(1)), 0);
        let f = fixture(&w, true);
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.status, ApplyStatus::AppliedUnverified);
        assert!(r.write_errors.is_empty(), "the write call reported success");
        assert_eq!(r.readback.as_ref().unwrap().not_taken, [w.codes[0].0.clone()]);
    }

    #[test]
    fn an_incomplete_read_back_is_unverified_and_has_no_diff_or_side_effects() {
        // After the first write the body stops answering for one of the baseline capabilities.
        let b = baseline_for("Z 9").unwrap();
        let victim = b.readable[5].code;
        let w = world_with(|c, _| c.failing_read_after_write(victim, Failure::Call(-126)), 0);
        let f = fixture(&w, true);
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.status, ApplyStatus::AppliedUnverified);
        let rb = r.readback.unwrap();
        assert!(matches!(rb.gate, Some(GateOutcome::Failed { .. })));
        assert!(rb.diff.is_none() && rb.side_effects.is_none(), "a partial read-back must not be compared");
        assert_eq!(r.written.len(), 4);
    }

    #[test]
    fn a_read_back_that_reads_nothing_is_unverified_with_no_gate_verdict() {
        let w = world_with(|c, codes| {
            // After the writes, everything the plan touched and the rest stops answering.
            let b = baseline_for("Z 9").unwrap();
            let _ = codes;
            b.readable.iter().fold(c, |c, cap| c.failing_read_after_write(cap.code, Failure::Call(-126)))
        }, 0);
        let f = fixture(&w, true);
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.status, ApplyStatus::AppliedUnverified);
        let rb = r.readback.unwrap();
        assert_eq!(rb.gate, None);
        assert!(r.write_errors.is_empty());
    }

    #[test]
    fn a_side_effect_is_reported_and_joins_the_undo_set() {
        // Writing the first target also moves a baseline capability nobody wrote.
        let b = baseline_for("Z 9").unwrap();
        let other = b.readable[3].code;
        let w = world_with(|c, codes| c.with_dependent(codes[0].1, other, json!(222)), 0);
        let f = fixture(&w, true);
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.status, ApplyStatus::AppliedVerified, "the planned writes all took");
        let se = r.readback.as_ref().unwrap().side_effects.as_ref().unwrap();
        assert_eq!(se.changed.len(), 1);
        assert!(r.undo_set.contains(&se.changed[0]));
        assert_eq!(r.undo_set.len(), r.written.len() + 1);
        assert_eq!(r.written.len(), 4, "the side effect is not counted as written");
    }

    #[test]
    fn only_the_planned_properties_are_ever_written() {
        let w = world();
        let f = fixture(&w, true);
        apply_planned(&w, &f).unwrap();
        let written = w.cam.codes_written();
        assert_eq!(written, w.codes.iter().map(|(_, c)| *c).collect::<BTreeSet<_>>());
    }

    #[test]
    fn the_undo_of_an_apply_restores_the_restore_point() {
        // The round trip the plan promises: apply, then restore the written + side-effect set from the restore point.
        let w = world();
        let f = fixture(&w, true);
        let r = apply_planned(&w, &f).unwrap();
        let rp = Snapshot::load_from_file(w.dir.path().join(&r.restore_point.as_ref().unwrap().file)).unwrap();
        let only: BTreeSet<String> = r.undo_set.iter().cloned().collect();
        let run = crate::ops::restore::execute(&w.cam, &rp.properties, Some(&only), false);
        assert_eq!(run.counts().errors, 0);
        for (_, code) in &w.codes {
            assert_eq!(w.cam.value(*code), Some(json!(1)));
        }
    }

    #[test]
    fn an_enum_write_is_judged_by_the_selected_option_not_by_the_sdks_junk_default_index() {
        // The first plan target is a raw-code enum. After it is written the body reports the same
        // selection but a different (stale-heap) default_index: that still took effect.
        let en = |idx: u64, default: u64| json!({"value_index": idx, "default_index": default, "elem_count": 2, "elem_type": 2, "elem_bytes": 4, "values": [5, 3]});
        let w = world_with(
            |c, codes| {
                let code = codes[0].1;
                c.with(code, RW, en(1, 0)).with_dependent(code, code, en(0, 72350))
            },
            0,
        );
        let mut f = fixture(&w, true);
        // Source wants code 5 (index 0 of [5, 3]); the target is on code 3 (index 1).
        let (name, code) = w.codes[0].clone();
        f.source.insert(name.clone(), code, en(0, 0));
        f.bytes = f.source.to_pretty_json().unwrap().into_bytes();
        f.options.allow_raw_code.insert(name.clone());
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.status, ApplyStatus::AppliedVerified, "{:?}", r.readback.as_ref().map(|rb| &rb.not_taken));
        assert!(r.written.contains(&name));
        // And a selection that did not stick is caught.
        let w2 = world_with(
            |c, codes| {
                let code = codes[0].1;
                c.with(code, RW, en(1, 0)).with_dependent(code, code, en(1, 0))
            },
            0,
        );
        let mut f2 = fixture(&w2, true);
        let (n2, c2) = w2.codes[0].clone();
        f2.source.insert(n2.clone(), c2, en(0, 0));
        f2.bytes = f2.source.to_pretty_json().unwrap().into_bytes();
        f2.options.allow_raw_code.insert(n2.clone());
        let r2 = apply_planned(&w2, &f2).unwrap();
        assert_eq!(r2.status, ApplyStatus::AppliedUnverified);
        assert_eq!(r2.readback.unwrap().not_taken, [n2]);
    }

    // ── took_effect ──────────────────────────────────────────────────────

    #[test]
    fn took_effect_by_kind() {
        // enum: the selected index
        let en = |i: u64| json!({"value_index": i, "default_index": 0, "values": [1, 2, 3]});
        assert!(took_effect(&en(2), &en(2)));
        assert!(!took_effect(&en(2), &en(1)));
        // range: the value, within 1e-6 / 0.1 %
        let rg = |v: f64| json!({"value": v, "lower": 0.0, "upper": 10.0, "steps": 0, "value_index": 0});
        assert!(took_effect(&rg(5.0), &rg(5.0000001)));
        assert!(!took_effect(&rg(5.0), &rg(5.1)));
        // numbers: 3 is 3.0
        assert!(took_effect(&json!(3), &json!(3.0)));
        assert!(!took_effect(&json!(3), &json!(4)));
        // strings and booleans: equality; a type change is not an effect
        assert!(took_effect(&json!("a"), &json!("a")));
        assert!(!took_effect(&json!("a"), &json!("b")));
        assert!(!took_effect(&json!(true), &json!(1)));
        assert!(!took_effect(&json!(3), &json!("3")));
    }

    #[test]
    fn statuses_and_error_tags_are_closed_and_distinct() {
        assert_eq!(serde_json::to_value(ApplyStatus::AppliedVerified).unwrap(), json!("applied_verified"));
        assert_eq!(serde_json::to_value(ApplyStatus::AppliedUnverified).unwrap(), json!("applied_unverified"));
        assert_eq!(serde_json::to_value(ApplyStatus::Failed).unwrap(), json!("failed"));
        assert_eq!(serde_json::to_value(ApplyStatus::Indeterminate).unwrap(), json!("indeterminate"));
        let errs = [
            ApplyError::MissingConfirmation { which: "digest" },
            ApplyError::StalePlan { what: "digest" },
            ApplyError::NoBaseline(NoBaselineReason::UnknownModel),
            ApplyError::NothingReadable { read_err: 0 },
            ApplyError::RestorePointIncomplete { missing: vec![], stats: CaptureStats::default() },
        ];
        let tags: BTreeSet<_> = errs.iter().map(ApplyError::tag).collect();
        assert_eq!(tags.len(), errs.len());
        assert!(errs.iter().all(|e| !e.to_string().is_empty()));
    }
}

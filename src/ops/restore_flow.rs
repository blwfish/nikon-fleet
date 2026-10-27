//! `restore_plan` and `restore_apply`: put a body back to a stored snapshot,
//! with a restore point first and a read-back after (docs/mcp-plan.md §4, §5.6).
//!
//! The same two-step shape as the transplant: a plan carries a digest and a
//! summary, apply must echo both, recomputes the plan and refuses a stale one.
//! Differences:
//!
//! - same body only: the snapshot's serial and model must be the target's
//!   (copying onto another body is a transplant). Refused as
//!   `serial_mismatch` / `model_mismatch`.
//! - the plan reads each property it would write, so the digest carries the
//!   unchanged/changed bit and a retry after success is stale, as in a transplant.
//! - the verdict comes from `restore::verify`: what was *written* is compared
//!   with the snapshot after a read-back. A write the body accepted and
//!   ignored (F18) is `applied_unverified`, never `applied_verified`.
//! - the undo of a restore is a restore of its own restore point
//!   (`mcp-restore-pre-restore-<ts>`), `only` = written + side effects.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::ops::apply::{ApplyStatus, RestorePoint, WriteError, require_confirmation, took_effect};
use crate::ops::baseline::{GateOutcome, MissingCapability, NoBaselineReason, gate};
use crate::ops::capture::{CaptureStats, capture};
use crate::ops::list::ListError;
use crate::ops::plan::NOT_TRANSFERABLE;
use crate::ops::plan::NotTransferable;
use crate::ops::port::CameraPort;
use crate::ops::readback::{SideEffects, side_effects};
use crate::ops::restore::{
    Counts, RestoreRun, StepOutcome, StepResult, Verdict, Verification, coherent_for_write, execute, verify,
};
use crate::ops::retention::{self, DataDirFull, LabelError, RestoreOp};
use crate::snapshot::{Camera, Snapshot, SnapshotError, Transport};

pub const DIGEST_FORMAT: u32 = 1;

pub struct RestorePlanInput<'a> {
    pub source: &'a Snapshot,
    /// The file name the snapshot was loaded from, shown in the summary.
    pub source_name: &'a str,
    /// The source file exactly as stored; hashed, never re-serialised.
    pub source_bytes: &'a [u8],
    pub target: &'a Camera,
    /// Resolved property names (see `restore::resolve_only`), or all of them.
    pub only: Option<&'a BTreeSet<String>>,
}

/// Why a restore was refused before the body was touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    SerialMismatch { snapshot: String, target: String },
    ModelMismatch { snapshot: String, target: String },
}

impl Refusal {
    pub fn tag(&self) -> &'static str {
        match self {
            Refusal::SerialMismatch { .. } => "serial_mismatch",
            Refusal::ModelMismatch { .. } => "model_mismatch",
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::SerialMismatch { snapshot, target } => write!(
                f,
                "the snapshot is of body {snapshot} but the target is {target}; copying onto another body is a transplant"
            ),
            Refusal::ModelMismatch { snapshot, target } => write!(
                f,
                "the snapshot is of a {snapshot} but the target is a {target}; copying across models is a transplant"
            ),
        }
    }
}

impl std::error::Error for Refusal {}

fn same_body(source: &Snapshot, target: &Camera) -> Result<(), Refusal> {
    if source.camera.serial != target.serial {
        return Err(Refusal::SerialMismatch { snapshot: source.camera.serial.clone(), target: target.serial.clone() });
    }
    if source.camera.model != target.model {
        return Err(Refusal::ModelMismatch { snapshot: source.camera.model.clone(), target: target.model.clone() });
    }
    Ok(())
}

/// One property in a restore plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanStep {
    pub name: String,
    pub code: u32,
    pub outcome: StepOutcome,
    /// Whether writing it changes the body: `Some` when the current value was
    /// read, `None` when it was not (not written) or could not be read.
    pub changes: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct RestorePlanReport {
    pub digest: String,
    pub summary: String,
    pub source: Camera,
    pub source_label: Option<String>,
    pub target: Camera,
    pub only: Option<Vec<String>>,
    pub counts: Counts,
    /// Of the properties that would be written, how many would change the body
    /// or have an unknown current value.
    pub would_change: usize,
    pub steps: Vec<PlanStep>,
    /// Named by `only` but not writable: they would not be restored.
    pub unrestorable: Vec<(String, &'static str)>,
    pub cannot_read: &'static [NotTransferable],
}

fn changes_for(port: &(impl CameraPort + ?Sized), code: u32, stored: &Value) -> Option<bool> {
    let live = port.read_capability(code).ok()?;
    Some(!took_effect(&coherent_for_write(stored), &live))
}

fn plan_steps<P: CameraPort + ?Sized>(
    port: &P,
    source: &Snapshot,
    only: Option<&BTreeSet<String>>,
) -> (Vec<PlanStep>, RestoreRun) {
    let run = execute(port, &source.properties, only, true);
    // Properties outside `only` are in the counts, not the list.
    let steps = run
        .steps
        .iter()
        .filter(|s| s.outcome != StepOutcome::NotSelected)
        .map(|s: &StepResult| PlanStep {
            name: s.name.clone(),
            code: s.code,
            outcome: s.outcome.clone(),
            changes: if s.outcome == StepOutcome::WouldWrite {
                changes_for(port, s.code, &source.properties[&s.name].value)
            } else {
                None
            },
        })
        .collect();
    (steps, run)
}

fn count_changes(steps: &[PlanStep]) -> usize {
    steps.iter().filter(|s| s.outcome == StepOutcome::WouldWrite && s.changes != Some(false)).count()
}

/// The line a human is shown and the caller must echo to apply.
pub fn summary_text(target: &Camera, source_name: &str, would_change: usize, only: Option<&BTreeSet<String>>) -> String {
    let scope = match only {
        None => "all".to_string(),
        Some(set) => set.iter().cloned().collect::<Vec<_>>().join(", "),
    };
    format!(
        "{} {}: restore from {source_name}: {would_change} propert{} would change; only: {scope}",
        target.model,
        target.serial,
        if would_change == 1 { "y" } else { "ies" },
    )
}

fn digest(source_bytes: &[u8], target_serial: &str, only: Option<&BTreeSet<String>>, steps: &[PlanStep]) -> String {
    let props: Vec<Value> = steps
        .iter()
        .map(|s| json!({"name": s.name, "code": s.code, "tag": s.outcome.tag(), "changes": s.changes}))
        .collect();
    let doc = json!({
        "format": DIGEST_FORMAT,
        "kind": "restore",
        "source_sha256": hex(&Sha256::digest(source_bytes)),
        "target_serial": target_serial,
        "only": only,
        "steps": props,
    });
    hex(&Sha256::digest(serde_json::to_vec(&doc).expect("a Value always serialises")))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Dry-run a restore and build the plan. Reads the body (each property it would
/// write, once); writes nothing.
pub fn build_restore_plan<P: CameraPort + ?Sized>(
    port: &P,
    input: &RestorePlanInput<'_>,
) -> Result<RestorePlanReport, Refusal> {
    same_body(input.source, input.target)?;
    let (steps, run) = plan_steps(port, input.source, input.only);
    let would_change = count_changes(&steps);
    Ok(RestorePlanReport {
        digest: digest(input.source_bytes, &input.target.serial, input.only, &steps),
        summary: summary_text(input.target, input.source_name, would_change, input.only),
        source: input.source.camera.clone(),
        source_label: input.source.label.clone(),
        target: input.target.clone(),
        only: input.only.map(|s| s.iter().cloned().collect()),
        counts: run.counts(),
        would_change,
        unrestorable: if input.only.is_some() { run.unrestored() } else { Vec::new() },
        steps,
        cannot_read: NOT_TRANSFERABLE,
    })
}

pub struct RestoreApplyRequest<'a> {
    pub plan: RestorePlanInput<'a>,
    pub snapshots_dir: &'a Path,
    pub name_lookup: &'a HashMap<u32, String>,
    pub expected_digest: &'a str,
    pub expected_summary: &'a str,
    pub captured_at: String,
}

#[derive(Debug)]
pub enum RestoreApplyError {
    MissingConfirmation { which: &'static str },
    Refused(Refusal),
    StalePlan { what: &'static str },
    List(ListError),
    RestorePointCap(DataDirFull),
    Label(LabelError),
    NoBaseline(NoBaselineReason),
    NothingReadable { read_err: usize },
    RestorePointIncomplete { missing: Vec<MissingCapability>, stats: CaptureStats },
    RestorePointWrite(SnapshotError),
}

impl RestoreApplyError {
    pub fn tag(&self) -> &'static str {
        match self {
            RestoreApplyError::MissingConfirmation { .. } => "missing_confirmation",
            RestoreApplyError::Refused(r) => r.tag(),
            RestoreApplyError::StalePlan { .. } => "stale_plan",
            RestoreApplyError::List(_) => "list_failed",
            RestoreApplyError::RestorePointCap(e) => e.tag(),
            RestoreApplyError::Label(e) => e.tag(),
            RestoreApplyError::NoBaseline(_) => "no_baseline",
            RestoreApplyError::NothingReadable { .. } => "nothing_readable",
            RestoreApplyError::RestorePointIncomplete { .. } => "restore_point_incomplete",
            RestoreApplyError::RestorePointWrite(_) => "restore_point_write_failed",
        }
    }
}

impl std::fmt::Display for RestoreApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestoreApplyError::MissingConfirmation { which } => {
                write!(f, "the plan's {which} must be echoed back to apply it; plan first and pass it on")
            }
            RestoreApplyError::Refused(r) => write!(f, "{r}"),
            RestoreApplyError::StalePlan { what } => {
                write!(f, "the plan is stale: the {what} no longer matches the camera and snapshot; plan again")
            }
            RestoreApplyError::List(e) => write!(f, "{e}"),
            RestoreApplyError::RestorePointCap(e) => write!(f, "{e}"),
            RestoreApplyError::Label(e) => write!(f, "{e}"),
            RestoreApplyError::NoBaseline(_) => {
                f.write_str("this model/firmware has no usable capture baseline, so a restore point and read-back could not be trusted; nothing was written")
            }
            RestoreApplyError::NothingReadable { read_err } => {
                write!(f, "the restore point could not be read ({read_err} read error(s)); nothing was written")
            }
            RestoreApplyError::RestorePointIncomplete { missing, .. } => write!(
                f,
                "the restore point capture is incomplete ({} baseline capabilit{} unreadable); nothing was written",
                missing.len(),
                if missing.len() == 1 { "y" } else { "ies" }
            ),
            RestoreApplyError::RestorePointWrite(e) => {
                write!(f, "could not create the restore point file: {e}; nothing was written")
            }
        }
    }
}

impl std::error::Error for RestoreApplyError {}

impl From<crate::ops::apply::ApplyError> for RestoreApplyError {
    fn from(e: crate::ops::apply::ApplyError) -> Self {
        match e {
            crate::ops::apply::ApplyError::MissingConfirmation { which } => RestoreApplyError::MissingConfirmation { which },
            _ => unreachable!("require_confirmation only returns MissingConfirmation"),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RestoreApplyReport {
    pub status: ApplyStatus,
    pub no_op: bool,
    pub plan_digest: String,
    pub restore_point: Option<RestorePoint>,
    /// Properties actually written, in name order.
    pub written: Vec<String>,
    pub write_errors: Vec<WriteError>,
    /// Named by `only` but not written.
    pub unrestored: Vec<(String, &'static str)>,
    pub steps: Vec<StepResult>,
    pub verification: Option<Verification>,
    /// What changed without being written (vs the restore point). Absent if there was no complete read-back.
    pub side_effects: Option<SideEffects>,
    /// What `restore_apply(restore_point, only=...)` needs to undo this: written + side effects.
    pub undo_set: Vec<String>,
}

fn compact_timestamp(captured_at: &str) -> String {
    captured_at.replace([':', '-'], "")
}

/// Run the restore. `progress` is told `plan_verified`, `restore_point_written`,
/// `writing` (the last point at which cancelling is safe) and `read_back`.
pub fn apply_restore<P: CameraPort + ?Sized>(
    port: &P,
    req: &RestoreApplyRequest<'_>,
    progress: &mut dyn FnMut(&str),
) -> Result<RestoreApplyReport, RestoreApplyError> {
    require_confirmation(req.expected_digest, req.expected_summary)?;
    same_body(req.plan.source, req.plan.target).map_err(RestoreApplyError::Refused)?;

    // 1. Recompute the plan on the live body and compare.
    let (steps_now, _) = plan_steps(port, req.plan.source, req.plan.only);
    let now_digest = digest(req.plan.source_bytes, &req.plan.target.serial, req.plan.only, &steps_now);
    if now_digest != req.expected_digest.trim() {
        return Err(RestoreApplyError::StalePlan { what: "digest" });
    }
    let n_change = count_changes(&steps_now);
    if summary_text(req.plan.target, req.plan.source_name, n_change, req.plan.only) != req.expected_summary.trim() {
        return Err(RestoreApplyError::StalePlan { what: "summary" });
    }
    progress("plan_verified");

    // 2. Nothing to change: say so, touch nothing. A targeted restore that cannot do
    //    what was named is a failure, not a quiet success.
    //    Writing a value the body already holds is not a change: a retry after success
    //    lands here instead of making another restore point.
    if n_change == 0 {
        let dry = execute(port, &req.plan.source.properties, req.plan.only, true);
        let unrestored = if req.plan.only.is_some() { dry.unrestored() } else { Vec::new() };
        let status = if unrestored.is_empty() { ApplyStatus::AppliedVerified } else { ApplyStatus::Failed };
        return Ok(RestoreApplyReport {
            status,
            no_op: true,
            plan_digest: now_digest,
            restore_point: None,
            written: Vec::new(),
            write_errors: Vec::new(),
            unrestored,
            steps: dry.steps.into_iter().filter(|s| s.outcome != StepOutcome::NotSelected).collect(),
            verification: None,
            side_effects: None,
            undo_set: Vec::new(),
        });
    }

    // 3. The restore point, before any write.
    let label = retention::restore_point_label(RestoreOp::PreRestore, &compact_timestamp(&req.captured_at))
        .map_err(RestoreApplyError::Label)?;
    let counts = retention::count_for_camera(req.snapshots_dir, &req.plan.target.model, &req.plan.target.serial)
        .map_err(RestoreApplyError::List)?;
    retention::check_restore_point_cap(&counts).map_err(RestoreApplyError::RestorePointCap)?;
    let mut restore_point = Snapshot::new(req.plan.target.clone(), Transport::Usb, req.captured_at.clone());
    restore_point.label = Some(label.clone());
    let rp_stats = capture(port, req.name_lookup, &mut restore_point);
    if rp_stats.read_ok == 0 {
        return Err(RestoreApplyError::NothingReadable { read_err: rp_stats.read_err });
    }
    match gate(&restore_point) {
        GateOutcome::Passed => {}
        GateOutcome::Failed { missing } => return Err(RestoreApplyError::RestorePointIncomplete { missing, stats: rp_stats }),
        GateOutcome::NoBaseline(reason) => return Err(RestoreApplyError::NoBaseline(reason)),
    }
    std::fs::create_dir_all(req.snapshots_dir).map_err(|e| RestoreApplyError::RestorePointWrite(SnapshotError::Io(e)))?;
    let rp_file = restore_point.suggested_filename();
    restore_point.save_new(req.snapshots_dir.join(&rp_file)).map_err(RestoreApplyError::RestorePointWrite)?;
    progress("restore_point_written");

    // 4. The writes. From here on there is always a report.
    progress("writing");
    let run = execute(port, &req.plan.source.properties, req.plan.only, false);
    let written = run.written_names();
    let write_errors: Vec<WriteError> = run
        .steps
        .iter()
        .filter_map(|s| match &s.outcome {
            StepOutcome::WriteFailed(m) => Some(WriteError { name: s.name.clone(), message: m.clone() }),
            _ => None,
        })
        .collect();
    let unrestored = if req.plan.only.is_some() { run.unrestored() } else { Vec::new() };

    // 5. Read back and judge what was written against the snapshot.
    progress("read_back");
    let verification = verify(port, req.name_lookup, req.plan.target, req.plan.source, &written, &req.captured_at);
    let effects = verification
        .readback
        .as_ref()
        .and_then(|after| side_effects(&restore_point, after, &written).ok());

    let status = if !write_errors.is_empty() || !unrestored.is_empty() {
        ApplyStatus::Failed
    } else {
        match verification.verdict {
            Verdict::Verified | Verdict::NothingWritten => ApplyStatus::AppliedVerified,
            Verdict::ResidualFound | Verdict::ReadbackIncomplete => ApplyStatus::AppliedUnverified,
        }
    };

    let mut undo: BTreeSet<String> = written.clone();
    if let Some(se) = &effects {
        undo.extend(se.names());
    }
    Ok(RestoreApplyReport {
        status,
        no_op: false,
        plan_digest: now_digest,
        restore_point: Some(RestorePoint { file: rp_file, label }),
        written: written.into_iter().collect(),
        write_errors,
        unrestored,
        steps: run.steps.into_iter().filter(|s| s.outcome != StepOutcome::NotSelected).collect(),
        verification: Some(verification),
        side_effects: effects,
        undo_set: undo.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::baseline::baseline_for;
    use crate::ops::port::{FakeCamera, Failure};
    use crate::ops::retention::MAX_RESTORE_POINTS_PER_CAMERA;
    use crate::sdk::{OP_GET, OP_SET};
    use tempfile::TempDir;

    const RW: u32 = OP_GET | OP_SET;
    const AT: &str = "2026-10-27T17:26:00.688812Z";

    fn stepped(index: u64) -> Value {
        json!({"value": 0.0, "default": 0.0, "value_index": index, "default_index": 612,
               "lower": 0.0, "upper": 1224.0, "steps": 1225})
    }

    fn body() -> Camera {
        Camera { model: "Z 9".into(), serial: "T1".into(), firmware: "5.31".into() }
    }

    /// A fake Z 9 that reads and writes exactly the baseline's capabilities (value 1 each), except
    /// four restore targets: two strings, a number and a stepped range (live index 613).
    struct World {
        cam: FakeCamera,
        names: HashMap<u32, String>,
        targets: Vec<(String, u32)>,
        dir: TempDir,
    }

    fn world_with(build: impl Fn(FakeCamera, &[(String, u32)]) -> FakeCamera, drop_tail: usize) -> World {
        let b = baseline_for("Z 9").unwrap();
        let targets: Vec<(String, u32)> = b.readable.iter().skip(6).take(4).map(|c| (c.name.clone(), c.code)).collect();
        let mut cam = FakeCamera::new();
        let mut names = HashMap::new();
        for c in &b.readable[..b.readable.len() - drop_tail] {
            let live = if c.code == targets[3].1 { stepped(613) } else { json!(1) };
            cam = cam.with(c.code, RW, live);
            names.insert(c.code, c.name.clone());
        }
        // The real SDK reads a stepped range back with `value` 0.0 whatever was written (F18).
        World { cam: build(cam.with_sdk_range_quirk(), &targets), names, targets, dir: TempDir::new().unwrap() }
    }

    fn world() -> World {
        world_with(|c, _| c, 0)
    }

    struct Fixture {
        source: Snapshot,
        bytes: Vec<u8>,
        target: Camera,
        only: BTreeSet<String>,
    }

    /// The stored state: every baseline capability at 1 except the four targets, which differ
    /// from the live body (strings, 55, range index 612).
    fn fixture(w: &World) -> Fixture {
        let b = baseline_for("Z 9").unwrap();
        let mut source = Snapshot::new(body(), Transport::Usb, "2026-10-27T10:00:00Z".into());
        for c in &b.readable {
            let v = match w.targets.iter().position(|(_, code)| *code == c.code) {
                Some(0) | Some(1) => json!(format!("text {}", c.code)),
                Some(2) => json!(55),
                Some(3) => stepped(612),
                _ => json!(1),
            };
            source.insert(c.name.clone(), c.code, v);
        }
        let bytes = source.to_pretty_json().unwrap().into_bytes();
        Fixture { source, bytes, target: body(), only: w.targets.iter().map(|(n, _)| n.clone()).collect() }
    }

    fn input<'a>(f: &'a Fixture, only: Option<&'a BTreeSet<String>>) -> RestorePlanInput<'a> {
        RestorePlanInput { source: &f.source, source_name: "Z_9_base.json", source_bytes: &f.bytes, target: &f.target, only }
    }

    fn plan_of(w: &World, f: &Fixture) -> RestorePlanReport {
        build_restore_plan(&w.cam, &input(f, Some(&f.only))).unwrap()
    }

    fn run(w: &World, f: &Fixture, digest: &str, summary: &str, phases: &mut Vec<String>) -> Result<RestoreApplyReport, RestoreApplyError> {
        apply_restore(
            &w.cam,
            &RestoreApplyRequest {
                plan: input(f, Some(&f.only)),
                snapshots_dir: w.dir.path(),
                name_lookup: &w.names,
                expected_digest: digest,
                expected_summary: summary,
                captured_at: AT.into(),
            },
            &mut |p| phases.push(p.to_string()),
        )
    }

    fn apply_planned(w: &World, f: &Fixture) -> Result<RestoreApplyReport, RestoreApplyError> {
        let p = plan_of(w, f);
        run(w, f, &p.digest, &p.summary, &mut Vec::new())
    }

    fn dir_files(w: &World) -> Vec<String> {
        let mut v: Vec<String> =
            std::fs::read_dir(w.dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    // ── the plan ─────────────────────────────────────────────────────────

    #[test]
    fn a_plan_reads_the_body_and_writes_nothing() {
        let w = world();
        let f = fixture(&w);
        let p = plan_of(&w, &f);
        assert!(w.cam.writes().is_empty());
        assert_eq!(p.would_change, 4);
        assert_eq!(p.counts.written, 4);
        assert_eq!(p.digest.len(), 64);
        assert_eq!(p.steps.len(), 4, "unselected properties are counted, not listed");
        assert!(p.steps.iter().all(|s| s.changes == Some(true)));
        assert_eq!(p.summary, "Z 9 T1: restore from Z_9_base.json: 4 properties would change; only: ".to_string() + &f.only.iter().cloned().collect::<Vec<_>>().join(", "));
        assert!(p.unrestorable.is_empty());
        assert_eq!(dir_files(&w), Vec::<String>::new(), "a plan leaves no file behind");
    }

    #[test]
    fn a_plan_over_everything_counts_only_what_would_change() {
        let w = world();
        let f = fixture(&w);
        let p = build_restore_plan(&w.cam, &input(&f, None)).unwrap();
        assert_eq!(p.would_change, 4, "196 writable, 4 differ");
        assert!(p.counts.written > 100);
        assert!(p.summary.ends_with("only: all"));
        assert!(p.only.is_none());
    }

    #[test]
    fn a_value_the_body_already_holds_is_not_a_change() {
        let w = world();
        let f = fixture(&w);
        w.cam.write_capability(w.targets[0].1, 0, &json!("text ".to_string() + &w.targets[0].1.to_string())).unwrap();
        let p = plan_of(&w, &f);
        assert_eq!(p.would_change, 3);
        assert_eq!(p.steps.iter().find(|s| s.code == w.targets[0].1).unwrap().changes, Some(false));
    }

    #[test]
    fn a_stepped_range_at_the_stored_index_is_not_a_change_even_under_the_sdk_quirk() {
        let w = world_with(|c, t| c.with(t[3].1, RW, stepped(612)), 0);
        let f = fixture(&w);
        let p = plan_of(&w, &f);
        assert_eq!(p.would_change, 3);
        assert_eq!(p.steps.iter().find(|s| s.code == w.targets[3].1).unwrap().changes, Some(false));
    }

    #[test]
    fn a_plan_names_what_cannot_be_restored() {
        let w = world_with(|c, t| c.with(t[1].1, OP_GET, json!("ro")), 0);
        let f = fixture(&w);
        let p = plan_of(&w, &f);
        assert_eq!(p.would_change, 3);
        assert_eq!(p.unrestorable, vec![(w.targets[1].0.clone(), "read_only")]);
        let all = build_restore_plan(&w.cam, &input(&f, None)).unwrap();
        assert!(all.unrestorable.is_empty(), "an untargeted restore does not list every skip as a failure");
    }

    #[test]
    fn the_digest_covers_the_source_the_body_the_scope_and_the_state() {
        let w = world();
        let f = fixture(&w);
        let d = plan_of(&w, &f).digest;
        assert_eq!(d, plan_of(&w, &f).digest, "stable");
        // different scope
        let one: BTreeSet<String> = BTreeSet::from([w.targets[0].0.clone()]);
        assert_ne!(d, build_restore_plan(&w.cam, &input(&f, Some(&one))).unwrap().digest);
        // different source bytes
        let mut f2 = fixture(&w);
        f2.bytes.push(b' ');
        assert_ne!(d, plan_of(&w, &f2).digest);
        // different body state
        w.cam.write_capability(w.targets[2].1, 0, &json!(55)).unwrap();
        assert_ne!(d, plan_of(&w, &f).digest);
        // different target serial (same source, same state)
        let w2 = world();
        let mut f3 = fixture(&w2);
        let d2 = plan_of(&w2, &f3).digest;
        f3.target.serial = "T2".into();
        f3.source.camera.serial = "T2".into();
        assert_ne!(d2, plan_of(&w2, &f3).digest);
    }

    // ── refusals before the body is touched ──────────────────────────────

    #[test]
    fn another_body_or_model_is_refused_in_plan_and_apply() {
        let w = world();
        let f = fixture(&w);
        let mut other_serial = fixture(&w);
        other_serial.target.serial = "T2".into();
        let e = build_restore_plan(&w.cam, &input(&other_serial, None)).unwrap_err();
        assert_eq!(e, Refusal::SerialMismatch { snapshot: "T1".into(), target: "T2".into() });
        assert_eq!(e.tag(), "serial_mismatch");
        let mut other_model = fixture(&w);
        other_model.target.model = "Z6_3".into();
        let e = build_restore_plan(&w.cam, &input(&other_model, None)).unwrap_err();
        assert_eq!(e.tag(), "model_mismatch");

        let p = plan_of(&w, &f);
        let reads = w.cam.reads().len();
        let e = run(&w, &other_serial, &p.digest, &p.summary, &mut Vec::new()).unwrap_err();
        assert!(matches!(e, RestoreApplyError::Refused(Refusal::SerialMismatch { .. })), "{e:?}");
        assert_eq!(e.tag(), "serial_mismatch");
        let e = run(&w, &other_model, &p.digest, &p.summary, &mut Vec::new()).unwrap_err();
        assert_eq!(e.tag(), "model_mismatch");
        assert_eq!(w.cam.reads().len(), reads, "refused before any read");
        assert!(w.cam.writes().is_empty() && dir_files(&w).is_empty());
    }

    #[test]
    fn a_missing_digest_or_summary_is_refused_before_the_camera_is_touched() {
        let w = world();
        let f = fixture(&w);
        let p = plan_of(&w, &f);
        let reads = w.cam.reads().len();
        for (d, s, which) in [("", p.summary.as_str(), "digest"), ("  ", p.summary.as_str(), "digest"), (p.digest.as_str(), "", "summary"), (p.digest.as_str(), " \n", "summary")] {
            let e = run(&w, &f, d, s, &mut Vec::new()).unwrap_err();
            assert!(matches!(e, RestoreApplyError::MissingConfirmation { which: w2 } if w2 == which), "{which}: {e:?}");
            assert_eq!(e.tag(), "missing_confirmation");
        }
        assert_eq!(w.cam.reads().len(), reads);
        assert!(w.cam.writes().is_empty() && dir_files(&w).is_empty());
    }

    #[test]
    fn a_plan_made_before_the_body_changed_is_stale_and_nothing_happens() {
        let w = world();
        let f = fixture(&w);
        let p = plan_of(&w, &f);
        // Someone already restored one of the four.
        w.cam.write_capability(w.targets[2].1, 0, &json!(55)).unwrap();
        let writes = w.cam.writes().len();
        let e = run(&w, &f, &p.digest, &p.summary, &mut Vec::new()).unwrap_err();
        assert!(matches!(e, RestoreApplyError::StalePlan { what: "digest" }), "{e:?}");
        assert_eq!(e.tag(), "stale_plan");
        assert_eq!(w.cam.writes().len(), writes);
        assert!(dir_files(&w).is_empty());
    }

    #[test]
    fn a_right_digest_with_a_wrong_summary_is_stale() {
        let w = world();
        let f = fixture(&w);
        let p = plan_of(&w, &f);
        let e = run(&w, &f, &p.digest, &p.summary.replace("4 properties", "3 properties"), &mut Vec::new()).unwrap_err();
        assert!(matches!(e, RestoreApplyError::StalePlan { what: "summary" }), "{e:?}");
        assert!(w.cam.writes().is_empty());
    }

    #[test]
    fn surrounding_whitespace_in_the_echo_is_tolerated() {
        let w = world();
        let f = fixture(&w);
        let p = plan_of(&w, &f);
        let r = run(&w, &f, &format!(" {}\n", p.digest), &format!("\n{} ", p.summary), &mut Vec::new()).unwrap();
        assert_eq!(r.status, ApplyStatus::AppliedVerified);
    }

    // ── the happy path ───────────────────────────────────────────────────

    #[test]
    fn a_planned_restore_writes_reads_back_and_is_verified() {
        let w = world();
        let f = fixture(&w);
        let p = plan_of(&w, &f);
        let mut phases = Vec::new();
        let r = run(&w, &f, &p.digest, &p.summary, &mut phases).unwrap();
        assert_eq!(r.status, ApplyStatus::AppliedVerified);
        assert!(!r.no_op);
        assert_eq!(phases, ["plan_verified", "restore_point_written", "writing", "read_back"]);
        assert_eq!(r.written.len(), 4);
        assert!(r.write_errors.is_empty() && r.unrestored.is_empty());
        assert_eq!(r.verification.as_ref().unwrap().verdict, Verdict::Verified);
        assert!(r.side_effects.as_ref().unwrap().is_empty());
        assert_eq!(r.undo_set, r.written, "no side effects: the undo set is the written set");
        assert_eq!(r.plan_digest, p.digest);
        let (a, b, c, d) = (&w.targets[0].1, &w.targets[1].1, &w.targets[2].1, &w.targets[3].1);
        assert_eq!(w.cam.value(*a), Some(json!(format!("text {a}"))));
        assert_eq!(w.cam.value(*b), Some(json!(format!("text {b}"))));
        assert_eq!(w.cam.value(*c), Some(json!(55)));
        assert_eq!(w.cam.value(*d).unwrap()["value_index"], json!(612));
        assert_eq!(w.cam.codes_written(), BTreeSet::from([*a, *b, *c, *d]), "only the named four were written");
    }

    #[test]
    fn the_restore_point_holds_the_pre_write_state_and_exists_before_the_first_write() {
        let w = world();
        let f = fixture(&w);
        let r = apply_planned(&w, &f).unwrap();
        let rp = r.restore_point.unwrap();
        assert_eq!(rp.label, "mcp-restore-pre-restore-20261027T172600.688812Z");
        assert!(rp.file.contains("_mcp-restore-pre-restore-"), "{}", rp.file);
        let saved = Snapshot::load_from_file(w.dir.path().join(&rp.file)).unwrap();
        assert_eq!(saved.label.as_deref(), Some(rp.label.as_str()));
        assert_eq!(saved.properties.len(), 196);
        let code = w.targets[2].1;
        assert_eq!(saved.properties.values().find(|p| p.code == code).unwrap().value, json!(1), "before, not after");
        assert_eq!(dir_files(&w), [rp.file]);
    }

    #[test]
    fn restoring_everything_writes_what_differs_and_verifies_it() {
        let w = world();
        let f = fixture(&w);
        let p = build_restore_plan(&w.cam, &input(&f, None)).unwrap();
        let r = apply_restore(
            &w.cam,
            &RestoreApplyRequest {
                plan: input(&f, None),
                snapshots_dir: w.dir.path(),
                name_lookup: &w.names,
                expected_digest: &p.digest,
                expected_summary: &p.summary,
                captured_at: AT.into(),
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(r.status, ApplyStatus::AppliedVerified);
        assert!(r.written.len() > 100, "an untargeted restore writes every writable property");
        assert!(r.unrestored.is_empty(), "skips are not failures without --only");
    }

    #[test]
    fn a_retry_after_success_is_a_no_op_with_no_new_restore_point() {
        let w = world();
        let f = fixture(&w);
        apply_planned(&w, &f).unwrap();
        let files = dir_files(&w);
        let writes = w.cam.writes().len();
        let p = plan_of(&w, &f);
        assert_eq!(p.would_change, 0);
        let mut phases = Vec::new();
        let r = run(&w, &f, &p.digest, &p.summary, &mut phases).unwrap();
        assert!(r.no_op);
        assert_eq!(r.status, ApplyStatus::AppliedVerified);
        assert!(r.restore_point.is_none() && r.verification.is_none() && r.written.is_empty() && r.undo_set.is_empty());
        assert_eq!(phases, ["plan_verified"]);
        assert_eq!(w.cam.writes().len(), writes);
        assert_eq!(dir_files(&w), files);
    }

    #[test]
    fn naming_only_unrestorable_properties_is_a_failure_not_a_quiet_success() {
        let w = world_with(|c, t| c.with(t[1].1, OP_GET, json!("ro")), 0);
        let f = fixture(&w);
        let mut f = f;
        f.only = BTreeSet::from([w.targets[1].0.clone()]);
        let r = apply_planned(&w, &f).unwrap();
        assert!(r.no_op);
        assert_eq!(r.status, ApplyStatus::Failed);
        assert_eq!(r.unrestored, vec![(w.targets[1].0.clone(), "read_only")]);
        assert!(w.cam.writes().is_empty() && dir_files(&w).is_empty());
    }

    #[test]
    fn a_partly_unrestorable_selection_writes_the_rest_and_is_failed() {
        let w = world_with(|c, t| c.with(t[1].1, OP_GET, json!("ro")), 0);
        let f = fixture(&w);
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.written.len(), 3);
        assert_eq!(r.unrestored, vec![(w.targets[1].0.clone(), "read_only")]);
        assert_eq!(r.status, ApplyStatus::Failed, "what was asked for was not all done");
        assert_eq!(r.verification.as_ref().unwrap().verdict, Verdict::Verified, "what was written did take");
    }

    // ── a body that does not do what it was told ─────────────────────────

    #[test]
    fn a_body_that_accepts_and_ignores_a_write_is_applied_unverified_with_the_residual() {
        let w = world_with(|c, t| c.ignoring_writes(t[1].1), 0);
        let f = fixture(&w);
        let r = apply_planned(&w, &f).unwrap();
        assert!(r.write_errors.is_empty(), "the write call said success");
        assert_eq!(r.written.len(), 4);
        assert_eq!(r.status, ApplyStatus::AppliedUnverified);
        let v = r.verification.unwrap();
        assert_eq!(v.verdict, Verdict::ResidualFound);
        assert_eq!(v.residual.unwrap().changed, [w.targets[1].0.clone()]);
        assert!(r.undo_set.contains(&w.targets[1].0), "still listed: the restore point is the way back");
    }

    #[test]
    fn a_body_that_ignores_blind_range_writes_is_still_restored_because_the_range_is_read_first() {
        let w = world_with(|c, t| c.ignoring_blind_writes(t[3].1), 0);
        let f = fixture(&w);
        let r = apply_planned(&w, &f).unwrap();
        assert!(w.cam.ignored_writes().is_empty(), "no write was blind");
        assert_eq!(r.status, ApplyStatus::AppliedVerified);
        assert_eq!(w.cam.value(w.targets[3].1).unwrap()["value_index"], json!(612));
    }

    #[test]
    fn a_write_the_body_rejects_is_failed_and_the_rest_still_happen() {
        let w = world_with(|c, t| c.failing_write(t[0].1, Failure::Call(-125)), 0);
        let f = fixture(&w);
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.status, ApplyStatus::Failed);
        assert_eq!(r.write_errors.len(), 1);
        assert_eq!(r.write_errors[0].name, w.targets[0].0);
        assert_eq!(r.written.len(), 3);
        assert!(!r.written.contains(&w.targets[0].0));
        assert!(r.restore_point.is_some(), "the restore point was made before the failed write");
    }

    #[test]
    fn an_untargeted_restore_with_a_rejected_write_is_failed_too() {
        // Without `only` nothing is "named", so the failure must come from the write error itself.
        let w = world_with(|c, t| c.failing_write(t[0].1, Failure::Call(-125)), 0);
        let f = fixture(&w);
        let p = build_restore_plan(&w.cam, &input(&f, None)).unwrap();
        let r = run_all(&w, &f, &p).unwrap();
        assert!(r.unrestored.is_empty());
        assert_eq!(r.write_errors.len(), 1);
        assert_eq!(r.status, ApplyStatus::Failed);
    }

    #[test]
    fn a_read_back_that_cannot_be_completed_is_unverified_not_verified() {
        // An unrelated baseline capability stops reading after the first write: the read-back
        // fails the baseline gate, so nothing can be claimed about the four.
        let w = world_with(|c, _| c.failing_read_after_write(baseline_for("Z 9").unwrap().readable[100].code, Failure::Call(-126)), 0);
        let f = fixture(&w);
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.status, ApplyStatus::AppliedUnverified);
        assert_eq!(r.verification.as_ref().unwrap().verdict, Verdict::ReadbackIncomplete);
        assert!(r.side_effects.is_none(), "no complete read-back: no claim about side effects");
        assert_eq!(r.undo_set, r.written);
    }

    #[test]
    fn a_side_effect_is_reported_and_joins_the_undo_set() {
        let other = baseline_for("Z 9").unwrap().readable[50].code;
        let w = world_with(|c, t| c.with_dependent(t[2].1, other, json!(9)), 0);
        let f = fixture(&w);
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.status, ApplyStatus::AppliedVerified, "the four took; the extra change is reported, not hidden");
        let se = r.side_effects.unwrap();
        let other_name = w.names[&other].clone();
        assert!(se.names().contains(&other_name));
        assert!(r.undo_set.contains(&other_name));
        assert!(r.undo_set.len() == r.written.len() + 1);
    }

    // ── restore-point refusals: nothing is written ───────────────────────

    #[test]
    fn an_incomplete_restore_point_refuses_before_any_write() {
        let w = world_with(|c, _| c, 20);
        let f = fixture(&w);
        let e = apply_planned(&w, &f).unwrap_err();
        assert!(matches!(e, RestoreApplyError::RestorePointIncomplete { .. }), "{e:?}");
        assert_eq!(e.tag(), "restore_point_incomplete");
        assert!(w.cam.writes().is_empty() && dir_files(&w).is_empty());
    }

    #[test]
    fn a_body_that_reads_nothing_refuses_before_any_write() {
        let b = baseline_for("Z 9").unwrap();
        let mut cam = FakeCamera::new();
        let mut names = HashMap::new();
        for c in &b.readable {
            cam = cam.with_unreadable(c.code, RW);
            names.insert(c.code, c.name.clone());
        }
        let targets: Vec<(String, u32)> = b.readable.iter().skip(6).take(4).map(|c| (c.name.clone(), c.code)).collect();
        let w = World { cam, names, targets, dir: TempDir::new().unwrap() };
        let f = fixture(&w);
        // Unreadable properties cannot be compared, so the plan has no `changes` bit; build the
        // confirmation from the plan as a caller would.
        let p = plan_of(&w, &f);
        let e = run(&w, &f, &p.digest, &p.summary, &mut Vec::new()).unwrap_err();
        assert!(matches!(e, RestoreApplyError::NothingReadable { .. }), "{e:?}");
        assert!(w.cam.writes().is_empty() && dir_files(&w).is_empty());
    }

    #[test]
    fn a_model_without_a_baseline_cannot_be_restored() {
        let mut cam = FakeCamera::new();
        let mut names = HashMap::new();
        for code in 1..=3u32 {
            cam = cam.with(code, RW, json!("old"));
            names.insert(code, format!("kNkMAIDCapability_P{code}"));
        }
        let w = World { cam, names, targets: vec![], dir: TempDir::new().unwrap() };
        let mut source = Snapshot::new(Camera { model: "Z 5".into(), serial: "T1".into(), firmware: "1".into() }, Transport::Usb, "t".into());
        for code in 1..=3u32 {
            source.insert(format!("kNkMAIDCapability_P{code}"), code, json!("new"));
        }
        let bytes = source.to_pretty_json().unwrap().into_bytes();
        let target = Camera { model: "Z 5".into(), serial: "T1".into(), firmware: "1".into() };
        let f = Fixture { source, bytes, target, only: BTreeSet::new() };
        let p = build_restore_plan(&w.cam, &input(&f, None)).unwrap();
        let e = run_all(&w, &f, &p).unwrap_err();
        assert_eq!(e.tag(), "no_baseline");
        assert!(w.cam.writes().is_empty() && dir_files(&w).is_empty());
    }

    fn run_all(w: &World, f: &Fixture, p: &RestorePlanReport) -> Result<RestoreApplyReport, RestoreApplyError> {
        apply_restore(
            &w.cam,
            &RestoreApplyRequest {
                plan: input(f, None),
                snapshots_dir: w.dir.path(),
                name_lookup: &w.names,
                expected_digest: &p.digest,
                expected_summary: &p.summary,
                captured_at: AT.into(),
            },
            &mut |_| {},
        )
    }

    #[test]
    fn the_restore_point_cap_refuses_before_any_write_and_deletes_nothing() {
        let w = world();
        let f = fixture(&w);
        for i in 0..MAX_RESTORE_POINTS_PER_CAMERA {
            let mut s = Snapshot::new(f.target.clone(), Transport::Usb, format!("2026-01-01T00:00:00.{i:03}Z"));
            s.label = Some("mcp-restore-pre-restore-old".into());
            s.insert("kNkMAIDCapability_A", 1, json!(1));
            s.save_to_file(w.dir.path().join(format!("rp-{i}.json"))).unwrap();
        }
        let before = dir_files(&w);
        let e = apply_planned(&w, &f).unwrap_err();
        assert!(matches!(e, RestoreApplyError::RestorePointCap(_)), "{e:?}");
        assert_eq!(e.tag(), "data_dir_full");
        assert!(w.cam.writes().is_empty());
        assert_eq!(dir_files(&w), before);
    }

    #[test]
    fn an_existing_restore_point_file_is_never_overwritten() {
        let w = world();
        let f = fixture(&w);
        let r = apply_planned(&w, &f).unwrap();
        let rp_path = w.dir.path().join(&r.restore_point.unwrap().file);
        // Put the state back and try the same second again: same timestamp, same file name.
        for (n, c) in &w.targets {
            let _ = n;
            w.cam.write_capability(*c, 0, &json!(1)).unwrap();
        }
        let before = std::fs::read(&rp_path).unwrap();
        let p = plan_of(&w, &f);
        let writes = w.cam.writes().len();
        let e = run(&w, &f, &p.digest, &p.summary, &mut Vec::new()).unwrap_err();
        assert!(matches!(e, RestoreApplyError::RestorePointWrite(_)), "{e:?}");
        assert_eq!(e.tag(), "restore_point_write_failed");
        assert_eq!(std::fs::read(&rp_path).unwrap(), before);
        assert_eq!(w.cam.writes().len(), writes);
    }

    // ── the undo of a restore is a restore of its restore point ──────────

    #[test]
    fn restoring_the_restore_point_with_the_undo_set_puts_the_body_back() {
        let w = world();
        let f = fixture(&w);
        let before: Vec<Value> = w.targets.iter().map(|(_, c)| w.cam.value(*c).unwrap()).collect();
        let r = apply_planned(&w, &f).unwrap();
        let rp = r.restore_point.clone().unwrap();
        let bytes = std::fs::read(w.dir.path().join(&rp.file)).unwrap();
        let source = Snapshot::load_from_file(w.dir.path().join(&rp.file)).unwrap();
        let undo_only: BTreeSet<String> = r.undo_set.iter().cloned().collect();
        let uf = Fixture { source, bytes, target: body(), only: undo_only };
        let up = plan_of(&w, &uf);
        assert_eq!(up.would_change, 4);
        let mut w2_phases = Vec::new();
        let ur = apply_restore(
            &w.cam,
            &RestoreApplyRequest {
                plan: input(&uf, Some(&uf.only)),
                snapshots_dir: w.dir.path(),
                name_lookup: &w.names,
                expected_digest: &up.digest,
                expected_summary: &up.summary,
                captured_at: "2026-10-27T17:27:00.000001Z".into(),
            },
            &mut |p| w2_phases.push(p.to_string()),
        )
        .unwrap();
        assert_eq!(ur.status, ApplyStatus::AppliedVerified);
        let after: Vec<Value> = w.targets.iter().map(|(_, c)| w.cam.value(*c).unwrap()).collect();
        assert_eq!(after, before);
        assert_ne!(ur.restore_point.unwrap().file, rp.file);
    }
}

//! `restore_plan` and `restore_apply`: put a body back to a stored snapshot,
//! with a restore point first and a read-back after (docs/mcp-plan.md §4, §5.6).
//!
//! The same two-step shape as the transplant: a plan carries a digest and a
//! summary, apply must echo both, recomputes the plan and refuses a stale one.
//! Differences:
//!
//! - the snapshot's model must be the target's, and for the MCP its serial too (copying onto
//!   another model is a transplant). Refused as `serial_mismatch` / `model_mismatch`. The CLI's
//!   `restore --serial` accepts another body of the same model ([`BodyPolicy::SameModel`]); a
//!   snapshot of another body then leaves out the settings that read differently per body.
//! - the plan reads each property it would write, so the digest carries the
//!   unchanged/changed bit; a retry after success is stale (same digest) or, planned afresh, a
//!   no-op that makes no second restore point.
//! - the verdict comes from `restore::verify`: what was *written* is compared
//!   with the snapshot after a read-back. A write the body accepted and
//!   ignored (F18) is `applied_unverified`, never `applied_verified`.
//! - the undo of a restore is a restore of its own restore point
//!   (`mcp-restore-pre-restore-<ts>`), `only` = the report's `undo_set`.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::ops::apply::{ApplyError, ApplyStatus, MadeRestorePoint, RestorePoint, WriteError, make_restore_point, require_confirmation, took_effect};
use crate::ops::baseline::GatePolicy;
use crate::ops::plan::{NOT_TRANSFERABLE, hex, property_count_phrase};
use crate::ops::plan::NotTransferable;
use crate::ops::port::CameraPort;
use crate::ops::readback::{SideEffects, side_effects};
use crate::ops::restore::{
    Counts, RestoreRun, Source, StepOutcome, StepResult, Verdict, Verification, coherent_for_write, execute_from, undo_set, verify_with,
};
use crate::ops::worker::{PHASE_PLAN_VERIFIED, PHASE_READ_BACK, PHASE_RESTORE_POINT_WRITTEN, PHASE_WRITING};
use crate::ops::retention::RestoreOp;
use crate::snapshot::{Camera, Snapshot};

/// Format 2: what a step's tag means changed (`NeverCopied` depends on whose snapshot it is, and
/// `range_scale_differs` is a new tag), so a digest made by an older binary is not comparable.
pub const DIGEST_FORMAT: u32 = 2;

pub struct RestorePlanInput<'a> {
    pub source: &'a Snapshot,
    /// The file name the snapshot was loaded from, shown in the summary.
    pub source_name: &'a str,
    /// The source file exactly as stored; hashed, never re-serialised.
    pub source_bytes: &'a [u8],
    pub target: &'a Camera,
    /// Resolved property names (see `restore::resolve_only`), or all of them.
    pub only: Option<&'a BTreeSet<String>>,
    pub body_policy: BodyPolicy,
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

/// Which body a snapshot may be restored onto.
///
/// `SameBody` (the MCP): the snapshot's serial and model must both be the target's; copying onto
/// another body is a transplant. `SameModel` (the CLI's `restore --serial`): the model must match,
/// the serial may differ, which is how one body's settings are put on a second body of the same
/// model. A model mismatch is refused under both; use `transplant` for that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BodyPolicy {
    #[default]
    SameBody,
    SameModel,
}

fn same_body(source: &Snapshot, target: &Camera, policy: BodyPolicy) -> Result<(), Refusal> {
    if policy == BodyPolicy::SameBody && source.camera.serial != target.serial {
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

/// The array in a serialised [`RestorePlanReport`] a client pages through (see `plan::PLAN_LIST_KEY`).
pub const RESTORE_PLAN_LIST_KEY: &str = "steps";

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
    scope: Source,
) -> (Vec<PlanStep>, RestoreRun) {
    let run = execute_from(port, &source.properties, only, true, scope);
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

/// Whether a step would change the body or might: the one definition behind a plan's `would_change`
/// count and the front ends that read the plan's JSON (`is_would_write`: its outcome tag is
/// `would_write`; `changes`: its `changes` value, null for a live value that could not be read).
pub fn step_would_change(is_would_write: bool, changes: Option<bool>) -> bool {
    crate::ops::plan::planned_would_change(is_would_write.then_some(changes))
}

fn count_changes(steps: &[PlanStep]) -> usize {
    steps.iter().filter(|s| step_would_change(s.outcome == StepOutcome::WouldWrite, s.changes)).count()
}

/// The line a human is shown and the caller must echo to apply.
pub fn summary_text(target: &Camera, source_name: &str, would_change: usize, only: Option<&BTreeSet<String>>) -> String {
    let scope = match only {
        None => "all".to_string(),
        Some(set) => set.iter().cloned().collect::<Vec<_>>().join(", "),
    };
    format!(
        "{} {}: restore from {source_name}: {} would change; only: {scope}",
        target.model,
        target.serial,
        property_count_phrase(would_change),
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

/// Dry-run a restore and build the plan. Reads the body (each property it would
/// write, once); writes nothing.
pub fn build_restore_plan<P: CameraPort + ?Sized>(
    port: &P,
    input: &RestorePlanInput<'_>,
) -> Result<RestorePlanReport, Refusal> {
    same_body(input.source, input.target, input.body_policy)?;
    let (steps, run) = plan_steps(port, input.source, input.only, Source::of(&input.source.camera.serial, &input.target.serial));
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
    pub gate_policy: GatePolicy,
}

#[derive(Debug)]
pub enum RestoreApplyError {
    /// The snapshot is not of this body (or model).
    Refused(Refusal),
    /// Everything a restore shares with a transplant before it writes: the echoed digest and
    /// summary, a stale plan, the restore point and the baseline gate. One definition
    /// (`apply::ApplyError`), so the two cannot word the same refusal differently.
    Apply(ApplyError),
}

impl From<ApplyError> for RestoreApplyError {
    fn from(e: ApplyError) -> Self {
        RestoreApplyError::Apply(e)
    }
}

impl RestoreApplyError {
    pub fn tag(&self) -> &'static str {
        match self {
            RestoreApplyError::Refused(r) => r.tag(),
            RestoreApplyError::Apply(e) => e.tag(),
        }
    }
}

impl std::fmt::Display for RestoreApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestoreApplyError::Refused(r) => write!(f, "{r}"),
            RestoreApplyError::Apply(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RestoreApplyError {}

#[derive(Debug, Serialize)]
pub struct RestoreApplyReport {
    pub status: ApplyStatus,
    pub no_op: bool,
    pub plan_digest: String,
    pub restore_point: Option<RestorePoint>,
    /// The restore point failed or could not be judged against the baseline and the caller allowed
    /// that (`GatePolicy::AllowPartial`). Always false under `Strict`.
    pub restore_point_gate_waived: bool,
    /// Properties actually written, in name order.
    pub written: Vec<String>,
    pub write_errors: Vec<WriteError>,
    /// Named by `only` but not written.
    pub unrestored: Vec<(String, &'static str)>,
    /// Tallies over the whole run (properties not selected are counted, not listed in `steps`).
    pub counts: Counts,
    pub steps: Vec<StepResult>,
    pub verification: Option<Verification>,
    /// What changed without being written (vs the restore point). Absent if there was no complete read-back.
    pub side_effects: Option<SideEffects>,
    /// What `restore_apply(restore_point, only=...)` needs to undo this: written + side effects,
    /// each only if the restore point holds it and the body will take a write of it. Always
    /// accepted as `only`.
    pub undo_set: Vec<String>,
    /// Touched or changed properties an undo cannot put back; not in `undo_set`.
    pub undo_not_restorable: Vec<String>,
}

/// Run the restore. `progress` is told `plan_verified`, `restore_point_written`,
/// `writing` (the last point at which cancelling is safe) and `read_back`.
pub fn apply_restore<P: CameraPort + ?Sized>(
    port: &P,
    req: &RestoreApplyRequest<'_>,
    progress: &mut dyn FnMut(&str),
) -> Result<RestoreApplyReport, RestoreApplyError> {
    require_confirmation(req.expected_digest, req.expected_summary)?;
    same_body(req.plan.source, req.plan.target, req.plan.body_policy).map_err(RestoreApplyError::Refused)?;

    // 1. Recompute the plan on the live body and compare.
    // A snapshot of another body does not carry the settings that read differently per body.
    let scope = Source::of(&req.plan.source.camera.serial, &req.plan.target.serial);
    let (steps_now, now_run) = plan_steps(port, req.plan.source, req.plan.only, scope);
    let now_digest = digest(req.plan.source_bytes, &req.plan.target.serial, req.plan.only, &steps_now);
    if now_digest != req.expected_digest.trim() {
        return Err(ApplyError::StalePlan { what: "digest" }.into());
    }
    let n_change = count_changes(&steps_now);
    if summary_text(req.plan.target, req.plan.source_name, n_change, req.plan.only) != req.expected_summary.trim() {
        return Err(ApplyError::StalePlan { what: "summary" }.into());
    }
    progress(PHASE_PLAN_VERIFIED);

    // 2. Nothing to change: say so, touch nothing. A targeted restore that cannot do
    //    what was named is a failure, not a quiet success.
    //    Writing a value the body already holds is not a change: a retry after success
    //    lands here instead of making another restore point.
    if n_change == 0 {
        let dry = now_run;
        let unrestored = if req.plan.only.is_some() { dry.unrestored() } else { Vec::new() };
        // Nothing to write, but a range left alone because its scale changed is not "verified".
        let status = if !unrestored.is_empty() {
            ApplyStatus::Failed
        } else if dry.counts().skipped_scale > 0 {
            ApplyStatus::AppliedUnverified
        } else {
            ApplyStatus::AppliedVerified
        };
        return Ok(RestoreApplyReport {
            status,
            no_op: true,
            plan_digest: now_digest,
            restore_point: None,
            restore_point_gate_waived: false,
            written: Vec::new(),
            write_errors: Vec::new(),
            unrestored,
            counts: dry.counts(),
            steps: dry.steps.into_iter().filter(|s| s.outcome != StepOutcome::NotSelected).collect(),
            verification: None,
            side_effects: None,
            undo_set: Vec::new(),
            undo_not_restorable: Vec::new(),
        });
    }

    // 3. The restore point, before any write (the same sequence, and the same refusals, as a transplant).
    let MadeRestorePoint { snapshot: restore_point, file: rp_file, label, gate_waived } = make_restore_point(
        port,
        req.plan.target,
        RestoreOp::PreRestore,
        req.name_lookup,
        &req.captured_at,
        req.snapshots_dir,
        req.gate_policy,
    )?;
    progress(PHASE_RESTORE_POINT_WRITTEN);

    // 4. The writes. From here on there is always a report.
    progress(PHASE_WRITING);
    let run = execute_from(port, &req.plan.source.properties, req.plan.only, false, scope);
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
    progress(PHASE_READ_BACK);
    let verification =
        verify_with(port, req.name_lookup, req.plan.target, req.plan.source, &written, &req.captured_at, req.gate_policy);
    // The comparison can only fail on a model mismatch, which two snapshots of one Camera cannot
    // have; if it ever did, the report must not read as a clean verified restore.
    let effects_result = verification.readback.as_ref().map(|after| side_effects(&restore_point, after, &written));
    let effects_failed = matches!(effects_result, Some(Err(_)));
    let effects = effects_result.and_then(|r| r.ok());

    // Writes were attempted and the body rejected every one of them by kind: nothing was written,
    // which the read-back reports as "nothing to check". A few rejected kinds among written
    // properties are routine on a full restore; all of them is not.
    let all_rejected = written.is_empty()
        && run.steps.iter().any(|s| matches!(s.outcome, StepOutcome::UnsupportedWriteType));

    // A settable range the body's scale no longer matches was left alone: that is a property the
    // restore did not put back, so on a full restore (where it is not named, hence not in
    // `unrestored`) the result cannot be called verified.
    let scale_skipped = run.counts().skipped_scale > 0;
    let status = if !write_errors.is_empty() || !unrestored.is_empty() || all_rejected {
        ApplyStatus::Failed
    } else if scale_skipped {
        ApplyStatus::AppliedUnverified
    } else {
        match verification.verdict {
            Verdict::Verified | Verdict::NothingWritten if effects_failed => ApplyStatus::AppliedUnverified,
            Verdict::Verified | Verdict::NothingWritten => ApplyStatus::AppliedVerified,
            Verdict::ResidualFound | Verdict::ReadbackIncomplete | Verdict::Unjudged => ApplyStatus::AppliedUnverified,
        }
    };

    // What an undo can put back: what was written, what may have landed (a write that errored can
    // still have changed the body), and what changed as a side effect, limited to what the restore
    // point holds and the body will take.
    let touched: BTreeSet<String> = written.iter().cloned().chain(write_errors.iter().map(|e| e.name.clone())).collect();
    let undo = undo_set(&restore_point, port.capabilities(), &touched, effects.as_ref(), verification.readback.as_ref());
    Ok(RestoreApplyReport {
        status,
        no_op: false,
        plan_digest: now_digest,
        restore_point: Some(RestorePoint { file: rp_file, label }),
        restore_point_gate_waived: gate_waived,
        written: written.into_iter().collect(),
        write_errors,
        unrestored,
        counts: run.counts(),
        steps: run.steps.into_iter().filter(|s| s.outcome != StepOutcome::NotSelected).collect(),
        verification: Some(verification),
        side_effects: effects,
        undo_set: undo.names,
        undo_not_restorable: undo.not_restorable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Transport;
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
            // The two string targets hold strings, the number a number, the range its range: the real
            // write refuses any other shape for a kind.
            let live = match targets.iter().position(|(_, code)| *code == c.code) {
                Some(3) => stepped(613),
                Some(0) | Some(1) => json!("old"),
                _ => json!(1),
            };
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
        RestorePlanInput { source: &f.source, source_name: "Z_9_base.json", source_bytes: &f.bytes, target: &f.target, only, body_policy: BodyPolicy::SameBody }
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
                gate_policy: GatePolicy::Strict,
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
    fn the_list_a_client_pages_is_a_real_array_in_the_report() {
        let w = world();
        let f = fixture(&w);
        let v = serde_json::to_value(plan_of(&w, &f)).unwrap();
        assert!(v[RESTORE_PLAN_LIST_KEY].is_array(), "{RESTORE_PLAN_LIST_KEY} must name an array in the serialised plan");
    }

    #[test]
    fn golden_restore_digest_for_a_fixed_plan() {
        // Pins what the digest covers and how it is written, so a change to either is deliberate and
        // bumps DIGEST_FORMAT (the transplant digest has the same kind of test in plan.rs).
        let w = world();
        let f = fixture(&w);
        let p = plan_of(&w, &f);
        assert_eq!(p.digest, "56a9c7290ccd6c683d6b535154ee725d6d511f0a7f2108b2d8d9896a3ca6c0ab");
    }

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
            assert!(matches!(e, RestoreApplyError::Apply(ApplyError::MissingConfirmation { which: w2 }) if w2 == which), "{which}: {e:?}");
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
        assert!(matches!(e, RestoreApplyError::Apply(ApplyError::StalePlan { what: "digest" })), "{e:?}");
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
        assert!(matches!(e, RestoreApplyError::Apply(ApplyError::StalePlan { what: "summary" })), "{e:?}");
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
                gate_policy: GatePolicy::Strict,
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
    fn a_full_restore_that_leaves_a_range_alone_because_its_scale_changed_is_not_verified() {
        // The body's range is on a 751-step scale now, the snapshot's on 1225: the range is skipped
        // (range_scale_differs). Not named, so not "unrestored"; but a settable property was not put
        // back, so the result is applied_unverified, with the count saying why.
        let now = json!({"value": 0.0, "default": 0.0, "value_index": 100, "default_index": 0, "lower": 2500.0, "upper": 10000.0, "steps": 751});
        let w = world_with(|c, t| c.with(t[3].1, RW, now.clone()), 0);
        let f = fixture(&w);
        let p = build_restore_plan(&w.cam, &input(&f, None)).unwrap();
        let r = run_all(&w, &f, &p).unwrap();
        assert_eq!(r.counts.skipped_scale, 1);
        assert_eq!(r.status, ApplyStatus::AppliedUnverified);
        assert_eq!(w.cam.value(w.targets[3].1).unwrap()["value_index"], json!(100), "left alone");
        // Named with --only, it is a failure instead.
        let only: BTreeSet<String> = [w.targets[3].0.clone()].into();
        let named = RestorePlanInput { only: Some(&only), ..input(&f, None) };
        let p2 = build_restore_plan(&w.cam, &named).unwrap();
        let r2 = apply_restore(
            &w.cam,
            &RestoreApplyRequest {
                plan: RestorePlanInput { only: Some(&only), ..input(&f, None) },
                snapshots_dir: w.dir.path(),
                name_lookup: &w.names,
                expected_digest: &p2.digest,
                expected_summary: &p2.summary,
                captured_at: "2026-10-27T17:27:00.000001Z".into(),
                gate_policy: GatePolicy::Strict,
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(r2.status, ApplyStatus::Failed);
        assert_eq!(r2.unrestored, [(w.targets[3].0.clone(), "range_scale_differs")]);
    }

    #[test]
    fn a_no_op_restore_that_left_a_range_alone_because_its_scale_changed_is_not_verified() {
        // Everything else already holds its stored value, so there is nothing to write; the one range
        // whose scale changed was left alone. "Nothing to do, verified" would be false.
        let now = json!({"value": 0.0, "default": 0.0, "value_index": 100, "default_index": 0, "lower": 2500.0, "upper": 10000.0, "steps": 751});
        let w = world_with(|c, t| c.with(t[3].1, RW, now.clone()), 0);
        let mut f = fixture(&w);
        // Make the stored strings and number equal what the body holds.
        for (i, (name, code)) in w.targets.iter().enumerate().take(3) {
            let held = if i < 2 { json!("old") } else { json!(1) };
            f.source.insert(name.clone(), *code, held);
        }
        f.bytes = f.source.to_pretty_json().unwrap().into_bytes();
        let p = build_restore_plan(&w.cam, &input(&f, None)).unwrap();
        assert_eq!(p.would_change, 0, "{:?}", p.steps.iter().filter(|s| s.changes != Some(false)).collect::<Vec<_>>());
        let r = run_all(&w, &f, &p).unwrap();
        assert!(r.no_op && r.written.is_empty() && r.restore_point.is_none());
        assert_eq!(r.counts.skipped_scale, 1);
        assert_eq!(r.status, ApplyStatus::AppliedUnverified);
    }

    #[test]
    fn a_write_that_errored_may_have_landed_and_stays_in_the_undo_set() {
        // The sibling of apply's rule: an errored write can still have changed the body, and the
        // restore point is the way back, so the name is listed (and accepted by the undo).
        let w = world_with(|c, t| c.failing_write(t[0].1, Failure::Call(-125)), 0);
        let f = fixture(&w);
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.write_errors.len(), 1);
        assert!(r.undo_set.contains(&w.targets[0].0), "{:?}", r.undo_set);
        assert!(r.undo_not_restorable.is_empty(), "{:?}", r.undo_not_restorable);
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
    fn a_restore_whose_every_write_was_rejected_by_kind_is_not_verified() {
        // Nothing written, so the read-back verdict is `nothing_written`; but writes were attempted
        // and the body rejected all of them, which is not "verified".
        let b = baseline_for("Z 9").unwrap();
        let w = world_with(|c, _| b.readable.iter().fold(c, |c, cap| c.failing_write(cap.code, Failure::UnsupportedWrite)), 0);
        let f = fixture(&w);
        let p = build_restore_plan(&w.cam, &input(&f, None)).unwrap();
        let r = run_all(&w, &f, &p).unwrap();
        assert!(r.written.is_empty());
        assert!(r.counts.skipped_type > 0);
        assert_eq!(r.status, ApplyStatus::Failed);
    }

    #[test]
    fn an_untargeted_restore_with_a_few_unsupported_kinds_is_still_verified() {
        // The datetime and array capabilities are rejected by kind on every full restore (routine,
        // reported in `counts.skipped_type`); the rest was written, so that is not a failure.
        let w = world_with(|c, t| c.failing_write(t[0].1, Failure::UnsupportedWrite), 0);
        let f = fixture(&w);
        let p = build_restore_plan(&w.cam, &input(&f, None)).unwrap();
        let r = run_all(&w, &f, &p).unwrap();
        assert!(r.write_errors.is_empty());
        assert_eq!(r.counts.skipped_type, 1);
        assert_eq!(r.status, ApplyStatus::AppliedVerified);
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
        assert!(matches!(e, RestoreApplyError::Apply(ApplyError::RestorePointIncomplete { .. })), "{e:?}");
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
        assert!(matches!(e, RestoreApplyError::Apply(ApplyError::NothingReadable { .. })), "{e:?}");
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
                gate_policy: GatePolicy::Strict,
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
        assert!(matches!(e, RestoreApplyError::Apply(ApplyError::RestorePointCap(_))), "{e:?}");
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
        for (i, (_, c)) in w.targets.iter().enumerate() {
            let before = match i {
                3 => stepped(613),
                0 | 1 => json!("old"),
                _ => json!(1),
            };
            w.cam.write_capability(*c, 0, &before).unwrap();
        }
        let before = std::fs::read(&rp_path).unwrap();
        let p = plan_of(&w, &f);
        let writes = w.cam.writes().len();
        let e = run(&w, &f, &p.digest, &p.summary, &mut Vec::new()).unwrap_err();
        assert!(matches!(e, RestoreApplyError::Apply(ApplyError::RestorePointWrite(_))), "{e:?}");
        assert_eq!(e.tag(), "restore_point_write_failed");
        assert_eq!(std::fs::read(&rp_path).unwrap(), before);
        assert_eq!(w.cam.writes().len(), writes);
    }

    // ── every way the apply is refused leaves the body untouched ─────────

    /// `apply_restore` returns `Err` only before its first write (the CLI maps "no report" to exit
    /// 6, "nothing was written"). The match names every `ApplyError` variant: a new one fails to
    /// compile here, and its refusal then has to be added to the table below.
    #[test]
    fn every_refusal_the_apply_can_return_happens_before_the_first_write() {
        fn named(e: &RestoreApplyError) -> &'static str {
            match e {
                RestoreApplyError::Refused(_) => "refused",
                RestoreApplyError::Apply(a) => match a {
                    ApplyError::MissingConfirmation { .. } => "missing_confirmation",
                    ApplyError::StalePlan { .. } => "stale_plan",
                    ApplyError::List(_) => "list",
                    ApplyError::RestorePointCap(_) => "cap",
                    // Built from constants by the library: no input of a restore can make it fail.
                    ApplyError::Label(_) => "label",
                    ApplyError::NoBaseline(_) => "no_baseline",
                    ApplyError::NothingReadable { .. } => "nothing_readable",
                    ApplyError::RestorePointIncomplete { .. } => "incomplete",
                    ApplyError::RestorePointWrite(_) => "write",
                },
            }
        }
        let seen = std::cell::RefCell::new(BTreeSet::new());
        let check = |w: &World, e: RestoreApplyError, label: &'static str| {
            assert_eq!(named(&e), label, "{e:?}");
            assert!(w.cam.writes().is_empty(), "{label}: the body was written before the refusal");
            seen.borrow_mut().insert(label);
        };
        // refused: another body
        let w = world();
        let mut f = fixture(&w);
        let p = plan_of(&w, &f);
        f.target.serial = "T2".into();
        check(&w, run(&w, &f, &p.digest, &p.summary, &mut Vec::new()).unwrap_err(), "refused");
        // the echo
        let f = fixture(&w);
        check(&w, run(&w, &f, "", &p.summary, &mut Vec::new()).unwrap_err(), "missing_confirmation");
        check(&w, run(&w, &f, "not-the-digest", &p.summary, &mut Vec::new()).unwrap_err(), "stale_plan");
        check(&w, run(&w, &f, &p.digest, "not the summary", &mut Vec::new()).unwrap_err(), "stale_plan");
        // the snapshots directory cannot be listed (a file where the directory should be)
        let not_a_dir = w.dir.path().join("a-file");
        std::fs::write(&not_a_dir, b"x").unwrap();
        let e = apply_restore(
            &w.cam,
            &RestoreApplyRequest {
                plan: input(&f, Some(&f.only)),
                snapshots_dir: &not_a_dir,
                name_lookup: &w.names,
                expected_digest: &p.digest,
                expected_summary: &p.summary,
                captured_at: AT.into(),
                gate_policy: GatePolicy::Strict,
            },
            &mut |_| {},
        )
        .unwrap_err();
        assert!(matches!(named(&e), "list" | "write"), "{e:?}");
        assert!(w.cam.writes().is_empty());
        seen.borrow_mut().insert("list_or_write");
        // an incomplete body
        let w = world_with(|c, _| c, 20);
        check(&w, apply_planned(&w, &fixture(&w)).unwrap_err(), "incomplete");
        // the cap
        let w = world();
        let f = fixture(&w);
        for i in 0..MAX_RESTORE_POINTS_PER_CAMERA {
            let mut s = Snapshot::new(f.target.clone(), Transport::Usb, format!("2026-01-01T00:00:00.{i:03}Z"));
            s.label = Some("mcp-restore-pre-restore-old".into());
            s.insert("kNkMAIDCapability_A", 1, json!(1));
            s.save_to_file(w.dir.path().join(format!("rp-{i}.json"))).unwrap();
        }
        check(&w, apply_planned(&w, &f).unwrap_err(), "cap");
        assert_eq!(seen.borrow().len(), 6, "{:?}", seen.borrow());
    }

    // ── the CLI's policies ───────────────────────────────────────────────

    fn run_policy(w: &World, f: &Fixture, body: BodyPolicy, gate: GatePolicy) -> Result<RestoreApplyReport, RestoreApplyError> {
        let plan_input = RestorePlanInput { body_policy: body, ..input(f, Some(&f.only)) };
        let p = build_restore_plan(&w.cam, &plan_input).map_err(RestoreApplyError::Refused)?;
        apply_restore(
            &w.cam,
            &RestoreApplyRequest {
                plan: RestorePlanInput { body_policy: body, ..input(f, Some(&f.only)) },
                snapshots_dir: w.dir.path(),
                name_lookup: &w.names,
                expected_digest: &p.digest,
                expected_summary: &p.summary,
                captured_at: AT.into(),
                gate_policy: gate,
            },
            &mut |_| {},
        )
    }

    #[test]
    fn same_model_lets_a_snapshot_of_another_body_be_restored_but_same_body_does_not() {
        let w = world();
        let mut f = fixture(&w);
        f.source.camera.serial = "OTHER".into();
        let e = run_policy(&w, &f, BodyPolicy::SameBody, GatePolicy::Strict).unwrap_err();
        assert_eq!(e.tag(), "serial_mismatch");
        assert!(w.cam.writes().is_empty());
        let r = run_policy(&w, &f, BodyPolicy::SameModel, GatePolicy::Strict).unwrap();
        assert_eq!(r.status, ApplyStatus::AppliedVerified);
        assert_eq!(r.written.len(), 4);
    }

    #[test]
    fn a_snapshot_of_another_body_does_not_restore_what_reads_differently_per_body() {
        // ShootingLimit (33086) is never translated between bodies: a Z6III at infinity reads index 1,
        // which on a second Z6III is 1 shot per burst. Restoring the body's own snapshot brings it
        // back; restoring another body's snapshot (CLI `restore --serial`) must not.
        let step_for = |p: &RestorePlanReport| p.steps.iter().find(|s| s.code == 33086).map(|s| s.outcome.clone());
        let w = world();
        let mut f = fixture(&w);
        let own = build_restore_plan(&w.cam, &input(&f, None)).unwrap();
        assert_eq!(step_for(&own), Some(StepOutcome::WouldWrite), "the body's own snapshot restores it");
        f.source.camera.serial = "OTHER".into();
        let other = build_restore_plan(&w.cam, &RestorePlanInput { body_policy: BodyPolicy::SameModel, ..input(&f, None) }).unwrap();
        assert_eq!(step_for(&other), Some(StepOutcome::Skipped(crate::ops::restore::RestoreSkip::NeverCopied)));
        assert_ne!(own.digest, other.digest);
    }

    #[test]
    fn applying_another_bodys_snapshot_never_writes_a_never_translated_setting() {
        let w = world();
        let mut f = fixture(&w);
        f.source.camera.serial = "OTHER".into();
        f.only = f.only.iter().cloned().chain(["kNkMAIDCapability_ShootingLimit".to_string()]).collect();
        let r = run_policy(&w, &f, BodyPolicy::SameModel, GatePolicy::Strict).unwrap();
        assert!(w.cam.writes().iter().all(|(code, _)| *code != 33086), "ShootingLimit was written");
        assert_eq!(r.status, ApplyStatus::Failed, "a named property that cannot be restored is a failure");
        assert!(r.unrestored.iter().any(|(n, why)| n.ends_with("ShootingLimit") && *why == "never_copied"));
    }

    #[test]
    fn a_model_mismatch_is_refused_under_both_body_policies() {
        let w = world();
        let mut f = fixture(&w);
        f.source.camera.model = "Z6_3".into();
        for policy in [BodyPolicy::SameBody, BodyPolicy::SameModel] {
            let e = run_policy(&w, &f, policy, GatePolicy::Strict).unwrap_err();
            assert_eq!(e.tag(), "model_mismatch", "{policy:?}");
        }
        assert!(w.cam.writes().is_empty() && dir_files(&w).is_empty());
    }

    #[test]
    fn allow_partial_goes_ahead_with_an_incomplete_restore_point_and_says_so() {
        let w = world_with(|c, _| c, 178);
        let f = fixture(&w);
        assert!(run_policy(&w, &f, BodyPolicy::SameBody, GatePolicy::Strict).is_err());
        assert!(w.cam.writes().is_empty());
        let r = run_policy(&w, &f, BodyPolicy::SameBody, GatePolicy::AllowPartial).unwrap();
        assert!(r.restore_point_gate_waived);
        assert_eq!(r.written.len(), 4);
        assert!(r.restore_point.is_some());
        assert_eq!(r.status, ApplyStatus::AppliedUnverified);
    }

    #[test]
    fn a_passing_restore_point_is_not_marked_waived() {
        let w = world();
        let f = fixture(&w);
        let r = run_policy(&w, &f, BodyPolicy::SameBody, GatePolicy::AllowPartial).unwrap();
        assert!(!r.restore_point_gate_waived);
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
                gate_policy: GatePolicy::Strict,
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

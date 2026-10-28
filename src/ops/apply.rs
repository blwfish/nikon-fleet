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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::diff::{Diff, DiffOptions, diff};
use crate::ops::baseline::{GateOutcome, GatePolicy, MissingCapability, NoBaselineReason, gate};
use crate::ops::capture::{CaptureStats, capture};
use crate::ops::list::ListError;
use crate::ops::plan::{PlanInput, digest, summary_text, would_change};
use crate::ops::port::CameraPort;
use crate::ops::readback::{SideEffects, side_effects};
use crate::ops::restore::undo_set;
use crate::ops::worker::{PHASE_PLAN_VERIFIED, PHASE_READ_BACK, PHASE_RESTORE_POINT_WRITTEN, PHASE_WRITING};
use crate::ops::retention::{self, DataDirFull, LabelError, RestoreOp};
use crate::snapshot::{PropertyEntry, Snapshot, SnapshotError, Transport};
use crate::transplant::{CodeNameGuard, Severity, SkipReason, TransplantOutcome, numbers_equal, transplant};
use crate::value_shape::{is_range_shape, range_values_agree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyStatus {
    AppliedVerified,
    AppliedUnverified,
    Failed,
    /// Produced by the caller of the worker (the MCP supervisor), never by this module: the worker
    /// gave no result after it began writing.
    Indeterminate,
    /// Likewise: the worker gave no result before it began writing, so nothing was changed.
    NotApplied,
}

impl ApplyStatus {
    /// Every status, for tests and for anything that must name them all.
    pub const ALL: [ApplyStatus; 5] = [
        ApplyStatus::AppliedVerified,
        ApplyStatus::AppliedUnverified,
        ApplyStatus::Failed,
        ApplyStatus::Indeterminate,
        ApplyStatus::NotApplied,
    ];

    /// The closed tag clients read in `status` (the same string serde writes).
    pub fn tag(&self) -> &'static str {
        match self {
            ApplyStatus::AppliedVerified => "applied_verified",
            ApplyStatus::AppliedUnverified => "applied_unverified",
            ApplyStatus::Failed => "failed",
            ApplyStatus::Indeterminate => "indeterminate",
            ApplyStatus::NotApplied => "not_applied",
        }
    }
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
    pub gate_policy: GatePolicy,
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
    /// The restore point failed or could not be judged against the baseline and the caller
    /// allowed that (`GatePolicy::AllowPartial`). Always false under `Strict`.
    pub restore_point_gate_waived: bool,
    /// Properties actually written, in name order.
    pub written: Vec<String>,
    pub write_errors: Vec<WriteError>,
    /// Planned properties the body would not take by kind (`unsupported_write_type`): routine,
    /// as in a restore, and a failure only when nothing at all was written.
    pub rejected_by_kind: Vec<String>,
    pub outcomes: Vec<TransplantOutcome>,
    pub readback: Option<Readback>,
    /// What `restore_apply(restore_point, only=...)` needs to undo this: the written properties,
    /// any whose write may have landed, and the side effects, each only if the restore point
    /// holds it and the body will take a write of it. Always accepted as `only`.
    pub undo_set: Vec<String>,
    /// Properties this apply touched or changed that an undo cannot put back (not in the restore
    /// point, read-only, or never copied): they are not in `undo_set`.
    pub undo_not_restorable: Vec<String>,
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
pub(crate) fn took_effect(planned: &Value, got: &Value) -> bool {
    if is_range_shape(planned) {
        // A range: the same position, by the one comparator every range check uses.
        return range_values_agree(planned, got);
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

/// A restore point that has been captured, held to the baseline and stored.
pub(crate) struct MadeRestorePoint {
    pub snapshot: Snapshot,
    /// File name within the snapshots directory.
    pub file: String,
    pub label: String,
    /// It failed or could not be judged against the baseline and the caller allowed that
    /// (`GatePolicy::AllowPartial`).
    pub gate_waived: bool,
}

/// Capture the body as it is now and store it, before any write: refuse if the data directory is
/// at its restore-point cap, if nothing could be read, or (under `Strict`) if the capture is not
/// complete against the model's baseline. Shared by a transplant and a restore, so the two cannot
/// drift on what a usable restore point is or on what they say when it is not.
pub(crate) fn make_restore_point<P: CameraPort + ?Sized>(
    port: &P,
    target: &crate::snapshot::Camera,
    op: RestoreOp,
    name_lookup: &HashMap<u32, String>,
    captured_at: &str,
    snapshots_dir: &Path,
    gate_policy: GatePolicy,
) -> Result<MadeRestorePoint, ApplyError> {
    let label = retention::restore_point_label(op, &retention::compact_timestamp(captured_at)).map_err(ApplyError::Label)?;
    let counts = retention::count_for_camera(snapshots_dir, &target.model, &target.serial).map_err(ApplyError::List)?;
    retention::check_restore_point_cap(&counts).map_err(ApplyError::RestorePointCap)?;

    let mut snapshot = Snapshot::new(target.clone(), Transport::Usb, captured_at.to_string());
    snapshot.label = Some(label.clone());
    let stats = capture(port, name_lookup, &mut snapshot);
    if stats.read_ok == 0 {
        return Err(ApplyError::NothingReadable { read_err: stats.read_err });
    }
    let mut gate_waived = false;
    match (gate(&snapshot), gate_policy) {
        (GateOutcome::Passed, _) => {}
        (GateOutcome::Failed { .. } | GateOutcome::NoBaseline(_), GatePolicy::AllowPartial) => gate_waived = true,
        (GateOutcome::Failed { missing }, GatePolicy::Strict) => {
            return Err(ApplyError::RestorePointIncomplete { missing, stats });
        }
        (GateOutcome::NoBaseline(reason), GatePolicy::Strict) => return Err(ApplyError::NoBaseline(reason)),
    }
    std::fs::create_dir_all(snapshots_dir).map_err(|e| ApplyError::RestorePointWrite(SnapshotError::Io(e)))?;
    let file = snapshot.suggested_filename();
    snapshot.save_new(snapshots_dir.join(&file)).map_err(ApplyError::RestorePointWrite)?;
    Ok(MadeRestorePoint { snapshot, file, label, gate_waived })
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
    progress(PHASE_PLAN_VERIFIED);

    // 2. Nothing to change: say so, touch nothing. A copied value the body already holds is not a
    //    change (a retry after success lands here instead of making another restore point, the
    //    same rule as `restore`). But if nothing was copied *because of problems* (a read that
    //    failed, no matching option, an unreadable shape) the request did not succeed: "applied,
    //    verified" would be false.
    if n_change == 0 {
        let copies = planned.iter().any(|o| o.result.is_ok());
        // Routine skips (absent on the target, read-only) mean the two bodies simply have nothing in
        // common. Anything else (a policy refusal such as `range_unverified`, or a real problem)
        // means something the caller asked for was not copied, so nothing is "applied, verified".
        let blocked = !copies
            && planned.iter().any(|o| o.result.as_ref().err().is_some_and(|r| r.severity() != Severity::Routine));
        return Ok(ApplyReport {
            status: if blocked { ApplyStatus::Failed } else { ApplyStatus::AppliedVerified },
            no_op: true,
            plan_digest: now_digest,
            restore_point: None,
            restore_point_gate_waived: false,
            written: Vec::new(),
            write_errors: Vec::new(),
            rejected_by_kind: Vec::new(),
            outcomes: planned,
            readback: None,
            undo_set: Vec::new(),
            undo_not_restorable: Vec::new(),
        });
    }

    // 3. The restore point, before any write.
    let made = make_restore_point(
        port,
        req.plan.target,
        RestoreOp::PreTransplant,
        req.name_lookup,
        &req.captured_at,
        req.snapshots_dir,
        req.gate_policy,
    )?;
    let MadeRestorePoint { snapshot: restore_point, file: rp_file, label, gate_waived } = made;
    progress(PHASE_RESTORE_POINT_WRITTEN);

    // 4. The writes. From here on there is always a report.
    progress(PHASE_WRITING);
    // Only what the caller confirmed. The real run re-evaluates every property on the live body,
    // so a property the verified dry run skipped (a read that failed that time, a gate that did
    // not trip) must not be written now: it was never in the plan whose digest was echoed.
    let planned_names: BTreeSet<&str> = planned.iter().filter(|o| o.result.is_ok()).map(|o| o.name.as_str()).collect();
    let confirmed: BTreeMap<String, PropertyEntry> =
        props.iter().filter(|(name, _)| planned_names.contains(name.as_str())).map(|(n, p)| (n.clone(), p.clone())).collect();
    let outcomes = transplant(port, &confirmed, req.guard, req.plan.options, false);
    // The restore point and the read-back are keyed by the TARGET model's names, which can differ
    // from the source's for one code (`names_compatible`, e.g. 33200); everything compared with
    // them is keyed the same way.
    let target_name = |o: &TransplantOutcome| crate::ops::capture::property_key(req.name_lookup, o.code);
    let mut written: BTreeSet<String> = BTreeSet::new();
    let mut write_errors = Vec::new();
    let mut may_have_landed: BTreeSet<String> = BTreeSet::new();
    let mut rejected_by_kind = Vec::new();
    for o in &outcomes {
        let name = target_name(o);
        match &o.result {
            Ok(_) => {
                written.insert(name);
            }
            // The write was made and failed, or was made and could not be confirmed: the body may
            // have changed.
            Err(SkipReason::WriteFailed(msg)) => {
                may_have_landed.insert(name.clone());
                write_errors.push(WriteError { name, message: msg.clone() });
            }
            // The dry run cannot see a write being rejected, and a full restore treats a kind the
            // body will not take as routine (a few are on every run); so does this. Reported, and a
            // failure only if it left nothing written (below).
            Err(SkipReason::UnsupportedWriteType) => rejected_by_kind.push(name),
            // Every outcome here was planned as a copy. Any other reason it was not (a live read
            // failed, the live shape changed) is a planned write that did not happen, and the
            // report must say so rather than omit it. Nothing was written, so nothing to undo.
            Err(other) => {
                write_errors.push(WriteError {
                    name,
                    message: format!("planned write not performed: {} ({other})", other.tag()),
                });
            }
        }
    }

    // 5. Read back, with the same gate.
    progress(PHASE_READ_BACK);
    let mut after = Snapshot::new(req.plan.target.clone(), Transport::Usb, req.captured_at.clone());
    after.label = Some("readback".into());
    let after_stats = capture(port, req.name_lookup, &mut after);
    let after_gate = if after_stats.read_ok == 0 {
        None
    } else {
        Some(gate(&after))
    };
    // Under `AllowPartial` a read-back that cannot be judged for completeness (no baseline) is
    // still compared: the caller waived the judgement, not the comparison.
    let complete = match (&after_gate, req.gate_policy) {
        (Some(GateOutcome::Passed), _) => true,
        (Some(GateOutcome::NoBaseline(_)), GatePolicy::AllowPartial) => true,
        _ => false,
    };

    // The comparison can only fail on a model mismatch, which two snapshots of one Camera cannot
    // have; if it ever did, the read-back is not one this report can stand behind.
    let (diff_vs_rp, effects, complete) = if complete {
        match (diff(&restore_point, &after, &DiffOptions::default()), side_effects(&restore_point, &after, &written)) {
            (Ok(d), Ok(se)) => (Some(d), Some(se), true),
            _ => (None, None, false),
        }
    } else {
        (None, None, false)
    };

    let mut not_taken = Vec::new();
    let mut unverifiable = Vec::new();
    for o in &outcomes {
        let (Ok(_), Some(p)) = (&o.result, &o.planned) else { continue };
        let name = target_name(o);
        match after.properties.get(&name) {
            Some(got) if complete => {
                if !took_effect(&p.value, &got.value) {
                    not_taken.push(name);
                }
            }
            _ => unverifiable.push(name),
        }
    }

    let status = if !write_errors.is_empty() || (written.is_empty() && !rejected_by_kind.is_empty()) {
        ApplyStatus::Failed
    } else if !complete || !not_taken.is_empty() || !unverifiable.is_empty() || !rejected_by_kind.is_empty() {
        // A planned change that the body would not take by kind was never judged by the read-back
        // (it was not written), so the apply cannot be called verified.
        ApplyStatus::AppliedUnverified
    } else {
        ApplyStatus::AppliedVerified
    };

    // What an undo can put back: what was written, what may have landed, and what changed as a
    // side effect, limited to what the restore point holds and the body will take.
    let touched: BTreeSet<String> = written.union(&may_have_landed).cloned().collect();
    let undo = undo_set(&restore_point, port.capabilities(), &touched, effects.as_ref(), complete.then_some(&after));

    Ok(ApplyReport {
        status,
        no_op: false,
        plan_digest: now_digest,
        restore_point: Some(RestorePoint { file: rp_file, label }),
        restore_point_gate_waived: gate_waived,
        written: written.into_iter().collect(),
        write_errors,
        rejected_by_kind,
        outcomes,
        readback: Some(Readback {
            gate: after_gate,
            stats: after_stats,
            diff: diff_vs_rp,
            side_effects: effects,
            not_taken,
            unverifiable,
        }),
        undo_set: undo.names,
        undo_not_restorable: undo.not_restorable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::baseline::baseline_for;
    use crate::ops::plan::{PlanReport, build_plan};
    use crate::ops::port::{FakeCamera, Failure};
    use crate::sdk::{OP_GET, OP_SET};
    use crate::snapshot::Camera;
    use crate::test_support::parse_config;
    use crate::transplant::TransplantOptions;
    use serde_json::json;
    use tempfile::TempDir;

    const RW: u32 = OP_GET | OP_SET;
    const AT: &str = "2026-10-27T17:26:00.688812Z";
    /// A range that is on the verified-range table for both models the fixture uses (Z6_3 and Z 9).
    const WB_TUNE_AUTO: (&str, u32) = ("kNkMAIDCapability_WBTuneAuto", 33049);

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
        // Plan targets: three string properties and one numeric, from among the readable ones. A
        // string target holds a string on the body: the real write refuses a string for an integer.
        let codes: Vec<(String, u32)> = b.readable.iter().skip(6).take(4).map(|c| (c.name.clone(), c.code)).collect();
        for c in &b.readable[..b.readable.len() - drop_tail] {
            let held = codes.iter().position(|(_, code)| *code == c.code).map_or(json!(1), initial);
            cam = cam.with(c.code, RW, held);
            names.insert(c.code, c.name.clone());
        }
        World { cam: cam_builder(cam, &codes), names, codes, dir: TempDir::new().unwrap() }
    }

    fn world() -> World {
        world_with(|c, _| c, 0)
    }

    /// What plan target `i` (see `world_with`) holds before an apply: strings for the first three,
    /// a number for the fourth.
    fn initial(i: usize) -> serde_json::Value {
        if i < 3 { json!("old") } else { json!(1) }
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
        run_with(w, f, digest, summary, phases, GatePolicy::Strict)
    }

    fn run_with(
        w: &World,
        f: &Fixture,
        digest: &str,
        summary: &str,
        phases: &mut Vec<String>,
        gate_policy: GatePolicy,
    ) -> Result<ApplyReport, ApplyError> {
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
                gate_policy,
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
        for (i, (_, code)) in w.codes.iter().enumerate() {
            let p = saved.properties.values().find(|p| p.code == *code).unwrap();
            assert_eq!(p.value, initial(i), "the value before the apply, not after");
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
    fn a_plan_that_copied_nothing_because_of_a_problem_is_a_failure_not_a_verified_no_op() {
        // The source range does not say its scale, so nothing can be copied: `malformed_source`, a
        // skip that needs attention. Reporting "applied, verified" would be false.
        let st = json!({"value": 0.0, "default": 0.0, "value_index": 612, "default_index": 612, "lower": 0.0, "upper": 1224.0, "steps": 1225});
        let (name, code) = (WB_TUNE_AUTO.0.to_string(), WB_TUNE_AUTO.1);
        let w = world_with(|c, _| c.with(code, RW, st.clone()), 0);
        let mut f = fixture(&w, true);
        f.source.properties.clear();
        f.source.insert(name, code, json!({"value": 5.0, "lower": 0.0, "upper": 1224.0}));
        f.bytes = f.source.to_pretty_json().unwrap().into_bytes();
        let r = apply_planned(&w, &f).unwrap();
        assert!(r.no_op);
        assert_eq!(r.status, ApplyStatus::Failed);
        assert!(w.cam.writes().is_empty() && dir_files(&w).is_empty(), "nothing was touched");
    }

    #[test]
    fn a_plan_that_copied_nothing_because_policy_refused_it_is_not_a_verified_no_op_either() {
        // The only property is a numeric literal that was not opted in (`literal_unconfirmed`, a
        // policy skip): nothing was copied, and the caller asked for it. Not "applied, verified".
        let w = world();
        let mut f = fixture(&w, false);
        let (name, code) = w.codes[3].clone();
        let value = f.source.properties[&name].value.clone();
        f.source.properties.clear();
        f.source.insert(name, code, value);
        f.bytes = f.source.to_pretty_json().unwrap().into_bytes();
        let r = apply_planned(&w, &f).unwrap();
        assert!(r.no_op);
        assert!(matches!(r.outcomes[0].result, Err(SkipReason::LiteralUnconfirmed { .. })), "{:?}", r.outcomes[0].result);
        assert_eq!(r.status, ApplyStatus::Failed);
        assert!(w.cam.writes().is_empty());
    }

    #[test]
    fn a_retry_with_a_fresh_plan_after_success_is_a_no_op_when_every_copied_value_is_known_unchanged() {
        // The numeric property is read, so a fresh plan after success says `0 properties would
        // change`; like a restore, that is a no-op: no second restore point, no rewrite. (Strings and
        // booleans are written without a read and always count as changes: a plan's digest does not
        // depend on the current value of a property that can vary on its own.)
        let w = world();
        let mut f = fixture(&w, true);
        let (name, code) = w.codes[3].clone();
        let value = f.source.properties[&name].value.clone();
        f.source.properties.clear();
        f.source.insert(name, code, value);
        f.bytes = f.source.to_pretty_json().unwrap().into_bytes();
        let first = apply_planned(&w, &f).unwrap();
        assert!(!first.no_op && first.written.len() == 1, "{:?}", first.written);
        let writes = w.cam.writes().len();
        let again = apply_planned(&w, &f).unwrap();
        assert!(again.no_op && again.written.is_empty());
        assert_eq!(again.status, ApplyStatus::AppliedVerified);
        assert_eq!(w.cam.writes().len(), writes, "no rewrite");
        assert_eq!(dir_files(&w).len(), 1, "one restore point, not two");
    }

    #[test]
    fn applying_the_same_plan_twice_is_refused_as_stale_and_writes_nothing() {
        // A client that retries the SAME plan after a timeout: the second call finds every outcome
        // already unchanged, so its digest differs and it is refused as stale. (A fresh plan after
        // success is a no-op: see the retry test above.)
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
            w.cam.write_capability(*code, 0, &initial(i)).unwrap();
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

    // ── the CLI's escape hatch (GatePolicy::AllowPartial) ─────────────────

    #[test]
    fn strict_is_the_default_and_refuses_what_allow_partial_lets_through() {
        assert_eq!(GatePolicy::default(), GatePolicy::Strict);
        let w = world_with(|c, _| c, 178);
        let f = fixture(&w, true);
        let p = plan(&w, &f);
        assert!(run_with(&w, &f, &p.digest, &p.summary, &mut Vec::new(), GatePolicy::Strict).is_err());
        assert!(w.cam.writes().is_empty());
    }

    #[test]
    fn allow_partial_goes_ahead_with_an_incomplete_restore_point_and_says_so() {
        let w = world_with(|c, _| c, 178);
        let f = fixture(&w, true);
        let p = plan(&w, &f);
        let r = run_with(&w, &f, &p.digest, &p.summary, &mut Vec::new(), GatePolicy::AllowPartial).unwrap();
        assert!(r.restore_point_gate_waived);
        assert_eq!(r.written.len(), 4, "the writes happened");
        let rp = r.restore_point.as_ref().expect("the restore point is still taken");
        assert!(w.dir.path().join(&rp.file).exists());
        assert_eq!(r.status, ApplyStatus::AppliedUnverified, "an incomplete read-back is still not a verification");
    }

    #[test]
    fn a_passing_restore_point_is_not_marked_waived_even_under_allow_partial() {
        let w = world();
        let f = fixture(&w, true);
        let p = plan(&w, &f);
        let r = run_with(&w, &f, &p.digest, &p.summary, &mut Vec::new(), GatePolicy::AllowPartial).unwrap();
        assert!(!r.restore_point_gate_waived);
        assert_eq!(r.status, ApplyStatus::AppliedVerified);
    }

    #[test]
    fn allow_partial_applies_to_a_model_without_a_baseline_and_still_compares_the_read_back() {
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
        let p = plan(&w, &f);
        let r = run_with(&w, &f, &p.digest, &p.summary, &mut Vec::new(), GatePolicy::AllowPartial).unwrap();
        assert!(r.restore_point_gate_waived);
        assert!(!r.written.is_empty());
        assert_eq!(r.status, ApplyStatus::AppliedVerified, "the read-back was compared even though it could not be judged for completeness");
    }

    #[test]
    fn allow_partial_still_refuses_a_restore_point_that_read_nothing() {
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
        let p = plan(&w, &f);
        let e = run_with(&w, &f, &p.digest, &p.summary, &mut Vec::new(), GatePolicy::AllowPartial).unwrap_err();
        assert!(matches!(e, ApplyError::NothingReadable { .. }), "{e:?}");
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
        assert_eq!(w.cam.value(w.codes[1].1), Some(initial(1)), "the failed property kept its old value");
    }

    #[test]
    fn one_property_the_body_rejects_by_kind_is_reported_and_the_apply_is_not_called_verified() {
        // The dry run cannot see a write being rejected. The rest was written, so it is not a
        // failure, but a planned change that never happened was never judged by the read-back:
        // `applied_unverified`, with the property named in `rejected_by_kind`.
        let w = world_with(|c, codes| c.failing_write(codes[0].1, Failure::UnsupportedWrite), 0);
        let f = fixture(&w, true);
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.status, ApplyStatus::AppliedUnverified);
        assert!(r.write_errors.is_empty());
        assert_eq!(r.rejected_by_kind, [w.codes[0].0.clone()]);
        assert_eq!(r.written.len(), 3);
        assert!(!r.undo_set.contains(&w.codes[0].0), "never written, nothing to undo");
    }

    #[test]
    fn nothing_written_because_every_planned_property_was_rejected_by_kind_is_a_failure() {
        let w = world_with(
            |c, codes| codes.iter().fold(c, |c, (_, code)| c.failing_write(*code, Failure::UnsupportedWrite)),
            0,
        );
        let f = fixture(&w, true);
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.status, ApplyStatus::Failed, "writes were attempted and none landed");
        assert!(r.written.is_empty());
        assert_eq!(r.rejected_by_kind.len(), 4);
        assert!(r.undo_set.is_empty());
    }

    #[test]
    fn a_planned_property_that_cannot_be_written_for_any_other_reason_is_a_failure_and_not_in_the_undo_set() {
        // The dry run read the range; at write time that read fails. The property was in the plan the
        // caller confirmed and was not written: that must show, and there is nothing to undo for it.
        let st = |i: u64| json!({"value": 0.0, "default": 0.0, "value_index": i, "default_index": 612, "lower": 0.0, "upper": 1224.0, "steps": 1225});
        let (name, code) = (WB_TUNE_AUTO.0.to_string(), WB_TUNE_AUTO.1);
        let w = world_with(|c, _| c.with(code, RW, st(612)).with_sdk_range_quirk().failing_read_after_write(code, Failure::Call(-126)), 0);
        let mut f = fixture(&w, true);
        f.source.insert(name.clone(), code, st(613));
        f.bytes = f.source.to_pretty_json().unwrap().into_bytes();
        let r = apply_planned(&w, &f).unwrap();
        // The other properties were written first (name order puts WBTuneAuto last), so the read at
        // write time fails only after a write has happened.
        assert_eq!(r.status, ApplyStatus::Failed, "{:?}", r.write_errors);
        let e = r.write_errors.iter().find(|e| e.name == name).expect("the range is reported");
        assert!(e.message.contains("planned write not performed"), "{}", e.message);
        assert!(!r.undo_set.contains(&name), "it was never written, so there is nothing to undo: {:?}", r.undo_set);
        assert!(!r.written.contains(&name));
    }

    #[test]
    fn the_real_write_run_is_limited_to_what_the_confirmed_plan_copied() {
        // Plan: the numeric property (55 on the source) is NOT opted in and differs from the body's
        // 1, so the dry run skips it (`literal_unconfirmed`). Writing any of the three string
        // properties makes the body hold 55 there, which the literal gate treats as a no-op and lets
        // through — so a real run that re-evaluates everything would write it, though it was never
        // in the plan whose digest the caller echoed.
        let probe = world_with(|c, _| c, 0);
        let numeric = probe.codes[3].clone();
        let w = world_with(
            |c, codes| codes[..3].iter().fold(c, |c, (_, code)| c.with_dependent(*code, numeric.1, json!(55))),
            0,
        );
        let f = fixture(&w, false);
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.written.len(), 3, "only the three confirmed properties: {:?}", r.written);
        assert!(!r.written.contains(&numeric.0));
        assert!(w.cam.writes().iter().all(|(code, _)| *code != numeric.1), "the numeric property was never written");
        assert!(r.outcomes.iter().all(|o| o.name != numeric.0), "not in the planned set, not in the real run");
    }

    #[test]
    fn the_undo_set_of_an_apply_with_a_read_only_side_effect_is_accepted_by_the_undo() {
        // Writing the first property makes a read-only property on the body change. It lands in the
        // side effects, and an undo that was told to put it back would have failed (a read-only
        // property is skipped, an unrestored named property is a failure).
        let probe = world();
        let read_only = baseline_for("Z 9").unwrap().readable[50].code;
        let w = world_with(
            |c, codes| c.with(read_only, OP_GET, json!(1)).with_dependent(codes[0].1, read_only, json!(9)),
            0,
        );
        let f = fixture(&w, true);
        let r = apply_planned(&w, &f).unwrap();
        let ro_name = probe.names[&read_only].clone();
        assert!(r.readback.as_ref().unwrap().side_effects.as_ref().unwrap().changed.contains(&ro_name), "a side effect");
        assert!(!r.undo_set.contains(&ro_name), "{:?}", r.undo_set);
        assert!(r.undo_not_restorable.contains(&ro_name), "{:?}", r.undo_not_restorable);
        // The documented undo: resolve `only` against the restore point, then restore it.
        let rp = Snapshot::load_from_file(w.dir.path().join(&r.restore_point.as_ref().unwrap().file)).unwrap();
        let only = crate::ops::restore::resolve_only(&r.undo_set, &rp.properties).expect("every undo_set name is in the restore point");
        let run = crate::ops::restore::execute(&w.cam, &rp.properties, Some(&only), false);
        assert!(run.unrestored().is_empty(), "{:?}", run.unrestored());
    }

    #[test]
    fn a_property_named_differently_on_the_target_is_judged_and_undone_under_the_targets_name() {
        // `names_compatible` accepts one code under two names (BracketingType on the Z6III,
        // AEBracketingType on the Z 9). The restore point and the read-back are keyed by the
        // target's name, so the apply must look the property up by that, or it can never be verified.
        let w = world();
        let mut f = fixture(&w, true);
        let (target_name, code) = w.codes[0].clone();
        let source_name = "kNkMAIDCapability_SourceSideName".to_string();
        let moved = f.source.properties.remove(&target_name).unwrap();
        f.source.insert(source_name.clone(), code, moved.value);
        f.bytes = f.source.to_pretty_json().unwrap().into_bytes();
        // The guard knows the code only on the source model, so it does not object to the names.
        let caps: Vec<(&str, u32)> = w.codes[1..].iter().map(|(n, c)| (n.trim_start_matches("kNkMAIDCapability_"), *c)).collect();
        let src_caps: Vec<(&str, u32)> = std::iter::once(("SourceSideName", code)).chain(caps.iter().copied()).collect();
        let cfg = parse_config(&[("Z6_3", &src_caps), ("Z 9", &caps)]);
        f.guard = CodeNameGuard::new(&cfg, "Z6_3", "Z 9").unwrap();
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.status, ApplyStatus::AppliedVerified, "{:?}", r.readback.as_ref().map(|rb| (&rb.not_taken, &rb.unverifiable)));
        assert!(r.written.contains(&target_name) && !r.written.contains(&source_name), "{:?}", r.written);
        assert!(r.undo_set.contains(&target_name) && !r.undo_set.contains(&source_name), "{:?}", r.undo_set);
        assert!(r.readback.as_ref().unwrap().side_effects.as_ref().unwrap().is_empty(), "no false side effect");
    }

    #[test]
    fn a_failed_write_that_may_have_landed_stays_in_the_undo_set() {
        let w = world_with(|c, codes| c.failing_write(codes[1].1, Failure::Call(-125)), 0);
        let f = fixture(&w, true);
        let r = apply_planned(&w, &f).unwrap();
        assert!(r.undo_set.contains(&w.codes[1].0), "{:?}", r.undo_set);
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
        for (i, (_, code)) in w.codes.iter().enumerate() {
            assert_eq!(w.cam.value(*code), Some(initial(i)));
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

    #[test]
    fn a_stepped_range_is_applied_and_verified_against_a_body_that_reads_its_value_back_as_zero() {
        // The real SDK shape (F18): the position is right, the `value` field reads 0.0.
        let st = |i: u64| json!({"value": 0.0, "default": 0.0, "value_index": i, "default_index": 612, "lower": 0.0, "upper": 1224.0, "steps": 1225});
        // WBTuneAuto: on the verified-range table for the Z6_3 to Z 9 pair the fixture uses.
        let (name, code) = (WB_TUNE_AUTO.0.to_string(), WB_TUNE_AUTO.1);
        let w = world_with(|c, _| c.with(code, RW, st(612)).with_sdk_range_quirk(), 0);
        let mut f = fixture(&w, true);
        f.source.insert(name.clone(), code, st(613));
        f.bytes = f.source.to_pretty_json().unwrap().into_bytes();
        let r = apply_planned(&w, &f).unwrap();
        assert_eq!(r.status, ApplyStatus::AppliedVerified, "{:?}", r.readback.as_ref().map(|rb| (&rb.not_taken, &rb.unverifiable)));
        assert_eq!(w.cam.value(code).unwrap()["value_index"], json!(613));
        // A body that ignores the write is caught even though the call said success.
        let w2 = world_with(|c, _| c.with(code, RW, st(612)).with_sdk_range_quirk().ignoring_writes(code), 0);
        let mut f2 = fixture(&w2, true);
        let (n2, c2) = (name.clone(), code);
        f2.source.insert(n2.clone(), c2, st(613));
        f2.bytes = f2.source.to_pretty_json().unwrap().into_bytes();
        let r2 = apply_planned(&w2, &f2).unwrap();
        // Ranges are checked twice: the per-write read-back inside the transplant already turns an
        // ignored write into a write failure, before the overall read-back runs.
        assert_eq!(r2.status, ApplyStatus::Failed);
        assert_eq!(r2.write_errors.len(), 1);
        assert_eq!(r2.write_errors[0].name, n2);
        assert!(r2.write_errors[0].message.contains("reports"), "{}", r2.write_errors[0].message);
        assert!(r2.written.iter().all(|w| *w != n2), "an ignored write is not counted as written");
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
        // a stepped range: the position (its `value` field is 0.0 on the SDK), not the junk field
        let st = |i: u64| json!({"value": 0.0, "value_index": i, "lower": 0.0, "upper": 1224.0, "steps": 1225});
        assert!(took_effect(&st(612), &st(612)));
        assert!(!took_effect(&st(612), &st(0)), "index 0 is not 612 even though both `value` fields read 0.0");
        // The scale's own top end, where the old relative tolerance (0.1% of the value) exceeded one
        // step: adjacent positions are different positions at every point of a stepped scale.
        assert!(!took_effect(&st(1224), &st(1223)), "WBTune top: 1224 vs 1223");
        assert!(!took_effect(&st(1223), &st(1224)));
        assert!(!took_effect(&st(1000), &st(999)), "where the old tolerance first reached 1.0");
        assert!(took_effect(&st(1224), &st(1224)));
        let kelvin = |i: u64| json!({"value": 0.0, "value_index": i, "lower": 2500.0, "upper": 10000.0, "steps": 751});
        assert!(!took_effect(&kelvin(750), &kelvin(749)), "10000 K vs 9990 K, which the old tolerance accepted at exactly 10 <= 10");
        // A continuous range: at, below and above the float tolerance, and no relative leg.
        let big = |v: f64| json!({"value": v, "lower": 0.0, "upper": 5000.0, "steps": 0, "value_index": 0});
        assert!(took_effect(&big(1000.0), &big(1000.0 + 5e-7)), "below 1e-6");
        assert!(!took_effect(&big(1000.0), &big(1000.0 + 2e-6)), "above 1e-6");
        assert!(!took_effect(&big(1000.0), &big(1000.5)), "0.05% apart: inside the old relative tolerance");
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
        assert_eq!(serde_json::to_value(ApplyStatus::NotApplied).unwrap(), json!("not_applied"));
        // `tag()` is the one place the strings are written by hand; serde must agree for every status.
        for status in ApplyStatus::ALL {
            assert_eq!(serde_json::to_value(status).unwrap(), json!(status.tag()));
        }
        let tags: BTreeSet<_> = ApplyStatus::ALL.iter().map(ApplyStatus::tag).collect();
        assert_eq!(tags.len(), ApplyStatus::ALL.len());
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

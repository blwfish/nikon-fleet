// ── The guarded write: plan, confirm, apply ───────────────────────────────
//
// Everything a write to a camera shares, whichever command it runs (`transplant`, `restore`): one
// state machine, so a rule added to it holds for both, and the two cannot drift apart.
//
//   Idle -> Planning -> Planned -> (Aside ->) ConfirmApply -> Applying -> Done
//
// A plan and a confirmation are good for the request they were made for: any change to it (the
// snapshot, the whole camera row, the selection, the data directory) withdraws them, and what is
// sent is the plan that was shown, never the form as it is now. A worker that dies ends an apply
// that was sent as Indeterminate (the camera may have changed) and a command that never reached the
// worker as NotApplied. Each flow keeps one piece of state that belongs to its plan (transplant's
// opt-ins, restore's selection); `clear_selection` is called on EVERY transition out of the plan, so
// it cannot outlive it.
//
// Also here: the pre-flight, the plan and apply runners, the target check, the confirmation's
// head and tail, and the result view's tail, which were copies of one another.

use super::*;
use crate::restore::{draw_restore_point, undo_offer};
use std::fmt::Debug;

/// What distinguishes one guarded write from another.
pub(crate) trait Flow: Sized + Clone + PartialEq + Debug {
    /// What is asked for. A plan is good for this value only.
    type Request: Clone + PartialEq + Debug;
    /// A plan the CLI returned, with the request it is for.
    type Plan: Clone + PartialEq + Debug;
    /// A question put about a plan before the apply is asked (transplant's opt-in); restore has none.
    type Aside: Clone + PartialEq + Debug;
    type Report: AppliedReport + Clone + PartialEq + Debug;

    /// The apply, in the words of a message ("the apply was never sent", "the restore was never sent").
    const APPLY_WORD: &'static str;
    /// What a missing form field is told.
    const CHOOSE_FIRST: &'static str;

    fn request_target(r: &Self::Request) -> &CameraRow;
    fn plan_request(p: &Self::Plan) -> &Self::Request;
    fn aside_plan(a: &Self::Aside) -> &Self::Plan;
    fn plan_digest(p: &Self::Plan) -> &str;
    fn plan_summary(p: &Self::Plan) -> &str;
    /// May this plan be offered for confirmation?
    fn applicable(p: &Self::Plan) -> bool;
    fn plan_cmd(request: Self::Request) -> Cmd;
    fn apply_cmd(request: Self::Request, digest: String, summary: String) -> Cmd;
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stage<F: Flow> {
    Idle,
    /// A plan was asked for this request and has not come back.
    Planning(F::Request),
    Planned(F::Plan),
    /// Asked a question about the plan; waiting for the answer.
    Aside(F::Aside),
    /// Asked to apply; waiting for the confirmation.
    ConfirmApply(F::Plan),
    /// Sent; the answer is the only thing that ends this.
    Applying(F::Request),
    Done(Box<Applied<F>>),
}

impl<F: Flow> Default for Stage<F> {
    fn default() -> Self {
        Stage::Idle
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Applied<F: Flow> {
    pub request: F::Request,
    pub outcome: ApplyOutcome<F::Report>,
}

type Req<U> = <<U as GuardedUi>::F as Flow>::Request;
type Plan<U> = <<U as GuardedUi>::F as Flow>::Plan;

/// A window's state machine. A flow's state implements the accessors and the two selection hooks;
/// the transitions are provided.
pub(crate) trait GuardedUi {
    type F: Flow;

    fn stage(&self) -> &Stage<Self::F>;
    fn stage_mut(&mut self) -> &mut Stage<Self::F>;
    fn notice_mut(&mut self) -> &mut String;
    /// The request the form asks for now, if it asks for one.
    fn request(&self, cameras: &[CameraRow]) -> Option<Req<Self>>;
    /// The part of the form that belongs to a (snapshot, camera) pair, first thing every frame: it
    /// drops what was chosen for another pair.
    fn sync_selection(&mut self, cameras: &[CameraRow]);
    /// Forget the selection that belongs to a plan (opt-ins, a restore's `only`). Called on every
    /// transition out of a plan or a result, so it never waits unseen to ride into the next one.
    fn clear_selection(&mut self);

    /// Nothing is held any more.
    fn go_idle(&mut self) {
        *self.stage_mut() = Stage::Idle;
        self.clear_selection();
    }

    /// Run every frame, before anything is drawn.
    fn sync(&mut self, cameras: &[CameraRow]) {
        self.sync_selection(cameras);
        let held = match self.stage() {
            Stage::Planning(r) => Some(r.clone()),
            Stage::Planned(p) | Stage::ConfirmApply(p) => Some(Self::F::plan_request(p).clone()),
            Stage::Aside(a) => Some(Self::F::plan_request(Self::F::aside_plan(a)).clone()),
            Stage::Idle | Stage::Applying(_) | Stage::Done(_) => None,
        };
        if let Some(held) = held {
            if self.request(cameras).as_ref() != Some(&held) {
                let was_confirming = matches!(self.stage(), Stage::ConfirmApply(_) | Stage::Aside(_));
                self.go_idle();
                *self.notice_mut() = format!(
                    "{} withdrawn: the snapshot, the camera or the selection changed. Nothing was written.",
                    if was_confirming { "Confirmation" } else { "Plan" }
                );
            }
        }
    }

    /// "Discard plan" / "Close this result".
    fn discard(&mut self) {
        if matches!(self.stage(), Stage::Planned(_) | Stage::Done(_)) {
            self.go_idle();
        }
    }

    /// The data directory changed under a plan or a confirmation: the snapshots and the restore
    /// points it names are no longer where it said, so it is withdrawn. An apply on the wire is not:
    /// its answer must still find it.
    fn data_dir_changed(&mut self) {
        if matches!(self.stage(), Stage::Planning(_) | Stage::Planned(_) | Stage::Aside(_) | Stage::ConfirmApply(_)) {
            self.go_idle();
            *self.notice_mut() = "Plan withdrawn: the data directory changed. Nothing was written.".to_string();
        }
    }

    /// The worker thread is gone: nothing more will be answered. A plan being made is lost; an apply
    /// on the wire may have written, whatever stopped.
    fn worker_lost(&mut self) {
        match self.stage().clone() {
            Stage::Planning(_) => {
                self.go_idle();
                *self.notice_mut() = "The worker thread stopped while the camera was being read. Nothing was written.".to_string();
            }
            Stage::Applying(request) => {
                let outcome = ApplyOutcome::Indeterminate {
                    message: format!("The worker thread stopped while the {} was running, so what it wrote is unknown.", Self::F::APPLY_WORD),
                    cli_log: String::new(),
                };
                self.finish(request, outcome);
            }
            _ => {}
        }
    }

    /// A command could not be handed to the worker, which is gone: the camera was not asked.
    fn not_sent(&mut self) {
        match self.stage().clone() {
            Stage::Planning(_) => {
                self.go_idle();
                *self.notice_mut() = "The worker thread has stopped, so the camera was not read. Nothing was written.".to_string();
            }
            Stage::Applying(request) => {
                let outcome = ApplyOutcome::NotApplied {
                    why: NotAppliedWhy::Refused,
                    message: format!("The worker thread has stopped, so the {} was never sent.", Self::F::APPLY_WORD),
                    cli_log: String::new(),
                };
                self.finish(request, outcome);
            }
            _ => {}
        }
    }

    /// The result is in: the plan's selection goes with it.
    fn finish(&mut self, request: Req<Self>, outcome: ApplyOutcome<<Self::F as Flow>::Report>) {
        self.clear_selection();
        *self.stage_mut() = Stage::Done(Box::new(Applied { request, outcome }));
    }

    /// Start a plan for the form as it is. The command to send, or why not.
    fn begin_plan(&mut self, cameras: &[CameraRow]) -> Result<Cmd, String> {
        if matches!(self.stage(), Stage::Planning(_) | Stage::Applying(_)) {
            return Err(format!("Another {} step is still running.", Self::F::APPLY_WORD));
        }
        let request = self.request(cameras).ok_or(Self::F::CHOOSE_FIRST)?;
        if let Some(why) = write_target_refusal(Self::F::request_target(&request)) {
            return Err(why);
        }
        self.notice_mut().clear();
        *self.stage_mut() = Stage::Planning(request.clone());
        Ok(Self::F::plan_cmd(request))
    }

    /// A plan came back. Dropped unless it is the one asked for and still wanted. `build` makes the
    /// held plan from what arrived (it may read the form, for what the flow keeps beside the plan).
    fn plan_arrived_with<X>(
        &mut self,
        request: Req<Self>,
        result: Result<X, String>,
        build: impl FnOnce(&Self, Req<Self>, X) -> Plan<Self>,
    ) {
        if *self.stage() != Stage::Planning(request.clone()) {
            return;
        }
        match result {
            Ok(arrived) => {
                let plan = build(self, request, arrived);
                *self.stage_mut() = Stage::Planned(plan);
            }
            Err(e) => {
                self.go_idle();
                *self.notice_mut() = e;
            }
        }
    }

    /// Ask for the confirmation to apply. Only a plan the flow allows can be applied.
    fn ask_apply(&mut self) {
        if let Stage::Planned(p) = self.stage() {
            if Self::F::applicable(p) {
                *self.stage_mut() = Stage::ConfirmApply(self.planned().cloned().expect("just matched"));
            }
        }
    }

    /// The plan being reviewed, if one is.
    fn planned(&self) -> Option<&Plan<Self>> {
        match self.stage() {
            Stage::Planned(p) => Some(p),
            _ => None,
        }
    }

    /// Back from a confirmation or a question to the plan; nothing was asked of the camera.
    fn cancel_confirmation(&mut self) {
        let plan = match self.stage() {
            Stage::ConfirmApply(p) => p.clone(),
            Stage::Aside(a) => Self::F::aside_plan(a).clone(),
            _ => return,
        };
        *self.stage_mut() = Stage::Planned(plan);
    }

    /// The apply was confirmed: what is sent is the plan that was shown, not the form as it is now.
    fn confirm_apply(&mut self) -> Option<Cmd> {
        let Stage::ConfirmApply(p) = self.stage() else {
            return None;
        };
        let request = Self::F::plan_request(p).clone();
        let cmd = Self::F::apply_cmd(request.clone(), Self::F::plan_digest(p).to_string(), Self::F::plan_summary(p).to_string());
        *self.stage_mut() = Stage::Applying(request);
        Some(cmd)
    }

    /// The answer to an apply. Dropped unless it is for the apply on the wire.
    fn applied(&mut self, request: Req<Self>, outcome: ApplyOutcome<<Self::F as Flow>::Report>) {
        if *self.stage() != Stage::Applying(request.clone()) {
            return;
        }
        self.finish(request, outcome);
    }
}

// ── Before the camera is touched ─────────────────────────────────────────

/// Why a write is refused before the camera is touched, beyond what the CLI itself checks: it must
/// be aimed at one body with certainty.
pub(crate) fn write_target_refusal(camera: &CameraRow) -> Option<String> {
    if is_id_fallback(&camera.serial) {
        return Some(format!(
            "{} reports no USB serial (it is known only as {}), so a write cannot be aimed at it with certainty.",
            camera.model, camera.serial
        ));
    }
    camera
        .pairing_ambiguous()
        .then(|| AmbiguousPairing { model: camera.model.clone(), count: camera.model_count }.to_string())
}

/// The full pre-flight: the target's own check, then a fresh USB-only look at the bus (the same rule
/// a snapshot uses to trust a serial pairing), then that the body still runs the firmware the plan
/// was made for (the digest does not cover it). `noun` is "Transplant" / "Restore".
pub(crate) fn write_preflight(noun: &str, camera: &CameraRow, scan: &CameraList) -> Option<String> {
    let reason = write_target_refusal(camera)
        .or_else(|| snapshot_refusal(camera, scan).map(|r| r.reason()))
        .or_else(|| firmware_moved(camera, scan))?;
    Some(format!("{noun} refused, nothing was read or written: {reason}"))
}

/// The bus reports another firmware for this body than the one the request was made for.
fn firmware_moved(camera: &CameraRow, scan: &CameraList) -> Option<String> {
    let on_bus = scan.cameras.iter().find(|c| c.serial == camera.serial && c.model == camera.model)?;
    (on_bus.firmware != camera.firmware).then(|| {
        format!(
            "{} {} now reports firmware {}, not the {} it was discovered with: Discover again.",
            camera.model, camera.serial, on_bus.firmware, camera.firmware
        )
    })
}

/// A plan names the body it was made for; it is shown and confirmed only for the body (at the
/// firmware it runs now) that was asked for.
pub(crate) fn check_plan_target(plan: &BodyJson, wanted: &CameraRow) -> Result<(), String> {
    if plan.serial != wanted.serial || plan.model != wanted.model {
        return Err(format!(
            "the plan is for {} {}, not for the {} {} that was asked for; nothing was written",
            plan.model, plan.serial, wanted.model, wanted.serial
        ));
    }
    if plan.firmware != wanted.firmware {
        return Err(format!(
            "the plan was made for firmware {} on {} {}, which is {} now; nothing was written",
            plan.firmware, plan.model, plan.serial, wanted.firmware
        ));
    }
    Ok(())
}

/// How a plan is read from what the CLI printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanFailure {
    /// A run that did not succeed has no plan.
    NoPlan,
    /// The command prints its plan and then exits 1 in a documented case (restore: it names a
    /// property it cannot restore). Only that exit keeps the plan; a run that was stopped, timed out
    /// or killed after printing does not.
    PlanThenExitOne,
}

/// Ask the CLI for a plan. Reads the body, writes nothing.
pub(crate) fn plan_via<P: serde::de::DeserializeOwned>(
    noun: &str,
    cli: &Path,
    argv: impl FnOnce() -> Result<Vec<String>, String>,
    camera: &CameraRow,
    scan: &dyn Fn() -> CameraList,
    timeout: Duration,
    failure: PlanFailure,
) -> Result<(P, String), String> {
    if let Some(refusal) = write_preflight(noun, camera, &scan()) {
        return Err(refusal);
    }
    let argv = argv()?;
    let (stdout, stderr, failed) = match run_cli(cli, &argv, None, timeout) {
        Ok(out) => (out.stdout, out.stderr, None),
        Err(e) => {
            let keep = failure == PlanFailure::PlanThenExitOne && e.kind == CliFailure::Exit && e.code == Some(1);
            if !keep {
                return Err(format!("Planning failed: {}", e.message));
            }
            (e.stdout.clone(), e.stderr.clone(), Some(e))
        }
    };
    serde_json::from_str::<P>(stdout.trim()).map(|plan| (plan, stderr)).map_err(|e| match failed {
        Some(f) => format!("Planning failed: {} (and its plan could not be read: {e})", f.message),
        None => format!("the plan command succeeded but its report could not be read ({e}); nothing was written"),
    })
}

/// Apply exactly the plan that was confirmed: the CLI is given its digest and summary and refuses if
/// the body or the snapshot no longer give it.
pub(crate) fn apply_via<R: AppliedReport>(
    noun: &str,
    cli: &Path,
    argv: impl FnOnce() -> Result<Vec<String>, String>,
    camera: &CameraRow,
    digest: &str,
    scan: &dyn Fn() -> CameraList,
    timeout: Duration,
) -> ApplyOutcome<R> {
    let refused = |message: String| ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, message, cli_log: String::new() };
    if let Some(refusal) = write_preflight(noun, camera, &scan()) {
        return refused(refusal);
    }
    match argv() {
        Ok(argv) => classify_apply_as::<R>(run_cli(cli, &argv, None, timeout), digest),
        Err(e) => refused(e),
    }
}

/// The apply when the CLI cannot even be found.
pub(crate) fn no_cli<R>(message: String) -> ApplyOutcome<R> {
    ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, message, cli_log: String::new() }
}

// ── The confirmation, and the result ─────────────────────────────────────

/// The first lines of a confirmation: the body, what is written onto it, and the plan's summary.
/// `source` is the clause after "the settings" ("of X (…)" / "stored in X").
pub(crate) fn confirm_head(target: &CameraRow, source: &str, summary: &str) -> String {
    format!(
        "Write to {} (serial {}, firmware {}) the settings {source}?\n{summary}",
        target.model, target.serial, target.firmware
    )
}

/// The last line of every confirmation.
pub(crate) const CONFIRM_TAIL: &str =
    "\nA restore point of the camera is saved first and every write is read back; the result says whether it took.";

/// What a click in a result view asks for, whichever window drew it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppliedAction {
    View(String),
    Copy(String),
    Undo(UndoOffer),
}

/// The part of a result every flow shows alike: not-applied and indeterminate in full, and for a
/// report its headline, note, lines, the flow's own extra, the restore point with its undo, and the
/// CLI log. `lines` words the report; `extra` draws what only this flow's report has.
pub(crate) fn draw_applied_outcome<R: AppliedReport>(
    ui: &mut egui::Ui,
    target: &CameraRow,
    id: &str,
    outcome: &ApplyOutcome<R>,
    busy: bool,
    data_dir: Option<&Path>,
    lines: impl Fn(&R) -> Vec<(Level, String)>,
    extra: impl FnOnce(&mut egui::Ui, &R),
) -> Option<AppliedAction> {
    let mut chosen = None;
    match outcome {
        ApplyOutcome::Report { report, note, cli_log } => {
            let (level, headline) = status_headline(report.status());
            ui.heading(report.status().tag());
            colored(ui, level, headline);
            if let Some(note) = note {
                colored(ui, Level::Warn, note.clone());
            }
            for (level, line) in lines(report) {
                colored(ui, level, line);
            }
            extra(ui, report);
            if let Some(rp) = report.restore_point() {
                let offer = undo_offer(target, data_dir, outcome);
                chosen = draw_restore_point(ui, rp, offer.as_ref(), report.undo_set(), data_dir, busy);
            }
            draw_log(ui, id, cli_log);
        }
        ApplyOutcome::NotApplied { why, message, cli_log } => {
            ui.heading("not_applied");
            colored(ui, Level::Warn, "The camera was not changed.");
            if *why == NotAppliedWhy::PlanStale {
                colored(
                    ui,
                    Level::Warn,
                    "The camera or the snapshot no longer gives the plan you approved. Plan again to see what would be written now.",
                );
            }
            ui.label(message);
            draw_log(ui, id, cli_log);
        }
        ApplyOutcome::Indeterminate { message, cli_log } => {
            ui.heading("indeterminate");
            colored(ui, Level::Error, "The camera may have been changed.");
            ui.label(message);
            ui.label("Take a snapshot of the camera and compare it with the one you started from before doing anything else.");
            draw_log(ui, id, cli_log);
        }
    }
    chosen
}

fn draw_log(ui: &mut egui::Ui, id: &str, log: &str) {
    if !log.trim().is_empty() {
        egui::CollapsingHeader::new("CLI messages").id_salt((id, "log")).show(ui, |ui| {
            ui.label(egui::RichText::new(log.trim_end()).monospace());
        });
    }
}

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
//
// And the window both restore commands share (`GuardedWindow`, `draw_window`, `act`,
// `start_undo_in`): the pickers for a body and one of its snapshots, the plan button, the stage
// switch, the result and the undo. A restore window is a flow plus what only it draws (its plan, its
// confirmation, its report, what it adds to the form); everything else is written once.

use super::*;
use crate::restore::{NoAside, UndoOffer, draw_restore_point, undo_offer};
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

/// The plan's count of changes disagrees with what it lists: how both restore windows word it, so the
/// invariant reads the same wherever it is enforced. The caller adds its own ending ("so it cannot be
/// trusted" / "nothing was written") and its own capital. `noun` is what the plan lists ("steps", "items").
pub(crate) fn count_disagreement_clause(claimed: usize, listed: usize, noun: &str) -> String {
    format!("own count of changes ({claimed}) disagrees with its {noun} ({listed})")
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
                chosen = draw_restore_point(ui, rp, offer.as_ref(), report.undo_set(), report.undo_kind(), data_dir, busy);
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

// ── The window the restore commands share ─────────────────────────────────

/// What a window picks: a body and one of its own snapshots.
#[derive(Debug, Default)]
pub(crate) struct Pick {
    pub open: bool,
    /// The chosen target, by identity; the live row is found among the discovered cameras.
    pub target_key: Option<(String, String)>,
    pub source_file: Option<String>,
}

impl Pick {
    pub(crate) fn target_camera<'a>(&self, cameras: &'a [CameraRow]) -> Option<&'a CameraRow> {
        let (model, serial) = self.target_key.as_ref()?;
        cameras.iter().find(|c| &c.model == model && &c.serial == serial)
    }
}

/// What a click in a restore window asks for. Collected while drawing and applied after. The window's
/// own buttons (`O`) ride beside the ones every guarded write has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WindowAct<O> {
    Plan,
    AskApply,
    ConfirmApply,
    CancelConfirmation,
    Discard,
    Applied(AppliedAction),
    Own(O),
}

/// A window with no buttons of its own: this type has no values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NoOwn {}

/// What applying a click asks the app to do beyond changing the window.
pub(crate) enum Effect {
    Nothing,
    /// Hand this to the worker (and tell the window if the worker is gone).
    Send(Cmd),
    /// A click in the result view: open, copy or undo.
    Applied(AppliedAction),
}

/// The part of the app a window needs to be drawn, borrowed apart from the window itself.
pub(crate) struct WindowEnv<'a> {
    pub cameras: &'a [CameraRow],
    pub listing: Option<&'a SnapListing>,
    pub busy: bool,
    pub data_dir: Option<&'a Path>,
}

type ReportOf<W> = <<W as GuardedUi>::F as Flow>::Report;

/// A restore window: a guarded write aimed at one body from one of its own snapshots. The trait
/// holds only what differs between the windows; the transitions are `GuardedUi`'s, the layout and the
/// undo are provided here.
pub(crate) trait GuardedWindow: GuardedUi {
    /// The buttons only this window has.
    type Own;

    const TITLE: &'static str;
    /// Prefix of the ids of this window's widgets (two windows must not share them).
    const ID: &'static str;
    /// What the empty window says.
    const IDLE: &'static str;
    /// The small print under the snapshot picker.
    const SNAPSHOT_HINT: &'static str;

    fn pick(&self) -> &Pick;
    fn pick_mut(&mut self) -> &mut Pick;

    /// First line of the window, if it has one.
    fn intro(_ui: &mut egui::Ui) {}
    /// A snapshot in the picker; `camera_firmware` is the chosen body's.
    fn snapshot_option_label(row: &SnapRow, camera_firmware: Option<&str>) -> String;
    /// The rest of the form, between the snapshot and the plan button.
    fn draw_form(&mut self, _ui: &mut egui::Ui, _busy: bool) {}
    /// The plan under review. `can_edit` is false while a confirmation is up or the camera is busy.
    fn draw_plan(ui: &mut egui::Ui, plan: &Plan<Self>, can_edit: bool, action: &mut Option<WindowAct<Self::Own>>);
    /// Under the buttons of a plan under review.
    fn plan_footer(_ui: &mut egui::Ui, _plan: &Plan<Self>) {}
    fn confirm_text(plan: &Plan<Self>) -> String;
    /// The snapshot a request reads from, for the line above a result.
    fn request_source(request: &Req<Self>) -> &str;
    fn report_lines(report: &ReportOf<Self>) -> Vec<(Level, String)>;
    fn report_extra(ui: &mut egui::Ui, report: &ReportOf<Self>);
    /// A button of this window's own. The command to send, if it sends one.
    fn act_own(&mut self, own: Self::Own, cameras: &[CameraRow]) -> Result<Option<Cmd>, String>;
    /// Take the selection an undo of `offer` is (everything it puts back, nothing else). Called before
    /// the window is touched: an `Err` leaves it as it was.
    fn adopt_undo(&mut self, offer: &UndoOffer, camera: &CameraRow) -> Result<(), String>;

    fn target_camera<'a>(&self, cameras: &'a [CameraRow]) -> Option<&'a CameraRow> {
        self.pick().target_camera(cameras)
    }

    /// Set the form to undo `offer` (a restore point and what the apply wrote) and plan it. The plan
    /// is read-only; the person reviews it and confirms it like any restore. The body must be
    /// connected, and no step may be in flight.
    fn start_undo(&mut self, offer: &UndoOffer, cameras: &[CameraRow]) -> Result<Cmd, String> {
        if matches!(self.stage(), Stage::Applying(_) | Stage::Planning(_)) {
            return Err("Another restore step is still running.".to_string());
        }
        let Some(camera) = cameras.iter().find(|c| c.model == offer.target.model && c.serial == offer.target.serial) else {
            return Err(format!("{} {} is not connected: connect it and Discover, then undo.", offer.target.model, offer.target.serial));
        };
        if offer.undo_set.is_empty() {
            return Err("There is nothing to undo.".to_string());
        }
        self.adopt_undo(offer, camera)?;
        let pick = self.pick_mut();
        pick.open = true;
        pick.target_key = Some((camera.model.clone(), camera.serial.clone()));
        pick.source_file = Some(offer.restore_point.clone());
        *self.stage_mut() = Stage::Idle;
        self.begin_plan(cameras)
    }
}

/// Undo an apply in `win`: it opens on the restore point with the apply's undo set and the plan is
/// asked for. The command to send, if there is one; otherwise the window says why.
pub(crate) fn start_undo_in<W: GuardedWindow>(win: &mut W, offer: &UndoOffer, cameras: &[CameraRow], data_dir: Option<&Path>) -> Option<Cmd> {
    // The restore point is a file in the data directory the apply used.
    if data_dir != Some(offer.data_dir.as_path()) {
        win.pick_mut().open = true;
        *win.notice_mut() = format!(
            "The data directory changed since this apply: its restore point {} is in {}. Switch back to undo it.",
            offer.restore_point,
            offer.data_dir.display()
        );
        return None;
    }
    match win.start_undo(offer, cameras) {
        Ok(cmd) => Some(cmd),
        Err(e) => {
            win.pick_mut().open = true;
            *win.notice_mut() = e;
            None
        }
    }
}

/// Apply a click. Refusals become the window's notice; what the app must do is returned.
pub(crate) fn act<W: GuardedWindow>(win: &mut W, cameras: &[CameraRow], click: WindowAct<W::Own>) -> Effect {
    let sent = |win: &mut W, r: Result<Cmd, String>| match r {
        Ok(cmd) => Effect::Send(cmd),
        Err(e) => {
            *win.notice_mut() = e;
            Effect::Nothing
        }
    };
    match click {
        WindowAct::Plan => {
            let r = win.begin_plan(cameras);
            sent(win, r)
        }
        WindowAct::Own(own) => match win.act_own(own, cameras) {
            Ok(Some(cmd)) => Effect::Send(cmd),
            Ok(None) => Effect::Nothing,
            Err(e) => {
                *win.notice_mut() = e;
                Effect::Nothing
            }
        },
        WindowAct::AskApply => {
            win.ask_apply();
            Effect::Nothing
        }
        WindowAct::ConfirmApply => win.confirm_apply().map_or(Effect::Nothing, Effect::Send),
        WindowAct::CancelConfirmation => {
            win.cancel_confirmation();
            Effect::Nothing
        }
        WindowAct::Discard => {
            win.discard();
            Effect::Nothing
        }
        WindowAct::Applied(a) => Effect::Applied(a),
    }
}

/// Draw the window if it is open: the body, the snapshot, what the window adds, the plan button, and
/// whatever stage the write is in. The click, if any, is returned to be applied after drawing.
pub(crate) fn draw_window<W: GuardedWindow>(ctx: &egui::Context, win: &mut W, env: &WindowEnv<'_>) -> Option<WindowAct<W::Own>>
where
    <W as GuardedUi>::F: Flow<Aside = NoAside>,
{
    if !win.pick().open {
        return None;
    }
    let mut open = true;
    let mut action: Option<WindowAct<W::Own>> = None;
    let WindowEnv { cameras, listing, busy, data_dir } = *env;
    let data_ready = data_dir.is_some();
    egui::Window::new(W::TITLE).open(&mut open).default_size([700.0, 620.0]).resizable(true).collapsible(false).show(ctx, |ui| {
        egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(720.0).show(ui, |ui| {
            W::intro(ui);
            ui.label(egui::RichText::new("1. The camera: a connected body").strong());
            if cameras.is_empty() {
                ui.label(egui::RichText::new("No camera is connected: use Discover first.").italics().weak());
            }
            ui.add_enabled_ui(!busy, |ui| {
                let pick = win.pick_mut();
                let text = pick.target_camera(cameras).map_or("choose a body…".to_string(), camera_label);
                egui::ComboBox::from_id_salt((W::ID, "target")).selected_text(text).width(420.0).show_ui(ui, |ui| {
                    for c in cameras {
                        let picked = pick.target_key.as_ref() == Some(&(c.model.clone(), c.serial.clone()));
                        if ui.selectable_label(picked, camera_label(c)).clicked() && !picked {
                            pick.target_key = Some((c.model.clone(), c.serial.clone()));
                            pick.source_file = None;
                        }
                    }
                });
            });
            let target = win.target_camera(cameras);
            if win.pick().target_key.is_some() && target.is_none() {
                ui.colored_label(WARN_COLOR, "That body is no longer connected.");
            }
            let refusal = target.and_then(write_target_refusal);
            if let Some(why) = &refusal {
                ui.colored_label(ERROR_COLOR, why);
            }

            ui.add_space(6.0);
            ui.label(egui::RichText::new("2. The snapshot: one of that body's own").strong());
            let all_rows: Vec<&SnapRow> = match (listing, target) {
                (Some(l), Some(c)) => l.rows_for(&c.model, &c.serial).collect(),
                _ => Vec::new(),
            };
            let hidden = all_rows.iter().filter(|row| row.name_lossy).count();
            let rows: Vec<&SnapRow> = all_rows.iter().copied().filter(|row| !row.name_lossy).collect();
            if target.is_some() && rows.is_empty() {
                ui.label(egui::RichText::new(if hidden > 0 { "No snapshot with a usable file name." } else { "This body has no stored snapshots." }).italics().weak());
            }
            if hidden > 0 {
                ui.colored_label(WARN_COLOR, format!("{hidden} snapshot(s) of this body have file names that cannot be shown and are not offered."));
            }
            let camera_fw = target.map(|c| c.firmware.clone());
            ui.add_enabled_ui(!busy, |ui| {
                let pick = win.pick_mut();
                let text = pick
                    .source_file
                    .as_ref()
                    .map(|f| rows.iter().find(|row| &row.file == f).map_or_else(|| f.clone(), |row| W::snapshot_option_label(row, camera_fw.as_deref())))
                    .unwrap_or_else(|| "choose a snapshot…".to_string());
                egui::ComboBox::from_id_salt((W::ID, "source")).selected_text(text).width(560.0).show_ui(ui, |ui| {
                    for row in &rows {
                        let picked = pick.source_file.as_ref() == Some(&row.file);
                        if ui.selectable_label(picked, W::snapshot_option_label(row, camera_fw.as_deref())).clicked() {
                            pick.source_file = Some(row.file.clone());
                        }
                    }
                });
            });
            ui.label(egui::RichText::new(W::SNAPSHOT_HINT).small().weak());

            win.draw_form(ui, busy);

            ui.add_space(6.0);
            let planning_possible = !busy
                && data_ready
                && refusal.is_none()
                && win.request(cameras).is_some()
                && !matches!(win.stage(), Stage::Applying(_) | Stage::Planning(_));
            ui.add_enabled_ui(planning_possible, |ui| {
                if ui.button("Plan (reads the camera, writes nothing)").clicked() {
                    action = Some(WindowAct::Plan);
                }
            });
            if !win.notice_mut().is_empty() {
                ui.colored_label(WARN_COLOR, win.notice_mut().as_str());
            }
            ui.separator();

            match win.stage() {
                Stage::Idle => {
                    ui.label(egui::RichText::new(W::IDLE).weak());
                }
                Stage::Planning(_) => {
                    ui.label("Planning… the camera is being read.");
                }
                Stage::Planned(p) => {
                    W::draw_plan(ui, p, !busy, &mut action);
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(!busy && <W::F as Flow>::applicable(p), |ui| {
                            if ui.button("Restore to camera…").clicked() {
                                action = Some(WindowAct::AskApply);
                            }
                        });
                        if ui.button("Discard plan").clicked() {
                            action = Some(WindowAct::Discard);
                        }
                    });
                    W::plan_footer(ui, p);
                }
                Stage::Aside(a) => match *a {},
                Stage::ConfirmApply(p) => {
                    W::draw_plan(ui, p, false, &mut action);
                    ui.separator();
                    ui.colored_label(WARN_COLOR, W::confirm_text(p));
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("Confirm: write to camera").clicked() {
                                action = Some(WindowAct::ConfirmApply);
                            }
                            if ui.button("Cancel").clicked() {
                                action = Some(WindowAct::CancelConfirmation);
                            }
                        });
                    });
                }
                Stage::Applying(req) => {
                    let t = <W::F as Flow>::request_target(req);
                    ui.colored_label(WARN_COLOR, format!("Writing to {} {}… keep it connected and do not close this program.", t.model, t.serial));
                }
                Stage::Done(a) => {
                    let t = <W::F as Flow>::request_target(&a.request);
                    ui.label(egui::RichText::new(format!("{}  →  {} {}", W::request_source(&a.request), t.model, t.serial)).weak());
                    let id = format!("{}_applied", W::ID);
                    let chosen = draw_applied_outcome(ui, t, &id, &a.outcome, busy, data_dir, W::report_lines, W::report_extra);
                    if let Some(c) = chosen {
                        action = Some(WindowAct::Applied(c));
                    }
                    ui.separator();
                    if ui.button("Close this result").clicked() {
                        action = Some(WindowAct::Discard);
                    }
                }
            }
        });
    });
    win.pick_mut().open = open;
    action
}

// ── Parity: both restore windows, the same events, the same transitions ───

#[cfg(test)]
pub(crate) mod parity {
    use super::*;

    /// How a plan arrives in a window.
    #[derive(Debug, Clone, Copy)]
    pub(crate) enum Delivery {
        /// A plan that changes something and can be confirmed.
        Applicable,
        /// A plan that would change nothing.
        NothingToChange,
        /// The CLI failed to plan.
        Failed,
    }

    /// How an apply ends.
    #[derive(Debug, Clone, Copy)]
    pub(crate) enum End {
        Verified,
        Stale,
        Indeterminate,
    }

    /// What a window must supply for the same events to be fed to it: its own fixtures. Everything
    /// else is the shared state machine.
    pub(crate) trait Kit: GuardedWindow + Sized {
        /// A window aimed at a connected body and a snapshot, synced, with the cameras it knows.
        fn ready() -> (Self, Vec<CameraRow>);
        fn deliver(&mut self, request: Req<Self>, how: Delivery);
        fn outcome(end: End) -> ApplyOutcome<ReportOf<Self>>;
        /// The same request for another snapshot.
        fn other_source(request: Req<Self>) -> Req<Self>;
        fn offer() -> UndoOffer;
    }

    fn shape<F: Flow>(s: &Stage<F>) -> String {
        match s {
            Stage::Idle => "idle".into(),
            Stage::Planning(_) => "planning".into(),
            Stage::Planned(p) => format!("planned(applicable={})", F::applicable(p)),
            Stage::Aside(_) => "aside".into(),
            Stage::ConfirmApply(_) => "confirm".into(),
            Stage::Applying(_) => "applying".into(),
            Stage::Done(a) => match &a.outcome {
                ApplyOutcome::Report { report, .. } => format!("done(report {:?})", report.status()),
                ApplyOutcome::NotApplied { why, .. } => format!("done(not_applied {why:?})"),
                ApplyOutcome::Indeterminate { .. } => "done(indeterminate)".into(),
            },
        }
    }

    /// The state a person sees: the stage and whether a notice is up.
    fn seen<K: Kit>(w: &mut K) -> String {
        let notice = !w.notice_mut().is_empty();
        format!("{} notice={notice}", shape(w.stage()))
    }

    /// The request a plan was asked for, as the worker would answer it.
    fn asked<K: Kit>(w: &mut K) -> Req<K> {
        match w.stage() {
            Stage::Planning(r) => r.clone(),
            other => panic!("no plan was asked for: {}", shape(other)),
        }
    }

    fn draw<K: Kit>(w: &mut K, cameras: &[CameraRow], busy: bool) -> bool
    where
        <K as GuardedUi>::F: Flow<Aside = NoAside>,
    {
        let ctx = egui::Context::default();
        let env = WindowEnv { cameras, listing: None, busy, data_dir: Some(Path::new("/data")) };
        w.pick_mut().open = true;
        let mut clicked = false;
        let _ = ctx.run(egui::RawInput::default(), |ctx| clicked = draw_window(ctx, w, &env).is_some());
        clicked
    }

    /// The same sequence of events, recorded as what the person sees after each.
    pub(crate) fn trace<K: Kit>() -> Vec<String>
    where
        <K as GuardedUi>::F: Flow<Aside = NoAside>,
    {
        let mut t: Vec<String> = Vec::new();
        let mut note = |t: &mut Vec<String>, what: &str, w: &mut K, cameras: &[CameraRow]| {
            // every state draws, busy or not, and drawing alone clicks nothing
            assert!(!draw(w, cameras, false) && !draw(w, cameras, true), "{what}: drawing is not a click");
            t.push(format!("{what}: {}", seen(w)));
        };

        // plan ok, confirm in two steps, apply ok, close
        let (mut w, cameras) = K::ready();
        note(&mut t, "start", &mut w, &cameras);
        w.begin_plan(&cameras).expect("a plan");
        note(&mut t, "plan asked", &mut w, &cameras);
        t.push(format!("second plan refused: {}", w.begin_plan(&cameras).is_err()));
        let request = asked(&mut w);
        w.deliver(request.clone(), Delivery::Applicable);
        note(&mut t, "plan arrived", &mut w, &cameras);
        t.push(format!("confirm without asking: {}", w.confirm_apply().is_some()));
        w.ask_apply();
        note(&mut t, "apply asked", &mut w, &cameras);
        w.cancel_confirmation();
        note(&mut t, "cancelled", &mut w, &cameras);
        w.ask_apply();
        t.push(format!("confirmed: {}", w.confirm_apply().is_some()));
        note(&mut t, "applying", &mut w, &cameras);
        t.push(format!("second confirm: {}", w.confirm_apply().is_some()));
        t.push(format!("plan during apply refused: {}", w.begin_plan(&cameras).is_err()));
        t.push(format!("undo during apply refused: {}", w.start_undo(&K::offer(), &cameras).is_err()));
        w.applied(K::other_source(request.clone()), K::outcome(End::Verified));
        note(&mut t, "answer for another apply dropped", &mut w, &cameras);
        w.applied(request.clone(), K::outcome(End::Verified));
        note(&mut t, "applied", &mut w, &cameras);
        w.discard();
        note(&mut t, "closed", &mut w, &cameras);

        // the other ways an apply ends
        for end in [End::Stale, End::Indeterminate] {
            let (mut w, cameras) = K::ready();
            w.begin_plan(&cameras).unwrap();
            let request = asked(&mut w);
            w.deliver(request.clone(), Delivery::Applicable);
            w.ask_apply();
            w.confirm_apply().unwrap();
            w.applied(request, K::outcome(end));
            note(&mut t, &format!("ended {end:?}"), &mut w, &cameras);
        }

        // a plan that changes nothing is not offered; a failed plan is a notice; a stray plan is dropped
        let (mut w, cameras) = K::ready();
        w.begin_plan(&cameras).unwrap();
        let request = asked(&mut w);
        w.deliver(K::other_source(request.clone()), Delivery::Applicable);
        note(&mut t, "plan for another request", &mut w, &cameras);
        w.deliver(request.clone(), Delivery::NothingToChange);
        w.ask_apply();
        note(&mut t, "nothing to change, asked", &mut w, &cameras);
        w.discard();
        w.begin_plan(&cameras).unwrap();
        w.deliver(request.clone(), Delivery::Failed);
        note(&mut t, "plan failed", &mut w, &cameras);
        w.deliver(request.clone(), Delivery::Applicable);
        note(&mut t, "plan nobody asked for", &mut w, &cameras);

        // a change withdraws a plan and each confirmation but not an apply on the wire
        for stage in ["planned", "confirming", "applying"] {
            let (mut w, cameras) = K::ready();
            w.begin_plan(&cameras).unwrap();
            let request = asked(&mut w);
            w.deliver(request, Delivery::Applicable);
            if stage != "planned" {
                w.ask_apply();
            }
            if stage == "applying" {
                w.confirm_apply().unwrap();
            }
            w.pick_mut().source_file = Some("another.json".into());
            w.sync(&cameras);
            note(&mut t, &format!("snapshot changed while {stage}"), &mut w, &cameras);
            let (mut w, cameras) = K::ready();
            w.begin_plan(&cameras).unwrap();
            let request = asked(&mut w);
            w.deliver(request, Delivery::Applicable);
            if stage != "planned" {
                w.ask_apply();
            }
            if stage == "applying" {
                w.confirm_apply().unwrap();
            }
            let mut moved = cameras[0].clone();
            moved.firmware = "9.99".into();
            w.sync(&[moved]);
            note(&mut t, &format!("camera changed while {stage}"), &mut w, &cameras);
            let (mut w, cameras) = K::ready();
            w.begin_plan(&cameras).unwrap();
            let request = asked(&mut w);
            w.deliver(request, Delivery::Applicable);
            if stage != "planned" {
                w.ask_apply();
            }
            if stage == "applying" {
                w.confirm_apply().unwrap();
            }
            w.data_dir_changed();
            note(&mut t, &format!("data directory changed while {stage}"), &mut w, &cameras);
        }

        // a dead worker: a plan is lost, an apply is unknown, a command that never left was not applied
        let (mut w, cameras) = K::ready();
        w.begin_plan(&cameras).unwrap();
        w.worker_lost();
        note(&mut t, "worker lost while planning", &mut w, &cameras);
        let (mut w, cameras) = K::ready();
        w.begin_plan(&cameras).unwrap();
        w.not_sent();
        note(&mut t, "plan not sent", &mut w, &cameras);
        for lost in [true, false] {
            let (mut w, cameras) = K::ready();
            w.begin_plan(&cameras).unwrap();
            let request = asked(&mut w);
            w.deliver(request, Delivery::Applicable);
            w.ask_apply();
            w.confirm_apply().unwrap();
            if lost {
                w.worker_lost();
            } else {
                w.not_sent();
            }
            note(&mut t, if lost { "worker lost while applying" } else { "apply not sent" }, &mut w, &cameras);
        }

        // an undo plans the restore point, and is refused while a step is in flight or the body is not there
        let (mut w, cameras) = K::ready();
        t.push(format!("undo of nothing refused: {}", w.start_undo(&UndoOffer { undo_set: vec![], ..K::offer() }, &cameras).is_err()));
        t.push(format!("undo of an absent body refused: {}", w.start_undo(&K::offer(), &[]).is_err()));
        t.push(format!("undo planned: {}", w.start_undo(&K::offer(), &cameras).is_ok()));
        note(&mut t, "undo", &mut w, &cameras);
        t.push(format!("undo while planning refused: {}", w.start_undo(&K::offer(), &cameras).is_err()));
        t
    }

    #[test]
    fn both_restore_windows_make_the_same_transitions_for_the_same_events() {
        use crate::restore::RestoreUi;
        use crate::restore_ext::ExtRestoreUi;
        let plain = trace::<RestoreUi>();
        let ext = trace::<ExtRestoreUi>();
        assert!(plain.len() > 35, "the sequence ran: {}", plain.len());
        for (i, (a, b)) in plain.iter().zip(&ext).enumerate() {
            assert_eq!(a, b, "event {i}: the settings restore and the owner-data restore disagree");
        }
        assert_eq!(plain.len(), ext.len());
        // Equal traces would pass if both were empty or both stuck: pin what the sequence ends in at its landmarks.
        for (at, expect) in [
            (3, "plan arrived: planned(applicable=true) notice=false"),
            (13, "applied: done(report AppliedVerified) notice=false"),
            (15, "ended Stale: done(not_applied PlanStale) notice=false"),
            (16, "ended Indeterminate: done(indeterminate) notice=false"),
            (18, "nothing to change, asked: planned(applicable=false) notice=false"),
            (21, "snapshot changed while planned: idle notice=true"),
            (27, "snapshot changed while applying: applying notice=false"),
            (32, "worker lost while applying: done(indeterminate) notice=false"),
            (33, "apply not sent: done(not_applied Refused) notice=false"),
        ] {
            assert_eq!(plain[at], expect, "landmark {at}");
        }
    }
}

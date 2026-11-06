// ── Restore owner data: plan, review, confirm, apply ──────────────────────
//
// Put a stored snapshot's owner strings (Artist, Copyright, Image comment), IPTC presets, switches and
// FTP profiles back on the body it was taken from. It is `nikon-fleet restore-extended`'s work, run as
// a subprocess like every camera operation here, through the state machine every guarded write
// shares (`guarded.rs`); the MAID `restore` does not touch any of these.
//
//   1. PLAN     `restore-extended --dry-run --json`: reads the body over raw USB, writes nothing,
//               returns a digest, a one-line summary and one item per string / preset / switch / list.
//   2. REVIEW   the person sees what would change (old -> new, never a password), what is skipped and
//               why, and ticks which categories to restore. A change of ticks is a new request, so it
//               withdraws the plan.
//   3. CONFIRM  a text that names the exact body, the snapshot and what will be written, and says so
//               when the FTP list is to be rebuilt.
//   4. APPLY    `restore-extended --json --expect-digest --expect-summary`: the CLI recomputes the
//               plan on the body as it is now and refuses (exit `EXIT_STALE_PLAN`, nothing written)
//               unless it is the plan that was confirmed. Then restore point, writes, read-back.
//
// The restore point it makes can be restored in turn (Undo): the window opens on it with the
// categories the apply wrote, through the same four steps.

use super::*;
use crate::restore::{NoAside, RESTORE_PLAN_TIMEOUT, UndoKind, UndoOffer};
use nikon_fleet::ops::restore_extended::{Category, SkipKind};

/// What an extended restore is asked for. These exact values reach the CLI: a plan is good for this
/// request only, and a change to any part of it withdraws the plan and any confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExtRestoreRequest {
    /// The source, a file name in `snapshots/` (a snapshot of the target body itself).
    pub source_file: String,
    /// The body written to, as discovered (a changed firmware or pairing is a different request).
    pub target: CameraRow,
    /// The categories to restore and nothing else, or every category.
    pub only: Option<BTreeSet<Category>>,
}

// The CLI's JSON, as `ops::restore_extended::ExtendedPlanReport` and `ExtendedApplyReport` serialise
// it. The closed vocabularies (`Category`, `SkipKind`) are the library's own types: a variant added to
// the library fails to compile in the exhaustive matches that word them here (`category_words`, the skip
// words), instead of being shown as nothing. They have NO catch-all, so output from a CLI built from a
// newer library that carries a variant this build lacks does not parse at all: the plan or report is
// refused with the parse error, nothing is written (fails safe; same-commit builds never meet this).
// The two the library only serialises (`ItemAction`, `ItemOutcome`) are read here, and one this build
// does not know is `Unknown`: shown (an unknown outcome as an error), and a plan containing an unknown
// action is not offered for confirmation. `Unknown` does not keep the raw tag (serde's `other` drops it).
// The mirror structs do not deny unknown fields, so a field the library adds is ignored here until it is
// added to the mirror; `the_mirror_structs_read_every_field_the_library_writes` holds them to the library's key sets in tests.

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum ExtActionJson {
    Unchanged,
    NotSelected,
    Write {
        from: Option<String>,
        to: String,
    },
    Skip {
        reason: SkipKind,
        #[serde(default)]
        detail: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ExtItemJson {
    pub id: String,
    pub label: String,
    pub category: Category,
    #[serde(flatten)]
    pub action: ExtActionJson,
}

// In tests the two top-level mirrors refuse a key they do not read (`deny_unknown_fields`), so a field the library
// adds to its report fails the tests that read the library's own output until it is mirrored here. In the program they
// stay lenient: a report that has already been printed (an apply has written) must not be thrown away for a new field.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, serde(deny_unknown_fields))]
pub(crate) struct ExtPlanJson {
    pub camera: BodyJson,
    /// The snapshot's file name.
    pub source: String,
    pub would_change: usize,
    pub summary: String,
    pub digest: String,
    pub items: Vec<ExtItemJson>,
}

impl ExtPlanJson {
    /// The invariant "the plan's own count of changes is the number of its `write` items", in one place: `None` when
    /// it holds, else `(claimed, items)`. The gate that offers a plan for confirmation and the check on a plan just
    /// returned both read this and word the refusal for their own moment.
    pub(crate) fn count_disagreement(&self) -> Option<(usize, usize)> {
        let writes = self.items.iter().filter(|i| matches!(i.action, ExtActionJson::Write { .. })).count();
        (self.would_change != writes).then_some((self.would_change, writes))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum ExtOutcomeJson {
    Written {
        verified: bool,
    },
    Failed {
        message: String,
    },
    Indeterminate {
        message: String,
    },
    NotAttempted {
        because: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ExtResultJson {
    pub id: String,
    pub label: String,
    pub category: Category,
    #[serde(flatten)]
    pub outcome: ExtOutcomeJson,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, serde(deny_unknown_fields))]
pub(crate) struct ExtApplyJson {
    pub status: ApplyStatus,
    pub no_op: bool,
    pub plan_digest: String,
    pub restore_point: Option<RestorePointJson>,
    /// Category tags: what an undo puts back.
    pub undo_set: Vec<String>,
    pub results: Vec<ExtResultJson>,
    pub still_differing: Vec<String>,
    pub after_read_failed: Option<String>,
}

impl AppliedReport for ExtApplyJson {
    fn plan_digest(&self) -> &str {
        &self.plan_digest
    }
    fn status(&self) -> ApplyStatus {
        self.status
    }
    fn restore_point(&self) -> Option<&RestorePointJson> {
        self.restore_point.as_ref()
    }
    fn undo_set(&self) -> &[String] {
        &self.undo_set
    }
    fn undo_kind(&self) -> UndoKind {
        UndoKind::Categories
    }
}

/// The category's name for a person, and what is in it. Exhaustive: a new category must be worded here.
pub(crate) fn category_words(c: Category) -> (&'static str, &'static str) {
    match c {
        Category::Strings => ("Owner strings", "Artist, Copyright, Image comment"),
        Category::Iptc => ("IPTC presets", "each stored preset"),
        Category::Switches => ("Switches", "EnableCopyright, IPTC preset select"),
        Category::Ftp => ("FTP profiles", "the whole list is rebuilt: everything stored is deleted, then the snapshot's profiles are added"),
    }
}

/// Why an item is skipped, in words. Exhaustive: a new reason must be worded here.
pub(crate) fn skip_words(reason: SkipKind) -> &'static str {
    match reason {
        SkipKind::SnapshotHasNoValue => "the snapshot has no value for it",
        SkipKind::BodyLacksIt => "this body does not have it",
        SkipKind::BodyUnreadable => "the body's current value could not be read, so a change cannot be judged",
        SkipKind::ValueNotWritable => "the value is not one the camera accepts",
        SkipKind::ModelNotConfirmed => "writing it has not been confirmed on this model",
        SkipKind::RecordNotWritable => "it is a kind of record there is no layout to write",
        SkipKind::SlotNotOnBody => "the snapshot has a slot this body does not",
        SkipKind::ListTruncated => "the snapshot read only part of the list, so the rest cannot be rebuilt",
        SkipKind::ProfileNotDecodable => "a stored profile does not decode, and an undecodable one is never sent",
        SkipKind::ListTooLong => "the snapshot has more profiles than this body holds",
    }
}

/// A plan the CLI returned for `request`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedExt {
    pub(crate) request: ExtRestoreRequest,
    pub(crate) plan: ExtPlanJson,
    pub(crate) cli_log: String,
}

impl PlannedExt {
    /// Every item that would be written.
    pub(crate) fn writes(&self) -> Vec<&ExtItemJson> {
        self.plan.items.iter().filter(|i| matches!(i.action, ExtActionJson::Write { .. })).collect()
    }

    /// The FTP list is among the writes: it is rebuilt whole, and a failure part-way leaves it partial.
    pub(crate) fn rebuilds_ftp(&self) -> bool {
        self.writes().iter().any(|i| i.category == Category::Ftp)
    }

    /// Why this plan cannot be offered for confirmation, if it cannot. The one gate: the button, the
    /// state machine and the words beside the button all read it.
    pub(crate) fn blocked_reason(&self) -> Option<String> {
        let writes = self.writes().len();
        if let Some((claimed, items)) = self.plan.count_disagreement() {
            return Some(format!("The plan's own count of changes ({claimed}) disagrees with its items ({items}), so it cannot be trusted."));
        }
        if writes == 0 {
            return Some("This plan would change nothing, so there is nothing to restore.".to_string());
        }
        if self.plan.items.iter().any(|i| i.action == ExtActionJson::Unknown) {
            return Some("The plan contains an action this program does not understand, so it cannot be applied.".to_string());
        }
        None
    }

    pub(crate) fn applicable(&self) -> bool {
        self.blocked_reason().is_none()
    }
}

/// Extended restore, as a guarded write.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExtFlow;

impl Flow for ExtFlow {
    type Request = ExtRestoreRequest;
    type Plan = PlannedExt;
    type Aside = NoAside;
    type Report = ExtApplyJson;

    const APPLY_WORD: &'static str = "extended restore";
    const CHOOSE_FIRST: &'static str = "Choose a connected body, one of its stored snapshots and at least one category first.";

    fn request_target(r: &ExtRestoreRequest) -> &CameraRow {
        &r.target
    }
    fn plan_request(p: &PlannedExt) -> &ExtRestoreRequest {
        &p.request
    }
    fn aside_plan(a: &NoAside) -> &PlannedExt {
        match *a {}
    }
    fn plan_digest(p: &PlannedExt) -> &str {
        &p.plan.digest
    }
    fn plan_summary(p: &PlannedExt) -> &str {
        &p.plan.summary
    }
    fn applicable(p: &PlannedExt) -> bool {
        p.applicable()
    }
    fn plan_cmd(request: ExtRestoreRequest) -> Cmd {
        Cmd::Plan(PlanJob::RestoreExt(request))
    }
    fn apply_cmd(request: ExtRestoreRequest, digest: String, summary: String) -> Cmd {
        Cmd::Apply(ApplyJob::RestoreExt { request, digest, summary })
    }
}

/// The CLI arguments for a plan (`confirmed` is `None`) or an apply: `nikon_fleet::gui_argv`, which the
/// CLI's own tests run through the real parser.
pub(crate) fn ext_argv(data_dir: &Path, request: &ExtRestoreRequest, confirmed: Option<(&str, &str)>) -> Result<Vec<String>, String> {
    let dir = data_dir
        .to_str()
        .ok_or_else(|| format!("the data directory {} is not valid UTF-8", data_dir.display()))?;
    let source = snapshot_path_for_cli(data_dir, &request.source_file)?;
    let only: Option<BTreeSet<String>> = request.only.as_ref().map(|s| s.iter().map(|c| c.tag().to_string()).collect());
    gui_argv::restore_extended_argv(dir, &request.target.serial, only.as_ref(), confirmed, &source).map_err(|e| e.to_string())
}

pub(crate) fn do_ext_plan(data_dir: &Path, request: &ExtRestoreRequest) -> Result<(ExtPlanJson, String), String> {
    ext_plan_via(&locate_cli()?, data_dir, request, &list_cameras, RESTORE_PLAN_TIMEOUT)
}

/// Ask the CLI for a plan. Reads the body, writes nothing. Unlike a MAID restore, a plan the CLI exits
/// non-zero after printing is not a plan: its only such exit is a refusal, which prints none.
pub(crate) fn ext_plan_via(
    cli: &Path,
    data_dir: &Path,
    request: &ExtRestoreRequest,
    scan: &dyn Fn() -> CameraList,
    timeout: Duration,
) -> Result<(ExtPlanJson, String), String> {
    let (plan, log) =
        plan_via::<ExtPlanJson>("Restore", cli, || ext_argv(data_dir, request, None), &request.target, scan, timeout, PlanFailure::NoPlan)?;
    check_ext_plan_is_for(&plan, request)?;
    Ok((plan, log))
}

/// A plan is shown and confirmed only if it is for the body asked for and made from the snapshot asked
/// for. (Its `camera` is the snapshot's identity, so the firmware is not compared: the library has
/// already held the live body to the snapshot's model and serial.)
pub(crate) fn check_ext_plan_is_for(plan: &ExtPlanJson, request: &ExtRestoreRequest) -> Result<(), String> {
    let t = &request.target;
    if (&plan.camera.model, &plan.camera.serial) != (&t.model, &t.serial) {
        return Err(format!(
            "the plan is for {} {}, not for the {} {} that was asked for; nothing was written",
            plan.camera.model, plan.camera.serial, t.model, t.serial
        ));
    }
    if plan.source != request.source_file {
        return Err(format!(
            "the plan is made from {}, not from the {} that was asked for; nothing was written",
            nikon_fleet::ops::render::printable(&plan.source),
            nikon_fleet::ops::render::printable(&request.source_file)
        ));
    }
    // Every item of a plan for a selection is in that selection or reported as not selected.
    if let Some(only) = &request.only {
        if let Some(i) = plan.items.iter().find(|i| !only.contains(&i.category) && !matches!(i.action, ExtActionJson::NotSelected)) {
            return Err(format!("the plan acts on {} although {} was not asked for; nothing was written", i.label, category_words(i.category).0));
        }
    }
    if let Some((claimed, items)) = plan.count_disagreement() {
        return Err(format!("the plan's own count of changes ({claimed}) disagrees with its items ({items}); nothing was written"));
    }
    Ok(())
}

/// How long an extended apply may run before the GUI kills it. Longer than a plain apply on purpose:
/// besides the restore point and read-back (each a full capture, now with the extended section), an
/// FTP rebuild deletes the whole list and then adds up to 20 profiles, each several transactions
/// with a read-back; a kill part-way leaves the camera's list emptied or partial (reported
/// Indeterminate, restore point named), which is far worse than waiting. UNMEASURED: no run on a
/// real body has timed a full 20-profile rebuild, so this is a generous bound, not a measurement
/// (twice the plain apply's 300 s); a test pins that it exceeds the plain apply and the teardown
/// watchdog's worst case. The MCP server has its own constant for the same work
/// (`Deadlines::extended_apply`).
pub(crate) const EXT_RESTORE_APPLY_TIMEOUT: Duration = Duration::from_secs(600);

pub(crate) fn do_ext_apply(data_dir: &Path, request: &ExtRestoreRequest, digest: &str, summary: &str) -> ApplyOutcome<ExtApplyJson> {
    match locate_cli() {
        Ok(cli) => ext_apply_via(&cli, data_dir, request, digest, summary, &list_cameras, EXT_RESTORE_APPLY_TIMEOUT),
        Err(message) => no_cli(message),
    }
}

/// Apply exactly the plan that was confirmed: the CLI is given its digest and summary and refuses if
/// the body or the snapshot no longer give it.
pub(crate) fn ext_apply_via(
    cli: &Path,
    data_dir: &Path,
    request: &ExtRestoreRequest,
    digest: &str,
    summary: &str,
    scan: &dyn Fn() -> CameraList,
    timeout: Duration,
) -> ApplyOutcome<ExtApplyJson> {
    apply_via::<ExtApplyJson>("Restore", cli, || ext_argv(data_dir, request, Some((digest, summary))), &request.target, digest, scan, timeout)
}

// What the person is shown, as text with a level. Pure, so each is tested without a window.

/// The text of the confirmation for an apply: it names the body and the snapshot, states what will be
/// written and carries the plan's summary, so what is approved is what is sent.
pub(crate) fn ext_confirm_text(p: &PlannedExt) -> String {
    let source = format!("stored in {}", nikon_fleet::ops::render::printable(&p.request.source_file));
    let mut s = confirm_head(&p.request.target, &source, &p.plan.summary);
    let n = p.writes().len();
    s.push_str(&format!("\n{n} item{} will be written to the camera; everything else in the plan is left as it is.", if n == 1 { "" } else { "s" }));
    if p.rebuilds_ftp() {
        s.push_str(
            "\nThe FTP profile list is REBUILT: every profile stored in the camera is deleted first, then the snapshot's are added. \
             If that fails part-way the list is left partial; the restore point holds the original list.",
        );
    }
    s.push_str(CONFIRM_TAIL);
    s
}

/// A readable account of an extended restore report: what was written, what was not, and what the
/// read-back found.
pub(crate) fn ext_report_lines(r: &ExtApplyJson) -> Vec<(Level, String)> {
    let mut out = Vec::new();
    let mut line = |level, text: String| out.push((level, text));
    if let Some(aftermath) = failed_aftermath(r) {
        line(Level::Error, aftermath.to_string());
    }
    if r.no_op {
        line(Level::Info, "Nothing to write: the camera already holds everything the plan restores. No restore point was taken.".to_string());
    }
    let verified = r.results.iter().filter(|x| x.outcome == ExtOutcomeJson::Written { verified: true }).count();
    if verified > 0 {
        line(Level::Info, format!("Written and read back: {verified} item{}.", if verified == 1 { "" } else { "s" }));
    }
    for x in &r.results {
        let label = nikon_fleet::ops::render::printable(&x.label);
        match &x.outcome {
            ExtOutcomeJson::Written { verified: true } => {}
            ExtOutcomeJson::Written { verified: false } => line(Level::Warn, format!("Written, but its own read-back did not confirm it: {label}.")),
            ExtOutcomeJson::Failed { message } => line(Level::Error, format!("Failed: {label}: {}", nikon_fleet::ops::render::printable(message))),
            ExtOutcomeJson::Indeterminate { message } => line(
                Level::Error,
                format!("May or may not have changed: {label}: {}", nikon_fleet::ops::render::printable(message)),
            ),
            ExtOutcomeJson::NotAttempted { because } => {
                line(Level::Warn, format!("Not attempted: {label}: {}", nikon_fleet::ops::render::printable(because)))
            }
            // A write whose end this program cannot read may have changed the camera: as loud as `Indeterminate`.
            ExtOutcomeJson::Unknown => line(
                Level::Error,
                format!("{label} ended in a way this program does not know; check the camera against the restore point."),
            ),
        }
    }
    if !r.still_differing.is_empty() {
        line(
            Level::Warn,
            format!("After the writes the camera still differs from the snapshot in: {}.", r.still_differing.iter().map(|i| nikon_fleet::ops::render::printable(i)).collect::<Vec<_>>().join(", ")),
        );
    }
    if let Some(why) = &r.after_read_failed {
        line(Level::Warn, format!("{}: nothing written could be checked.", nikon_fleet::ops::render::printable(why)));
    }
    out
}

/// The extended restore window's state.
#[derive(Debug)]
pub(crate) struct ExtRestoreUi {
    pub open: bool,
    /// The chosen target, by identity; the live row is found among the discovered cameras.
    pub target_key: Option<(String, String)>,
    pub source_file: Option<String>,
    /// The categories ticked. All of them is a restore of everything (no `--only`); none cannot be planned.
    pub categories: BTreeSet<Category>,
    pub(crate) stage: Stage<ExtFlow>,
    /// Why a plan or confirmation went away, or how a plan failed.
    pub notice: String,
}

impl Default for ExtRestoreUi {
    fn default() -> Self {
        ExtRestoreUi {
            open: false,
            target_key: None,
            source_file: None,
            categories: Category::ALL.into_iter().collect(),
            stage: Stage::Idle,
            notice: String::new(),
        }
    }
}

impl GuardedUi for ExtRestoreUi {
    type F = ExtFlow;

    fn stage(&self) -> &Stage<ExtFlow> {
        &self.stage
    }
    fn stage_mut(&mut self) -> &mut Stage<ExtFlow> {
        &mut self.stage
    }
    fn notice_mut(&mut self) -> &mut String {
        &mut self.notice
    }

    fn request(&self, cameras: &[CameraRow]) -> Option<ExtRestoreRequest> {
        if self.categories.is_empty() {
            return None;
        }
        Some(ExtRestoreRequest {
            source_file: self.source_file.clone()?,
            target: self.target_camera(cameras)?.clone(),
            only: (self.categories.len() != Category::ALL.len()).then(|| self.categories.clone()),
        })
    }

    /// Nothing belongs to a (snapshot, camera) pair here: the ticks are visible on the form and are
    /// part of the request, so changing one withdraws the plan by itself (`sync`).
    fn sync_selection(&mut self, _cameras: &[CameraRow]) {}

    fn clear_selection(&mut self) {}
}

impl ExtRestoreUi {
    pub fn target_camera<'a>(&self, cameras: &'a [CameraRow]) -> Option<&'a CameraRow> {
        let (model, serial) = self.target_key.as_ref()?;
        cameras.iter().find(|c| &c.model == model && &c.serial == serial)
    }

    /// A plan came back. Dropped unless it is the one asked for and still wanted.
    pub fn plan_arrived(&mut self, request: ExtRestoreRequest, result: Result<(ExtPlanJson, String), String>) {
        self.plan_arrived_with(request, result, |_, request, (plan, cli_log)| PlannedExt { request, plan, cli_log });
    }

    /// Tick or untick one category.
    pub fn toggle(&mut self, category: Category) {
        if !self.categories.remove(&category) {
            self.categories.insert(category);
        }
    }

    /// Set the form to undo `offer` (a restore point and the categories the apply wrote) and plan it.
    /// The plan is read-only; the person reviews it and confirms it like any restore. The body must be
    /// connected, and no step may be in flight.
    pub fn start_undo(&mut self, offer: &UndoOffer, cameras: &[CameraRow]) -> Result<Cmd, String> {
        if matches!(self.stage, Stage::Applying(_) | Stage::Planning(_)) {
            return Err("Another restore step is still running.".to_string());
        }
        let Some(camera) = cameras.iter().find(|c| c.model == offer.target.model && c.serial == offer.target.serial) else {
            return Err(format!("{} {} is not connected: connect it and Discover, then undo.", offer.target.model, offer.target.serial));
        };
        if offer.undo_set.is_empty() {
            return Err("There is nothing to undo.".to_string());
        }
        let categories: BTreeSet<Category> = offer
            .undo_set
            .iter()
            .map(|t| Category::parse(t).map_err(|e| format!("The undo names something this program does not know: {e}")))
            .collect::<Result<_, _>>()?;
        self.open = true;
        self.target_key = Some((camera.model.clone(), camera.serial.clone()));
        self.source_file = Some(offer.restore_point.clone());
        self.categories = categories;
        self.stage = Stage::Idle;
        self.begin_plan(cameras)
    }
}

// ── The window ────────────────────────────────────────────────────────────

/// What a click in the window asks for. Collected while drawing and applied after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExtAction {
    Plan,
    AskApply,
    ConfirmApply,
    CancelConfirmation,
    Discard,
    Applied(AppliedAction),
}

fn draw_ext_plan(ui: &mut egui::Ui, p: &PlannedExt) {
    let plan = &p.plan;
    ui.heading(format!("{} {}", p.request.target.model, p.request.target.serial));
    ui.label(&plan.summary);
    if let Some(why) = p.blocked_reason() {
        ui.colored_label(WARN_COLOR, why);
    }
    for category in Category::ALL {
        let items: Vec<&ExtItemJson> = plan.items.iter().filter(|i| i.category == category).collect();
        if items.is_empty() {
            continue;
        }
        ui.add_space(4.0);
        ui.label(egui::RichText::new(category_words(category).0).strong());
        for i in items {
            let label = nikon_fleet::ops::render::printable(&i.label);
            match &i.action {
                ExtActionJson::Write { from, to } => {
                    let from = from.as_deref().map(nikon_fleet::ops::render::printable).unwrap_or_else(|| "?".to_string());
                    ui.label(egui::RichText::new(format!("WRITE  {label}: {from}  →  {}", nikon_fleet::ops::render::printable(to))).strong());
                }
                ExtActionJson::Unchanged => {
                    ui.label(egui::RichText::new(format!("same   {label}")).weak());
                }
                ExtActionJson::NotSelected => {
                    ui.label(egui::RichText::new(format!("not selected   {label}")).weak());
                }
                ExtActionJson::Skip { reason, detail } => {
                    let detail = detail.as_deref().map(|d| format!(" ({})", nikon_fleet::ops::render::printable(d))).unwrap_or_default();
                    ui.colored_label(WARN_COLOR, format!("skip   {label}: {}{detail}", skip_words(*reason)));
                }
                ExtActionJson::Unknown => {
                    ui.colored_label(ERROR_COLOR, format!("?      {label}: an action this program does not know"));
                }
            }
        }
    }
    if !p.cli_log.trim().is_empty() {
        egui::CollapsingHeader::new("CLI messages").id_salt("rx_plan_log").show(ui, |ui| {
            ui.label(egui::RichText::new(p.cli_log.trim_end()).monospace());
        });
    }
}

impl FleetApp {
    pub(crate) fn ui_restore_ext(&mut self, ctx: &egui::Context) {
        if !self.restore_ext.open {
            return;
        }
        let mut open = true;
        let mut action: Option<ExtAction> = None;
        let busy = self.busy();
        let data_ready = self.settings.data_dir.is_some();
        let data_dir = self.settings.data_dir.clone();
        let data_dir = data_dir.as_deref();

        egui::Window::new("Restore owner data from a stored snapshot")
            .open(&mut open)
            .default_size([700.0, 620.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                let r = &mut self.restore_ext;
                let cameras = &self.cameras;
                let listing = self.listing.as_ref();
                egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(720.0).show(ui, |ui| {
                    ui.label("Artist, Copyright, Image comment, IPTC presets, switches and FTP profiles. The settings restore does not touch these.");
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("1. The camera: a connected body").strong());
                    if cameras.is_empty() {
                        ui.label(egui::RichText::new("No camera is connected: use Discover first.").italics().weak());
                    }
                    ui.add_enabled_ui(!busy, |ui| {
                        let text = r.target_camera(cameras).map_or("choose a body…".to_string(), camera_label);
                        egui::ComboBox::from_id_salt("rx_target").selected_text(text).width(420.0).show_ui(ui, |ui| {
                            for c in cameras {
                                let picked = r.target_key.as_ref() == Some(&(c.model.clone(), c.serial.clone()));
                                if ui.selectable_label(picked, camera_label(c)).clicked() && !picked {
                                    r.target_key = Some((c.model.clone(), c.serial.clone()));
                                    r.source_file = None;
                                }
                            }
                        });
                    });
                    if r.target_key.is_some() && r.target_camera(cameras).is_none() {
                        ui.colored_label(WARN_COLOR, "That body is no longer connected.");
                    }
                    let refusal = r.target_camera(cameras).and_then(write_target_refusal);
                    if let Some(why) = &refusal {
                        ui.colored_label(ERROR_COLOR, why);
                    }

                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("2. The snapshot: one of that body's own").strong());
                    let all_rows: Vec<&SnapRow> = match (listing, r.target_camera(cameras)) {
                        (Some(l), Some(c)) => l.rows_for(&c.model, &c.serial).collect(),
                        _ => Vec::new(),
                    };
                    let hidden = all_rows.iter().filter(|row| row.name_lossy).count();
                    let rows: Vec<&SnapRow> = all_rows.iter().copied().filter(|row| !row.name_lossy).collect();
                    if r.target_camera(cameras).is_some() && rows.is_empty() {
                        ui.label(egui::RichText::new(if hidden > 0 { "No snapshot with a usable file name." } else { "This body has no stored snapshots." }).italics().weak());
                    }
                    if hidden > 0 {
                        ui.colored_label(WARN_COLOR, format!("{hidden} snapshot(s) of this body have file names that cannot be shown and are not offered."));
                    }
                    ui.add_enabled_ui(!busy, |ui| {
                        let text = r
                            .source_file
                            .as_ref()
                            .map(|f| rows.iter().find(|row| &row.file == f).map_or_else(|| f.clone(), |row| snapshot_label(row)))
                            .unwrap_or_else(|| "choose a snapshot…".to_string());
                        egui::ComboBox::from_id_salt("rx_source").selected_text(text).width(560.0).show_ui(ui, |ui| {
                            for row in &rows {
                                let picked = r.source_file.as_ref() == Some(&row.file);
                                if ui.selectable_label(picked, snapshot_label(row)).clicked() {
                                    r.source_file = Some(row.file.clone());
                                }
                            }
                        });
                    });
                    ui.label(
                        egui::RichText::new("A snapshot taken without its extended section (an older one, or captured with --no-extended) is refused when planned.")
                            .small()
                            .weak(),
                    );

                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("3. What to restore").strong());
                    ui.add_enabled_ui(!busy, |ui| {
                        for category in Category::ALL {
                            let (name, what) = category_words(category);
                            let mut ticked = r.categories.contains(&category);
                            if ui.checkbox(&mut ticked, format!("{name}: {what}")).changed() {
                                r.toggle(category);
                            }
                        }
                    });
                    if r.categories.is_empty() {
                        ui.colored_label(WARN_COLOR, "Tick at least one category.");
                    }

                    ui.add_space(6.0);
                    let planning_possible = !busy
                        && data_ready
                        && refusal.is_none()
                        && r.request(cameras).is_some()
                        && !matches!(r.stage(), Stage::Applying(_) | Stage::Planning(_));
                    ui.add_enabled_ui(planning_possible, |ui| {
                        if ui.button("Plan (reads the camera, writes nothing)").clicked() {
                            action = Some(ExtAction::Plan);
                        }
                    });
                    if !r.notice.is_empty() {
                        ui.colored_label(WARN_COLOR, &r.notice);
                    }
                    ui.separator();

                    match r.stage() {
                        Stage::Idle => {
                            ui.label(
                                egui::RichText::new("Choose a body, a snapshot and what to restore, then plan. Nothing is sent to the camera until you have read the plan and confirmed it.")
                                    .weak(),
                            );
                        }
                        Stage::Planning(_) => {
                            ui.label("Planning… the camera is being read.");
                        }
                        Stage::Planned(p) => {
                            draw_ext_plan(ui, p);
                            ui.separator();
                            ui.horizontal(|ui| {
                                ui.add_enabled_ui(!busy && p.applicable(), |ui| {
                                    if ui.button("Restore to camera…").clicked() {
                                        action = Some(ExtAction::AskApply);
                                    }
                                });
                                if ui.button("Discard plan").clicked() {
                                    action = Some(ExtAction::Discard);
                                }
                            });
                        }
                        Stage::Aside(a) => match *a {},
                        Stage::ConfirmApply(p) => {
                            draw_ext_plan(ui, p);
                            ui.separator();
                            ui.colored_label(WARN_COLOR, ext_confirm_text(p));
                            ui.add_enabled_ui(!busy, |ui| {
                                ui.horizontal(|ui| {
                                    if ui.button("Confirm: write to camera").clicked() {
                                        action = Some(ExtAction::ConfirmApply);
                                    }
                                    if ui.button("Cancel").clicked() {
                                        action = Some(ExtAction::CancelConfirmation);
                                    }
                                });
                            });
                        }
                        Stage::Applying(req) => {
                            ui.colored_label(
                                WARN_COLOR,
                                format!("Writing to {} {}… keep it connected and do not close this program.", req.target.model, req.target.serial),
                            );
                        }
                        Stage::Done(a) => {
                            let t = &a.request.target;
                            ui.label(egui::RichText::new(format!("{}  →  {} {}", a.request.source_file, t.model, t.serial)).weak());
                            let chosen = draw_applied_outcome(ui, t, "rx_applied", &a.outcome, busy, data_dir, ext_report_lines, |ui, report| {
                                if !report.results.is_empty() {
                                    egui::CollapsingHeader::new(format!("Items written or tried ({})", report.results.len())).id_salt("rx_results").show(ui, |ui| {
                                        for x in &report.results {
                                            ui.label(format!("{}  ({})", nikon_fleet::ops::render::printable(&x.label), category_words(x.category).0));
                                        }
                                    });
                                }
                            });
                            if let Some(c) = chosen {
                                action = Some(ExtAction::Applied(c));
                            }
                            ui.separator();
                            if ui.button("Close this result").clicked() {
                                action = Some(ExtAction::Discard);
                            }
                        }
                    }
                });
            });
        self.restore_ext.open = open;
        if let Some(a) = action {
            self.ext_act(ctx, a);
        }
    }

    fn ext_act(&mut self, ctx: &egui::Context, action: ExtAction) {
        match action {
            ExtAction::Plan => match self.restore_ext.begin_plan(&self.cameras) {
                Ok(cmd) => self.send_restore_ext(cmd),
                Err(e) => self.restore_ext.notice = e,
            },
            ExtAction::AskApply => self.restore_ext.ask_apply(),
            ExtAction::ConfirmApply => {
                if let Some(cmd) = self.restore_ext.confirm_apply() {
                    self.send_restore_ext(cmd);
                }
            }
            ExtAction::CancelConfirmation => self.restore_ext.cancel_confirmation(),
            ExtAction::Discard => self.restore_ext.discard(),
            ExtAction::Applied(a) => self.applied_action(ctx, a),
        }
    }

    /// Hand a command to the worker. If the worker is gone the command was not delivered, and the
    /// window must not wait for an answer that cannot come.
    pub(crate) fn send_restore_ext(&mut self, cmd: Cmd) {
        if !self.send(cmd) {
            self.restore_ext.not_sent();
        }
    }

    /// Undo an extended restore: this window opens on its restore point with the categories the apply
    /// wrote, and the plan is made (read-only). The person reviews it and confirms it as any restore.
    pub(crate) fn start_undo_ext(&mut self, offer: UndoOffer) {
        if self.busy() {
            self.status = Status::new(Level::Warn, "Another camera operation is running; try the undo when it has finished.");
            return;
        }
        // The restore point is a file in the data directory the apply used.
        if self.settings.data_dir.as_deref() != Some(offer.data_dir.as_path()) {
            self.restore_ext.open = true;
            self.restore_ext.notice = format!(
                "The data directory changed since this apply: its restore point {} is in {}. Switch back to undo it.",
                offer.restore_point,
                offer.data_dir.display()
            );
            return;
        }
        match self.restore_ext.start_undo(&offer, &self.cameras) {
            Ok(cmd) => self.send_restore_ext(cmd),
            Err(e) => {
                self.restore_ext.open = true;
                self.restore_ext.notice = e;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{cli_error, row, scan_of, test_app, usb};
    use nikon_fleet::ops::apply::{EXIT_REFUSED_BEFORE_WRITE, EXIT_STALE_PLAN};
    use nikon_fleet::ops::restore_extended::{ExtendedApplyReport, ExtendedPlanReport, ItemAction, ItemOutcome, ItemResult, PlanItem};
    use nikon_fleet::snapshot::Camera;

    const FILE: &str = "Z_9_T1_20261106T010000Z.json";

    fn target() -> CameraRow {
        row("Z 9", "T1", 1)
    }

    fn item(id: &str, category: Category, action: ItemAction) -> PlanItem {
        PlanItem { id: id.into(), label: format!("label of {id}"), category, action, fingerprint: String::new() }
    }

    fn write(from: &str, to: &str) -> ItemAction {
        ItemAction::Write { from: Some(from.into()), to: to.into() }
    }

    /// A plan as the library serialises it, so that the GUI's reading is held to the real shape.
    fn library_plan(items: Vec<PlanItem>) -> ExtendedPlanReport {
        let would_change = items.iter().filter(|i| matches!(i.action, ItemAction::Write { .. })).count();
        ExtendedPlanReport {
            camera: Camera { model: "Z 9".into(), serial: "T1".into(), firmware: "5.31".into() },
            source: FILE.into(),
            would_change,
            summary: format!("Z 9 T1: restore-extended from {FILE}: {would_change} would change"),
            digest: "d1".into(),
            items,
        }
    }

    fn plan_json(items: Vec<PlanItem>) -> ExtPlanJson {
        serde_json::from_value(serde_json::to_value(library_plan(items)).unwrap()).expect("the GUI reads the library's plan")
    }

    fn mixed_items() -> Vec<PlanItem> {
        vec![
            item("0xd072", Category::Strings, write("Someone", "Brian")),
            item("0xd073", Category::Strings, ItemAction::Unchanged),
            item("0xd090", Category::Strings, ItemAction::Skip { reason: SkipKind::SnapshotHasNoValue, detail: Some("older snapshot".into()) }),
            item("iptc:1", Category::Iptc, ItemAction::NotSelected),
            item("ftp", Category::Ftp, write("1 profile(s)", "3 profile(s)")),
        ]
    }

    fn request(only: Option<&[Category]>) -> ExtRestoreRequest {
        ExtRestoreRequest { source_file: FILE.into(), target: target(), only: only.map(|c| c.iter().copied().collect()) }
    }

    fn planned(items: Vec<PlanItem>) -> PlannedExt {
        PlannedExt { request: request(None), plan: plan_json(items), cli_log: String::new() }
    }

    fn ready() -> (ExtRestoreUi, Vec<CameraRow>) {
        let ui = ExtRestoreUi { open: true, target_key: Some(("Z 9".into(), "T1".into())), source_file: Some(FILE.into()), ..ExtRestoreUi::default() };
        (ui, vec![target()])
    }

    fn with_plan() -> (ExtRestoreUi, Vec<CameraRow>) {
        let (mut ui, cameras) = ready();
        ui.sync(&cameras);
        let Cmd::Plan(PlanJob::RestoreExt(asked)) = ui.begin_plan(&cameras).unwrap() else { panic!("a plan command") };
        ui.plan_arrived(asked, Ok((plan_json(mixed_items()), String::new())));
        assert!(matches!(ui.stage(), Stage::Planned(_)));
        (ui, cameras)
    }

    // ── what the library emits is what the GUI reads ──

    #[test]
    fn the_librarys_plan_and_report_are_read_with_every_action_and_outcome() {
        let plan = plan_json(mixed_items());
        assert_eq!(plan.would_change, 2);
        assert_eq!(plan.items[0].action, ExtActionJson::Write { from: Some("Someone".into()), to: "Brian".into() });
        assert_eq!(plan.items[1].action, ExtActionJson::Unchanged);
        assert_eq!(plan.items[2].action, ExtActionJson::Skip { reason: SkipKind::SnapshotHasNoValue, detail: Some("older snapshot".into()) });
        assert_eq!(plan.items[3].action, ExtActionJson::NotSelected);
        assert_eq!(plan.items[4].category, Category::Ftp);
        // A skip with no detail, and a write with no old value (`from: null`), still read.
        let bare = plan_json(vec![
            item("a", Category::Strings, ItemAction::Skip { reason: SkipKind::BodyLacksIt, detail: None }),
            item("b", Category::Strings, ItemAction::Write { from: None, to: "x".into() }),
        ]);
        assert_eq!(bare.items[0].action, ExtActionJson::Skip { reason: SkipKind::BodyLacksIt, detail: None });
        assert_eq!(bare.items[1].action, ExtActionJson::Write { from: None, to: "x".into() });

        let result = |outcome| ItemResult { id: "i".into(), label: "l".into(), category: Category::Ftp, outcome };
        let report = ExtendedApplyReport {
            status: ApplyStatus::Failed,
            no_op: false,
            plan_digest: "d1".into(),
            restore_point: Some(nikon_fleet::ops::apply::RestorePoint { file: "rp.json".into(), label: "restore-ext".into() }),
            undo_set: vec!["strings".into(), "ftp".into()],
            results: vec![
                result(ItemOutcome::Written { verified: true }),
                result(ItemOutcome::Written { verified: false }),
                result(ItemOutcome::Failed { message: "no".into() }),
                result(ItemOutcome::Indeterminate { message: "maybe".into() }),
                result(ItemOutcome::NotAttempted { because: "earlier".into() }),
            ],
            still_differing: vec!["ftp".into()],
            after_read_failed: Some("could not read".into()),
        };
        let read: ExtApplyJson = serde_json::from_value(serde_json::to_value(&report).unwrap()).expect("the GUI reads the library's report");
        assert_eq!((read.status, read.plan_digest.as_str(), read.undo_set.len()), (ApplyStatus::Failed, "d1", 2));
        assert_eq!(read.restore_point, Some(RestorePointJson { file: "rp.json".into(), label: "restore-ext".into() }));
        assert_eq!(
            read.results.iter().map(|r| r.outcome.clone()).collect::<Vec<_>>(),
            [
                ExtOutcomeJson::Written { verified: true },
                ExtOutcomeJson::Written { verified: false },
                ExtOutcomeJson::Failed { message: "no".into() },
                ExtOutcomeJson::Indeterminate { message: "maybe".into() },
                ExtOutcomeJson::NotAttempted { because: "earlier".into() },
            ]
        );
        assert_eq!(read.undo_kind(), UndoKind::Categories);
        // A report with no restore point (nothing to change) omits the key entirely.
        let none = ExtendedApplyReport { restore_point: None, undo_set: vec![], results: vec![], still_differing: vec![], after_read_failed: None, no_op: true, status: ApplyStatus::AppliedVerified, ..report };
        let read: ExtApplyJson = serde_json::from_value(serde_json::to_value(&none).unwrap()).unwrap();
        assert!(read.restore_point.is_none() && read.no_op);
    }

    #[test]
    fn an_action_or_outcome_this_build_does_not_know_is_kept_and_blocks_the_plan() {
        let plan: ExtPlanJson = serde_json::from_value(serde_json::json!({
            "camera": {"model": "Z 9", "serial": "T1", "firmware": "5.31"},
            "source": FILE, "would_change": 1, "summary": "s", "digest": "d",
            "items": [
                {"id": "a", "label": "A", "category": "strings", "action": "write", "from": "x", "to": "y"},
                {"id": "b", "label": "B", "category": "ftp", "action": "teleport"}
            ]
        }))
        .unwrap();
        assert_eq!(plan.items[1].action, ExtActionJson::Unknown);
        let p = PlannedExt { request: request(None), plan, cli_log: String::new() };
        assert!(p.blocked_reason().unwrap().contains("does not understand"));
        let r: ExtResultJson = serde_json::from_value(serde_json::json!({"id": "i", "label": "l", "category": "ftp", "outcome": "levitated"})).unwrap();
        assert_eq!(r.outcome, ExtOutcomeJson::Unknown);
    }

    #[test]
    fn the_mirror_structs_read_every_field_the_library_writes() {
        // The library's own plan and apply report, serialised: every key must be one the mirror reads (in tests the
        // mirrors deny unknown fields), so a field added on the library side fails here instead of being dropped.
        let plan = serde_json::to_value(library_plan(mixed_items())).unwrap();
        let parsed: ExtPlanJson = serde_json::from_value(plan.clone()).expect("the mirror reads every key of the library's plan");
        assert_eq!(parsed.digest, "d1");
        let apply = ExtendedApplyReport {
            status: ApplyStatus::AppliedVerified,
            no_op: false,
            plan_digest: "d1".into(),
            restore_point: Some(nikon_fleet::ops::apply::RestorePoint { file: "rp.json".into(), label: "restore-ext".into() }),
            undo_set: vec!["strings".into()],
            results: vec![ItemResult { id: "a".into(), label: "A".into(), category: Category::Strings, outcome: ItemOutcome::Written { verified: true } }],
            still_differing: vec!["ftp".into()],
            after_read_failed: Some("why".into()),
        };
        let apply_json = serde_json::to_value(&apply).unwrap();
        let keys = |v: &serde_json::Value| v.as_object().unwrap().keys().cloned().collect::<BTreeSet<_>>();
        let parsed: ExtApplyJson = serde_json::from_value(apply_json.clone()).expect("the mirror reads every key of the library's report");
        assert_eq!(parsed.after_read_failed.as_deref(), Some("why"), "the fixture sets every optional key, so none is skipped");
        assert_eq!(keys(&apply_json).len(), 8, "the report has eight keys with every optional one set");
        // And the guard bites: a key the mirror does not know is refused in tests.
        for (name, mut v) in [("plan", plan), ("apply report", apply_json)] {
            v.as_object_mut().unwrap().insert("a_field_added_later".into(), serde_json::json!(1));
            let refused = if name == "plan" { serde_json::from_value::<ExtPlanJson>(v).err() } else { serde_json::from_value::<ExtApplyJson>(v).err() };
            assert!(refused.is_some_and(|e| e.to_string().contains("a_field_added_later")), "{name}");
        }
    }

    #[test]
    fn every_skip_reason_and_category_the_library_has_is_worded() {
        // No wildcard: a new variant fails to compile here until it is listed (and then worded).
        for reason in [
            SkipKind::SnapshotHasNoValue,
            SkipKind::BodyLacksIt,
            SkipKind::BodyUnreadable,
            SkipKind::ValueNotWritable,
            SkipKind::ModelNotConfirmed,
            SkipKind::RecordNotWritable,
            SkipKind::SlotNotOnBody,
            SkipKind::ListTruncated,
            SkipKind::ProfileNotDecodable,
            SkipKind::ListTooLong,
        ] {
            match reason {
                SkipKind::SnapshotHasNoValue
                | SkipKind::BodyLacksIt
                | SkipKind::BodyUnreadable
                | SkipKind::ValueNotWritable
                | SkipKind::ModelNotConfirmed
                | SkipKind::RecordNotWritable
                | SkipKind::SlotNotOnBody
                | SkipKind::ListTruncated
                | SkipKind::ProfileNotDecodable
                | SkipKind::ListTooLong => assert!(!skip_words(reason).is_empty(), "{reason:?}"),
            }
        }
        for c in Category::ALL {
            let (name, what) = category_words(c);
            assert!(!name.is_empty() && !what.is_empty(), "{c:?}");
        }
    }

    // ── the argv ──

    #[test]
    fn a_plan_is_a_dry_run_and_an_apply_presents_the_confirmed_plan() {
        let dir = Path::new("/d");
        let plan = ext_argv(dir, &request(None), None).unwrap();
        assert_eq!(plan[..6], ["--data-dir=/d", "restore-extended", "--json", "--serial=T1", "--dry-run", "--"].map(String::from));
        assert!(plan.last().unwrap().ends_with(FILE));
        let apply = ext_argv(dir, &request(None), Some(("dig", "sum"))).unwrap();
        assert!(apply.contains(&"--expect-digest=dig".to_string()) && apply.contains(&"--expect-summary=sum".to_string()));
        assert!(!apply.contains(&"--dry-run".to_string()), "an apply is never a dry run");
    }

    #[test]
    fn ticking_every_category_is_no_flag_and_fewer_is_one_flag_each() {
        let dir = Path::new("/d");
        let only = |a: &[&String]| a.iter().filter(|x| x.starts_with("--only=")).count();
        let all = ext_argv(dir, &request(None), None).unwrap();
        assert_eq!(only(&all.iter().collect::<Vec<_>>()), 0);
        let two = ext_argv(dir, &request(Some(&[Category::Ftp, Category::Strings])), None).unwrap();
        assert!(two.contains(&"--only=ftp".to_string()) && two.contains(&"--only=strings".to_string()));
        assert_eq!(only(&two.iter().collect::<Vec<_>>()), 2);
        assert!(ext_argv(dir, &request(Some(&[])), None).is_err(), "a selection of nothing is never rendered as 'everything'");
    }

    // ── the gate ──

    #[test]
    fn a_plan_is_applicable_at_one_change_and_not_at_zero_and_a_skip_does_not_block() {
        let one = planned(vec![item("a", Category::Strings, write("x", "y"))]);
        assert!(one.applicable(), "one change is enough (the boundary)");
        let none = planned(vec![item("a", Category::Strings, ItemAction::Unchanged)]);
        assert!(none.blocked_reason().unwrap().contains("nothing to restore"));
        let skipped = planned(vec![
            item("a", Category::Strings, write("x", "y")),
            item("b", Category::Iptc, ItemAction::Skip { reason: SkipKind::ModelNotConfirmed, detail: None }),
        ]);
        assert!(skipped.applicable(), "a skip is a typed, visible decision, not a reason to refuse the rest");
        let mut lying = planned(vec![item("a", Category::Strings, write("x", "y"))]);
        lying.plan.would_change = 2;
        assert!(lying.blocked_reason().unwrap().contains("disagrees"));
    }

    #[test]
    fn a_plan_is_checked_against_the_body_the_snapshot_and_the_selection_that_were_asked_for() {
        let good = plan_json(mixed_items());
        assert!(check_ext_plan_is_for(&good, &request(None)).is_ok());
        let mut other_body = good.clone();
        other_body.camera.serial = "T2".into();
        assert!(check_ext_plan_is_for(&other_body, &request(None)).unwrap_err().contains("T2"));
        let mut other_model = good.clone();
        other_model.camera.model = "Z6_3".into();
        assert!(check_ext_plan_is_for(&other_model, &request(None)).is_err());
        let mut other_file = good.clone();
        other_file.source = "elsewhere.json".into();
        assert!(check_ext_plan_is_for(&other_file, &request(None)).unwrap_err().contains("elsewhere.json"));
        // A different firmware in the plan's `camera` (it is the snapshot's) is not a mismatch.
        let mut fw = good.clone();
        fw.camera.firmware = "1.00".into();
        assert!(check_ext_plan_is_for(&fw, &request(None)).is_ok());
        // A plan that writes outside the ticked categories is refused; one that reports them as not selected is fine.
        assert!(check_ext_plan_is_for(&good, &request(Some(&[Category::Strings]))).unwrap_err().contains("FTP profiles"), "ftp is written but was not asked for");
        let narrow = plan_json(vec![item("a", Category::Strings, write("x", "y")), item("f", Category::Ftp, ItemAction::NotSelected)]);
        assert!(check_ext_plan_is_for(&narrow, &request(Some(&[Category::Strings]))).is_ok());
        let mut counted = good.clone();
        counted.would_change = 9;
        assert!(check_ext_plan_is_for(&counted, &request(None)).unwrap_err().contains("disagrees"));
    }

    #[test]
    fn the_plans_count_of_changes_must_equal_its_write_items_exactly_and_both_gates_read_the_same_rule() {
        let mut p = plan_json(mixed_items());
        let writes = p.items.iter().filter(|i| matches!(i.action, ExtActionJson::Write { .. })).count();
        assert_eq!(writes, 2, "the fixture writes two items");
        for (claimed, disagrees) in [(writes - 1, true), (writes, false), (writes + 1, true)] {
            p.would_change = claimed;
            assert_eq!(p.count_disagreement(), disagrees.then_some((claimed, writes)), "claimed {claimed}");
            // the pre-confirmation gate and the check on a fresh plan agree about it
            assert_eq!(check_ext_plan_is_for(&p, &request(None)).is_err(), disagrees, "check, claimed {claimed}");
            let gate = PlannedExt { request: request(None), plan: p.clone(), cli_log: String::new() };
            assert_eq!(gate.blocked_reason().is_some_and(|r| r.contains("disagrees")), disagrees, "gate, claimed {claimed}");
        }
        // zero items, zero claimed: consistent (the gate blocks it for another reason: nothing to restore)
        let empty = plan_json(vec![]);
        assert_eq!(empty.count_disagreement(), None);
        let mut lying_empty = empty.clone();
        lying_empty.would_change = 1;
        assert_eq!(lying_empty.count_disagreement(), Some((1, 0)));
    }

    // ── the confirmation and the result, in words ──

    #[test]
    fn the_confirmation_names_the_body_the_snapshot_and_the_count_and_warns_only_of_an_ftp_rebuild() {
        let with_ftp = ext_confirm_text(&planned(mixed_items()));
        for needle in ["Z 9", "T1", FILE, "2 items will be written", "REBUILT", "restore point"] {
            assert!(with_ftp.contains(needle), "{needle}: {with_ftp}");
        }
        let without = ext_confirm_text(&planned(vec![item("a", Category::Strings, write("x", "y"))]));
        assert!(without.contains("1 item will be written") && !without.contains("REBUILT"), "{without}");
    }

    fn report(f: impl FnOnce(&mut ExtApplyJson)) -> ExtApplyJson {
        let mut r = ExtApplyJson {
            status: ApplyStatus::AppliedVerified,
            no_op: false,
            plan_digest: "d1".into(),
            restore_point: Some(RestorePointJson { file: "rp.json".into(), label: "restore-ext".into() }),
            undo_set: vec!["strings".into()],
            results: vec![ExtResultJson { id: "a".into(), label: "Artist".into(), category: Category::Strings, outcome: ExtOutcomeJson::Written { verified: true } }],
            still_differing: vec![],
            after_read_failed: None,
        };
        f(&mut r);
        r
    }

    fn says(lines: &[(Level, String)], level: Level, needle: &str) -> bool {
        lines.iter().any(|(l, t)| *l == level && t.contains(needle))
    }

    #[test]
    fn every_way_an_extended_restore_ends_is_worded_at_its_level() {
        let ok = ext_report_lines(&report(|_| {}));
        assert!(says(&ok, Level::Info, "Written and read back: 1 item."), "{ok:?}");
        assert!(ok.iter().all(|(l, _)| *l == Level::Info), "a verified apply has nothing to warn about: {ok:?}");

        let noop = ext_report_lines(&report(|r| {
            r.no_op = true;
            r.results.clear();
            r.restore_point = None;
        }));
        assert!(says(&noop, Level::Info, "Nothing to write"), "{noop:?}");

        let mixed = ext_report_lines(&report(|r| {
            r.status = ApplyStatus::Failed;
            r.results = [
                ExtOutcomeJson::Written { verified: false },
                ExtOutcomeJson::Failed { message: "refused".into() },
                ExtOutcomeJson::Indeterminate { message: "timed out".into() },
                ExtOutcomeJson::NotAttempted { because: "earlier".into() },
                ExtOutcomeJson::Unknown,
            ]
            .into_iter()
            .map(|outcome| ExtResultJson { id: "x".into(), label: "Item".into(), category: Category::Ftp, outcome })
            .collect();
            r.still_differing = vec!["ftp".into()];
            r.after_read_failed = Some("the body could not be read after the writes".into());
        }));
        assert!(says(&mixed, Level::Error, "restore point"), "a failed apply points at the restore point: {mixed:?}");
        assert!(says(&mixed, Level::Warn, "did not confirm"), "{mixed:?}");
        assert!(says(&mixed, Level::Error, "Failed: Item: refused"), "{mixed:?}");
        assert!(says(&mixed, Level::Error, "May or may not have changed: Item: timed out"), "{mixed:?}");
        assert!(says(&mixed, Level::Warn, "Not attempted: Item: earlier"), "{mixed:?}");
        assert!(says(&mixed, Level::Error, "does not know"), "an outcome it cannot read is as loud as an indeterminate one: {mixed:?}");
        assert!(!says(&mixed, Level::Warn, "does not know"), "{mixed:?}");
        assert!(says(&mixed, Level::Warn, "still differs from the snapshot in: ftp"), "{mixed:?}");
        assert!(says(&mixed, Level::Warn, "nothing written could be checked"), "{mixed:?}");
    }

    // ── the state machine ──

    #[test]
    fn nothing_is_asked_of_the_camera_until_a_body_a_snapshot_and_a_category_are_chosen() {
        let mut ui = ExtRestoreUi::default();
        assert!(ui.begin_plan(&[target()]).is_err());
        ui.source_file = Some(FILE.into());
        assert!(ui.begin_plan(&[target()]).is_err(), "no body yet");
        ui.target_key = Some(("Z 9".into(), "T1".into()));
        assert!(ui.begin_plan(&[]).is_err(), "the chosen body is not connected");
        assert!(ui.begin_plan(&[row("Z 9", "T1", 2)]).is_err(), "a body that shares its model cannot be aimed at");
        ui.categories.clear();
        assert!(ui.begin_plan(&[target()]).is_err(), "no category is not 'every category'");
        assert_eq!(*ui.stage(), Stage::Idle);
        ui.toggle(Category::Ftp);
        let Ok(Cmd::Plan(PlanJob::RestoreExt(asked))) = ui.begin_plan(&[target()]) else { panic!("a plan") };
        assert_eq!(asked.only, Some([Category::Ftp].into_iter().collect()));
        assert!(ui.begin_plan(&[target()]).is_err(), "one plan at a time");
    }

    #[test]
    fn a_plan_for_another_request_is_dropped_and_a_failed_one_is_a_notice() {
        let (mut ui, cameras) = ready();
        ui.sync(&cameras);
        let Cmd::Plan(PlanJob::RestoreExt(asked)) = ui.begin_plan(&cameras).unwrap() else { panic!() };
        let mut other = asked.clone();
        other.source_file = "another.json".into();
        ui.plan_arrived(other, Ok((plan_json(mixed_items()), String::new())));
        assert!(matches!(ui.stage(), Stage::Planning(_)), "still waiting for the plan that was asked for");
        ui.plan_arrived(asked, Err("Planning failed: camera busy".into()));
        assert_eq!(*ui.stage(), Stage::Idle);
        assert!(ui.notice.contains("camera busy"));
        ui.plan_arrived(request(None), Ok((plan_json(mixed_items()), String::new())));
        assert_eq!(*ui.stage(), Stage::Idle, "a plan nobody asked for is not shown");
    }

    #[test]
    fn apply_is_confirmed_in_two_steps_and_sends_the_plan_that_was_shown() {
        let (mut ui, cameras) = with_plan();
        let shown = plan_json(mixed_items());
        assert!(ui.confirm_apply().is_none(), "no confirmation was asked for");
        ui.ask_apply();
        assert!(matches!(ui.stage(), Stage::ConfirmApply(_)));
        ui.cancel_confirmation();
        assert!(matches!(ui.stage(), Stage::Planned(_)));
        assert!(ui.confirm_apply().is_none(), "cancelled");
        ui.ask_apply();
        let Some(Cmd::Apply(ApplyJob::RestoreExt { request: sent, digest, summary })) = ui.confirm_apply() else { panic!("an apply") };
        assert_eq!((sent, digest, summary), (request(None), shown.digest, shown.summary));
        assert!(matches!(ui.stage(), Stage::Applying(_)));
        assert!(ui.confirm_apply().is_none(), "no second apply while one is on the wire");
        assert!(ui.begin_plan(&cameras).is_err(), "nor a plan");
    }

    #[test]
    fn a_plan_that_would_change_nothing_is_not_offered_for_confirmation() {
        let (mut ui, cameras) = ready();
        ui.sync(&cameras);
        let Cmd::Plan(PlanJob::RestoreExt(asked)) = ui.begin_plan(&cameras).unwrap() else { panic!() };
        ui.plan_arrived(asked, Ok((plan_json(vec![item("a", Category::Strings, ItemAction::Unchanged)]), String::new())));
        ui.ask_apply();
        assert!(matches!(ui.stage(), Stage::Planned(_)), "nothing to restore");
    }

    #[test]
    fn a_changed_form_or_camera_withdraws_the_plan_and_each_confirmation() {
        for confirming in [false, true] {
            // a tick
            let (mut ui, cameras) = with_plan();
            if confirming {
                ui.ask_apply();
            }
            ui.toggle(Category::Iptc);
            ui.sync(&cameras);
            assert_eq!(*ui.stage(), Stage::Idle, "a tick changed (confirming: {confirming})");
            assert!(ui.notice.contains("withdrawn") && ui.notice.contains("Nothing was written"), "{}", ui.notice);
            // the snapshot
            let (mut ui, cameras) = with_plan();
            if confirming {
                ui.ask_apply();
            }
            ui.source_file = Some("another.json".into());
            ui.sync(&cameras);
            assert_eq!(*ui.stage(), Stage::Idle);
            // the camera (firmware changed after a replug)
            let (mut ui, _) = with_plan();
            if confirming {
                ui.ask_apply();
            }
            let mut moved = target();
            moved.firmware = "9.99".into();
            ui.sync(&[moved]);
            assert_eq!(*ui.stage(), Stage::Idle);
        }
    }

    #[test]
    fn an_apply_in_flight_is_not_withdrawn_by_a_change_and_its_answer_is_kept() {
        let (mut ui, cameras) = with_plan();
        ui.ask_apply();
        let Some(Cmd::Apply(ApplyJob::RestoreExt { request: sent, .. })) = ui.confirm_apply() else { panic!() };
        ui.toggle(Category::Ftp);
        ui.sync(&cameras);
        assert!(matches!(ui.stage(), Stage::Applying(_)), "the apply is on the wire");
        let outcome = ApplyOutcome::Report { report: Box::new(report(|_| {})), note: None, cli_log: String::new() };
        ui.applied(sent, outcome);
        assert!(matches!(ui.stage(), Stage::Done(_)));
    }

    #[test]
    fn a_dead_worker_ends_a_plan_as_lost_and_an_apply_as_unknown_never_as_unchanged() {
        let (mut ui, cameras) = ready();
        ui.sync(&cameras);
        ui.begin_plan(&cameras).unwrap();
        ui.worker_lost();
        assert_eq!(*ui.stage(), Stage::Idle);
        assert!(ui.notice.contains("Nothing was written"));

        let (mut ui, _) = with_plan();
        ui.ask_apply();
        ui.confirm_apply().unwrap();
        ui.worker_lost();
        let Stage::Done(done) = ui.stage() else { panic!("an apply that was sent has an answer") };
        assert!(matches!(done.outcome, ApplyOutcome::Indeterminate { .. }), "what it wrote is unknown");

        let (mut ui, _) = with_plan();
        ui.ask_apply();
        ui.confirm_apply().unwrap();
        ui.not_sent();
        let Stage::Done(done) = ui.stage() else { panic!() };
        assert!(matches!(done.outcome, ApplyOutcome::NotApplied { .. }), "a command that never left was not applied");
    }

    #[test]
    fn a_change_of_data_directory_withdraws_a_plan_but_not_an_apply_on_the_wire() {
        let (mut ui, _) = with_plan();
        ui.data_dir_changed();
        assert_eq!(*ui.stage(), Stage::Idle);
        let (mut ui, _) = with_plan();
        ui.ask_apply();
        ui.confirm_apply().unwrap();
        ui.data_dir_changed();
        assert!(matches!(ui.stage(), Stage::Applying(_)));
    }

    // ── undo ──

    fn offer(set: &[&str]) -> UndoOffer {
        UndoOffer {
            kind: UndoKind::Categories,
            restore_point: "Z_9_T1_restore-ext_20261106T020000Z.json".into(),
            undo_set: set.iter().map(|s| s.to_string()).collect(),
            target: target(),
            data_dir: PathBuf::from("/data"),
        }
    }

    #[test]
    fn an_undo_opens_the_restore_point_limited_to_the_categories_and_plans_it_without_writing() {
        let mut ui = ExtRestoreUi::default();
        let cameras = vec![target()];
        let Ok(Cmd::Plan(PlanJob::RestoreExt(asked))) = ui.start_undo(&offer(&["strings", "ftp"]), &cameras) else { panic!("a plan") };
        assert!(ui.open);
        assert_eq!(asked.source_file, "Z_9_T1_restore-ext_20261106T020000Z.json");
        assert_eq!(asked.only, Some([Category::Strings, Category::Ftp].into_iter().collect()));
        assert!(matches!(ui.stage(), Stage::Planning(_)), "a plan, not an apply");
        // Every category is no restriction.
        let mut ui = ExtRestoreUi::default();
        let Ok(Cmd::Plan(PlanJob::RestoreExt(asked))) = ui.start_undo(&offer(&["strings", "iptc", "switches", "ftp"]), &cameras) else { panic!() };
        assert_eq!(asked.only, None);
    }

    #[test]
    fn an_undo_that_cannot_be_planned_says_why_and_leaves_the_window_alone() {
        let cameras = vec![target()];
        let mut ui = ExtRestoreUi::default();
        assert!(ui.start_undo(&offer(&[]), &cameras).unwrap_err().contains("nothing to undo"), "an empty set is not 'everything'");
        assert!(ui.start_undo(&offer(&["strings"]), &[]).unwrap_err().contains("not connected"));
        assert!(ui.start_undo(&offer(&["strings", "wifi"]), &cameras).unwrap_err().contains("does not know"));
        assert!(!ui.open && ui.source_file.is_none(), "a refused undo changed nothing");
        let mut busy = ExtRestoreUi { stage: Stage::Planning(request(None)), ..ExtRestoreUi::default() };
        assert!(busy.start_undo(&offer(&["strings"]), &cameras).unwrap_err().contains("still running"));
    }

    #[test]
    fn an_applied_extended_report_offers_an_undo_of_categories_and_a_noop_offers_none() {
        let t = target();
        let dir = PathBuf::from("/data");
        let outcome = |r: ExtApplyJson| ApplyOutcome::Report { report: Box::new(r), note: None, cli_log: String::new() };
        let o = crate::restore::undo_offer(&t, Some(dir.as_path()), &outcome(report(|r| r.undo_set = vec!["strings".into(), "ftp".into()]))).expect("an offer");
        assert_eq!((o.kind, o.restore_point.as_str(), o.undo_set.len()), (UndoKind::Categories, "rp.json", 2));
        assert!(crate::restore::undo_offer(&t, Some(dir.as_path()), &outcome(report(|r| r.undo_set.clear()))).is_none(), "nothing to put back");
        assert!(crate::restore::undo_offer(&t, Some(dir.as_path()), &outcome(report(|r| r.restore_point = None))).is_none(), "no restore point, no undo");
        assert!(crate::restore::undo_offer(&t, None, &outcome(report(|_| {}))).is_none(), "the data directory must be known");
    }

    // ── the CLI, as a subprocess ──

    /// A stand-in for the CLI in a directory of its own: it records its argv (`argv_of`), then prints `stdout` and exits `code`.
    #[cfg(unix)]
    fn cli_printing(stdout: &str, code: i32) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("out.json"), stdout).unwrap();
        let cli = crate::tests::recording_cli(dir.path(), &format!("cat '{}/out.json'\nexit {code}", dir.path().display()));
        (dir, cli)
    }

    #[test]
    fn a_refused_target_never_starts_the_cli() {
        let scan = || scan_of(vec![usb("Z 9", "T1")]);
        let ambiguous = row("Z 9", "T1", 2);
        let req = ExtRestoreRequest { target: ambiguous, ..request(None) };
        let err = ext_plan_via(Path::new("/does/not/exist"), Path::new("/d"), &req, &scan, Duration::from_secs(1)).unwrap_err();
        assert!(err.contains("refused") && err.contains("nothing was read or written"), "{err}");
        let outcome = ext_apply_via(Path::new("/does/not/exist"), Path::new("/d"), &req, "d", "s", &scan, Duration::from_secs(1));
        assert!(matches!(outcome, ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, .. }), "{outcome:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_plan_runs_the_cli_as_a_dry_run_and_a_failed_run_has_no_plan() {
        use crate::tests::argv_of;
        let json = serde_json::to_string(&library_plan(mixed_items())).unwrap();
        let (dir, cli) = cli_printing(&json, 0);
        let scan = || scan_of(vec![usb("Z 9", "T1")]);
        let (plan, _) = ext_plan_via(&cli, dir.path(), &request(None), &scan, Duration::from_secs(10)).expect("a plan");
        assert_eq!(plan.would_change, 2);
        let argv = argv_of(dir.path());
        assert!(argv.contains(&"restore-extended".to_string()) && argv.contains(&"--dry-run".to_string()), "{argv:?}");
        assert!(!argv.iter().any(|a| a.starts_with("--expect-digest")), "a plan never carries a confirmation: {argv:?}");
        // The CLI exiting non-zero after printing is not a plan (its only such exit is a refusal that prints none).
        let (dir, failing) = cli_printing(&json, 1);
        assert!(ext_plan_via(&failing, dir.path(), &request(None), &scan, Duration::from_secs(10)).unwrap_err().contains("Planning failed"));
        // A plan for another body than asked is an error, not a plan.
        let other = serde_json::to_string(&ExtendedPlanReport {
            camera: Camera { model: "Z 9".into(), serial: "T2".into(), firmware: "5.31".into() },
            ..library_plan(mixed_items())
        })
        .unwrap();
        let (dir, cli2) = cli_printing(&other, 0);
        assert!(ext_plan_via(&cli2, dir.path(), &request(None), &scan, Duration::from_secs(10)).unwrap_err().contains("T2"));
    }

    #[cfg(unix)]
    #[test]
    fn an_apply_presents_the_confirmed_plan_and_reads_the_status_it_printed() {
        use crate::tests::argv_of;
        let scan = || scan_of(vec![usb("Z 9", "T1")]);
        let printed = serde_json::to_string(&serde_json::json!({
            "status": "applied_verified", "no_op": false, "plan_digest": "d1",
            "restore_point": {"file": "rp.json", "label": "restore-ext"}, "undo_set": ["strings"],
            "results": [{"id": "a", "label": "Artist", "category": "strings", "outcome": "written", "verified": true}],
            "still_differing": []
        }))
        .unwrap();
        let (dir, cli) = cli_printing(&printed, 0);
        let outcome = ext_apply_via(&cli, dir.path(), &request(None), "d1", "the summary", &scan, Duration::from_secs(10));
        let ApplyOutcome::Report { report, .. } = outcome else { panic!("a report: {outcome:?}") };
        assert_eq!(report.status, ApplyStatus::AppliedVerified);
        let argv = argv_of(dir.path());
        assert!(argv.contains(&"--expect-digest=d1".to_string()) && argv.contains(&"--expect-summary=the summary".to_string()), "{argv:?}");
        assert!(!argv.contains(&"--dry-run".to_string()), "an apply is never a dry run: {argv:?}");
        // A report of another plan than the one confirmed is not the answer to it.
        let (dir, cli) = cli_printing(&printed, 0);
        let outcome = ext_apply_via(&cli, dir.path(), &request(None), "someone else's digest", "s", &scan, Duration::from_secs(10));
        assert!(matches!(outcome, ApplyOutcome::Indeterminate { .. }), "{outcome:?}");
    }

    #[test]
    fn without_a_report_only_the_clis_own_refusals_say_nothing_was_written() {
        let classify = |code: i32| classify_apply_as::<ExtApplyJson>(Err(cli_error(CliFailure::Exit, Some(code), "")), "d1");
        assert!(matches!(classify(EXIT_STALE_PLAN), ApplyOutcome::NotApplied { why: NotAppliedWhy::PlanStale, .. }));
        assert!(matches!(classify(EXIT_REFUSED_BEFORE_WRITE), ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, .. }));
        // Exit 4 is "a write may or may not have taken effect": with no report, that is all that is known.
        assert!(matches!(classify(4), ApplyOutcome::Indeterminate { .. }));
        assert!(matches!(classify(1), ApplyOutcome::Indeterminate { .. }), "an unexplained failure after the writes began is unknown");
    }

    // ── the window draws ──

    #[test]
    fn every_stage_of_the_window_draws() {
        let (mut app, _cmd_rx, _evt_tx, _dir) = test_app();
        let ctx = egui::Context::default();
        let draw = |app: &mut FleetApp| {
            let _ = ctx.run(egui::RawInput::default(), |ctx| app.ui_restore_ext(ctx));
        };
        app.restore_ext.open = true;
        draw(&mut app); // nothing chosen
        app.restore_ext.target_key = Some(("Z 9".into(), "T1".into()));
        app.restore_ext.source_file = Some(FILE.into());
        app.restore_ext.sync(&app.cameras);
        draw(&mut app);
        app.restore_ext.categories.clear();
        draw(&mut app); // no category ticked
        app.restore_ext.categories = Category::ALL.into_iter().collect();
        let Cmd::Plan(PlanJob::RestoreExt(asked)) = app.restore_ext.begin_plan(&app.cameras).unwrap() else { panic!() };
        draw(&mut app); // planning
        app.restore_ext.plan_arrived(asked.clone(), Ok((plan_json(mixed_items()), "a log line".into())));
        draw(&mut app); // planned
        app.restore_ext.ask_apply();
        draw(&mut app); // confirming
        let Some(Cmd::Apply(ApplyJob::RestoreExt { request: sent, .. })) = app.restore_ext.confirm_apply() else { panic!() };
        draw(&mut app); // applying
        let outcomes = [
            ApplyOutcome::Report { report: Box::new(report(|_| {})), note: Some("a note".into()), cli_log: "log".into() },
            ApplyOutcome::Report { report: Box::new(report(|r| r.status = ApplyStatus::Failed)), note: None, cli_log: String::new() },
            ApplyOutcome::Report { report: Box::new(report(|r| r.undo_set = Category::ALL.iter().map(|c| c.tag().to_string()).collect())), note: None, cli_log: String::new() },
            ApplyOutcome::NotApplied { why: NotAppliedWhy::PlanStale, message: "m".into(), cli_log: "log".into() },
            ApplyOutcome::Indeterminate { message: "m".into(), cli_log: "log".into() },
        ];
        for outcome in outcomes {
            app.restore_ext.stage = Stage::Applying(sent.clone());
            app.restore_ext.applied(sent.clone(), outcome);
            assert!(matches!(app.restore_ext.stage(), Stage::Done(_)));
            draw(&mut app);
        }
        // a blocked plan, a plan with an unknown action, and a notice
        app.restore_ext.stage = Stage::Planning(asked.clone());
        app.restore_ext.plan_arrived(asked.clone(), Ok((plan_json(vec![item("a", Category::Strings, ItemAction::Unchanged)]), String::new())));
        draw(&mut app);
        app.restore_ext.notice = "a notice".into();
        draw(&mut app);
    }
}

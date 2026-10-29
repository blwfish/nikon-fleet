// ── Restore and undo: plan, review, confirm, apply ────────────────────────
//
// Put a stored snapshot of a connected body back on that body, or undo an apply by restoring the
// restore point it made. It is `nikon-fleet restore`'s work, run as a subprocess like every camera
// operation here, through the state machine every guarded write shares (`guarded.rs`):
//
//   1. PLAN     `restore --dry-run --json`: reads the body, writes nothing, returns a digest, a
//               one-line summary and one step per property.
//   2. REVIEW   the person sees which properties would change and to what, and may narrow the
//               restore to some of them (`--only`); that is a new plan, and the ticks are not a
//               plan: a plan that was made for more than is ticked cannot be applied.
//   3. CONFIRM  a text that names the exact body, the snapshot, what will be written and the
//               plan's summary.
//   4. APPLY    `restore --json --expect-digest --expect-summary`: the CLI recomputes the plan on
//               the body as it is now and refuses (exit `EXIT_STALE_PLAN`, nothing written) unless it
//               is the plan that was confirmed. Then restore point, writes, read-back.
//
// Only a snapshot of the body itself is offered: putting one body's settings on another is a
// transplant. After any apply (this window's or a transplant's) the restore point it made can be
// restored with `only` set to the apply's `undo_set`, through the same four steps.

use super::*;
use nikon_fleet::ops::restore::{RestoreSkip, Verdict, readback_line};
use nikon_fleet::ops::restore_flow::step_would_change;

/// A plan reads every property it would write (4-20 s a read); the transplant's timeouts, for the
/// same work. Unmeasured for a restore, which does more camera work: a run killed at the limit ends
/// as Indeterminate, with the restore point named if the CLI got as far as printing it.
pub(crate) const RESTORE_PLAN_TIMEOUT: Duration = TRANSPLANT_PLAN_TIMEOUT;
/// An apply adds a restore point and a read-back, each a full capture, to the writes.
pub(crate) const RESTORE_APPLY_TIMEOUT: Duration = TRANSPLANT_APPLY_TIMEOUT;

/// What a restore is asked for. These exact values reach the CLI: a plan is good for this request
/// only, and a change to any part of it withdraws the plan and any confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RestoreRequest {
    /// The source, a file name in `snapshots/` (a snapshot of the target body itself).
    pub source_file: String,
    /// The body written to, as discovered (a changed firmware or pairing is a different request).
    pub target: CameraRow,
    /// Full property names to restore and nothing else, or every property.
    pub only: Option<BTreeSet<String>>,
}

// The CLI's JSON, as `ops::restore_flow::RestorePlanReport` and `RestoreApplyReport` serialise it.
// The closed vocabularies are closed here too, with one difference: a tag this build does not know
// is kept (as `Unknown` with its text), not a reason to lose the whole report, which after a real
// write is the only place the restore point is named. A test pins every tag the library emits to a
// known one.

/// `ops::restore::StepOutcome::tag` and `RestoreSkip::tag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StepTag {
    NotSelected,
    NeverCopied,
    AbsentOnTarget,
    ReadOnly,
    RangeScaleDiffers,
    WouldWrite,
    Written,
    UnsupportedWriteType,
    WriteFailed,
    /// A tag from a CLI this program does not know; the text is `StepOutcomeJson::raw_tag`.
    #[serde(skip)]
    Unknown,
}

/// Where a plan's step is listed: the one partition, so a step is in exactly one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PlanSection {
    /// Would be written; whether it changes the body decides between `Change` and `Same`.
    WriteChange,
    WriteSame,
    /// Skipped on purpose or as a matter of course: the camera does not hold or take the setting.
    Routine,
    /// Skipped or failed for a reason the person should see.
    Attention,
    /// Not part of the restore (outside the selection): counted, not listed.
    Hidden,
}

impl StepTag {
    /// The library's skip reason this tag names, if it names one.
    pub(crate) fn skip(self) -> Option<RestoreSkip> {
        match self {
            StepTag::NeverCopied => Some(RestoreSkip::NeverCopied),
            StepTag::AbsentOnTarget => Some(RestoreSkip::Absent),
            StepTag::ReadOnly => Some(RestoreSkip::ReadOnly),
            StepTag::RangeScaleDiffers => Some(RestoreSkip::RangeScaleDiffers),
            StepTag::NotSelected
            | StepTag::WouldWrite
            | StepTag::Written
            | StepTag::UnsupportedWriteType
            | StepTag::WriteFailed
            | StepTag::Unknown => None,
        }
    }

    /// Whether a skip is routine is the library's call ([`RestoreSkip::is_routine`]); this only
    /// files it. A skip tag that somehow maps to no skip is listed where it will be seen.
    pub(crate) fn section(self, changes: bool) -> PlanSection {
        match self {
            StepTag::WouldWrite if changes => PlanSection::WriteChange,
            StepTag::WouldWrite => PlanSection::WriteSame,
            StepTag::NeverCopied | StepTag::AbsentOnTarget | StepTag::ReadOnly | StepTag::RangeScaleDiffers => {
                match self.skip() {
                    Some(skip) if skip.is_routine() => PlanSection::Routine,
                    Some(_) | None => PlanSection::Attention,
                }
            }
            StepTag::NotSelected => PlanSection::Hidden,
            StepTag::Written | StepTag::UnsupportedWriteType | StepTag::WriteFailed | StepTag::Unknown => PlanSection::Attention,
        }
    }

    /// In a plan (a dry run), where nothing has been tried yet.
    fn plan_words(self) -> &'static str {
        match self {
            StepTag::NotSelected => "not selected",
            StepTag::NeverCopied => "never copied (state, not a setting)",
            StepTag::AbsentOnTarget => "absent on this camera",
            StepTag::ReadOnly => "read-only on this camera",
            StepTag::RangeScaleDiffers => "left alone: its scale on this camera differs from the snapshot's",
            StepTag::WouldWrite => "would be written",
            StepTag::Written => "written",
            StepTag::UnsupportedWriteType => "the camera refuses writes of this kind",
            StepTag::WriteFailed => "cannot be written as stored (no usable position in the snapshot)",
            StepTag::Unknown => "a result this program does not know",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StepOutcomeJson {
    pub(crate) tag: StepTag,
    /// The tag as the CLI wrote it.
    pub(crate) raw_tag: String,
    /// `{"message": ...}` for a failed write.
    pub(crate) detail: Option<serde_json::Value>,
}

impl<'de> Deserialize<'de> for StepOutcomeJson {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            tag: String,
            detail: Option<serde_json::Value>,
        }
        let raw = Raw::deserialize(d)?;
        let tag = serde_json::from_value::<StepTag>(serde_json::Value::String(raw.tag.clone())).unwrap_or(StepTag::Unknown);
        Ok(StepOutcomeJson { tag, raw_tag: raw.tag, detail: raw.detail })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct RestoreStepJson {
    pub(crate) name: String,
    pub(crate) code: u32,
    pub(crate) outcome: StepOutcomeJson,
    /// For a step that would be written: whether it changes the body. `None` (null): the live value
    /// could not be read, which counts as a possible change.
    #[serde(default)]
    pub(crate) changes: Option<bool>,
}

impl RestoreStepJson {
    /// Would be written, and changes the body or might: the library's one definition
    /// (`step_would_change`, behind a plan's `would_change`).
    pub(crate) fn may_change(&self) -> bool {
        step_would_change(self.outcome.tag == StepTag::WouldWrite, self.changes)
    }

    pub(crate) fn section(&self) -> PlanSection {
        self.outcome.tag.section(self.may_change())
    }

    fn line(&self, source: &SourceView) -> String {
        let name = short_name(&self.name);
        let shown = source.values.get(&self.name);
        let label = shown.and_then(|v| v.display_name.as_deref()).filter(|d| *d != name).map(|d| format!(" ({d})")).unwrap_or_default();
        match (self.outcome.tag, shown) {
            (StepTag::WouldWrite, Some(v)) => {
                let change = match self.changes {
                    Some(true) => "",
                    Some(false) => " (unchanged)",
                    None => " (current value unknown, so it may change)",
                };
                let notes = if v.notes.is_empty() { String::new() } else { format!(" [{}]", v.notes.join(", ")) };
                format!("{name}{label}  →  {}{notes}{change}", v.value)
            }
            (StepTag::WouldWrite, None) => format!("{name}{label}  →  (value not shown)"),
            (StepTag::Unknown, _) => format!("{name}{label}: {}: {:?}", self.outcome.tag.plan_words(), printable_text(&self.outcome.raw_tag)),
            (tag, _) => format!("{name}{label}: {}", tag.plan_words()),
        }
    }
}

/// Text from the CLI that is shown as it is, with control characters escaped.
fn printable_text(text: &str) -> String {
    nikon_fleet::ops::render::printable(text)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) struct RestoreCountsJson {
    pub(crate) written: usize,
    pub(crate) skipped_absent: usize,
    pub(crate) skipped_read_only: usize,
    pub(crate) skipped_type: usize,
    pub(crate) skipped_never: usize,
    pub(crate) skipped_scale: usize,
    pub(crate) not_selected: usize,
    pub(crate) errors: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct RestorePlanJson {
    pub(crate) digest: String,
    pub(crate) summary: String,
    pub(crate) source: BodyJson,
    pub(crate) source_label: Option<String>,
    pub(crate) target: BodyJson,
    pub(crate) only: Option<Vec<String>>,
    pub(crate) counts: RestoreCountsJson,
    pub(crate) would_change: usize,
    pub(crate) steps: Vec<RestoreStepJson>,
    /// Named by `only` but not writable: they would not be restored.
    pub(crate) unrestorable: Vec<(String, String)>,
    pub(crate) cannot_read: Vec<NotTransferableJson>,
}

/// The read-back's verdict: the library's own closed vocabulary, or a text this build does not know.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub(crate) enum VerdictJson {
    Known(Verdict),
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct VerificationJson {
    pub(crate) verdict: VerdictJson,
    /// The read-back held to the baseline gate; null when it was not taken or read nothing.
    pub(crate) gate: Option<GateJson>,
    pub(crate) residual: Option<nikon_fleet::ops::readback::Residual>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct RestoreStepResultJson {
    pub(crate) name: String,
    pub(crate) code: u32,
    pub(crate) outcome: StepOutcomeJson,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct RestoreApplyJson {
    pub(crate) status: ApplyStatus,
    pub(crate) no_op: bool,
    pub(crate) plan_digest: String,
    pub(crate) restore_point: Option<RestorePointJson>,
    pub(crate) restore_point_gate_waived: bool,
    pub(crate) written: Vec<String>,
    pub(crate) write_errors: Vec<WriteErrorJson>,
    pub(crate) unrestored: Vec<(String, String)>,
    pub(crate) counts: RestoreCountsJson,
    pub(crate) steps: Vec<RestoreStepResultJson>,
    pub(crate) verification: Option<VerificationJson>,
    pub(crate) side_effects: Option<SideEffectsJson>,
    pub(crate) undo_set: Vec<String>,
    pub(crate) undo_not_restorable: Vec<String>,
}

impl AppliedReport for RestoreApplyJson {
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
}

/// What the snapshot holds for each property, as the viewer shows it: the name a person knows it by
/// and its value. The plan carries neither (only names and codes), so a person could not otherwise
/// see what a restore would set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SourceView {
    pub(crate) values: BTreeMap<String, SourceValue>,
    /// The snapshot was read and rendered. Without it no value can be shown, and a restore whose
    /// values the person cannot see is not offered for confirmation.
    pub(crate) loaded: bool,
    /// Why some names or values are missing (no `sdk-runtime/`) or why nothing could be shown.
    pub(crate) note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceValue {
    pub(crate) display_name: Option<String>,
    pub(crate) value: String,
    /// The renderer's flags on the value (`truncated`, `malformed`, …), by tag.
    pub(crate) notes: Vec<String>,
}

/// The snapshot's values for display, read like the viewer reads them. Never fails the plan: a
/// restore does not depend on it, but it is not offered for confirmation without it.
pub(crate) fn load_source_view(data_dir: &Path, file: &str) -> SourceView {
    let sdk_runtime = resolve_sdk_runtime(std::env::var_os("NIKON_FLEET_SDK_RUNTIME"));
    match load_for_view(data_dir, &sdk_runtime, file) {
        Ok(r) => SourceView {
            values: r
                .rows
                .into_iter()
                .map(|row| {
                    let display_name = row.extra_display_name().map(str::to_string);
                    let notes = row.value.notes.iter().map(|n| n.tag().to_string()).collect();
                    (row.name, SourceValue { display_name, value: row.value.display, notes })
                })
                .collect(),
            loaded: true,
            note: r.labels_note,
        },
        Err(e) => SourceView { values: BTreeMap::new(), loaded: false, note: Some(format!("the snapshot's values cannot be shown here: {e}")) },
    }
}

/// A plan the CLI returned for `request`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedRestore {
    pub(crate) request: RestoreRequest,
    pub(crate) plan: RestorePlanJson,
    pub(crate) source: SourceView,
    pub(crate) cli_log: String,
    /// The properties ticked for a narrower restore. A draft: it is the request only once a new
    /// plan is made for it, and a plan made for more than is ticked cannot be applied.
    pub(crate) selected: BTreeSet<String>,
    /// An undo that was widened to every property of its restore point: the confirmation says what
    /// that means.
    pub(crate) widened_undo: bool,
}

impl PlannedRestore {
    pub(crate) fn new(request: RestoreRequest, plan: RestorePlanJson, source: SourceView, cli_log: String, widened_undo: bool) -> Self {
        let selected = plan.steps.iter().filter(|s| s.may_change()).map(|s| s.name.clone()).collect();
        PlannedRestore { request, plan, source, cli_log, selected, widened_undo }
    }

    /// Every step that would change the body or might.
    pub(crate) fn changing(&self) -> Vec<&RestoreStepJson> {
        self.plan.steps.iter().filter(|s| s.may_change()).collect()
    }

    /// How many properties the restore writes: the plan's own count of what it would write.
    pub(crate) fn writes(&self) -> usize {
        self.plan.counts.written
    }

    /// The ticks differ from the properties the plan changes: the plan on screen is for more (or
    /// other) than is ticked.
    pub(crate) fn selection_diverged(&self) -> bool {
        let changing: BTreeSet<&String> = self.changing().iter().map(|s| &s.name).collect();
        let ticked: BTreeSet<&String> = self.selected.iter().collect();
        changing != ticked
    }

    /// The ticks are a narrower restore than this plan and could be planned: something is ticked and
    /// the plan is for more, or it names what cannot be restored (a new plan, limited to the ticks,
    /// no longer names it).
    pub(crate) fn can_plan_selection(&self) -> bool {
        !self.selected.is_empty() && (self.selection_diverged() || !self.plan.unrestorable.is_empty())
    }

    /// Why this plan cannot be offered for confirmation, if it cannot. The one gate: the button, the
    /// state machine and the words beside the button all read it.
    pub(crate) fn blocked_reason(&self) -> Option<String> {
        let changing = self.changing().len();
        if self.plan.would_change != changing {
            return Some(format!(
                "The plan's own count of changes ({}) disagrees with its steps ({changing}), so it cannot be trusted.",
                self.plan.would_change
            ));
        }
        if changing == 0 {
            return Some("This plan would change nothing, so there is nothing to restore.".to_string());
        }
        if !self.plan.unrestorable.is_empty() {
            let list: Vec<String> = self.plan.unrestorable.iter().map(|(n, why)| format!("{} ({})", short_name(n), why.replace('_', " "))).collect();
            return Some(format!(
                "These were named but cannot be restored, so this plan cannot be applied: {}. Narrow the selection or restore everything.",
                list.join(", ")
            ));
        }
        if self.selection_diverged() {
            let unticked = self.changing().iter().filter(|s| !self.selected.contains(&s.name)).count();
            return Some(format!(
                "{unticked} propert{} unticked: this plan is for more than is ticked. Plan again for the ticked ones first.",
                if unticked == 1 { "y is" } else { "ies are" }
            ));
        }
        if !self.source.loaded {
            return Some("The values this restore would set could not be shown (the snapshot could not be read for display), so it cannot be confirmed here.".to_string());
        }
        let hidden: Vec<&str> = self.changing().iter().filter(|s| !self.source.values.contains_key(&s.name)).map(|s| short_name(&s.name)).collect();
        if !hidden.is_empty() {
            return Some(format!("The values of {} could not be shown, so this restore cannot be confirmed here.", hidden.join(", ")));
        }
        if self.plan.steps.iter().any(|s| matches!(s.outcome.tag, StepTag::WriteFailed | StepTag::Unknown)) {
            return Some("The plan contains a step that would fail or that this program does not understand, so it cannot be applied.".to_string());
        }
        None
    }

    pub(crate) fn applicable(&self) -> bool {
        self.blocked_reason().is_none()
    }

    /// The snapshot was taken on other firmware than the camera runs now.
    pub(crate) fn firmware_differs(&self) -> Option<String> {
        let (s, t) = (&self.plan.source.firmware, &self.plan.target.firmware);
        (s != t).then(|| {
            format!(
                "The snapshot was taken on firmware {} and the camera runs {}: a setting that changed meaning between them is written as stored.",
                printable_text(s),
                printable_text(t)
            )
        })
    }
}

/// Restore, as a guarded write.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RestoreFlow;

/// A restore has no question to put about a plan: this type has no values.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NoAside {}

impl Flow for RestoreFlow {
    type Request = RestoreRequest;
    type Plan = PlannedRestore;
    type Aside = NoAside;
    type Report = RestoreApplyJson;

    const APPLY_WORD: &'static str = "restore";
    const CHOOSE_FIRST: &'static str = "Choose a connected body and one of its stored snapshots first.";

    fn request_target(r: &RestoreRequest) -> &CameraRow {
        &r.target
    }
    fn plan_request(p: &PlannedRestore) -> &RestoreRequest {
        &p.request
    }
    fn aside_plan(a: &NoAside) -> &PlannedRestore {
        match *a {}
    }
    fn plan_digest(p: &PlannedRestore) -> &str {
        &p.plan.digest
    }
    fn plan_summary(p: &PlannedRestore) -> &str {
        &p.plan.summary
    }
    fn applicable(p: &PlannedRestore) -> bool {
        p.applicable()
    }
    fn plan_cmd(request: RestoreRequest) -> Cmd {
        Cmd::Plan(PlanJob::Restore(request))
    }
    fn apply_cmd(request: RestoreRequest, digest: String, summary: String) -> Cmd {
        Cmd::Apply(ApplyJob::Restore { request, digest, summary })
    }
}

/// The CLI arguments for a plan (`confirmed` is `None`) or an apply: `nikon_fleet::gui_argv`, which
/// the CLI's own tests run through the real parser. A selection of nothing is an error, not no flag.
pub(crate) fn restore_argv(data_dir: &Path, request: &RestoreRequest, confirmed: Option<(&str, &str)>) -> Result<Vec<String>, String> {
    let dir = data_dir
        .to_str()
        .ok_or_else(|| format!("the data directory {} is not valid UTF-8", data_dir.display()))?;
    let source = snapshot_path_for_cli(data_dir, &request.source_file)?;
    gui_argv::restore_argv(dir, &request.target.serial, request.only.as_ref(), confirmed, &source).map_err(|e| e.to_string())
}

pub(crate) fn do_restore_plan(data_dir: &Path, request: &RestoreRequest) -> Result<(RestorePlanJson, String, SourceView), String> {
    // The values come first: reading a file is quick, and the camera is not held while it happens.
    let source = load_source_view(data_dir, &request.source_file);
    let (plan, log) = restore_plan_via(&locate_cli()?, data_dir, request, &list_cameras, RESTORE_PLAN_TIMEOUT)?;
    Ok((plan, log, source))
}

/// Ask the CLI for a plan. Reads the body, writes nothing. A plan that names properties it cannot
/// restore is printed and then fails the command (exit 1): that plan is shown, with the list; a run
/// that was stopped or killed after printing is not a plan.
pub(crate) fn restore_plan_via(
    cli: &Path,
    data_dir: &Path,
    request: &RestoreRequest,
    scan: &dyn Fn() -> CameraList,
    timeout: Duration,
) -> Result<(RestorePlanJson, String), String> {
    let (plan, log) = plan_via::<RestorePlanJson>(
        "Restore",
        cli,
        || restore_argv(data_dir, request, None),
        &request.target,
        scan,
        timeout,
        PlanFailure::PlanThenExitOne,
    )?;
    check_restore_plan_is_for(&plan, request)?;
    Ok((plan, log))
}

/// A plan is shown and confirmed only if it is for the body (at the firmware it runs now), from a
/// snapshot of that same body, for the selection that was asked for, and its change count is its
/// steps'.
pub(crate) fn check_restore_plan_is_for(plan: &RestorePlanJson, request: &RestoreRequest) -> Result<(), String> {
    let t = &request.target;
    check_plan_target(&plan.target, t)?;
    if (&plan.source.model, &plan.source.serial) != (&t.model, &t.serial) {
        return Err(format!(
            "the snapshot is of {} {}, not of the {} {} it would be written to; copying between bodies is a transplant. Nothing was written",
            plan.source.model, plan.source.serial, t.model, t.serial
        ));
    }
    // "No selection" and "a selection of nothing" are different plans: the first restores everything.
    let planned: Option<BTreeSet<&String>> = plan.only.as_ref().map(|o| o.iter().collect());
    let asked: Option<BTreeSet<&String>> = request.only.as_ref().map(|o| o.iter().collect());
    if planned != asked {
        return Err("the plan does not carry the selection that was asked for; nothing was written".to_string());
    }
    let changing = plan.steps.iter().filter(|s| s.may_change()).count();
    if plan.would_change != changing {
        return Err(format!(
            "the plan's own count of changes ({}) disagrees with its steps ({changing}); nothing was written",
            plan.would_change
        ));
    }
    Ok(())
}

pub(crate) fn do_restore_apply(data_dir: &Path, request: &RestoreRequest, digest: &str, summary: &str) -> ApplyOutcome<RestoreApplyJson> {
    match locate_cli() {
        Ok(cli) => restore_apply_via(&cli, data_dir, request, digest, summary, &list_cameras, RESTORE_APPLY_TIMEOUT),
        Err(message) => no_cli(message),
    }
}

/// Apply exactly the plan that was confirmed: the CLI is given its digest and summary and refuses
/// if the body or the snapshot no longer give it.
pub(crate) fn restore_apply_via(
    cli: &Path,
    data_dir: &Path,
    request: &RestoreRequest,
    digest: &str,
    summary: &str,
    scan: &dyn Fn() -> CameraList,
    timeout: Duration,
) -> ApplyOutcome<RestoreApplyJson> {
    apply_via::<RestoreApplyJson>(
        "Restore",
        cli,
        || restore_argv(data_dir, request, Some((digest, summary))),
        &request.target,
        digest,
        scan,
        timeout,
    )
}

// What the person is shown, as text with a level. Pure, so each is tested without a window.

/// The text of the confirmation for an apply: it names the body and the snapshot, states what will
/// be written (all of it, the unchanged included) and carries the plan's summary, so what is
/// approved is what is sent.
pub(crate) fn restore_confirm_text(p: &PlannedRestore) -> String {
    let plan = &p.plan;
    let source = format!(
        "stored in {} (taken on firmware {}{})",
        p.request.source_file,
        printable_text(&plan.source.firmware),
        plan.source_label.as_deref().map(|l| format!(", {:?}", printable_text(l))).unwrap_or_default(),
    );
    let mut s = confirm_head(&p.request.target, &source, &plan.summary);
    let (written, changing) = (p.writes(), p.changing().len());
    s.push_str(&format!(
        "\n{written} propert{} will be written to the camera; {changing} of them change{} what it holds now{}.",
        if written == 1 { "y" } else { "ies" },
        if changing == 1 { "s" } else { "" },
        match written.checked_sub(changing) {
            Some(0) | None => String::new(),
            Some(same) => format!(", and the other {same} already hold the stored value and are written again unchanged"),
        }
    ));
    if let Some(only) = &plan.only {
        s.push_str(&format!(" Nothing else is touched: this restore is limited to the {} propert(ies) named in the plan.", only.len()));
    }
    if p.widened_undo {
        s.push_str(
            "\nThis is no longer an undo: it restores EVERY property of the restore point, which also puts back anything that was changed on the camera after that apply, not only what the apply changed.",
        );
    }
    if let Some(w) = p.firmware_differs() {
        s.push('\n');
        s.push_str(&w);
    }
    s.push_str(CONFIRM_TAIL);
    s
}

/// A verdict's level: a failure to keep a write is an error, an unverified or incomplete read-back a
/// warning, and only a verified (or empty) one is plain.
fn verdict_level(v: Verdict) -> Level {
    match v {
        Verdict::Verified | Verdict::NothingWritten => Level::Info,
        Verdict::ReadbackIncomplete | Verdict::Unjudged => Level::Warn,
        Verdict::ResidualFound => Level::Error,
    }
}

/// Why the gate could not call a read-back complete, in words.
fn gate_words(g: &GateJson) -> String {
    match (g.outcome, g.reason) {
        (GateKind::Passed, _) => "the read-back passed the model's completeness check".to_string(),
        (GateKind::Failed, _) => format!("{} capabilit(ies) the model should read were missing", g.missing.as_ref().map_or(0, Vec::len)),
        (GateKind::NoBaseline, Some(NoBaselineKind::UnknownModel)) => "there is no baseline for this model".to_string(),
        (GateKind::NoBaseline, Some(NoBaselineKind::FirmwareChanged)) => format!(
            "firmware {} differs from the baseline's {}",
            g.capture.as_deref().map(printable_text).unwrap_or_else(|| "?".into()),
            g.baseline.as_deref().map(printable_text).unwrap_or_else(|| "?".into())
        ),
        (GateKind::NoBaseline, None) => "the model has no baseline (no reason given)".to_string(),
    }
}

/// A readable account of a restore report: what was written, what was not, and what the read-back
/// found.
pub(crate) fn restore_report_lines(r: &RestoreApplyJson) -> Vec<(Level, String)> {
    let mut out = Vec::new();
    let mut line = |level, text: String| out.push((level, text));
    if let Some(aftermath) = failed_aftermath(r) {
        line(Level::Error, aftermath.to_string());
    }
    if r.no_op {
        let failed = r.status == ApplyStatus::Failed;
        line(
            if failed { Level::Error } else { Level::Info },
            if failed {
                "Nothing was written, and not because there was nothing to change; no restore point was taken.".to_string()
            } else {
                "Nothing to write: the camera already holds everything the plan restores. No restore point was taken.".to_string()
            },
        );
    }
    if r.restore_point_gate_waived {
        line(Level::Warn, "The restore point did not pass the model's completeness check and was kept anyway.".into());
    }
    // A full restore writes every property of the snapshot, changed or not: the names are in the
    // result's own list, not in this line.
    if !r.written.is_empty() {
        line(Level::Info, format!("Written: {} propert{} (the full list is below).", r.written.len(), if r.written.len() == 1 { "y" } else { "ies" }));
    }
    for e in &r.write_errors {
        line(Level::Error, format!("Write failed: {}: {}", short_name(&e.name), printable_text(&e.message)));
    }
    if !r.unrestored.is_empty() {
        let list: Vec<String> = r.unrestored.iter().map(|(n, why)| format!("{} ({why})", short_name(n))).collect();
        line(Level::Error, format!("Named but NOT written: {}", list.join(", ")));
    }
    let rejected = r.steps.iter().filter(|s| s.outcome.tag == StepTag::UnsupportedWriteType).count();
    if rejected > 0 {
        line(Level::Warn, format!("Not taken by the body (it refuses that kind of write): {rejected} propert(ies)."));
    }
    let scale = r.steps.iter().filter(|s| s.outcome.tag == StepTag::RangeScaleDiffers).count();
    if scale > 0 {
        line(Level::Warn, format!("{scale} range(s) were left alone because their scale on this camera differs from the snapshot's."));
    }
    let unknown: Vec<String> = r.steps.iter().filter(|s| s.outcome.tag == StepTag::Unknown).map(|s| format!("{:?}", printable_text(&s.outcome.raw_tag))).collect();
    if !unknown.is_empty() {
        line(Level::Warn, format!("{} step(s) ended in a way this program does not know ({}); check the camera against the restore point.", unknown.len(), unknown.join(", ")));
    }
    match &r.verification {
        Some(v) => {
            let (level, text) = match &v.verdict {
                VerdictJson::Known(verdict) => {
                    let mut text = readback_line(*verdict, v.residual.as_ref());
                    if *verdict == Verdict::ReadbackIncomplete {
                        if let Some(g) = &v.gate {
                            text.push_str(&format!(" ({})", gate_words(g)));
                        }
                    }
                    (verdict_level(*verdict), text)
                }
                VerdictJson::Unknown(raw) => (
                    Level::Warn,
                    format!("Read-back: a verdict this program does not know ({:?}); nothing is claimed.", printable_text(raw)),
                ),
            };
            line(level, text);
            if let Some(res) = &v.residual {
                if !res.unjudged.is_empty() {
                    line(Level::Warn, format!("Written but not compared ({}): {}", res.unjudged.len(), names_list(&res.unjudged)));
                }
            }
        }
        None if !r.no_op => line(Level::Warn, "No read-back was made, so nothing written is confirmed.".into()),
        None => {}
    }
    match &r.side_effects {
        Some(se) if !se.is_empty_effects() => {
            let mut names: Vec<String> = se.changed.iter().map(|n| format!("{} (changed)", short_name(n))).collect();
            names.extend(se.vanished.iter().map(|n| format!("{} (vanished)", short_name(n))));
            names.extend(se.appeared.iter().map(|n| format!("{} (appeared)", short_name(n))));
            line(Level::Warn, format!("Changed without being written: {}", names.join(", ")));
        }
        Some(_) => {}
        None if !r.no_op => line(Level::Warn, "Side effects: unknown (the read-back was not complete enough to compare).".into()),
        None => {}
    }
    if !r.undo_not_restorable.is_empty() {
        line(Level::Warn, format!("An undo cannot put these back: {}", names_list(&r.undo_not_restorable)));
    }
    out
}

/// Everything needed to undo an apply: the restore point it made, the properties that put back what
/// it changed, the body, and the data directory the restore point is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UndoOffer {
    /// The restore point, a file name in `snapshots/` of `data_dir`.
    pub restore_point: String,
    pub undo_set: Vec<String>,
    pub target: CameraRow,
    pub data_dir: PathBuf,
}

/// The undo an outcome offers, if it offers one: it needs a report that names a restore point and
/// something to put back, whatever its status says (a failed or unsettled apply may have written
/// some, and the plan the person reviews says what, if anything, differs). An answer without a
/// report names no restore point; the data directory must be known to find it again.
pub(crate) fn undo_offer<R: AppliedReport>(target: &CameraRow, data_dir: Option<&Path>, outcome: &ApplyOutcome<R>) -> Option<UndoOffer> {
    let ApplyOutcome::Report { report, .. } = outcome else {
        return None;
    };
    let point = report.restore_point()?;
    if report.undo_set().is_empty() {
        return None;
    }
    Some(UndoOffer {
        restore_point: point.file.clone(),
        undo_set: report.undo_set().to_vec(),
        target: target.clone(),
        data_dir: data_dir?.to_path_buf(),
    })
}

/// The restore window's state.
#[derive(Debug, Default)]
pub(crate) struct RestoreUi {
    pub open: bool,
    /// The chosen target, by identity; the live row is found among the discovered cameras.
    pub target_key: Option<(String, String)>,
    pub source_file: Option<String>,
    /// Full property names the restore is limited to (an undo, or a narrowed restore).
    pub(crate) only: Option<BTreeSet<String>>,
    /// The source file and the whole target row (firmware and pairing included) `only` was given
    /// for: any other has none, since a selection is of one snapshot's properties.
    only_for: Option<(String, CameraRow)>,
    /// The restore point an undo was started on: set with `only`, gone with it.
    undo_of: Option<String>,
    pub(crate) stage: Stage<RestoreFlow>,
    /// Why a plan or confirmation went away, or how a plan failed.
    pub notice: String,
}

impl GuardedUi for RestoreUi {
    type F = RestoreFlow;

    fn stage(&self) -> &Stage<RestoreFlow> {
        &self.stage
    }
    fn stage_mut(&mut self) -> &mut Stage<RestoreFlow> {
        &mut self.stage
    }
    fn notice_mut(&mut self) -> &mut String {
        &mut self.notice
    }

    fn request(&self, cameras: &[CameraRow]) -> Option<RestoreRequest> {
        Some(RestoreRequest {
            source_file: self.source_file.clone()?,
            target: self.target_camera(cameras)?.clone(),
            only: self.only.clone(),
        })
    }

    /// A selection belongs to the snapshot and body it was made for. When the person chose another
    /// snapshot it goes quietly. When the BODY changed under it (a replug and Discover, a firmware
    /// update) the form is withdrawn and says so: dropping only the restriction would make the next
    /// plan a restore of every property.
    fn sync_selection(&mut self, cameras: &[CameraRow]) {
        let pair = self.source_file.clone().zip(self.target_camera(cameras).cloned());
        if self.only_for == pair {
            return;
        }
        let was_for = std::mem::replace(&mut self.only_for, pair);
        let Some(only) = self.only.take() else { return };
        self.undo_of = None;
        let snapshot_unchanged = was_for.as_ref().map(|(f, _)| f) == self.source_file.as_ref();
        if snapshot_unchanged {
            self.source_file = None;
            self.notice = format!(
                "The restriction to {} propert(ies) was dropped because the camera changed. The snapshot was cleared too, so the next plan does not restore everything by accident: choose it again.",
                only.len()
            );
        }
    }

    fn clear_selection(&mut self) {
        self.only = None;
        self.undo_of = None;
    }
}

impl RestoreUi {
    pub fn target_camera<'a>(&self, cameras: &'a [CameraRow]) -> Option<&'a CameraRow> {
        let (model, serial) = self.target_key.as_ref()?;
        cameras.iter().find(|c| &c.model == model && &c.serial == serial)
    }

    #[cfg(test)]
    pub fn only(&self) -> Option<&BTreeSet<String>> {
        self.only.as_ref()
    }

    /// A plan came back. Dropped unless it is the one asked for and still wanted.
    pub fn plan_arrived(&mut self, request: RestoreRequest, result: Result<(RestorePlanJson, String, SourceView), String>) {
        self.plan_arrived_with(request, result, |me, request, (plan, cli_log, source)| {
            let widened = request.only.is_none() && me.undo_of.as_deref() == Some(request.source_file.as_str());
            PlannedRestore::new(request, plan, source, cli_log, widened)
        });
    }

    /// Tick or untick one property of the plan being reviewed. Only a step that would change the
    /// body can be ticked.
    pub fn toggle(&mut self, name: &str) {
        if let Stage::Planned(p) = &mut self.stage {
            if !p.plan.steps.iter().any(|s| s.name == name && s.may_change()) {
                return;
            }
            if !p.selected.remove(name) {
                p.selected.insert(name.to_string());
            }
        }
    }

    /// Plan again for the ticked properties only. Nothing ticked is not a restore of everything: it
    /// is refused, since "no restriction" is what leaving the selection out means.
    pub fn plan_selection(&mut self, cameras: &[CameraRow]) -> Result<Cmd, String> {
        let Stage::Planned(p) = &self.stage else {
            return Err("There is no plan to narrow.".to_string());
        };
        if p.selected.is_empty() {
            return Err("Tick at least one property: a restore of nothing is not a restore.".to_string());
        }
        // The new plan is for the ticks: they are the selection from here (it is the one transition
        // that keeps one).
        self.only = Some(p.selected.clone());
        self.stage = Stage::Idle;
        self.begin_plan(cameras)
    }

    /// Plan again for every property, dropping the selection.
    pub fn plan_everything(&mut self, cameras: &[CameraRow]) -> Result<Cmd, String> {
        if !matches!(self.stage, Stage::Planned(_)) {
            return Err("There is no plan to widen.".to_string());
        }
        self.only = None;
        self.stage = Stage::Idle;
        self.begin_plan(cameras)
    }

    /// Set the form to undo `offer` and plan it. The plan is read-only; the person reviews it and
    /// confirms it like any restore. The body must be connected, and no step may be in flight.
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
        self.open = true;
        self.target_key = Some((camera.model.clone(), camera.serial.clone()));
        self.source_file = Some(offer.restore_point.clone());
        self.only = Some(offer.undo_set.iter().cloned().collect());
        self.only_for = Some((offer.restore_point.clone(), camera.clone()));
        self.undo_of = Some(offer.restore_point.clone());
        self.stage = Stage::Idle;
        self.begin_plan(cameras)
    }

    /// Open the window on `file` of `body`, a snapshot of a connected body: a restore of everything
    /// in it, whatever an earlier undo had limited.
    pub fn start_from(&mut self, body: (String, String), file: String) {
        self.open = true;
        self.target_key = Some(body);
        self.source_file = Some(file);
        self.clear_selection();
    }
}

// ── The restore window ────────────────────────────────────────────────────

/// What a click in the restore window asks for. Collected while drawing and applied after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RestoreAction {
    Plan,
    Toggle(String),
    PlanSelection,
    PlanEverything,
    AskApply,
    ConfirmApply,
    CancelConfirmation,
    Discard,
    Applied(AppliedAction),
}

fn draw_restore_plan(ui: &mut egui::Ui, p: &PlannedRestore, can_edit: bool, action: &mut Option<RestoreAction>) {
    let plan = &p.plan;
    ui.heading(format!("{} {}", plan.target.model, plan.target.serial));
    ui.label(
        egui::RichText::new(format!(
            "{}{} (taken on fw {})  →  {} (fw {})",
            p.request.source_file,
            plan.source_label.as_deref().map(|l| format!("  ({})", printable_text(l))).unwrap_or_default(),
            printable_text(&plan.source.firmware),
            plan.target.model,
            printable_text(&plan.target.firmware)
        ))
        .weak(),
    );
    if let Some(w) = p.firmware_differs() {
        ui.colored_label(WARN_COLOR, w);
    }
    ui.label(egui::RichText::new(&plan.summary).strong());
    ui.label(egui::RichText::new(format!("plan {}", plan.digest.get(..12).unwrap_or(&plan.digest))).small().weak());
    let c = &plan.counts;
    ui.label(format!(
        "{} would be written ({} would change). Skipped: {} absent, {} read-only, {} never copied, {} scale differs, {} of another type.{}",
        c.written,
        plan.would_change,
        c.skipped_absent,
        c.skipped_read_only,
        c.skipped_never,
        c.skipped_scale,
        c.skipped_type,
        if c.not_selected > 0 { format!(" {} not selected.", c.not_selected) } else { String::new() },
    ));
    if let Some(note) = &p.source.note {
        ui.label(egui::RichText::new(format!("Names and values: {note}")).small().color(WARN_COLOR));
    }
    if let Some(only) = &plan.only {
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(WARN_COLOR, format!("Limited to {} propert(ies); everything else on the camera is left as it is.", only.len()));
            ui.add_enabled_ui(can_edit, |ui| {
                if ui.small_button("Restore everything instead").clicked() {
                    *action = Some(RestoreAction::PlanEverything);
                }
            });
        });
    }
    if let Some(why) = p.blocked_reason() {
        // A plan with nothing to restore is told plainly under the buttons; the rest is a reason it
        // cannot be applied yet.
        if plan.would_change > 0 {
            ui.colored_label(ERROR_COLOR, why);
        }
    }
    ui.separator();

    let in_section = |section: PlanSection| -> Vec<&RestoreStepJson> { plan.steps.iter().filter(|s| s.section() == section).collect() };
    let changing = in_section(PlanSection::WriteChange);
    if !changing.is_empty() {
        egui::CollapsingHeader::new(format!("Would change ({})", changing.len())).id_salt("rs_change").default_open(true).show(ui, |ui| {
            for s in &changing {
                ui.horizontal_wrapped(|ui| {
                    let mut ticked = p.selected.contains(&s.name);
                    ui.add_enabled_ui(can_edit, |ui| {
                        if ui.checkbox(&mut ticked, "").clicked() {
                            *action = Some(RestoreAction::Toggle(s.name.clone()));
                        }
                    });
                    ui.label(s.line(&p.source));
                });
            }
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{} of {} ticked.", p.selected.len(), changing.len())).weak());
                ui.add_enabled_ui(can_edit && p.can_plan_selection(), |ui| {
                    if ui.button("Plan again for the ticked ones only").clicked() {
                        *action = Some(RestoreAction::PlanSelection);
                    }
                });
            });
        });
    }
    for (section, title, id, open) in [
        (PlanSection::WriteSame, "Already the same", "rs_same", false),
        (PlanSection::Attention, "Skipped, needs attention", "rs_attention", true),
        (PlanSection::Routine, "Skipped as a matter of course", "rs_routine", false),
    ] {
        let rows = in_section(section);
        if rows.is_empty() {
            continue;
        }
        egui::CollapsingHeader::new(format!("{title} ({})", rows.len())).id_salt(id).default_open(open).show(ui, |ui| {
            for s in rows {
                if section == PlanSection::Attention {
                    ui.colored_label(WARN_COLOR, s.line(&p.source));
                } else {
                    ui.label(s.line(&p.source));
                }
            }
        });
    }
    if !plan.cannot_read.is_empty() {
        egui::CollapsingHeader::new(format!("Cannot be restored at all ({})", plan.cannot_read.len())).id_salt("rs_cannot").show(ui, |ui| {
            for c in &plan.cannot_read {
                ui.label(format!("{}: {}", c.what, c.why));
            }
        });
    }
    if !p.cli_log.trim().is_empty() {
        egui::CollapsingHeader::new("CLI messages").id_salt("rs_log").show(ui, |ui| {
            ui.label(egui::RichText::new(p.cli_log.trim_end()).monospace());
        });
    }
}

/// The restore point of an apply, with the undo it offers: a button that plans the undo, and the
/// equivalent command to copy where it fits on a command line. Shared by the transplant and restore
/// results.
pub(crate) fn draw_restore_point(
    ui: &mut egui::Ui,
    restore_point: &RestorePointJson,
    offer: Option<&UndoOffer>,
    undo_set: &[String],
    data_dir: Option<&Path>,
    busy: bool,
) -> Option<AppliedAction> {
    let mut chosen = None;
    ui.separator();
    ui.label(egui::RichText::new("Restore point").strong());
    ui.label("The camera as it was just before this apply:");
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(&restore_point.file).monospace());
        if ui.small_button("View").clicked() {
            chosen = Some(AppliedAction::View(restore_point.file.clone()));
        }
    });
    match (offer, undo_set.is_empty()) {
        (Some(offer), _) => {
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    if ui
                        .button("Undo…")
                        .on_hover_text("Plan putting back what this changed. You review the plan and confirm it before anything is written.")
                        .clicked()
                    {
                        chosen = Some(AppliedAction::Undo(offer.clone()));
                    }
                });
                ui.label(format!("puts back {} propert(ies) from the restore point.", offer.undo_set.len()));
            });
        }
        (None, true) => {
            ui.label(egui::RichText::new("Nothing to put back.").weak());
        }
        (None, false) => {
            ui.colored_label(WARN_COLOR, "Undo is not available here: the data directory is not known.");
        }
    }
    // A command only where there is one: more names than fit on a command line are a sentence, and
    // a sentence is not offered as something to paste.
    let dir = data_dir.and_then(Path::to_str);
    match nikon_fleet::ops::restore::undo_command_exact(&restore_point.file, undo_set, dir) {
        Some(command) => {
            ui.label("The same from a terminal:");
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(&command).monospace());
                if ui.small_button("Copy").clicked() {
                    chosen = Some(AppliedAction::Copy(command.clone()));
                }
            });
        }
        None if !undo_set.is_empty() => {
            ui.label(
                egui::RichText::new(format!("{} properties are too many for a command line: use Undo… above.", undo_set.len())).weak(),
            );
        }
        None => {}
    }
    if !undo_set.is_empty() {
        egui::CollapsingHeader::new(format!("Properties an undo puts back ({})", undo_set.len()))
            .id_salt("undo_set")
            .show(ui, |ui| ui.label(names_list(undo_set)));
    }
    chosen
}

impl FleetApp {
    pub(crate) fn ui_restore(&mut self, ctx: &egui::Context) {
        if !self.restore.open {
            return;
        }
        let mut open = true;
        let mut action: Option<RestoreAction> = None;
        let busy = self.busy();
        let data_ready = self.settings.data_dir.is_some();
        let data_dir = self.settings.data_dir.clone();
        let data_dir = data_dir.as_deref();

        egui::Window::new("Restore a camera from a stored snapshot")
            .open(&mut open)
            .default_size([700.0, 620.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                let r = &mut self.restore;
                let cameras = &self.cameras;
                let listing = self.listing.as_ref();
                egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(720.0).show(ui, |ui| {
                    ui.label(egui::RichText::new("1. The camera: a connected body").strong());
                    if cameras.is_empty() {
                        ui.label(egui::RichText::new("No camera is connected: use Discover first.").italics().weak());
                    }
                    ui.add_enabled_ui(!busy, |ui| {
                        let text = r.target_camera(cameras).map_or("choose a body…".to_string(), camera_label);
                        egui::ComboBox::from_id_salt("rs_target").selected_text(text).width(420.0).show_ui(ui, |ui| {
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
                    let camera_fw = r.target_camera(cameras).map(|c| c.firmware.clone());
                    ui.add_enabled_ui(!busy, |ui| {
                        let text = r
                            .source_file
                            .as_ref()
                            .map(|f| rows.iter().find(|row| &row.file == f).map_or_else(|| f.clone(), |row| restore_snapshot_label(row, camera_fw.as_deref())))
                            .unwrap_or_else(|| "choose a snapshot…".to_string());
                        egui::ComboBox::from_id_salt("rs_source").selected_text(text).width(560.0).show_ui(ui, |ui| {
                            for row in &rows {
                                let picked = r.source_file.as_ref() == Some(&row.file);
                                if ui.selectable_label(picked, restore_snapshot_label(row, camera_fw.as_deref())).clicked() {
                                    r.source_file = Some(row.file.clone());
                                }
                            }
                        });
                    });
                    ui.label(
                        egui::RichText::new("Only this body's own snapshots are offered; to put another body's settings here, use Transplant.").small().weak(),
                    );

                    ui.add_space(6.0);
                    let planning_possible = !busy
                        && data_ready
                        && refusal.is_none()
                        && r.request(cameras).is_some()
                        && !matches!(r.stage(), Stage::Applying(_) | Stage::Planning(_));
                    ui.add_enabled_ui(planning_possible, |ui| {
                        if ui.button("Plan (reads the camera, writes nothing)").clicked() {
                            action = Some(RestoreAction::Plan);
                        }
                    });
                    if !r.notice.is_empty() {
                        ui.colored_label(WARN_COLOR, &r.notice);
                    }
                    ui.separator();

                    match r.stage() {
                        Stage::Idle => {
                            ui.label(
                                egui::RichText::new("Choose a body and a snapshot, then plan. Nothing is sent to the camera until you have read the plan and confirmed it.")
                                    .weak(),
                            );
                        }
                        Stage::Planning(_) => {
                            ui.label("Planning… the camera is being read.");
                        }
                        Stage::Planned(p) => {
                            draw_restore_plan(ui, p, !busy, &mut action);
                            ui.separator();
                            ui.horizontal(|ui| {
                                ui.add_enabled_ui(!busy && p.applicable(), |ui| {
                                    if ui.button("Restore to camera…").clicked() {
                                        action = Some(RestoreAction::AskApply);
                                    }
                                });
                                if ui.button("Discard plan").clicked() {
                                    action = Some(RestoreAction::Discard);
                                }
                            });
                            if p.plan.would_change == 0 {
                                ui.label(egui::RichText::new("This plan would change nothing, so there is nothing to restore.").weak());
                            }
                        }
                        Stage::Aside(a) => match *a {},
                        Stage::ConfirmApply(p) => {
                            draw_restore_plan(ui, p, false, &mut action);
                            ui.separator();
                            ui.colored_label(WARN_COLOR, restore_confirm_text(p));
                            ui.add_enabled_ui(!busy, |ui| {
                                ui.horizontal(|ui| {
                                    if ui.button("Confirm: write to camera").clicked() {
                                        action = Some(RestoreAction::ConfirmApply);
                                    }
                                    if ui.button("Cancel").clicked() {
                                        action = Some(RestoreAction::CancelConfirmation);
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
                            let chosen = draw_applied_outcome(ui, t, "rs_applied", &a.outcome, busy, data_dir, restore_report_lines, |ui, report| {
                                if !report.written.is_empty() {
                                    egui::CollapsingHeader::new(format!("Properties written ({})", report.written.len())).id_salt("rs_written").show(ui, |ui| {
                                        ui.label(names_list(&report.written));
                                    });
                                }
                            });
                            if let Some(c) = chosen {
                                action = Some(RestoreAction::Applied(c));
                            }
                            ui.separator();
                            if ui.button("Close this result").clicked() {
                                action = Some(RestoreAction::Discard);
                            }
                        }
                    }
                });
            });
        self.restore.open = open;
        if let Some(a) = action {
            self.restore_act(ctx, a);
        }
    }

    fn restore_act(&mut self, ctx: &egui::Context, action: RestoreAction) {
        match action {
            RestoreAction::Plan => match self.restore.begin_plan(&self.cameras) {
                Ok(cmd) => self.send_restore(cmd),
                Err(e) => self.restore.notice = e,
            },
            RestoreAction::Toggle(name) => self.restore.toggle(&name),
            RestoreAction::PlanSelection => match self.restore.plan_selection(&self.cameras) {
                Ok(cmd) => self.send_restore(cmd),
                Err(e) => self.restore.notice = e,
            },
            RestoreAction::PlanEverything => match self.restore.plan_everything(&self.cameras) {
                Ok(cmd) => self.send_restore(cmd),
                Err(e) => self.restore.notice = e,
            },
            RestoreAction::AskApply => self.restore.ask_apply(),
            RestoreAction::ConfirmApply => {
                if let Some(cmd) = self.restore.confirm_apply() {
                    self.send_restore(cmd);
                }
            }
            RestoreAction::CancelConfirmation => self.restore.cancel_confirmation(),
            RestoreAction::Discard => self.restore.discard(),
            RestoreAction::Applied(a) => self.applied_action(ctx, a),
        }
    }

    /// Hand a restore command to the worker. If the worker is gone the command was not delivered,
    /// and the window must not wait for an answer that cannot come.
    pub(crate) fn send_restore(&mut self, cmd: Cmd) {
        if !self.send(cmd) {
            self.restore.not_sent();
        }
    }

    /// A click in a result view (either window's).
    pub(crate) fn applied_action(&mut self, ctx: &egui::Context, action: AppliedAction) {
        match action {
            AppliedAction::View(file) => self.open_snapshot(ctx, &file, false, false),
            AppliedAction::Copy(text) => ctx.copy_text(text),
            AppliedAction::Undo(offer) => self.start_undo(offer),
        }
    }

    /// Undo an apply: the restore window opens on its restore point with the apply's undo set, and
    /// the plan is made (read-only). The person reviews it and confirms it as any restore.
    pub(crate) fn start_undo(&mut self, offer: UndoOffer) {
        if self.busy() {
            self.status = Status::new(Level::Warn, "Another camera operation is running; try the undo when it has finished.");
            return;
        }
        // The restore point is a file in the data directory the apply used.
        if self.settings.data_dir.as_deref() != Some(offer.data_dir.as_path()) {
            self.restore.open = true;
            self.restore.notice = format!(
                "The data directory changed since this apply: its restore point {} is in {}. Switch back to undo it.",
                offer.restore_point,
                offer.data_dir.display()
            );
            return;
        }
        match self.restore.start_undo(&offer, &self.cameras) {
            Ok(cmd) => self.send_restore(cmd),
            Err(e) => {
                self.restore.open = true;
                self.restore.notice = e;
            }
        }
    }

    /// Open the window on `file` of `body`, a snapshot of a connected body.
    pub(crate) fn start_restore_from(&mut self, body: (String, String), file: String) {
        self.restore.start_from(body, file);
    }
}

/// A snapshot in the restore picker: when, which label and the firmware it was taken on, flagged when
/// that is not the firmware the camera runs.
fn restore_snapshot_label(row: &SnapRow, camera_firmware: Option<&str>) -> String {
    let differs = camera_firmware.is_some_and(|f| f != row.firmware);
    format!("{}  fw {}{}", snapshot_label(row), printable_text(&row.firmware), if differs { "  ⚠ other firmware than the camera's" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guarded::write_preflight;
    #[cfg(unix)]
    use crate::tests::{argv_of, recording_cli};
    use crate::tests::{cli_error, library_apply_json, row, scan_of, test_app, usb};
    use nikon_fleet::ops::apply::{EXIT_REFUSED_BEFORE_WRITE, EXIT_STALE_PLAN};
    use nikon_fleet::ops::baseline::{GatePolicy, baseline_for};
    use nikon_fleet::ops::port::{FakeCamera, Failure};
    use nikon_fleet::ops::restore::{RestoreSkip, StepOutcome};
    use nikon_fleet::ops::restore_flow::{
        BodyPolicy, RestoreApplyRequest, RestorePlanInput, apply_restore, build_restore_plan,
    };
    use nikon_fleet::sdk::{OP_GET, OP_SET};
    use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
    use std::collections::HashMap;
    use tempfile::TempDir;

    const T: Duration = Duration::from_secs(30);
    const FILE: &str = "2026-10-27T100000Z_Z_9_T1.json";
    const RW: u32 = OP_GET | OP_SET;

    /// The values of every property a fake Z 9 has, as the viewer would have rendered them.
    fn all_shown() -> SourceView {
        let values = baseline_for("Z 9")
            .unwrap()
            .readable
            .iter()
            .map(|c| (c.name.clone(), SourceValue { display_name: None, value: "stored".into(), notes: vec![] }))
            .collect();
        SourceView { values, loaded: true, note: None }
    }

    fn body() -> Camera {
        Camera { model: "Z 9".into(), serial: "T1".into(), firmware: "5.31".into() }
    }

    fn target() -> CameraRow {
        row("Z 9", "T1", 1)
    }

    fn request(only: Option<&[&str]>) -> RestoreRequest {
        RestoreRequest {
            source_file: FILE.into(),
            target: target(),
            only: only.map(|o| o.iter().map(|s| s.to_string()).collect()),
        }
    }

    /// A fake Z 9 that reads and writes exactly the baseline's capabilities (value 1 each) except
    /// three restore targets: two strings and a number, which differ from the stored snapshot.
    struct World {
        cam: FakeCamera,
        names: HashMap<u32, String>,
        targets: Vec<(String, u32)>,
        source: Snapshot,
        bytes: Vec<u8>,
    }

    /// `same`: indexes of targets that already hold what the snapshot says. `unreadable`: indexes
    /// of targets whose current value cannot be read. `fail_write`: index whose write fails.
    fn world_with(same: &[usize], unreadable: &[usize], fail_write: Option<usize>) -> World {
        let b = baseline_for("Z 9").unwrap();
        let targets: Vec<(String, u32)> = b.readable.iter().skip(6).take(3).map(|c| (c.name.clone(), c.code)).collect();
        let stored = |i: usize| if i == 2 { serde_json::json!(55) } else { serde_json::json!(format!("text {i}")) };
        let mut cam = FakeCamera::new();
        let mut names = HashMap::new();
        let mut source = Snapshot::new(body(), Transport::Usb, "2026-10-27T10:00:00Z".into());
        for c in &b.readable {
            let at = targets.iter().position(|(_, code)| *code == c.code);
            let (live, held) = match at {
                Some(i) => (if same.contains(&i) { stored(i) } else if i == 2 { serde_json::json!(1) } else { serde_json::json!("old") }, stored(i)),
                None => (serde_json::json!(1), serde_json::json!(1)),
            };
            cam = if at.is_some_and(|i| unreadable.contains(&i)) { cam.with_unreadable(c.code, RW) } else { cam.with(c.code, RW, live) };
            names.insert(c.code, c.name.clone());
            source.insert(c.name.clone(), c.code, held);
        }
        if let Some(i) = fail_write {
            cam = cam.failing_write(targets[i].1, Failure::Call(-125));
        }
        for i in unreadable {
            cam = cam.failing_read(targets[*i].1, Failure::Call(-1));
        }
        let bytes = source.to_pretty_json().unwrap().into_bytes();
        World { cam, names, targets, source, bytes }
    }

    fn world() -> World {
        world_with(&[], &[], None)
    }

    fn input<'a>(w: &'a World, target: &'a Camera, only: Option<&'a BTreeSet<String>>) -> RestorePlanInput<'a> {
        RestorePlanInput { source: &w.source, source_name: FILE, source_bytes: &w.bytes, target, only, body_policy: BodyPolicy::SameBody }
    }

    fn plan_text(w: &World, only: Option<&BTreeSet<String>>) -> String {
        let t = body();
        serde_json::to_string_pretty(&build_restore_plan(&w.cam, &input(w, &t, only)).unwrap()).unwrap()
    }

    fn plan_of(w: &World) -> RestorePlanJson {
        serde_json::from_str(&plan_text(w, None)).expect("the library's plan parses")
    }

    fn planned() -> PlannedRestore {
        PlannedRestore::new(request(None), plan_of(&world()), all_shown(), String::new(), false)
    }

    /// The library's own restore report for `w`'s camera, as the CLI prints it, and the digest of
    /// the plan it was held to.
    fn apply_text(w: &World, only: Option<&BTreeSet<String>>) -> (String, String) {
        let t = body();
        let plan = build_restore_plan(&w.cam, &input(w, &t, only)).unwrap();
        let dir = TempDir::new().unwrap();
        let report = apply_restore(
            &w.cam,
            &RestoreApplyRequest {
                plan: input(w, &t, only),
                snapshots_dir: dir.path(),
                name_lookup: &w.names,
                expected_digest: &plan.digest,
                expected_summary: &plan.summary,
                captured_at: "2026-10-27T17:26:00.688812Z".into(),
                gate_policy: GatePolicy::Strict,
            },
            &mut |_| {},
        )
        .expect("the apply ran");
        (serde_json::to_string_pretty(&report).unwrap(), plan.digest)
    }

    // ── the library's reports, read by the GUI ───────────────────────────

    #[test]
    fn a_plans_change_count_is_the_librarys_for_a_changing_a_same_and_an_unreadable_property() {
        // three differ: all three may change
        let all = plan_of(&world());
        assert_eq!(all.would_change, 3);
        assert_eq!(all.steps.iter().filter(|s| s.may_change()).count(), all.would_change);
        // one already holds the stored value: it is "would write", not a change
        let one_same = plan_of(&world_with(&[1], &[], None));
        assert_eq!(one_same.would_change, 2);
        assert_eq!(one_same.steps.iter().filter(|s| s.may_change()).count(), 2);
        let same_step = one_same.steps.iter().find(|s| s.changes == Some(false)).expect("a step the body already holds");
        assert_eq!(same_step.outcome.tag, StepTag::WouldWrite);
        assert!(!same_step.may_change());
        // a current value that cannot be read is a possible change, never "the same"
        let unknown = plan_of(&world_with(&[], &[2], None));
        let step = unknown.steps.iter().find(|s| s.changes.is_none() && s.outcome.tag == StepTag::WouldWrite).expect("an unreadable one");
        assert!(step.may_change());
        assert_eq!(unknown.would_change, 3);
        assert_eq!(unknown.steps.iter().filter(|s| s.may_change()).count(), 3);
        // every target the same: nothing to restore
        let none = plan_of(&world_with(&[0, 1, 2], &[], None));
        assert_eq!((none.would_change, none.steps.iter().filter(|s| s.may_change()).count()), (0, 0));
    }

    #[test]
    fn a_plan_keeps_the_names_it_is_about_and_nothing_it_was_not_asked_for() {
        let w = world();
        let only = BTreeSet::from([w.targets[0].0.clone()]);
        let p: RestorePlanJson = serde_json::from_str(&plan_text(&w, Some(&only))).unwrap();
        assert_eq!(p.only, Some(vec![w.targets[0].0.clone()]));
        assert_eq!(p.steps.iter().filter(|s| s.outcome.tag == StepTag::WouldWrite).count(), 1);
        assert!(p.counts.not_selected > 0 && p.steps.iter().all(|s| s.outcome.tag != StepTag::NotSelected), "not-selected is counted, not listed");
        assert_eq!((p.source.serial.as_str(), p.target.serial.as_str()), ("T1", "T1"));
    }

    #[test]
    fn every_step_tag_the_library_can_emit_is_one_the_gui_reads() {
        // These two matches name every variant: a new one fails to compile here, then the list below.
        fn every_outcome(o: &StepOutcome) {
            match o {
                StepOutcome::NotSelected | StepOutcome::Skipped(_) | StepOutcome::WouldWrite | StepOutcome::Written
                | StepOutcome::UnsupportedWriteType | StepOutcome::WriteFailed(_) => {}
            }
        }
        fn every_skip(s: RestoreSkip) {
            match s {
                RestoreSkip::NeverCopied | RestoreSkip::Absent | RestoreSkip::ReadOnly | RestoreSkip::RangeScaleDiffers => {}
            }
        }
        let outcomes = [
            StepOutcome::NotSelected,
            StepOutcome::Skipped(RestoreSkip::NeverCopied),
            StepOutcome::Skipped(RestoreSkip::Absent),
            StepOutcome::Skipped(RestoreSkip::ReadOnly),
            StepOutcome::Skipped(RestoreSkip::RangeScaleDiffers),
            StepOutcome::WouldWrite,
            StepOutcome::Written,
            StepOutcome::UnsupportedWriteType,
            StepOutcome::WriteFailed("-125".into()),
        ];
        let mut seen = BTreeSet::new();
        for o in &outcomes {
            every_outcome(o);
            if let StepOutcome::Skipped(s) = o {
                every_skip(*s);
            }
            let parsed: StepOutcomeJson = serde_json::from_value(serde_json::to_value(o).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", o.tag()));
            assert_ne!(parsed.tag, StepTag::Unknown, "the library's own tag {:?} is one the GUI knows", o.tag());
            assert_eq!(parsed.raw_tag, o.tag(), "the tag as written is kept");
            seen.insert(o.tag());
            let _ = parsed.tag.plan_words();
        }
        assert_eq!(seen.len(), outcomes.len(), "nine different tags");
        // a tag from another build is kept with its text, never a reason to lose the whole report
        let junk: StepOutcomeJson = serde_json::from_value(serde_json::json!({"tag": "a_new_kind_of_step"})).unwrap();
        assert_eq!((junk.tag, junk.raw_tag.as_str()), (StepTag::Unknown, "a_new_kind_of_step"));
        assert_eq!(StepTag::Unknown.section(false), PlanSection::Attention, "and it is listed where it will be seen");
        assert!(serde_json::from_value::<StepOutcomeJson>(serde_json::json!({"detail": 1})).is_err(), "a step with no tag at all is not a step");
    }

    #[test]
    fn a_skip_is_filed_by_the_library_s_severity_and_every_skip_tag_maps_back_to_its_skip() {
        for skip in RestoreSkip::ALL {
            let parsed: StepOutcomeJson = serde_json::from_value(serde_json::to_value(StepOutcome::Skipped(skip)).unwrap()).unwrap();
            assert_eq!(parsed.tag.skip(), Some(skip), "{}: the GUI's tag names the library's skip", skip.tag());
            let want = if skip.is_routine() { PlanSection::Routine } else { PlanSection::Attention };
            // `changes` is about writes; a skip is filed the same whatever it carries
            assert_eq!(parsed.tag.section(false), want, "{}", skip.tag());
            assert_eq!(parsed.tag.section(true), want, "{}", skip.tag());
        }
        // pinned by name too, so a change of the library's table is a visible decision here
        assert_eq!(StepTag::RangeScaleDiffers.section(false), PlanSection::Attention);
        assert_eq!(StepTag::ReadOnly.section(false), PlanSection::Routine);
        // outcomes that are not skips name no skip
        for t in [StepTag::NotSelected, StepTag::WouldWrite, StepTag::Written, StepTag::UnsupportedWriteType, StepTag::WriteFailed, StepTag::Unknown] {
            assert_eq!(t.skip(), None, "{t:?}");
        }
    }

    #[test]
    fn every_verdict_the_library_can_emit_is_one_the_gui_reads_and_another_is_kept() {
        let mut seen = BTreeSet::new();
        for v in Verdict::ALL {
            let parsed: VerdictJson = serde_json::from_value(serde_json::to_value(v).unwrap()).unwrap();
            assert_eq!(parsed, VerdictJson::Known(v), "{}", v.tag());
            let _ = verdict_level(v);
            seen.insert(v.tag());
        }
        assert_eq!(seen.len(), 5);
        let other: VerdictJson = serde_json::from_value(serde_json::json!("a_new_verdict")).unwrap();
        assert_eq!(other, VerdictJson::Unknown("a_new_verdict".into()));
        // ... and a report that carries one is still a report, with its restore point
        let mut v: serde_json::Value = serde_json::from_str(&apply_text(&world(), None).0).unwrap();
        v["verification"]["verdict"] = serde_json::json!("a_new_verdict");
        let r: RestoreApplyJson = serde_json::from_value(v).expect("the report is read");
        assert!(r.restore_point.is_some());
        let lines = restore_report_lines(&r);
        assert!(lines.iter().any(|(l, t)| *l == Level::Warn && t.contains("a verdict this program does not know") && t.contains("a_new_verdict")), "{lines:?}");
    }

    #[test]
    fn the_librarys_restore_reports_are_read_for_each_way_an_apply_ends() {
        let verified = apply_text(&world(), None).0;
        let r: RestoreApplyJson = serde_json::from_str(&verified).expect("verified");
        assert_eq!((r.status, r.no_op), (ApplyStatus::AppliedVerified, false));
        // A full restore writes every property of the snapshot, the ones that already match included.
        assert!(r.restore_point.is_some() && !r.undo_set.is_empty());
        // ... and an undo puts back every one of them from the restore point.
        assert!(r.written.len() > 3 && r.undo_set.len() == r.written.len(), "{} written, {} to undo", r.written.len(), r.undo_set.len());
        assert_eq!(r.verification.as_ref().map(|v| v.verdict.clone()), Some(VerdictJson::Known(Verdict::Verified)));

        let failed: RestoreApplyJson = serde_json::from_str(&apply_text(&world_with(&[], &[], Some(1)), None).0).expect("failed");
        assert_eq!(failed.status, ApplyStatus::Failed);
        assert_eq!(failed.write_errors.len(), 1);
        assert!(failed.restore_point.is_some(), "a failed apply still names its restore point");

        let nothing: RestoreApplyJson = serde_json::from_str(&apply_text(&world_with(&[0, 1, 2], &[], None), None).0).expect("no-op");
        assert!(nothing.no_op && nothing.restore_point.is_none() && nothing.undo_set.is_empty());
    }

    // ── what is asked of the CLI ─────────────────────────────────────────

    #[test]
    fn a_plan_asks_for_a_dry_run_and_an_apply_presents_the_confirmed_plan_and_is_never_one() {
        let d = Path::new("/data");
        let plan = restore_argv(d, &request(None), None).unwrap();
        assert!(plan.contains(&"--dry-run".to_string()) && !plan.iter().any(|a| a.starts_with("--expect")), "{plan:?}");
        let apply = restore_argv(d, &request(None), Some(("dig", "-a summary: 1"))).unwrap();
        assert!(apply.contains(&"--expect-digest=dig".to_string()), "{apply:?}");
        assert!(apply.contains(&"--expect-summary=-a summary: 1".to_string()), "a value starting with - stays attached: {apply:?}");
        assert!(!apply.contains(&"--dry-run".to_string()));
        for argv in [&plan, &apply] {
            assert!(argv.contains(&"--json".to_string()) && argv.contains(&"--serial=T1".to_string()), "{argv:?}");
            let dashes = argv.iter().position(|a| a == "--").expect("-- before the file");
            assert_eq!(dashes, argv.len() - 2, "only the file follows: {argv:?}");
        }
    }

    #[test]
    fn the_selection_reaches_the_cli_one_property_per_flag_and_an_empty_one_is_no_flag() {
        let d = Path::new("/data");
        assert!(!restore_argv(d, &request(None), None).unwrap().iter().any(|a| a.starts_with("--only")));
        let some = restore_argv(d, &request(Some(&["kNkMAIDCapability_A", "kNkMAIDCapability_B"])), None).unwrap();
        let only: Vec<&String> = some.iter().filter(|a| a.starts_with("--only=")).collect();
        assert_eq!(only, ["--only=kNkMAIDCapability_A", "--only=kNkMAIDCapability_B"]);
    }

    #[test]
    fn the_snapshot_reaches_the_cli_as_an_absolute_path_inside_the_data_directory() {
        let argv = restore_argv(Path::new("data"), &request(None), None).unwrap();
        let file = argv.last().unwrap();
        assert!(Path::new(file).is_absolute(), "{file}");
        assert!(Path::new(file).ends_with(Path::new("snapshots").join(FILE)), "{file}");
        let mut bad = request(None);
        bad.source_file = "../elsewhere.json".into();
        assert!(restore_argv(Path::new("data"), &bad, None).is_err(), "a path is not a snapshot name");
    }

    // ── a plan, asked of the CLI ─────────────────────────────────────────

    #[test]
    fn a_plan_for_another_body_firmware_snapshot_or_selection_is_not_shown() {
        let plan = plan_of(&world());
        assert!(check_restore_plan_is_for(&plan, &request(None)).is_ok());
        let mut other = request(None);
        other.target.serial = "OTHER".into();
        assert!(check_restore_plan_is_for(&plan, &other).unwrap_err().contains("not for the Z 9 OTHER"));
        let mut newer = request(None);
        newer.target.firmware = "5.40".into();
        assert!(check_restore_plan_is_for(&plan, &newer).unwrap_err().contains("firmware 5.31"));
        let mut from_elsewhere = plan.clone();
        from_elsewhere.source.serial = "SRC".into();
        assert!(check_restore_plan_is_for(&from_elsewhere, &request(None)).unwrap_err().contains("transplant"));
        let mut other_model = plan.clone();
        other_model.source.model = "Z6_3".into();
        assert!(check_restore_plan_is_for(&other_model, &request(None)).is_err());
        // a selection the plan does not carry (either way round, and a different set)
        assert!(check_restore_plan_is_for(&plan, &request(Some(&["kNkMAIDCapability_A"]))).is_err());
        let mut limited = plan.clone();
        limited.only = Some(vec!["kNkMAIDCapability_A".into()]);
        assert!(check_restore_plan_is_for(&limited, &request(None)).is_err());
        assert!(check_restore_plan_is_for(&limited, &request(Some(&["kNkMAIDCapability_A"]))).is_ok());
        assert!(check_restore_plan_is_for(&limited, &request(Some(&["kNkMAIDCapability_B"]))).is_err());
        limited.only = Some(vec![]);
        assert!(check_restore_plan_is_for(&limited, &request(Some(&[]))).is_ok(), "empty and empty agree; the CLI refuses an empty selection before this");
    }

    #[cfg(unix)]
    #[test]
    fn a_plan_runs_the_cli_as_a_dry_run_and_shows_what_it_printed() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("plan.json"), plan_text(&world(), None)).unwrap();
        let cli = recording_cli(dir.path(), &format!("cat '{}/plan.json'\necho 'a warning' >&2", dir.path().display()));
        let (plan, log) = restore_plan_via(&cli, Path::new("/data"), &request(None), &|| scan_of(vec![usb("Z 9", "T1")]), T).unwrap();
        assert_eq!(plan, plan_of(&world()));
        assert_eq!(log, "a warning");
        assert!(argv_of(dir.path()).contains(&"--dry-run".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn a_plan_the_cli_printed_before_failing_is_shown_and_one_it_never_printed_is_an_error() {
        let dir = TempDir::new().unwrap();
        // Named properties that cannot be restored: the plan is printed, then the command ends 1.
        let w = world();
        let mut unrestorable: serde_json::Value = serde_json::from_str(&plan_text(&w, None)).unwrap();
        unrestorable["unrestorable"] = serde_json::json!([["kNkMAIDCapability_X", "read_only"]]);
        fs::write(dir.path().join("plan.json"), unrestorable.to_string()).unwrap();
        let cli = recording_cli(dir.path(), &format!("cat '{}/plan.json'\necho 'targeted restore incomplete' >&2\nexit 1", dir.path().display()));
        let scan = || scan_of(vec![usb("Z 9", "T1")]);
        let (plan, _) = restore_plan_via(&cli, Path::new("/d"), &request(None), &scan, T).unwrap();
        assert_eq!(plan.unrestorable, [("kNkMAIDCapability_X".to_string(), "read_only".to_string())]);

        let busy = recording_cli(dir.path(), "echo 'camera busy' >&2\nexit 1");
        let e = restore_plan_via(&busy, Path::new("/d"), &request(None), &scan, T).unwrap_err();
        assert!(e.starts_with("Planning failed") && e.contains("camera busy"), "{e}");
        let junk = recording_cli(dir.path(), "echo 'not json'");
        let e = restore_plan_via(&junk, Path::new("/d"), &request(None), &scan, T).unwrap_err();
        assert!(e.contains("nothing was written"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_target_never_starts_the_cli() {
        let dir = TempDir::new().unwrap();
        let cli = recording_cli(dir.path(), "exit 0");
        let two = || scan_of(vec![usb("Z 9", "T1"), usb("Z 9", "T2")]);
        assert!(restore_plan_via(&cli, Path::new("/d"), &request(None), &two, T).unwrap_err().starts_with("Restore refused"));
        let out = restore_apply_via(&cli, Path::new("/d"), &request(None), "d", "s", &two, T);
        assert!(matches!(out, ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, .. }), "{out:?}");
        let mut no_serial = request(None);
        no_serial.target.serial = "id-1".into();
        let out = restore_apply_via(&cli, Path::new("/d"), &no_serial, "d", "s", &|| scan_of(vec![usb("Z 9", "id-1")]), T);
        assert!(matches!(out, ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, .. }), "{out:?}");
        assert!(!dir.path().join("argv.txt").exists(), "the CLI was never run");
    }

    // ── an apply, asked of the CLI ───────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn an_apply_runs_the_cli_with_the_confirmed_plan_and_reports_the_status_it_printed() {
        let dir = TempDir::new().unwrap();
        let (json, digest) = apply_text(&world_with(&[], &[], Some(1)), None);
        fs::write(dir.path().join("report.json"), &json).unwrap();
        // A failed apply exits non-zero after printing its report.
        let cli = recording_cli(dir.path(), &format!("cat '{}/report.json'\nexit 1", dir.path().display()));
        let out = restore_apply_via(&cli, Path::new("/data"), &request(None), &digest, "the summary", &|| scan_of(vec![usb("Z 9", "T1")]), T);
        let ApplyOutcome::Report { report, .. } = out else { panic!("{out:?}") };
        assert_eq!(report.status, ApplyStatus::Failed);
        let argv = argv_of(dir.path());
        assert!(argv.contains(&format!("--expect-digest={digest}")) && argv.contains(&"--expect-summary=the summary".to_string()), "{argv:?}");
        assert!(!argv.contains(&"--dry-run".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn a_report_of_another_plan_than_the_one_confirmed_is_not_the_answer() {
        let (json, _) = apply_text(&world(), None);
        let out = classify_apply_as::<RestoreApplyJson>(Ok(CliOutput { stdout: json, stderr: String::new() }), "not-the-confirmed-digest");
        let ApplyOutcome::Indeterminate { message, .. } = out else { panic!("{out:?}") };
        assert!(message.contains("different plan"), "{message}");
    }

    #[test]
    fn without_a_report_only_the_clis_own_refusals_say_nothing_was_written() {
        let classify = |e| classify_apply_as::<RestoreApplyJson>(Err(e), "d");
        let stale = classify(cli_error(CliFailure::Exit, Some(EXIT_STALE_PLAN), ""));
        assert!(matches!(stale, ApplyOutcome::NotApplied { why: NotAppliedWhy::PlanStale, .. }), "{stale:?}");
        let refused = classify(cli_error(CliFailure::Exit, Some(EXIT_REFUSED_BEFORE_WRITE), ""));
        assert!(matches!(refused, ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, .. }), "{refused:?}");
        let usage = classify(cli_error(CliFailure::Exit, Some(2), ""));
        assert!(matches!(usage, ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, .. }), "the parser ran nothing: {usage:?}");
        let never_ran = classify(cli_error(CliFailure::Spawn, None, ""));
        assert!(matches!(never_ran, ApplyOutcome::NotApplied { .. }));
        for e in [
            cli_error(CliFailure::TimedOut, None, ""),
            cli_error(CliFailure::Wait, None, ""),
            cli_error(CliFailure::Exit, None, ""),
            cli_error(CliFailure::Exit, Some(1), ""),
            cli_error(CliFailure::Exit, Some(4), ""),
            cli_error(CliFailure::Exit, Some(101), ""),
            cli_error(CliFailure::Exit, Some(3), ""),
            cli_error(CliFailure::Exit, Some(EXIT_REFUSED_BEFORE_WRITE + 1), ""),
        ] {
            let label = format!("{:?} {:?}", e.kind, e.code);
            let out = classify(e);
            assert!(matches!(out, ApplyOutcome::Indeterminate { .. }), "{label}: {out:?}");
        }
        let exited_zero_without_a_report = classify_apply_as::<RestoreApplyJson>(Ok(CliOutput { stdout: String::new(), stderr: String::new() }), "d");
        assert!(matches!(exited_zero_without_a_report, ApplyOutcome::Indeterminate { .. }));
    }

    // ── the stages ───────────────────────────────────────────────────────

    fn ui_ready() -> (RestoreUi, Vec<CameraRow>) {
        let ui = RestoreUi {
            open: true,
            target_key: Some(("Z 9".into(), "T1".into())),
            source_file: Some(FILE.into()),
            ..RestoreUi::default()
        };
        (ui, vec![target()])
    }

    fn ui_with_plan() -> (RestoreUi, Vec<CameraRow>) {
        let (mut ui, cameras) = ui_ready();
        ui.sync(&cameras);
        let Cmd::Plan(PlanJob::Restore(request)) = ui.begin_plan(&cameras).unwrap() else { panic!("a plan command") };
        ui.plan_arrived(request, Ok((plan_of(&world()), String::new(), all_shown())));
        assert!(matches!(ui.stage(), Stage::Planned(_)));
        (ui, cameras)
    }

    #[test]
    fn nothing_is_asked_of_the_camera_until_a_body_and_its_snapshot_are_chosen() {
        let mut ui = RestoreUi::default();
        assert!(ui.begin_plan(&[target()]).is_err());
        ui.source_file = Some(FILE.into());
        assert!(ui.begin_plan(&[target()]).is_err(), "no body yet");
        ui.target_key = Some(("Z 9".into(), "T1".into()));
        assert!(ui.begin_plan(&[]).is_err(), "the chosen body is not connected");
        assert_eq!(*ui.stage(), Stage::Idle);
        assert!(matches!(ui.begin_plan(&[row("Z 9", "T1", 2)]), Err(_)), "a body that shares its model cannot be aimed at");
        assert!(matches!(ui.begin_plan(&[target()]), Ok(Cmd::Plan(PlanJob::Restore(_)))));
        assert!(ui.begin_plan(&[target()]).is_err(), "one plan at a time");
    }

    #[test]
    fn a_plan_that_comes_back_for_another_request_is_dropped_and_a_failed_one_is_a_notice() {
        let (mut ui, cameras) = ui_ready();
        ui.sync(&cameras);
        let Cmd::Plan(PlanJob::Restore(asked)) = ui.begin_plan(&cameras).unwrap() else { panic!() };
        let mut other = asked.clone();
        other.source_file = "another.json".into();
        ui.plan_arrived(other, Ok((plan_of(&world()), String::new(), all_shown())));
        assert!(matches!(ui.stage(), Stage::Planning(_)), "still waiting for the plan that was asked for");
        ui.plan_arrived(asked, Err("Planning failed: camera busy".into()));
        assert_eq!(*ui.stage(), Stage::Idle);
        assert!(ui.notice.contains("camera busy"));
        let stray = request(None);
        ui.plan_arrived(stray, Ok((plan_of(&world()), String::new(), all_shown())));
        assert_eq!(*ui.stage(), Stage::Idle, "a plan nobody asked for is not shown");
    }

    #[test]
    fn apply_is_confirmed_in_two_steps_and_sends_the_plan_that_was_shown() {
        let (mut ui, _) = ui_with_plan();
        let shown = plan_of(&world());
        assert!(ui.confirm_apply().is_none(), "no confirmation was asked for");
        ui.ask_apply();
        assert!(matches!(ui.stage(), Stage::ConfirmApply(_)));
        ui.cancel_confirmation();
        assert!(matches!(ui.stage(), Stage::Planned(_)));
        assert!(ui.confirm_apply().is_none(), "cancelled");
        ui.ask_apply();
        let Some(Cmd::Apply(ApplyJob::Restore { request: sent, digest, summary })) = ui.confirm_apply() else { panic!("an apply") };
        assert_eq!((sent, digest, summary), (request(None), shown.digest, shown.summary));
        assert!(matches!(ui.stage(), Stage::Applying(_)));
        assert!(ui.confirm_apply().is_none(), "no second apply while one is on the wire");
        assert!(ui.begin_plan(&[target()]).is_err(), "nor a plan");
    }

    #[test]
    fn a_plan_that_would_change_nothing_or_names_what_cannot_be_restored_cannot_be_applied() {
        let (mut ui, _) = ui_ready();
        let cameras = vec![target()];
        ui.sync(&cameras);
        let Cmd::Plan(PlanJob::Restore(request)) = ui.begin_plan(&cameras).unwrap() else { panic!() };
        ui.plan_arrived(request.clone(), Ok((plan_of(&world_with(&[0, 1, 2], &[], None)), String::new(), all_shown())));
        ui.ask_apply();
        assert!(matches!(ui.stage(), Stage::Planned(_)), "nothing to restore");
        // one change is enough (the boundary)
        let one = plan_of(&world_with(&[0, 1], &[], None));
        assert_eq!(one.would_change, 1);
        assert!(PlannedRestore::new(request.clone(), one, all_shown(), String::new(), false).applicable());
        // a change count of zero is not
        assert!(!PlannedRestore::new(request.clone(), plan_of(&world_with(&[0, 1, 2], &[], None)), all_shown(), String::new(), false).applicable());
        let mut named = plan_of(&world());
        named.unrestorable = vec![("kNkMAIDCapability_X".into(), "read_only".into())];
        assert!(!PlannedRestore::new(request, named, all_shown(), String::new(), false).applicable());
    }

    #[test]
    fn a_changed_form_or_camera_withdraws_the_plan_and_each_confirmation() {
        let changed: Vec<(&str, Box<dyn Fn(&mut RestoreUi, &mut Vec<CameraRow>)>)> = vec![
            ("firmware", Box::new(|_, c| c[0].firmware = "5.40".into())),
            ("pairing", Box::new(|_, c| c[0].model_count = 2)),
            ("body gone", Box::new(|_, c| c.clear())),
            ("snapshot", Box::new(|u, _| u.source_file = Some("another.json".into()))),
            ("other body", Box::new(|u, c| {
                c.push(row("Z 9", "T2", 1));
                u.target_key = Some(("Z 9".into(), "T2".into()));
            })),
        ];
        for (what, change) in &changed {
            for confirming in [false, true] {
                let (mut ui, mut cameras) = ui_with_plan();
                if confirming {
                    ui.ask_apply();
                    assert!(matches!(ui.stage(), Stage::ConfirmApply(_)));
                }
                ui.sync(&cameras);
                assert!(!matches!(ui.stage(), Stage::Idle), "{what}: nothing changed yet");
                change(&mut ui, &mut cameras);
                ui.sync(&cameras);
                assert_eq!(*ui.stage(), Stage::Idle, "{what} (confirming: {confirming})");
                assert!(ui.notice.contains("withdrawn"), "{what}: {}", ui.notice);
                assert!(ui.confirm_apply().is_none(), "{what}: nothing is left to confirm");
            }
        }
        // a plan still being made is withdrawn too, and its answer is then ignored
        let (mut ui, mut cameras) = ui_ready();
        ui.sync(&cameras);
        let Cmd::Plan(PlanJob::Restore(request)) = ui.begin_plan(&cameras).unwrap() else { panic!() };
        cameras[0].firmware = "5.40".into();
        ui.sync(&cameras);
        assert_eq!(*ui.stage(), Stage::Idle);
        ui.plan_arrived(request, Ok((plan_of(&world()), String::new(), all_shown())));
        assert_eq!(*ui.stage(), Stage::Idle);
    }

    #[test]
    fn an_apply_in_flight_is_not_withdrawn_by_a_change_and_its_answer_is_kept() {
        let (mut ui, mut cameras) = ui_with_plan();
        ui.ask_apply();
        let Some(Cmd::Apply(ApplyJob::Restore { request, .. })) = ui.confirm_apply() else { panic!() };
        cameras[0].firmware = "5.40".into();
        ui.source_file = Some("another.json".into());
        ui.sync(&cameras);
        assert!(matches!(ui.stage(), Stage::Applying(_)), "the answer must still find it");
        // an answer for another request is not this apply's
        let mut elsewhere = request.clone();
        elsewhere.source_file = "another.json".into();
        ui.applied(elsewhere, ApplyOutcome::Indeterminate { message: "x".into(), cli_log: String::new() });
        assert!(matches!(ui.stage(), Stage::Applying(_)));
        ui.applied(request, ApplyOutcome::NotApplied { why: NotAppliedWhy::PlanStale, message: "m".into(), cli_log: String::new() });
        assert!(matches!(ui.stage(), Stage::Done(_)));
    }

    #[test]
    fn a_change_of_data_directory_withdraws_a_plan_and_each_confirmation_but_not_an_apply_on_the_wire() {
        for confirming in [false, true] {
            let (mut ui, _) = ui_with_plan();
            if confirming {
                ui.ask_apply();
            }
            ui.data_dir_changed();
            assert_eq!(*ui.stage(), Stage::Idle, "confirming: {confirming}");
            assert!(ui.notice.contains("data directory"));
        }
        let (mut ui, _) = ui_with_plan();
        ui.ask_apply();
        ui.confirm_apply().unwrap();
        ui.data_dir_changed();
        assert!(matches!(ui.stage(), Stage::Applying(_)));
    }

    #[test]
    fn a_dead_worker_ends_a_plan_as_lost_and_an_apply_as_unknown_never_as_unchanged() {
        let (mut ui, cameras) = ui_ready();
        ui.sync(&cameras);
        ui.begin_plan(&cameras).unwrap();
        ui.worker_lost();
        assert_eq!(*ui.stage(), Stage::Idle);
        assert!(ui.notice.contains("Nothing was written"));

        let (mut ui, _) = ui_with_plan();
        ui.ask_apply();
        ui.confirm_apply().unwrap();
        ui.worker_lost();
        let Stage::Done(done) = ui.stage() else { panic!("{:?}", ui.stage()) };
        assert!(matches!(done.outcome, ApplyOutcome::Indeterminate { .. }), "{:?}", done.outcome);
        assert!(applied_status_of("Restore", &done.outcome).text.contains("may have"), "said as unknown");

        // the command never reached the worker: then the camera was not asked
        let (mut ui, _) = ui_with_plan();
        ui.ask_apply();
        ui.confirm_apply().unwrap();
        ui.not_sent();
        let Stage::Done(done) = ui.stage() else { panic!() };
        assert!(matches!(done.outcome, ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, .. }));
        // a plan being reviewed is untouched by a dead worker (nothing is in flight)
        let (mut ui, _) = ui_with_plan();
        ui.worker_lost();
        assert!(matches!(ui.stage(), Stage::Planned(_)));
    }

    // ── narrowing the restore ────────────────────────────────────────────

    #[test]
    fn a_selection_starts_as_every_changing_property_and_only_those_can_be_ticked() {
        let (mut ui, _) = ui_with_plan();
        let Stage::Planned(p) = ui.stage() else { panic!() };
        assert_eq!(p.selected.len(), 3);
        assert!(!p.can_plan_selection(), "everything ticked is not a narrower restore");
        let first = p.changing()[0].name.clone();
        ui.toggle(&first);
        let Stage::Planned(p) = ui.stage() else { panic!() };
        assert_eq!(p.selected.len(), 2);
        assert!(p.can_plan_selection());
        ui.toggle("kNkMAIDCapability_NotInThePlan");
        let Stage::Planned(p) = ui.stage() else { panic!() };
        assert_eq!(p.selected.len(), 2, "a property the plan does not change cannot be ticked");
    }

    #[test]
    fn narrowing_at_none_one_and_all_ticked() {
        let mk = |same: &[usize]| {
            let (mut ui, cameras) = ui_ready();
            ui.sync(&cameras);
            let Cmd::Plan(PlanJob::Restore(request)) = ui.begin_plan(&cameras).unwrap() else { panic!() };
            ui.plan_arrived(request, Ok((plan_of(&world_with(same, &[], None)), String::new(), all_shown())));
            (ui, cameras)
        };
        let (mut ui, cameras) = mk(&[]);
        let names: Vec<String> = ui.selected_for_test();
        for n in &names {
            ui.toggle(n);
        }
        let Stage::Planned(none) = ui.stage() else { panic!() };
        assert!(none.selected.is_empty() && !none.can_plan_selection(), "nothing ticked is not a narrower restore");
        assert!(ui.plan_selection(&cameras).is_err(), "nothing ticked is not a restore of everything");
        assert!(matches!(ui.stage(), Stage::Planned(_)) && ui.only().is_none(), "and nothing was changed");
        ui.toggle(&names[1]);
        let Stage::Planned(p) = ui.stage() else { panic!() };
        assert!(p.can_plan_selection(), "one of three");
        let Ok(Cmd::Plan(PlanJob::Restore(request))) = ui.plan_selection(&cameras) else { panic!("a new plan") };
        assert_eq!(request.only, Some(BTreeSet::from([names[1].clone()])));
        assert!(matches!(ui.stage(), Stage::Planning(_)));
        // two of three, then everything ticked again
        let (mut ui, _) = mk(&[]);
        ui.toggle(&names[0]);
        let Stage::Planned(p) = ui.stage() else { panic!() };
        assert!(p.can_plan_selection());
        ui.toggle(&names[0]);
        let Stage::Planned(p) = ui.stage() else { panic!() };
        assert!(!p.can_plan_selection());
    }

    impl RestoreUi {
        fn selected_for_test(&self) -> Vec<String> {
            match &self.stage {
                Stage::Planned(p) => p.selected.iter().cloned().collect(),
                _ => Vec::new(),
            }
        }
    }

    #[test]
    fn a_selection_belongs_to_its_snapshot_and_body_and_widening_drops_it() {
        let (mut ui, cameras) = ui_ready();
        ui.sync(&cameras);
        ui.only = Some(BTreeSet::from(["kNkMAIDCapability_A".to_string()]));
        ui.sync(&cameras);
        assert!(ui.only().is_some(), "same snapshot and body");
        ui.source_file = Some("another.json".into());
        ui.sync(&cameras);
        assert!(ui.only().is_none(), "another snapshot has other properties");
        ui.source_file = Some(FILE.into());
        ui.sync(&cameras);
        ui.only = Some(BTreeSet::from(["kNkMAIDCapability_A".to_string()]));
        let mut newer = cameras.clone();
        newer[0].firmware = "5.40".into();
        ui.sync(&newer);
        assert!(ui.only().is_none(), "the whole row: a firmware change is another body");

        let (mut ui, cameras) = ui_ready();
        ui.sync(&cameras);
        let Cmd::Plan(PlanJob::Restore(request)) = ui.begin_plan(&cameras).unwrap() else { panic!() };
        ui.plan_arrived(request, Ok((plan_of(&world()), String::new(), all_shown())));
        ui.only = Some(BTreeSet::from(["kNkMAIDCapability_A".to_string()]));
        let Ok(Cmd::Plan(PlanJob::Restore(request))) = ui.plan_everything(&cameras) else { panic!() };
        assert_eq!(request.only, None);
    }

    // ── the confirmation and the report ──────────────────────────────────

    #[test]
    fn the_confirmation_names_the_body_the_snapshot_the_summary_and_any_limit() {
        let text = restore_confirm_text(&planned());
        let p = planned();
        for needle in ["Z 9", "serial T1", "firmware 5.31", FILE, p.plan.summary.as_str(), "restore point"] {
            assert!(text.contains(needle), "{needle:?} missing from:\n{text}");
        }
        assert!(text.contains(&format!("the settings stored in {FILE}")), "the snapshot is named in the question itself: {text}");
        assert!(text.contains("3 properties"), "{text}");
        assert!(!text.contains("limited"), "{text}");
        let w = world();
        let only = BTreeSet::from([w.targets[0].0.clone()]);
        let limited: RestorePlanJson = serde_json::from_str(&plan_text(&w, Some(&only))).unwrap();
        let mut req = request(None);
        req.only = Some(only);
        let text = restore_confirm_text(&PlannedRestore::new(req, limited, all_shown(), String::new(), false));
        assert!(text.contains("1 property") && text.contains("limited to the 1 propert"), "{text}");
    }

    #[test]
    fn a_failed_restore_says_some_writes_may_have_landed_and_points_to_the_restore_point() {
        let failed: RestoreApplyJson = serde_json::from_str(&apply_text(&world_with(&[], &[], Some(1)), None).0).unwrap();
        let lines = restore_report_lines(&failed);
        let (level, first) = &lines[0];
        assert_eq!(*level, Level::Error);
        assert!(first.contains("may have landed") && first.contains("restore point"), "{first}");
        assert!(lines.iter().any(|(l, t)| *l == Level::Error && t.starts_with("Write failed")), "{lines:?}");
        let status = applied_status_of("Restore", &ApplyOutcome::Report { report: Box::new(failed.clone()), note: None, cli_log: String::new() });
        assert_eq!(status.level, Level::Error);
        assert!(status.text.contains("may have landed"), "{}", status.text);
        // a failed apply that took no restore point has nothing to point to
        let mut bare = failed;
        bare.restore_point = None;
        assert!(!restore_report_lines(&bare).iter().any(|(_, t)| t.contains("may have landed")));
        // a verified one has no error line; a written property the read-back cannot compare (a clock) is a warning, not hidden
        let ok: RestoreApplyJson = serde_json::from_str(&apply_text(&world(), None).0).unwrap();
        let lines = restore_report_lines(&ok);
        assert!(lines.iter().all(|(l, _)| *l != Level::Error), "{lines:?}");
        assert!(lines.iter().any(|(_, t)| t.contains("reads back as restored")), "{lines:?}");
        assert!(lines.iter().any(|(l, t)| *l == Level::Warn && t.starts_with("Written but not compared")), "{lines:?}");
    }

    // ── undo ─────────────────────────────────────────────────────────────

    fn report(json: &str) -> ApplyOutcome<RestoreApplyJson> {
        ApplyOutcome::Report { report: Box::new(serde_json::from_str(json).unwrap()), note: None, cli_log: String::new() }
    }

    #[test]
    fn an_apply_that_wrote_offers_an_undo_of_what_it_put_there_and_one_that_did_not_offers_none() {
        let verified = report(&apply_text(&world(), None).0);
        let offer = undo_offer(&target(), Some(Path::new("/data")), &verified).expect("a verified apply can be undone");
        assert!(offer.restore_point.ends_with(".json") && !offer.undo_set.is_empty(), "{offer:?}");
        assert_eq!(offer.target, target());
        // a failed apply may have written some: its undo is offered too
        assert!(undo_offer(&target(), Some(Path::new("/data")), &report(&apply_text(&world_with(&[], &[], Some(1)), None).0)).is_some());
        // nothing was changed: no restore point, nothing to put back
        assert!(undo_offer(&target(), Some(Path::new("/data")), &report(&apply_text(&world_with(&[0, 1, 2], &[], None), None).0)).is_none());
        // an answer without a report names no restore point
        for outcome in [
            ApplyOutcome::<RestoreApplyJson>::Indeterminate { message: "m".into(), cli_log: String::new() },
            ApplyOutcome::<RestoreApplyJson>::NotApplied { why: NotAppliedWhy::Refused, message: "m".into(), cli_log: String::new() },
        ] {
            assert!(undo_offer(&target(), Some(Path::new("/data")), &outcome).is_none(), "{outcome:?}");
        }
        // an empty undo set (nothing the restore point can put back) offers nothing: the boundary is one name
        let ApplyOutcome::Report { report: mut r, .. } = verified else { panic!() };
        r.undo_set = vec!["kNkMAIDCapability_A".into()];
        let one = ApplyOutcome::Report { report: r.clone(), note: None, cli_log: String::new() };
        assert!(undo_offer(&target(), Some(Path::new("/data")), &one).is_some());
        r.undo_set.clear();
        let none = ApplyOutcome::Report { report: r.clone(), note: None, cli_log: String::new() };
        assert!(undo_offer(&target(), Some(Path::new("/data")), &none).is_none());
        // a report that names a restore point and something to put back offers the undo whatever its
        // status says: the plan the person reviews says what, if anything, differs
        r.undo_set = vec!["kNkMAIDCapability_A".into()];
        for status in ApplyStatus::ALL {
            r.status = status;
            let offered = undo_offer(&target(), Some(Path::new("/data")), &ApplyOutcome::Report { report: r.clone(), note: None, cli_log: String::new() });
            assert!(offered.is_some(), "{}", status.tag());
        }
        // ... but not without a restore point, and not where the restore point cannot be found again
        let mut bare = r.clone();
        bare.restore_point = None;
        assert!(undo_offer(&target(), Some(Path::new("/data")), &ApplyOutcome::Report { report: bare, note: None, cli_log: String::new() }).is_none());
        assert!(undo_offer(&target(), None, &ApplyOutcome::Report { report: r, note: None, cli_log: String::new() }).is_none(), "no data directory");
    }

    #[test]
    fn a_transplants_apply_offers_the_same_undo() {
        let (json, status) = library_apply_json("failed");
        assert_eq!(status, ApplyStatus::Failed);
        let outcome = ApplyOutcome::Report { report: Box::new(serde_json::from_str::<ApplyJson>(&json).unwrap()), note: None, cli_log: String::new() };
        let offer = undo_offer(&target(), Some(Path::new("/data")), &outcome).expect("a failed transplant may have written");
        assert!(!offer.undo_set.is_empty());
    }

    #[test]
    fn an_undo_opens_the_restore_point_limited_to_the_undo_set_and_plans_it_without_writing() {
        let offer = UndoOffer {
            restore_point: "2026-10-27T172600Z_Z_9_T1_pre-restore.json".into(),
            undo_set: vec!["kNkMAIDCapability_B".into(), "kNkMAIDCapability_A".into()],
            target: target(),
            data_dir: PathBuf::from("/data"),
        };
        let mut ui = RestoreUi::default();
        let cameras = vec![target()];
        let Ok(Cmd::Plan(PlanJob::Restore(request))) = ui.start_undo(&offer, &cameras) else { panic!("a plan") };
        assert_eq!(request.source_file, offer.restore_point);
        assert_eq!(request.only, Some(BTreeSet::from(["kNkMAIDCapability_A".to_string(), "kNkMAIDCapability_B".to_string()])));
        assert_eq!(request.target, target());
        assert!(ui.open && matches!(ui.stage(), Stage::Planning(_)));
        // the next frame does not take the selection away: it was made for this snapshot and body
        ui.sync(&cameras);
        assert!(ui.only().is_some() && matches!(ui.stage(), Stage::Planning(_)));
        // and what is planned still has to be confirmed
        assert!(ui.confirm_apply().is_none());

        // the body is gone, or a step is in flight: no plan
        let mut ui = RestoreUi::default();
        assert!(ui.start_undo(&offer, &[]).unwrap_err().contains("not connected"));
        assert!(ui.start_undo(&offer, &[row("Z 9", "OTHER", 1)]).is_err(), "another body is not this one");
        let (mut busy, _) = ui_with_plan();
        busy.ask_apply();
        busy.confirm_apply().unwrap();
        assert!(busy.start_undo(&offer, &cameras).is_err(), "no second apply while one is pending");
        assert!(matches!(busy.stage(), Stage::Applying(_)), "and the one in flight is left alone");
        let mut nothing = offer.clone();
        nothing.undo_set.clear();
        assert!(RestoreUi::default().start_undo(&nothing, &cameras).is_err());
    }

    // ── through the app ──────────────────────────────────────────────────

    #[test]
    fn the_app_plans_confirms_and_applies_through_the_worker_and_shows_the_answer_honestly() {
        let (mut app, cmd_rx, evt_tx, _dir) = test_app();
        let ctx = egui::Context::default();
        app.restore.target_key = Some(("Z 9".into(), "T1".into()));
        app.restore.source_file = Some(FILE.into());
        app.restore.sync(&app.cameras);
        app.restore_act(&ctx, RestoreAction::Plan);
        let Ok(Cmd::Plan(PlanJob::Restore(request))) = cmd_rx.try_recv() else { panic!("a plan") };
        assert!(app.busy());
        let plan = plan_of(&world());
        evt_tx.send(Evt::Planned(PlannedEvt::Restore { request: request.clone(), result: Ok((plan.clone(), String::new(), all_shown())) })).unwrap();
        app.poll();
        assert!(app.status.text.starts_with("Restore plan:"), "{}", app.status.text);
        // applying without having been asked the question sends nothing
        app.restore_act(&ctx, RestoreAction::ConfirmApply);
        assert!(cmd_rx.try_recv().is_err());
        app.restore_act(&ctx, RestoreAction::AskApply);
        assert!(cmd_rx.try_recv().is_err(), "asking is not applying");
        app.restore_act(&ctx, RestoreAction::ConfirmApply);
        let Ok(Cmd::Apply(ApplyJob::Restore { request: sent, digest, summary })) = cmd_rx.try_recv() else { panic!("an apply") };
        assert_eq!((sent.clone(), digest.clone(), summary), (request, plan.digest, plan.summary));
        // while it is pending nothing else starts
        app.restore_act(&ctx, RestoreAction::Plan);
        assert!(cmd_rx.try_recv().is_err());

        let (json, _) = apply_text(&world_with(&[], &[], Some(1)), None);
        let held_to = {
            let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
            v["plan_digest"] = serde_json::json!(digest);
            v.to_string()
        };
        let outcome = classify_apply_as::<RestoreApplyJson>(Ok(CliOutput { stdout: held_to, stderr: String::new() }), &digest);
        evt_tx.send(Evt::Applied(AppliedEvt::Restore { request: sent, outcome })).unwrap();
        app.poll();
        assert_eq!(app.status.level, Level::Error);
        assert!(app.status.text.starts_with("Restore failed:") && app.status.text.contains("may have landed"), "{}", app.status.text);
        assert!(matches!(cmd_rx.try_recv(), Ok(Cmd::RefreshListing)), "the restore point is a new file");
        assert!(matches!(app.restore.stage(), Stage::Done(_)));
    }

    #[test]
    fn a_refused_restore_changes_nothing_and_does_not_relist() {
        let (mut app, cmd_rx, evt_tx, _dir) = test_app();
        let ctx = egui::Context::default();
        app.restore.target_key = Some(("Z 9".into(), "T1".into()));
        app.restore.source_file = Some(FILE.into());
        app.restore.sync(&app.cameras);
        let (mut ui, _) = ui_with_plan();
        ui.ask_apply();
        let Some(Cmd::Apply(ApplyJob::Restore { request, .. })) = ui.confirm_apply() else { panic!() };
        app.restore = ui;
        app.pending = 1;
        let stale = ApplyOutcome::NotApplied { why: NotAppliedWhy::PlanStale, message: "stale".into(), cli_log: String::new() };
        evt_tx.send(Evt::Applied(AppliedEvt::Restore { request, outcome: stale })).unwrap();
        app.poll();
        assert_eq!(app.status.level, Level::Warn);
        assert!(app.status.text.contains("nothing was written") && app.status.text.contains("Plan again"), "{}", app.status.text);
        assert!(cmd_rx.try_recv().is_err(), "nothing was written, so nothing is re-listed");
        let _ = ctx;
    }

    #[test]
    fn a_transplants_undo_button_plans_the_undo_of_the_restore_point_it_made() {
        let (mut app, cmd_rx, _evt_tx, dir) = test_app();
        let ctx = egui::Context::default();
        let (json, _) = library_apply_json("failed");
        let applied: ApplyJson = serde_json::from_str(&json).unwrap();
        let outcome = ApplyOutcome::Report { report: Box::new(applied.clone()), note: None, cli_log: String::new() };
        let offer = undo_offer(&target(), Some(dir.path()), &outcome).expect("offered");
        app.transplant_act(&ctx, TransplantAction::Applied(AppliedAction::Undo(offer)));
        let Ok(Cmd::Plan(PlanJob::Restore(request))) = cmd_rx.try_recv() else { panic!("a plan, not an apply") };
        assert_eq!(request.source_file, applied.restore_point.unwrap().file);
        assert_eq!(request.only, Some(applied.undo_set.iter().cloned().collect()));
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn the_undo_button_plans_through_the_worker_and_waits_while_the_camera_is_busy() {
        let (mut app, cmd_rx, _evt_tx, dir) = test_app();
        let offer = UndoOffer { restore_point: "rp.json".into(), undo_set: vec!["kNkMAIDCapability_A".into()], target: target(), data_dir: dir.path().to_path_buf() };
        app.pending = 1;
        app.start_undo(offer.clone());
        assert!(cmd_rx.try_recv().is_err(), "a camera operation is running");
        assert_eq!(app.status.level, Level::Warn);
        app.pending = 0;
        app.start_undo(offer);
        let Ok(Cmd::Plan(PlanJob::Restore(request))) = cmd_rx.try_recv() else { panic!("a plan") };
        assert_eq!(request.source_file, "rp.json");
        assert!(app.restore.open);
        assert!(cmd_rx.try_recv().is_err(), "a plan, not an apply: the person still confirms");
    }
    // ── review round: what is written, what is ticked, what is shown ─────

    fn arrived(w: &World, only: Option<&BTreeSet<String>>) -> (RestorePlanJson, String, SourceView) {
        (serde_json::from_str(&plan_text(w, only)).unwrap(), String::new(), all_shown())
    }

    /// A form that has been planned for `w`, with the ticks as the plan left them.
    fn ui_for(w: &World) -> (RestoreUi, Vec<CameraRow>) {
        let (mut ui, cameras) = ui_ready();
        ui.sync(&cameras);
        let Cmd::Plan(PlanJob::Restore(request)) = ui.begin_plan(&cameras).unwrap() else { panic!() };
        ui.plan_arrived(request, Ok(arrived(w, None)));
        (ui, cameras)
    }

    fn planned_of(ui: &RestoreUi) -> &PlannedRestore {
        match ui.stage() {
            Stage::Planned(p) | Stage::ConfirmApply(p) => p,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_confirmation_states_how_many_properties_are_written_and_how_many_of_them_change() {
        // a full restore writes every property of the snapshot: the plan's `counts.written`, not the changing rows
        let w = world();
        let p = PlannedRestore::new(request(None), plan_of(&w), all_shown(), String::new(), false);
        let (written, changing) = (p.plan.counts.written, p.changing().len());
        assert!(written > changing && changing == 3, "{written} written, {changing} changing");
        let text = restore_confirm_text(&p);
        assert!(text.contains(&format!("{written} properties will be written to the camera; 3 of them change what it holds now")), "{text}");
        assert!(text.contains(&format!("the other {} already hold the stored value and are written again unchanged", written - 3)), "{text}");
        // every number in the sentence is the plan's own
        let other = written - changing;
        let sentence = format!("{written} properties will be written to the camera; {changing} of them change what it holds now, and the other {other} already hold");
        assert!(text.contains(&sentence), "{text}");
        // boundary: all of them change -> nothing is said about unchanged ones; one of one is singular
        let limited_names = BTreeSet::from([w.targets[0].0.clone()]);
        let limited: RestorePlanJson = serde_json::from_str(&plan_text(&w, Some(&limited_names))).unwrap();
        assert_eq!((limited.counts.written, limited.would_change), (1, 1));
        let mut req = request(None);
        req.only = Some(limited_names);
        let one = restore_confirm_text(&PlannedRestore::new(req, limited, all_shown(), String::new(), false));
        assert!(one.contains("1 property will be written to the camera; 1 of them changes what it holds now."), "{one}");
        assert!(!one.contains("written again unchanged"), "{one}");
        // one already holding its value: written again, not a change
        let same = PlannedRestore::new(request(None), plan_of(&world_with(&[1], &[], None)), all_shown(), String::new(), false);
        assert_eq!(same.changing().len(), 2);
        assert!(restore_confirm_text(&same).contains("2 of them change what it holds now"), "{}", restore_confirm_text(&same));
    }

    #[test]
    fn unticking_a_property_withdraws_the_apply_and_the_new_plan_sends_only_what_is_ticked() {
        let w = world();
        let (mut ui, cameras) = ui_for(&w);
        let all: BTreeSet<String> = planned_of(&ui).selected.clone();
        assert_eq!(all.len(), 3);
        assert!(planned_of(&ui).applicable());
        // untick one: the plan on screen is for more than is ticked
        let dropped = all.iter().next().unwrap().clone();
        ui.toggle(&dropped);
        let p = planned_of(&ui);
        assert!(p.selection_diverged() && !p.applicable());
        assert!(p.blocked_reason().unwrap().contains("1 property is unticked"), "{:?}", p.blocked_reason());
        ui.ask_apply();
        assert!(matches!(ui.stage(), Stage::Planned(_)), "no confirmation is asked for a plan the ticks do not match");
        assert!(ui.confirm_apply().is_none());
        // tick it back: the plan is for exactly what is ticked again
        ui.toggle(&dropped);
        assert!(planned_of(&ui).applicable());
        ui.toggle(&dropped);
        // plan again for the ticks: the selection is the request, and the apply that follows carries it
        let Ok(Cmd::Plan(PlanJob::Restore(request))) = ui.plan_selection(&cameras) else { panic!("a new plan") };
        let ticked: BTreeSet<String> = all.iter().filter(|n| **n != dropped).cloned().collect();
        assert_eq!(request.only.as_ref(), Some(&ticked));
        ui.plan_arrived(request.clone(), Ok(arrived(&w, Some(&ticked))));
        assert!(planned_of(&ui).applicable());
        assert_eq!(planned_of(&ui).plan.only.as_ref().map(|o| o.len()), Some(2));
        ui.ask_apply();
        let Some(Cmd::Apply(ApplyJob::Restore { request: sent, digest, .. })) = ui.confirm_apply() else { panic!("an apply") };
        assert_eq!(sent.only.as_ref(), Some(&ticked), "what is sent is the ticked selection");
        let shown: RestorePlanJson = serde_json::from_str(&plan_text(&w, Some(&ticked))).unwrap();
        assert_eq!(digest, shown.digest, "and the digest of the plan made for it");
        assert_eq!(sent, request);
    }

    #[test]
    fn the_list_the_ticks_the_confirmation_and_the_gate_agree_on_what_changes() {
        for (what, w) in [
            ("all differ", world()),
            ("one the same", world_with(&[1], &[], None)),
            ("one unreadable", world_with(&[], &[2], None)),
            ("two the same", world_with(&[0, 2], &[], None)),
        ] {
            let p = PlannedRestore::new(request(None), plan_of(&w), all_shown(), String::new(), false);
            let n = p.plan.would_change;
            assert_eq!(p.changing().len(), n, "{what}: the list is the library's count");
            assert_eq!(p.selected.len(), n, "{what}: the ticks start as the list");
            assert!(restore_confirm_text(&p).contains(&format!("{n} of them change")), "{what}");
            assert!(p.applicable(), "{what}");
            assert!(!p.selection_diverged(), "{what}");
            let sections = p.plan.steps.iter().filter(|s| s.section() == PlanSection::WriteChange).count();
            assert_eq!(sections, n, "{what}: the section is the same list");
        }
        // nothing changes: no list, no ticks, no gate
        let none = PlannedRestore::new(request(None), plan_of(&world_with(&[0, 1, 2], &[], None)), all_shown(), String::new(), false);
        assert_eq!((none.plan.would_change, none.changing().len(), none.selected.len()), (0, 0, 0));
        assert!(!none.applicable() && none.blocked_reason().unwrap().contains("nothing to restore"));
        // a plan whose own count disagrees with its steps is not applicable, whichever way it is wrong
        for wrong in [2usize, 4] {
            let mut lying = plan_of(&world());
            lying.would_change = wrong;
            let p = PlannedRestore::new(request(None), lying.clone(), all_shown(), String::new(), false);
            assert!(!p.applicable() && p.blocked_reason().unwrap().contains("disagrees"), "{wrong}");
            assert!(check_restore_plan_is_for(&lying, &request(None)).unwrap_err().contains("disagrees"), "{wrong}");
        }
    }

    #[test]
    fn a_restore_whose_values_cannot_be_shown_is_not_offered_for_confirmation() {
        let plan = plan_of(&world());
        let mk = |source: SourceView| PlannedRestore::new(request(None), plan.clone(), source, String::new(), false);
        assert!(mk(all_shown()).applicable());
        let not_loaded = SourceView { loaded: false, note: Some("no such file".into()), ..SourceView::default() };
        let p = mk(not_loaded);
        assert!(!p.applicable() && p.blocked_reason().unwrap().contains("could not be shown"), "{:?}", p.blocked_reason());
        // values on hand but the snapshot not marked as read: the flag is the gate, not the values
        let stale = SourceView { loaded: false, ..all_shown() };
        let p = mk(stale);
        assert!(!p.applicable() && p.blocked_reason().unwrap().contains("could not be read for display"), "{:?}", p.blocked_reason());
        // loaded, but one changing property has no value to show
        let mut shown = all_shown();
        let gone = p.changing()[0].name.clone();
        shown.values.remove(&gone);
        let p = mk(shown);
        assert!(!p.applicable());
        assert!(p.blocked_reason().unwrap().contains(short_name(&gone)), "{:?}", p.blocked_reason());
        // a plan that contains a step that would fail, or one this program does not understand
        for tag in ["write_failed", "a_new_kind_of_step"] {
            let mut odd = plan.clone();
            odd.steps[0].outcome = serde_json::from_value(serde_json::json!({"tag": tag})).unwrap();
            odd.steps.push(odd.steps[0].clone());
            let p = PlannedRestore::new(request(None), odd, all_shown(), String::new(), false);
            if p.plan.would_change == p.changing().len() {
                assert!(!p.applicable(), "{tag}: {:?}", p.blocked_reason());
            }
        }
        let mut failing = plan.clone();
        let idx = failing.steps.iter().position(|s| !s.may_change()).unwrap_or(0);
        failing.steps[idx].outcome = serde_json::from_value(serde_json::json!({"tag": "write_failed", "detail": {"message": "-125"}})).unwrap();
        failing.steps[idx].changes = Some(false);
        let p = PlannedRestore::new(request(None), failing, all_shown(), String::new(), false);
        assert!(!p.applicable() && p.blocked_reason().unwrap().contains("would fail"), "{:?}", p.blocked_reason());
        assert!(p.plan.steps[idx].line(&all_shown()).contains("cannot be written as stored"), "a plan does not say 'the write failed': nothing was tried");
    }

    #[test]
    fn a_plan_that_names_what_cannot_be_restored_can_be_narrowed_even_with_every_change_ticked() {
        let w = world();
        let (mut ui, cameras) = ui_for(&w);
        let mut p = planned_of(&ui).clone();
        p.plan.unrestorable = vec![("kNkMAIDCapability_X".into(), "read_only".into())];
        assert!(!p.selection_diverged(), "everything that changes is ticked");
        assert!(p.can_plan_selection(), "and a new plan without the unrestorable name is still possible");
        assert!(!p.applicable());
        // nothing ticked: still not possible
        p.selected.clear();
        assert!(!p.can_plan_selection());
        // through the window
        ui.stage = Stage::Planned(PlannedRestore { plan: RestorePlanJson { unrestorable: vec![("kNkMAIDCapability_X".into(), "read_only".into())], ..planned_of(&ui).plan.clone() }, ..planned_of(&ui).clone() });
        assert!(matches!(ui.plan_selection(&cameras), Ok(Cmd::Plan(_))));
    }

    #[test]
    fn the_snapshots_firmware_is_shown_and_a_difference_from_the_cameras_is_warned_about() {
        let mut plan = plan_of(&world());
        assert_eq!(PlannedRestore::new(request(None), plan.clone(), all_shown(), String::new(), false).firmware_differs(), None);
        plan.source.firmware = "5.00".into();
        let p = PlannedRestore::new(request(None), plan, all_shown(), String::new(), false);
        let warning = p.firmware_differs().expect("a different firmware is flagged");
        assert!(warning.contains("5.00") && warning.contains("5.31"), "{warning}");
        let text = restore_confirm_text(&p);
        assert!(text.contains("taken on firmware 5.00") && text.contains("firmware 5.31") && text.contains(&warning), "{text}");
        let same = restore_confirm_text(&planned());
        assert!(same.contains("taken on firmware 5.31") && !same.contains("was taken on firmware"), "{same}");
        let mut row = SnapRow {
            file: "a.json".into(),
            model: "Z 9".into(),
            serial: "T1".into(),
            firmware: "5.00".into(),
            label: Some("studio".into()),
            captured_at: "2026-10-27T10:00:00Z".into(),
            is_reference: false,
            name_lossy: false,
        };
        assert!(restore_snapshot_label(&row, Some("5.31")).contains("fw 5.00  ⚠ other firmware"), "{}", restore_snapshot_label(&row, Some("5.31")));
        assert!(!restore_snapshot_label(&row, Some("5.00")).contains('⚠'));
        assert!(!restore_snapshot_label(&row, None).contains('⚠'), "nothing to compare with");
        row.firmware = String::new();
        assert!(restore_snapshot_label(&row, Some("5.31")).contains('⚠'), "a snapshot with no firmware recorded is not 'the same'");
    }

    #[test]
    fn widening_an_undo_to_everything_is_said_in_the_confirmation() {
        let offer = UndoOffer {
            restore_point: FILE.into(),
            undo_set: vec![world().targets[0].0.clone()],
            target: target(),
            data_dir: PathBuf::from("/data"),
        };
        let w = world();
        let mut ui = RestoreUi::default();
        let cameras = vec![target()];
        let Ok(Cmd::Plan(PlanJob::Restore(request))) = ui.start_undo(&offer, &cameras) else { panic!() };
        let only = request.only.clone().unwrap();
        ui.plan_arrived(request, Ok(arrived(&w, Some(&only))));
        ui.ask_apply();
        let Stage::ConfirmApply(p) = ui.stage() else { panic!("{:?}", ui.stage()) };
        assert!(!p.widened_undo && !restore_confirm_text(p).contains("no longer an undo"));
        ui.cancel_confirmation();
        let Ok(Cmd::Plan(PlanJob::Restore(wide))) = ui.plan_everything(&cameras) else { panic!() };
        assert_eq!(wide.only, None);
        ui.plan_arrived(wide, Ok(arrived(&w, None)));
        ui.ask_apply();
        let Stage::ConfirmApply(p) = ui.stage() else { panic!() };
        assert!(p.widened_undo);
        let text = restore_confirm_text(p);
        assert!(text.contains("no longer an undo") && text.contains("EVERY property of the restore point"), "{text}");
        // a plain restore of the same file is not an undo
        let (ui2, _) = ui_for(&w);
        assert!(!planned_of(&ui2).widened_undo);
    }

    #[test]
    fn the_selection_goes_with_the_plan_on_every_way_out_of_it() {
        let w = world();
        let only = BTreeSet::from([w.targets[0].0.clone()]);
        let held = |ui: &RestoreUi| ui.only.is_some() || ui.undo_of.is_some();
        let planned_for_only = || {
            let (mut ui, cameras) = ui_ready();
            ui.sync(&cameras);
            ui.only = Some(only.clone());
            ui.undo_of = Some(FILE.into());
            ui.sync(&cameras);
            let Cmd::Plan(PlanJob::Restore(request)) = ui.begin_plan(&cameras).unwrap() else { panic!() };
            (ui, cameras, request)
        };
        // a plan that failed
        let (mut ui, _, request) = planned_for_only();
        ui.plan_arrived(request, Err("camera busy".into()));
        assert!(!held(&ui), "after a failed plan");
        // discarded
        let (mut ui, _, request) = planned_for_only();
        ui.plan_arrived(request, Ok(arrived(&w, Some(&only))));
        assert!(held(&ui), "while it is on screen");
        ui.discard();
        assert!(!held(&ui) && matches!(ui.stage(), Stage::Idle), "after Discard");
        // a result closed
        let (mut ui, _, request) = planned_for_only();
        ui.plan_arrived(request, Ok(arrived(&w, Some(&only))));
        ui.ask_apply();
        let Some(Cmd::Apply(ApplyJob::Restore { request, .. })) = ui.confirm_apply() else { panic!() };
        assert!(held(&ui), "while the apply is on the wire");
        ui.applied(request, ApplyOutcome::NotApplied { why: NotAppliedWhy::PlanStale, message: "m".into(), cli_log: String::new() });
        assert!(!held(&ui), "once it is answered");
        // withdrawn by a change
        let (mut ui, mut cameras, request) = planned_for_only();
        ui.plan_arrived(request, Ok(arrived(&w, Some(&only))));
        cameras[0].firmware = "5.40".into();
        ui.sync(&cameras);
        assert!(!held(&ui) && matches!(ui.stage(), Stage::Idle), "after a withdrawal");
        // worker lost: planning, and applying
        let (mut ui, _, _) = planned_for_only();
        ui.worker_lost();
        assert!(!held(&ui), "worker lost while planning");
        let (mut ui, _, request) = planned_for_only();
        ui.plan_arrived(request, Ok(arrived(&w, Some(&only))));
        ui.ask_apply();
        ui.confirm_apply().unwrap();
        ui.worker_lost();
        assert!(!held(&ui) && matches!(ui.stage(), Stage::Done(_)), "worker lost while applying");
        // not sent: planning, and applying
        let (mut ui, _, _) = planned_for_only();
        ui.not_sent();
        assert!(!held(&ui) && matches!(ui.stage(), Stage::Idle), "not sent while planning");
        let (mut ui, _, request) = planned_for_only();
        ui.plan_arrived(request, Ok(arrived(&w, Some(&only))));
        ui.ask_apply();
        ui.confirm_apply().unwrap();
        ui.not_sent();
        assert!(!held(&ui), "not sent while applying");
        // the data directory changed: while planning, and while reviewing
        let (mut ui, _, _) = planned_for_only();
        ui.data_dir_changed();
        assert!(!held(&ui) && matches!(ui.stage(), Stage::Idle), "data dir changed while planning");
        let (mut ui, _, request) = planned_for_only();
        ui.plan_arrived(request, Ok(arrived(&w, Some(&only))));
        ui.data_dir_changed();
        assert!(!held(&ui) && matches!(ui.stage(), Stage::Idle), "data dir changed while reviewing");
        // the late answer to a withdrawn plan is ignored and brings nothing back
        let (mut ui, _, request) = planned_for_only();
        ui.data_dir_changed();
        ui.plan_arrived(request, Ok(arrived(&w, Some(&only))));
        assert!(matches!(ui.stage(), Stage::Idle) && !held(&ui));
        // the "Restore…" button on a snapshot row is a restore of that snapshot, not of an old undo set
        let (mut ui, _, _) = planned_for_only();
        ui.start_from(("Z 9".into(), "T1".into()), "another.json".into());
        assert!(!held(&ui));
        assert_eq!(ui.source_file.as_deref(), Some("another.json"));
    }

    #[test]
    fn an_undo_whose_camera_changed_is_withdrawn_with_a_notice_and_never_becomes_a_full_restore() {
        let offer = UndoOffer {
            restore_point: "rp.json".into(),
            undo_set: vec!["kNkMAIDCapability_A".into(), "kNkMAIDCapability_B".into()],
            target: target(),
            data_dir: PathBuf::from("/data"),
        };
        let start = || {
            let mut ui = RestoreUi::default();
            let cameras = vec![target()];
            ui.start_undo(&offer, &cameras).unwrap();
            ui.sync(&cameras);
            assert!(ui.only().is_some());
            (ui, cameras)
        };
        // the same camera, replugged with new firmware
        let (mut ui, mut cameras) = start();
        ui.go_idle_for_test();
        cameras[0].firmware = "5.40".into();
        ui.sync(&cameras);
        assert!(ui.only().is_none() && ui.source_file.is_none(), "{:?} / {:?}", ui.only(), ui.source_file);
        assert!(ui.notice.contains("2 propert") && ui.notice.contains("choose it again"), "{}", ui.notice);
        assert!(ui.begin_plan(&cameras).is_err(), "nothing can be planned: it would restore everything");
        // the camera is gone (unplugged, Discover): the same
        let (mut ui, _) = start();
        ui.go_idle_for_test();
        ui.sync(&[]);
        assert!(ui.only().is_none() && ui.source_file.is_none() && ui.notice.contains("camera changed"), "{}", ui.notice);
        ui.sync(&[target()]);
        assert!(ui.begin_plan(&[target()]).is_err(), "and it does not come back as a full restore when the camera does");
        // the person chose another snapshot: the selection of the old one goes quietly
        let (mut ui, cameras) = start();
        ui.go_idle_for_test();
        ui.source_file = Some("another.json".into());
        ui.sync(&cameras);
        assert!(ui.only().is_none() && ui.source_file.as_deref() == Some("another.json") && ui.notice.is_empty(), "{}", ui.notice);
    }

    impl RestoreUi {
        /// The state a window is in between an undo's plan being asked for and answered, without it.
        fn go_idle_for_test(&mut self) {
            self.stage = Stage::Idle;
        }
    }

    #[test]
    fn an_undo_is_refused_in_another_data_directory_than_the_one_its_restore_point_is_in() {
        let (mut app, cmd_rx, _evt_tx, dir) = test_app();
        let mut offer = UndoOffer { restore_point: "rp.json".into(), undo_set: vec!["kNkMAIDCapability_A".into()], target: target(), data_dir: dir.path().to_path_buf() };
        app.start_undo(offer.clone());
        assert!(matches!(cmd_rx.try_recv(), Ok(Cmd::Plan(PlanJob::Restore(_)))), "the same directory plans");
        let (mut app, cmd_rx, _evt_tx, _dir) = test_app();
        offer.data_dir = PathBuf::from("/elsewhere");
        app.start_undo(offer);
        assert!(cmd_rx.try_recv().is_err(), "nothing is planned against a directory the restore point is not in");
        assert!(app.restore.open && app.restore.notice.contains("data directory changed") && app.restore.notice.contains("/elsewhere"), "{}", app.restore.notice);
    }

    #[cfg(unix)]
    #[test]
    fn a_plan_the_cli_printed_and_then_was_stopped_after_is_not_a_plan() {
        // a run killed at the time limit after printing: the plan on stdout is not the end of a plan
        let plan = plan_text(&world(), None);
        let stopped = |kind, code| CliError { kind, message: "stopped".into(), stdout: plan.clone(), stderr: String::new(), code };
        let one = guarded_plan_result(Err(stopped(CliFailure::Exit, Some(1))), PlanFailure::PlanThenExitOne);
        assert!(one.is_ok(), "the documented plan-then-exit-1 is a plan: {one:?}");
        for (kind, code) in [(CliFailure::TimedOut, None), (CliFailure::Wait, None), (CliFailure::Exit, None), (CliFailure::Exit, Some(101)), (CliFailure::Exit, Some(5))] {
            let r = guarded_plan_result(Err(stopped(kind, code)), PlanFailure::PlanThenExitOne);
            assert!(r.is_err(), "{kind:?} {code:?} printed a plan and then did not end the way a plan ends");
        }
        // a transplant's plan is never kept past a failure
        assert!(guarded_plan_result(Err(stopped(CliFailure::Exit, Some(1))), PlanFailure::NoPlan).is_err());
        // an unreadable plan keeps its cause
        let junk = CliError { kind: CliFailure::Exit, message: "failed".into(), stdout: "{\"digest\"".into(), stderr: String::new(), code: Some(1) };
        let e = guarded_plan_result(Err(junk), PlanFailure::PlanThenExitOne).unwrap_err();
        assert!(e.contains("failed") && e.contains("could not be read"), "{e}");
    }

    /// `plan_via` without a process: the same reading of what the CLI printed.
    #[cfg(unix)]
    fn guarded_plan_result(result: Result<CliOutput, CliError>, failure: PlanFailure) -> Result<RestorePlanJson, String> {
        let dir = TempDir::new().unwrap();
        let (stdout, code) = match &result {
            Ok(o) => (o.stdout.clone(), 0),
            Err(e) => (e.stdout.clone(), e.code.unwrap_or(1)),
        };
        let kind = result.as_ref().err().map(|e| e.kind);
        fs::write(dir.path().join("out.json"), stdout).unwrap();
        let tail = match kind {
            Some(CliFailure::TimedOut) => format!("cat '{}/out.json'\nsleep 5", dir.path().display()),
            Some(CliFailure::Exit) if result.as_ref().err().is_some_and(|e| e.code.is_none()) => format!("cat '{}/out.json'\nkill -9 $$", dir.path().display()),
            Some(CliFailure::Wait) => return Err("not simulated".into()),
            _ => format!("cat '{}/out.json'\nexit {code}", dir.path().display()),
        };
        let cli = recording_cli(dir.path(), &tail);
        let timeout = if kind == Some(CliFailure::TimedOut) { Duration::from_millis(500) } else { T };
        plan_via::<RestorePlanJson>("Restore", &cli, || Ok(vec!["x".into()]), &target(), &|| scan_of(vec![usb("Z 9", "T1")]), timeout, failure).map(|(p, _)| p)
    }

    #[test]
    fn a_body_whose_firmware_changed_since_it_was_discovered_is_not_written() {
        let scan = |fw: &str| {
            let mut u = usb("Z 9", "T1");
            u.firmware = fw.into();
            scan_of(vec![u])
        };
        assert!(write_preflight("Restore", &target(), &scan("5.31")).is_none());
        let moved = write_preflight("Restore", &target(), &scan("5.40")).unwrap();
        assert!(moved.starts_with("Restore refused") && moved.contains("5.40") && moved.contains("5.31") && moved.contains("Discover again"), "{moved}");
        // a bus that does not list the body says nothing about its firmware (the other checks decide)
        assert!(write_preflight("Restore", &target(), &scan_of(vec![])).is_none());
        // another body's firmware is not this one's
        let mut other = usb("Z 9", "T2");
        other.firmware = "9.99".into();
        assert!(write_preflight("Restore", &target(), &scan_of(vec![usb("Z 9", "T1"), other])).is_some(), "two of one model: refused for that");
    }

    #[test]
    fn the_end_of_an_apply_is_worded_from_what_it_printed_and_how_it_ended() {
        let (json, digest) = apply_text(&world(), None);
        let ok = |code: Option<i32>, kind| Err(CliError { kind, message: "m".into(), stdout: json.clone(), stderr: String::new(), code });
        // exit 1 after a report is the documented failure: no note
        let ApplyOutcome::Report { note, .. } = classify_apply_as::<RestoreApplyJson>(ok(Some(1), CliFailure::Exit), &digest) else { panic!() };
        assert_eq!(note, None);
        // any other end after the report is said
        for (code, kind, says) in [(Some(3), CliFailure::Exit, "status 3"), (Some(101), CliFailure::Exit, "status 101"), (None, CliFailure::Exit, "signal"), (None, CliFailure::TimedOut, "stopped after it reported")] {
            let ApplyOutcome::Report { note, .. } = classify_apply_as::<RestoreApplyJson>(ok(code, kind), &digest) else { panic!() };
            assert!(note.as_deref().is_some_and(|n| n.contains(says)), "{code:?} {kind:?}: {note:?}");
        }
        // a report of another plan names its restore point
        let mismatched = classify_apply_as::<RestoreApplyJson>(ok(Some(1), CliFailure::Exit), "another-digest");
        let ApplyOutcome::Indeterminate { message, .. } = mismatched else { panic!() };
        let rp = serde_json::from_str::<RestoreApplyJson>(&json).unwrap().restore_point.unwrap().file;
        assert!(message.contains("different plan") && message.contains(&rp), "{message}");
        // an unreadable report keeps why
        let cut = CliError { kind: CliFailure::Exit, message: "m".into(), stdout: "{\"status\": \"applied".into(), stderr: String::new(), code: Some(1) };
        let ApplyOutcome::Indeterminate { message, .. } = classify_apply_as::<RestoreApplyJson>(Err(cut), "d") else { panic!() };
        assert!(message.contains("could not be read") && message.contains("EOF"), "{message}");
        let ApplyOutcome::Indeterminate { message, .. } = classify_apply_as::<RestoreApplyJson>(Ok(CliOutput { stdout: "{}".into(), stderr: String::new() }), "d") else { panic!() };
        assert!(message.contains("exited 0") && message.contains("missing field"), "{message}");
        // nothing printed at all: nothing to add
        let none = CliError { kind: CliFailure::Exit, message: "m".into(), stdout: String::new(), stderr: String::new(), code: Some(1) };
        let ApplyOutcome::Indeterminate { message, .. } = classify_apply_as::<RestoreApplyJson>(Err(none), "d") else { panic!() };
        assert!(!message.contains("could not be read"), "{message}");
    }

    /// The library's report as JSON, edited.
    fn report_with(edit: impl FnOnce(&mut serde_json::Value)) -> RestoreApplyJson {
        let mut v: serde_json::Value = serde_json::from_str(&apply_text(&world(), None).0).unwrap();
        edit(&mut v);
        serde_json::from_value(v).expect("an edited report still parses")
    }

    fn says(lines: &[(Level, String)], level: Level, needle: &str) -> bool {
        lines.iter().any(|(l, t)| *l == level && t.contains(needle))
    }

    #[test]
    fn every_line_that_matters_about_a_restore_is_there_and_at_its_level() {
        use serde_json::json;
        let lines = |r: &RestoreApplyJson| restore_report_lines(r);
        let l = lines(&report_with(|v| v["side_effects"] = json!({"changed": ["kNkMAIDCapability_A"], "vanished": ["kNkMAIDCapability_B"], "appeared": ["kNkMAIDCapability_C"]})));
        assert!(says(&l, Level::Warn, "Changed without being written: A (changed), B (vanished), C (appeared)"), "{l:?}");
        let l = lines(&report_with(|v| v["unrestored"] = json!([["kNkMAIDCapability_A", "read_only"]])));
        assert!(says(&l, Level::Error, "Named but NOT written: A (read_only)"), "{l:?}");
        let l = lines(&report_with(|v| v["restore_point_gate_waived"] = json!(true)));
        assert!(says(&l, Level::Warn, "did not pass the model's completeness check"), "{l:?}");
        let l = lines(&report_with(|v| v["undo_not_restorable"] = json!(["kNkMAIDCapability_A"])));
        assert!(says(&l, Level::Warn, "An undo cannot put these back: A"), "{l:?}");
        let l = lines(&report_with(|v| v["steps"] = json!([{"name": "kNkMAIDCapability_A", "code": 1, "outcome": {"tag": "unsupported_write_type"}}, {"name": "kNkMAIDCapability_B", "code": 2, "outcome": {"tag": "range_scale_differs"}}, {"name": "kNkMAIDCapability_C", "code": 3, "outcome": {"tag": "mystery"}}])));
        assert!(says(&l, Level::Warn, "Not taken by the body") && says(&l, Level::Warn, "1 range(s) were left alone"), "{l:?}");
        assert!(says(&l, Level::Warn, "does not know (\"mystery\")"), "{l:?}");
        let l = lines(&report_with(|v| v["verification"] = json!(null)));
        assert!(says(&l, Level::Warn, "No read-back was made"), "{l:?}");
        let l = lines(&report_with(|v| v["side_effects"] = json!(null)));
        assert!(says(&l, Level::Warn, "Side effects: unknown"), "{l:?}");
        // a no-op that is a failure, and a no-op that is not
        let l = lines(&report_with(|v| {
            v["no_op"] = json!(true);
            v["status"] = json!("failed");
            v["restore_point"] = json!(null);
        }));
        assert!(says(&l, Level::Error, "not because there was nothing to change"), "{l:?}");
        let l = lines(&report_with(|v| {
            v["no_op"] = json!(true);
            v["restore_point"] = json!(null);
            v["verification"] = json!(null);
            v["side_effects"] = json!(null);
        }));
        assert!(says(&l, Level::Info, "Nothing to write") && !l.iter().any(|(lv, _)| *lv == Level::Warn), "{l:?}");
        // the read-back verdicts, each at its level, with the library's own words
        for (verdict, level) in [("verified", Level::Info), ("nothing_written", Level::Info), ("unjudged", Level::Warn), ("readback_incomplete", Level::Warn), ("residual_found", Level::Error)] {
            let l = lines(&report_with(|v| v["verification"]["verdict"] = json!(verdict)));
            assert!(l.iter().any(|(lv, t)| *lv == level && t.starts_with("Read-back:")), "{verdict}: {l:?}");
        }
        let l = lines(&report_with(|v| {
            v["verification"]["verdict"] = json!("residual_found");
            v["verification"]["residual"] = json!({"changed": ["kNkMAIDCapability_A"], "missing": [], "unjudged": ["kNkMAIDCapability_B"], "judged": 1});
        }));
        assert!(says(&l, Level::Error, "A (differs)") && says(&l, Level::Warn, "Written but not compared (1): B"), "{l:?}");
        // an incomplete read-back says why: the gate
        let l = lines(&report_with(|v| {
            v["verification"]["verdict"] = json!("readback_incomplete");
            v["verification"]["gate"] = json!({"outcome": "no_baseline", "reason": "firmware_changed", "baseline": "5.00", "capture": "5.31"});
        }));
        assert!(says(&l, Level::Warn, "firmware 5.31 differs from the baseline's 5.00"), "{l:?}");
        let l = lines(&report_with(|v| {
            v["verification"]["verdict"] = json!("readback_incomplete");
            v["verification"]["gate"] = json!({"outcome": "failed", "missing": [{"code": 1, "name": "x"}, {"code": 2, "name": "y"}]});
        }));
        assert!(says(&l, Level::Warn, "2 capabilit(ies) the model should read were missing"), "{l:?}");
        // hostile text is escaped
        let l = lines(&report_with(|v| v["write_errors"] = json!([{"name": "kNkMAIDCapability_A", "message": "bad\u{1b}[31m"}])));
        assert!(l.iter().all(|(_, t)| !t.contains('\u{1b}')) && says(&l, Level::Error, "Write failed: A"), "{l:?}");
    }

    #[test]
    fn what_may_have_been_written_has_one_answer_for_the_status_the_listing_the_aftermath_and_the_undo() {
        let (json, _) = apply_text(&world(), None);
        for status in ApplyStatus::ALL {
            let r = report_with(|v| v["status"] = serde_json::json!(status.tag()));
            let outcome = ApplyOutcome::Report { report: Box::new(r.clone()), note: None, cli_log: String::new() };
            assert_eq!(outcome.may_have_written(), status.may_have_written(), "{}", status.tag());
            // the aftermath is said exactly for the statuses that are unsettled
            let unsettled = matches!(status, ApplyStatus::Failed | ApplyStatus::Indeterminate);
            assert_eq!(failed_aftermath(&r).is_some(), unsettled, "{}", status.tag());
            // the undo is offered whenever a restore point and something to put back are named
            assert!(undo_offer(&target(), Some(Path::new("/data")), &outcome).is_some(), "{}", status.tag());
        }
        let _ = json;
        // without a report only a refusal says the camera was not written
        assert!(ApplyOutcome::<RestoreApplyJson>::Indeterminate { message: "m".into(), cli_log: String::new() }.may_have_written());
        assert!(!ApplyOutcome::<RestoreApplyJson>::NotApplied { why: NotAppliedWhy::Refused, message: "m".into(), cli_log: String::new() }.may_have_written());
        // ... and the app lists the snapshots again after exactly those
        for (outcome, relist) in [
            (ApplyOutcome::Indeterminate { message: "m".into(), cli_log: String::new() }, true),
            (ApplyOutcome::NotApplied { why: NotAppliedWhy::PlanStale, message: "m".into(), cli_log: String::new() }, false),
        ] {
            let (mut app, cmd_rx, evt_tx, _dir) = test_app();
            let (mut ui, _) = ui_with_plan();
            ui.ask_apply();
            let Some(Cmd::Apply(ApplyJob::Restore { request, .. })) = ui.confirm_apply() else { panic!() };
            app.restore = ui;
            app.pending = 1;
            evt_tx.send(Evt::Applied(AppliedEvt::Restore { request, outcome })).unwrap();
            app.poll();
            assert_eq!(matches!(cmd_rx.try_recv(), Ok(Cmd::RefreshListing)), relist);
        }
    }

    #[test]
    fn the_snapshots_values_are_read_for_display_and_a_snapshot_that_cannot_be_read_says_so() {
        let d = TempDir::new().unwrap();
        let mut s = Snapshot::new(body(), Transport::Usb, "2026-10-27T10:00:00Z".into());
        s.insert("kNkMAIDCapability_Aperture", 1, serde_json::json!(56));
        s.insert("kNkMAIDCapability_Odd", 2, serde_json::json!({"value_index": "x", "values": 3}));
        let snaps = d.path().join("snapshots");
        fs::create_dir_all(&snaps).unwrap();
        s.save_to_file(snaps.join("a.json")).unwrap();
        let view = load_source_view(d.path(), "a.json");
        assert!(view.loaded);
        assert_eq!(view.values["kNkMAIDCapability_Aperture"].value, "56");
        assert!(view.values["kNkMAIDCapability_Aperture"].notes.is_empty());
        assert!(!view.values["kNkMAIDCapability_Odd"].notes.is_empty(), "a value the renderer flags carries its flags: {:?}", view.values["kNkMAIDCapability_Odd"]);
        let missing = load_source_view(d.path(), "missing.json");
        assert!(!missing.loaded && missing.values.is_empty() && missing.note.as_deref().is_some_and(|n| n.contains("cannot be shown")), "{missing:?}");
        // a flagged value is shown with its flags
        let step = RestoreStepJson {
            name: "kNkMAIDCapability_Odd".into(),
            code: 2,
            outcome: StepOutcomeJson { tag: StepTag::WouldWrite, raw_tag: "would_write".into(), detail: None },
            changes: Some(true),
        };
        assert!(step.line(&view).contains('['), "{}", step.line(&view));
    }

    #[test]
    fn the_copyable_command_is_a_command_or_nothing() {
        // the library decides; the window draws a Copy button only for Some
        let names = |n: usize| (0..n).map(|i| format!("kNkMAIDCapability_P{i}")).collect::<Vec<_>>();
        assert!(nikon_fleet::ops::restore::undo_command_exact("rp.json", &names(12), Some("/d")).is_some());
        assert!(nikon_fleet::ops::restore::undo_command_exact("rp.json", &names(13), Some("/d")).is_none());
    }

    #[test]
    fn inputs_that_could_be_ambiguous_are_pinned() {
        let plan = plan_of(&world());
        // the model differs and the serial is the same: not this body
        let mut other_model = plan.clone();
        other_model.target.model = "Z6_3".into();
        assert!(check_restore_plan_is_for(&other_model, &request(None)).unwrap_err().contains("not for the Z 9 T1"));
        // the same names twice in a selection are one selection
        let mut dup = plan.clone();
        dup.only = Some(vec!["kNkMAIDCapability_A".into(), "kNkMAIDCapability_A".into()]);
        assert!(check_restore_plan_is_for(&dup, &request(Some(&["kNkMAIDCapability_A"]))).is_ok());
        // an empty label is no label
        let mut blank = plan_of(&world());
        blank.source_label = Some(String::new());
        let text = restore_confirm_text(&PlannedRestore::new(request(None), blank, all_shown(), String::new(), false));
        assert!(text.contains("taken on firmware 5.31, \"\")"), "an empty label is shown as what it is: {text}");
        // a data directory that is not text cannot be passed on a command line
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let bad = Path::new(std::ffi::OsStr::from_bytes(b"/d\xff"));
            assert!(restore_argv(bad, &request(None), None).unwrap_err().contains("not valid UTF-8"));
        }
        // a selection of nothing is not a request
        let mut empty = request(None);
        empty.only = Some(BTreeSet::new());
        assert!(restore_argv(Path::new("/d"), &empty, None).unwrap_err().contains("restore limited to no properties"));
        let mut planned_empty = plan.clone();
        planned_empty.only = Some(vec![]);
        assert!(check_restore_plan_is_for(&planned_empty, &request(None)).is_err(), "None vs Some(nothing): different plans");
        assert!(check_restore_plan_is_for(&plan, &empty).is_err(), "Some(nothing) vs None");
    }

    #[test]
    fn every_stage_of_the_restore_window_draws() {
        let (mut app, _cmd_rx, _evt_tx, _dir) = test_app();
        let ctx = egui::Context::default();
        let mut draw = |app: &mut FleetApp| {
            let _ = ctx.run(egui::RawInput::default(), |ctx| app.ui_restore(ctx));
        };
        app.restore.open = true;
        draw(&mut app); // nothing chosen
        app.restore.target_key = Some(("Z 9".into(), "T1".into()));
        app.restore.source_file = Some(FILE.into());
        app.restore.sync(&app.cameras);
        draw(&mut app);
        let Cmd::Plan(PlanJob::Restore(request)) = app.restore.begin_plan(&app.cameras).unwrap() else { panic!() };
        draw(&mut app); // planning
        let w = world();
        app.restore.plan_arrived(request, Ok(arrived(&w, None)));
        draw(&mut app); // planned
        let dropped = planned_of(&app.restore).selected.iter().next().unwrap().clone();
        app.restore.toggle(&dropped);
        draw(&mut app); // planned, a tick off
        app.restore.toggle(&dropped);
        app.restore.ask_apply();
        draw(&mut app); // confirming
        let Some(Cmd::Apply(ApplyJob::Restore { request, .. })) = app.restore.confirm_apply() else { panic!() };
        draw(&mut app); // applying
        let (json, _) = apply_text(&w, None);
        let report: RestoreApplyJson = serde_json::from_str(&json).unwrap();
        let outcomes = [
            ApplyOutcome::Report { report: Box::new(report.clone()), note: Some("a note".into()), cli_log: "log".into() },
            ApplyOutcome::Report { report: Box::new(RestoreApplyJson { status: ApplyStatus::Failed, undo_set: vec![], ..report.clone() }), note: None, cli_log: String::new() },
            ApplyOutcome::Report { report: Box::new(RestoreApplyJson { undo_set: (0..40).map(|i| format!("kNkMAIDCapability_P{i}")).collect(), ..report }), note: None, cli_log: String::new() },
            ApplyOutcome::NotApplied { why: NotAppliedWhy::PlanStale, message: "m".into(), cli_log: "log".into() },
            ApplyOutcome::Indeterminate { message: "m".into(), cli_log: "log".into() },
        ];
        for outcome in outcomes {
            app.restore.stage = Stage::Applying(request.clone());
            app.restore.applied(request.clone(), outcome);
            assert!(matches!(app.restore.stage(), Stage::Done(_)));
            draw(&mut app);
        }
        // a plan that is blocked, and one that names what cannot be restored
        app.restore.stage = Stage::Idle;
        let mut blocked = arrived(&w, None);
        blocked.0.unrestorable = vec![("kNkMAIDCapability_X".into(), "read_only".into())];
        blocked.2 = SourceView { loaded: false, note: Some("gone".into()), ..SourceView::default() };
        app.restore.stage = Stage::Planning(request.clone());
        app.restore.plan_arrived(request, Ok(blocked));
        draw(&mut app);
        app.restore.notice = "a notice".into();
        draw(&mut app);
    }

}

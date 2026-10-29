// ── Restore and undo: plan, confirm, apply ────────────────────────────────
//
// Put a stored snapshot of a connected body back on that body, or undo an apply by restoring the
// restore point it made. It is `nikon-fleet restore`'s work, run as a subprocess like every camera
// operation here, and it keeps to the discipline the transplant window follows:
//
//   1. PLAN     `restore --dry-run --json`: reads the body, writes nothing, returns a digest, a
//               one-line summary and one step per property.
//   2. REVIEW   the person sees which properties would change and to what, and may narrow the
//               restore to some of them (`--only`); that is a new plan.
//   3. CONFIRM  a text that names the exact body, the snapshot and the plan's summary.
//   4. APPLY    `restore --json --expect-digest --expect-summary`: the CLI recomputes the plan on
//               the body as it is now and refuses (exit `EXIT_STALE_PLAN`, nothing written) unless it
//               is the plan that was confirmed. Then restore point, writes, read-back.
//
// Only a snapshot of the body itself is offered: putting one body's settings on another is a
// transplant. After any apply (this window's or a transplant's) the restore point it made can be
// restored with `only` set to the apply's `undo_set`, through the same four steps.

use super::*;

/// A plan reads every property it would write (4-20 s a read); the transplant's timeouts, for the
/// same work.
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
// The closed vocabularies are closed here too: a tag the library adds fails the parse loudly
// instead of showing as something it is not.

/// `ops::restore::StepOutcome::tag` and `RestoreSkip::tag`. A test pins this list to the library's.
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
}

impl StepTag {
    /// Skipped on purpose or as a matter of course: the camera does not hold or take the setting.
    fn is_routine_skip(self) -> bool {
        match self {
            StepTag::NeverCopied | StepTag::AbsentOnTarget | StepTag::ReadOnly => true,
            StepTag::NotSelected
            | StepTag::RangeScaleDiffers
            | StepTag::WouldWrite
            | StepTag::Written
            | StepTag::UnsupportedWriteType
            | StepTag::WriteFailed => false,
        }
    }

    fn words(self) -> &'static str {
        match self {
            StepTag::NotSelected => "not selected",
            StepTag::NeverCopied => "never copied (state, not a setting)",
            StepTag::AbsentOnTarget => "absent on this camera",
            StepTag::ReadOnly => "read-only on this camera",
            StepTag::RangeScaleDiffers => "left alone: its scale on this camera differs from the snapshot's",
            StepTag::WouldWrite => "would be written",
            StepTag::Written => "written",
            StepTag::UnsupportedWriteType => "the camera refuses writes of this kind",
            StepTag::WriteFailed => "the write failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct StepOutcomeJson {
    pub(crate) tag: StepTag,
    /// `{"message": ...}` for a failed write.
    pub(crate) detail: Option<serde_json::Value>,
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
    /// Would be written, and changes the body or might: the library's `count_changes` rule.
    fn may_change(&self) -> bool {
        self.outcome.tag == StepTag::WouldWrite && self.changes != Some(false)
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
                format!("{name}{label}  →  {}{change}", v.value)
            }
            (StepTag::WouldWrite, None) => format!("{name}{label}  →  (value not shown)"),
            (tag, _) => format!("{name}{label}: {}", tag.words()),
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerdictJson {
    Verified,
    ResidualFound,
    ReadbackIncomplete,
    NothingWritten,
    Unjudged,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ResidualJson {
    pub(crate) changed: Vec<String>,
    pub(crate) missing: Vec<String>,
    pub(crate) unjudged: Vec<String>,
    pub(crate) judged: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct VerificationJson {
    pub(crate) verdict: VerdictJson,
    pub(crate) residual: Option<ResidualJson>,
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
    /// Why some names or values are missing (no `sdk-runtime/`, a snapshot that cannot be shown).
    pub(crate) note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceValue {
    pub(crate) display_name: Option<String>,
    pub(crate) value: String,
}

/// The snapshot's values for display, read like the viewer reads them. Never fails the plan: a
/// restore does not depend on it, so a failure is a note.
pub(crate) fn load_source_view(data_dir: &Path, file: &str) -> SourceView {
    let sdk_runtime = resolve_sdk_runtime(std::env::var_os("NIKON_FLEET_SDK_RUNTIME"));
    match load_for_view(data_dir, &sdk_runtime, file) {
        Ok(r) => SourceView {
            values: r
                .rows
                .into_iter()
                .map(|row| {
                    let display_name = row.extra_display_name().map(str::to_string);
                    (row.name, SourceValue { display_name, value: row.value.display })
                })
                .collect(),
            note: r.labels_note,
        },
        Err(e) => SourceView { values: BTreeMap::new(), note: Some(format!("the snapshot's values cannot be shown here: {e}")) },
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
    /// plan is made for it.
    pub(crate) selected: BTreeSet<String>,
}

impl PlannedRestore {
    fn new(request: RestoreRequest, plan: RestorePlanJson, source: SourceView, cli_log: String) -> Self {
        let selected = plan.steps.iter().filter(|s| s.may_change()).map(|s| s.name.clone()).collect();
        PlannedRestore { request, plan, source, cli_log, selected }
    }

    /// Every step that would change the body or might.
    fn changing(&self) -> Vec<&RestoreStepJson> {
        self.plan.steps.iter().filter(|s| s.may_change()).collect()
    }

    /// The ticks differ from every changing property and are not empty: a new plan for them would
    /// be a narrower restore. (Nothing ticked is refused, not read as "everything".)
    fn narrowed(&self) -> bool {
        !self.selected.is_empty() && self.selected.len() != self.changing().len()
    }

    /// A plan that names properties it cannot restore is not applied: the restore would end failed.
    fn applicable(&self) -> bool {
        self.plan.would_change > 0 && self.plan.unrestorable.is_empty()
    }
}

/// Why a restore is refused before the camera is touched, beyond what the CLI itself checks: a write
/// must be aimed at one body with certainty. The same rule a transplant's target follows.
pub(crate) fn restore_target_refusal(camera: &CameraRow) -> Option<String> {
    transplant_target_refusal(camera)
}

/// The full pre-flight: the target's own check, then a fresh USB-only look at the bus.
pub(crate) fn restore_preflight(camera: &CameraRow, scan: &CameraList) -> Option<String> {
    let reason = restore_target_refusal(camera).or_else(|| snapshot_refusal(camera, scan).map(|r| r.reason()))?;
    Some(format!("Restore refused, nothing was read or written: {reason}"))
}

/// The CLI arguments for a plan (`confirmed` is `None`) or an apply (the digest and summary the
/// person confirmed). `--flag=value` throughout, so a value starting with `-` stays attached to its
/// flag, and `--` before the source so a file name cannot be taken for a flag.
pub(crate) fn restore_argv(data_dir: &Path, request: &RestoreRequest, confirmed: Option<(&str, &str)>) -> Result<Vec<String>, String> {
    let dir = data_dir
        .to_str()
        .ok_or_else(|| format!("the data directory {} is not valid UTF-8", data_dir.display()))?;
    let source = snapshot_path_for_cli(data_dir, &request.source_file)?;
    let mut argv = vec![
        format!("--data-dir={dir}"),
        "restore".into(),
        "--json".into(),
        format!("--serial={}", request.target.serial),
    ];
    match confirmed {
        None => argv.push("--dry-run".into()),
        Some((digest, summary)) => {
            argv.push(format!("--expect-digest={digest}"));
            argv.push(format!("--expect-summary={summary}"));
        }
    }
    for name in request.only.iter().flatten() {
        argv.push(format!("--only={name}"));
    }
    argv.push("--".into());
    argv.push(source);
    Ok(argv)
}

pub(crate) fn do_restore_plan(data_dir: &Path, request: &RestoreRequest) -> Result<(RestorePlanJson, String, SourceView), String> {
    let (plan, log) = restore_plan_via(&locate_cli()?, data_dir, request, &list_cameras, RESTORE_PLAN_TIMEOUT)?;
    Ok((plan, log, load_source_view(data_dir, &request.source_file)))
}

/// Ask the CLI for a plan. Reads the body, writes nothing. A plan that names properties it cannot
/// restore is printed and then fails the command (exit 1): the plan is what is shown, with that
/// said, so what is read is stdout whatever the exit was.
pub(crate) fn restore_plan_via(
    cli: &Path,
    data_dir: &Path,
    request: &RestoreRequest,
    scan: &dyn Fn() -> CameraList,
    timeout: Duration,
) -> Result<(RestorePlanJson, String), String> {
    if let Some(refusal) = restore_preflight(&request.target, &scan()) {
        return Err(refusal);
    }
    let argv = restore_argv(data_dir, request, None)?;
    let (stdout, stderr, failure) = match run_cli(cli, &argv, None, timeout) {
        Ok(out) => (out.stdout, out.stderr, None),
        Err(e) => (e.stdout.clone(), e.stderr.clone(), Some(e)),
    };
    match serde_json::from_str::<RestorePlanJson>(stdout.trim()) {
        Ok(plan) => {
            check_restore_plan_is_for(&plan, request)?;
            Ok((plan, stderr))
        }
        Err(e) => Err(match failure {
            Some(f) => format!("Planning failed: {}", f.message),
            None => format!("the plan command succeeded but its report could not be read ({e}); nothing was written"),
        }),
    }
}

/// A plan is shown and confirmed only if it is for the body (at the firmware it runs now), from a
/// snapshot of that same body, and for the selection that was asked for.
pub(crate) fn check_restore_plan_is_for(plan: &RestorePlanJson, request: &RestoreRequest) -> Result<(), String> {
    let t = &request.target;
    if plan.target.serial != t.serial || plan.target.model != t.model {
        return Err(format!(
            "the plan is for {} {}, not for the {} {} that was asked for; nothing was written",
            plan.target.model, plan.target.serial, t.model, t.serial
        ));
    }
    if plan.target.firmware != t.firmware {
        return Err(format!(
            "the plan was made for firmware {} on {} {}, which is {} now; nothing was written",
            plan.target.firmware, plan.target.model, plan.target.serial, t.firmware
        ));
    }
    if (&plan.source.model, &plan.source.serial) != (&t.model, &t.serial) {
        return Err(format!(
            "the snapshot is of {} {}, not of the {} {} it would be written to; copying between bodies is a transplant. Nothing was written",
            plan.source.model, plan.source.serial, t.model, t.serial
        ));
    }
    let planned: Option<BTreeSet<&String>> = plan.only.as_ref().map(|o| o.iter().collect());
    let asked: Option<BTreeSet<&String>> = request.only.as_ref().map(|o| o.iter().collect());
    if planned != asked {
        return Err("the plan does not carry the selection that was asked for; nothing was written".to_string());
    }
    Ok(())
}

pub(crate) fn do_restore_apply(data_dir: &Path, request: &RestoreRequest, digest: &str, summary: &str) -> ApplyOutcome<RestoreApplyJson> {
    match locate_cli() {
        Ok(cli) => restore_apply_via(&cli, data_dir, request, digest, summary, &list_cameras, RESTORE_APPLY_TIMEOUT),
        Err(message) => ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, message, cli_log: String::new() },
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
    let refused = |message: String| ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, message, cli_log: String::new() };
    if let Some(refusal) = restore_preflight(&request.target, &scan()) {
        return refused(refusal);
    }
    let argv = match restore_argv(data_dir, request, Some((digest, summary))) {
        Ok(a) => a,
        Err(e) => return refused(e),
    };
    classify_apply_as::<RestoreApplyJson>(run_cli(cli, &argv, None, timeout), digest)
}

// What the person is shown, as text with a level. Pure, so each is tested without a window.

/// The text of the confirmation for an apply: it names the body and the snapshot and carries the
/// plan's summary, so what is approved is what is sent.
pub(crate) fn restore_confirm_text(p: &PlannedRestore) -> String {
    let t = &p.request.target;
    let plan = &p.plan;
    let mut s = format!(
        "Write to {} (serial {}, firmware {}) the settings stored in {}{}?\n{}",
        t.model,
        t.serial,
        t.firmware,
        p.request.source_file,
        plan.source_label.as_deref().map(|l| format!(" ({l:?})")).unwrap_or_default(),
        plan.summary,
    );
    let changing = p.changing();
    s.push_str(&format!("\n{} propert{} listed under \"Would change\" will be written.", changing.len(), if changing.len() == 1 { "y" } else { "ies" }));
    if let Some(only) = &plan.only {
        s.push_str(&format!(" Nothing else is touched: this restore is limited to the {} propert(ies) named in the plan.", only.len()));
    }
    s.push_str("\nA restore point of the camera is saved first and every write is read back; the result says whether it took.");
    s
}

fn verdict_line(v: &VerificationJson) -> (Level, String) {
    match v.verdict {
        VerdictJson::Verified => (Level::Info, "Read-back: every written property reads back as restored.".into()),
        VerdictJson::NothingWritten => (Level::Info, "Read-back: nothing was written, so there was nothing to verify.".into()),
        VerdictJson::Unjudged => (
            Level::Warn,
            "Read-back: UNVERIFIED. None of the written properties could be compared (volatile-listed, never-copied or not in the snapshot); nothing is claimed.".into(),
        ),
        VerdictJson::ReadbackIncomplete => (
            Level::Warn,
            "Read-back: INCOMPLETE. The camera could not be read back well enough to verify the writes; nothing is claimed.".into(),
        ),
        VerdictJson::ResidualFound => (
            Level::Error,
            "Read-back: UNVERIFIED. The camera accepted a write and did not keep it, or changed it; see below.".into(),
        ),
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
        line(Level::Error, format!("Write failed: {}: {}", short_name(&e.name), e.message));
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
    if let Some(v) = &r.verification {
        let (level, text) = verdict_line(v);
        line(level, text);
        if let Some(res) = &v.residual {
            if !res.changed.is_empty() {
                line(Level::Error, format!("Did NOT read back as restored ({}): {}", res.changed.len(), names_list(&res.changed)));
            }
            if !res.missing.is_empty() {
                line(Level::Error, format!("Could not be read back ({}): {}", res.missing.len(), names_list(&res.missing)));
            }
            if !res.unjudged.is_empty() {
                line(Level::Warn, format!("Written but not compared ({}): {}", res.unjudged.len(), names_list(&res.unjudged)));
            }
        }
    } else if !r.no_op {
        line(Level::Warn, "No read-back was made, so nothing written is confirmed.".into());
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
/// it changed, and the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UndoOffer {
    /// The restore point, a file name in `snapshots/`.
    pub restore_point: String,
    pub undo_set: Vec<String>,
    pub target: CameraRow,
}

/// The undo an outcome offers, if it offers one: it needs a restore point and something to put
/// back. A failed apply offers it too (the writes are made one after another, so some may have
/// landed); an apply that never wrote, or whose outcome is unknown, offers none, because there is
/// no restore point to name.
pub(crate) fn undo_offer<R: AppliedReport>(target: &CameraRow, outcome: &ApplyOutcome<R>) -> Option<UndoOffer> {
    let ApplyOutcome::Report { report, .. } = outcome else {
        return None;
    };
    let wrote = match report.status() {
        ApplyStatus::AppliedVerified | ApplyStatus::AppliedUnverified | ApplyStatus::Failed => true,
        ApplyStatus::Indeterminate | ApplyStatus::NotApplied => false,
    };
    let point = report.restore_point()?;
    if !wrote || report.undo_set().is_empty() {
        return None;
    }
    Some(UndoOffer { restore_point: point.file.clone(), undo_set: report.undo_set().to_vec(), target: target.clone() })
}

/// Where a restore is: one plan at a time, and a plan or a confirmation is good only for the
/// request it was made for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum RestoreStage {
    #[default]
    Idle,
    /// A plan was asked for this request and has not come back.
    Planning(RestoreRequest),
    Planned(Box<PlannedRestore>),
    /// Asked to apply; waiting for the confirmation.
    ConfirmApply(Box<PlannedRestore>),
    /// Sent; the answer is the only thing that ends this.
    Applying(RestoreRequest),
    Done(Box<AppliedRestore>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppliedRestore {
    pub(crate) request: RestoreRequest,
    pub(crate) outcome: ApplyOutcome<RestoreApplyJson>,
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
    pub(crate) only_for: Option<(String, CameraRow)>,
    pub(crate) stage: RestoreStage,
    /// Why a plan or confirmation went away, or how a plan failed.
    pub notice: String,
}

impl RestoreUi {
    pub fn target_camera<'a>(&self, cameras: &'a [CameraRow]) -> Option<&'a CameraRow> {
        let (model, serial) = self.target_key.as_ref()?;
        cameras.iter().find(|c| &c.model == model && &c.serial == serial)
    }

    /// The request the form asks for now, if it asks for one.
    pub fn request(&self, cameras: &[CameraRow]) -> Option<RestoreRequest> {
        Some(RestoreRequest {
            source_file: self.source_file.clone()?,
            target: self.target_camera(cameras)?.clone(),
            only: self.only.clone(),
        })
    }

    /// Run every frame, before anything is drawn: a selection belongs to the snapshot and target it
    /// was made for, and a plan or confirmation does not outlive a change to its request.
    pub fn sync(&mut self, cameras: &[CameraRow]) {
        let pair = self.source_file.clone().zip(self.target_camera(cameras).cloned());
        if self.only_for != pair {
            self.only = None;
            self.only_for = pair;
        }
        let held = match &self.stage {
            RestoreStage::Planning(r) => Some(r),
            RestoreStage::Planned(p) | RestoreStage::ConfirmApply(p) => Some(&p.request),
            _ => None,
        };
        if let Some(held) = held {
            if self.request(cameras).as_ref() != Some(held) {
                let was_confirming = matches!(self.stage, RestoreStage::ConfirmApply(_));
                self.stage = RestoreStage::Idle;
                self.notice = format!(
                    "{} withdrawn: the snapshot, the camera or the selection changed. Nothing was written.",
                    if was_confirming { "Confirmation" } else { "Plan" }
                );
            }
        }
    }

    pub fn stage(&self) -> &RestoreStage {
        &self.stage
    }

    pub fn only(&self) -> Option<&BTreeSet<String>> {
        self.only.as_ref()
    }

    /// "Discard plan" / "Close this result".
    pub fn discard(&mut self) {
        if matches!(self.stage, RestoreStage::Planned(_) | RestoreStage::Done(_)) {
            self.stage = RestoreStage::Idle;
        }
    }

    /// The data directory changed under a plan or a confirmation: the snapshots and the restore
    /// points it names are no longer where it said, so it is withdrawn. An apply on the wire is not:
    /// its answer must still find it.
    pub fn data_dir_changed(&mut self) {
        if matches!(self.stage, RestoreStage::Planning(_) | RestoreStage::Planned(_) | RestoreStage::ConfirmApply(_)) {
            self.stage = RestoreStage::Idle;
            self.notice = "Plan withdrawn: the data directory changed. Nothing was written.".to_string();
        }
    }

    /// The worker thread is gone: nothing more will be answered. A plan being made is lost; an apply
    /// on the wire may have written, whatever stopped.
    pub fn worker_lost(&mut self) {
        match &self.stage {
            RestoreStage::Planning(_) => {
                self.stage = RestoreStage::Idle;
                self.notice = "The worker thread stopped while the camera was being read. Nothing was written.".to_string();
            }
            RestoreStage::Applying(request) => {
                let outcome = ApplyOutcome::Indeterminate {
                    message: "The worker thread stopped while the restore was running, so what it wrote is unknown.".to_string(),
                    cli_log: String::new(),
                };
                self.stage = RestoreStage::Done(Box::new(AppliedRestore { request: request.clone(), outcome }));
            }
            _ => {}
        }
    }

    /// A command could not be handed to the worker, which is gone: the camera was not asked.
    pub fn not_sent(&mut self) {
        match &self.stage {
            RestoreStage::Planning(_) => {
                self.stage = RestoreStage::Idle;
                self.notice = "The worker thread has stopped, so the camera was not read. Nothing was written.".to_string();
            }
            RestoreStage::Applying(request) => {
                let outcome = ApplyOutcome::NotApplied {
                    why: NotAppliedWhy::Refused,
                    message: "The worker thread has stopped, so the restore was never sent.".to_string(),
                    cli_log: String::new(),
                };
                self.stage = RestoreStage::Done(Box::new(AppliedRestore { request: request.clone(), outcome }));
            }
            _ => {}
        }
    }

    /// Start a plan for the form as it is. The command to send, or why not.
    pub fn begin_plan(&mut self, cameras: &[CameraRow]) -> Result<Cmd, String> {
        if matches!(self.stage, RestoreStage::Applying(_) | RestoreStage::Planning(_)) {
            return Err("Another restore step is still running.".to_string());
        }
        let request = self.request(cameras).ok_or("Choose a connected body and one of its stored snapshots first.")?;
        if let Some(why) = restore_target_refusal(&request.target) {
            return Err(why);
        }
        self.notice.clear();
        self.stage = RestoreStage::Planning(request.clone());
        Ok(Cmd::RestorePlan { request })
    }

    /// A plan came back. Dropped unless it is the one asked for and still wanted.
    pub fn plan_arrived(&mut self, request: RestoreRequest, result: Result<(RestorePlanJson, String, SourceView), String>) {
        if self.stage != RestoreStage::Planning(request.clone()) {
            return;
        }
        match result {
            Ok((plan, cli_log, source)) => {
                self.stage = RestoreStage::Planned(Box::new(PlannedRestore::new(request, plan, source, cli_log)));
            }
            Err(e) => {
                self.stage = RestoreStage::Idle;
                self.notice = e;
            }
        }
    }

    /// Tick or untick one property of the plan being reviewed. Only a step that would change the
    /// body can be ticked.
    pub fn toggle(&mut self, name: &str) {
        if let RestoreStage::Planned(p) = &mut self.stage {
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
        let RestoreStage::Planned(p) = &self.stage else {
            return Err("There is no plan to narrow.".to_string());
        };
        if p.selected.is_empty() {
            return Err("Tick at least one property: a restore of nothing is not a restore.".to_string());
        }
        let selection = p.selected.clone();
        self.only = Some(selection);
        self.stage = RestoreStage::Idle;
        self.begin_plan(cameras)
    }

    /// Plan again for every property, dropping the selection.
    pub fn plan_everything(&mut self, cameras: &[CameraRow]) -> Result<Cmd, String> {
        if !matches!(self.stage, RestoreStage::Planned(_)) {
            return Err("There is no plan to widen.".to_string());
        }
        self.only = None;
        self.stage = RestoreStage::Idle;
        self.begin_plan(cameras)
    }

    /// Ask for the confirmation to apply. Only a plan that would write something, and names nothing
    /// it cannot restore, can be applied.
    pub fn ask_apply(&mut self) {
        if let RestoreStage::Planned(p) = &self.stage {
            if p.applicable() {
                self.stage = RestoreStage::ConfirmApply(p.clone());
            }
        }
    }

    /// Back from a confirmation to the plan; nothing was asked of the camera.
    pub fn cancel_confirmation(&mut self) {
        if let RestoreStage::ConfirmApply(p) = &self.stage {
            self.stage = RestoreStage::Planned(p.clone());
        }
    }

    /// The apply was confirmed: what is sent is the plan that was shown, not the form as it is now.
    pub fn confirm_apply(&mut self) -> Option<Cmd> {
        let RestoreStage::ConfirmApply(p) = &self.stage else {
            return None;
        };
        let cmd = Cmd::RestoreApply { request: p.request.clone(), digest: p.plan.digest.clone(), summary: p.plan.summary.clone() };
        self.stage = RestoreStage::Applying(p.request.clone());
        Some(cmd)
    }

    /// The answer to an apply.
    pub fn applied(&mut self, request: RestoreRequest, outcome: ApplyOutcome<RestoreApplyJson>) {
        if self.stage != RestoreStage::Applying(request.clone()) {
            return;
        }
        self.only = None;
        self.stage = RestoreStage::Done(Box::new(AppliedRestore { request, outcome }));
    }

    /// Set the form to undo `offer` and plan it. The plan is read-only; the person reviews it and
    /// confirms it like any restore. The body must be connected, and no step may be in flight.
    pub fn start_undo(&mut self, offer: &UndoOffer, cameras: &[CameraRow]) -> Result<Cmd, String> {
        if matches!(self.stage, RestoreStage::Applying(_) | RestoreStage::Planning(_)) {
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
        self.stage = RestoreStage::Idle;
        self.begin_plan(cameras)
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
    View(String),
    Copy(String),
    Undo(UndoOffer),
}

fn draw_restore_plan(ui: &mut egui::Ui, p: &PlannedRestore, can_edit: bool, action: &mut Option<RestoreAction>) {
    let plan = &p.plan;
    ui.heading(format!("{} {}", plan.target.model, plan.target.serial));
    ui.label(
        egui::RichText::new(format!(
            "{}{}  →  {} (fw {})",
            p.request.source_file,
            plan.source_label.as_deref().map(|l| format!("  ({l})")).unwrap_or_default(),
            plan.target.model,
            plan.target.firmware
        ))
        .weak(),
    );
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
    if !plan.unrestorable.is_empty() {
        let list: Vec<String> = plan.unrestorable.iter().map(|(n, why)| format!("{} ({})", short_name(n), why.replace('_', " "))).collect();
        ui.colored_label(
            ERROR_COLOR,
            format!("These were named but cannot be restored, so this plan cannot be applied: {}. Narrow the selection or restore everything.", list.join(", ")),
        );
    }
    ui.separator();

    let changing = p.changing();
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
                ui.add_enabled_ui(can_edit && p.narrowed(), |ui| {
                    if ui.button("Plan again for the ticked ones only").clicked() {
                        *action = Some(RestoreAction::PlanSelection);
                    }
                });
            });
        });
    }
    let same: Vec<&RestoreStepJson> = plan.steps.iter().filter(|s| s.outcome.tag == StepTag::WouldWrite && !s.may_change()).collect();
    if !same.is_empty() {
        egui::CollapsingHeader::new(format!("Already the same ({})", same.len())).id_salt("rs_same").show(ui, |ui| {
            for s in same {
                ui.label(s.line(&p.source));
            }
        });
    }
    let attention: Vec<&RestoreStepJson> = plan
        .steps
        .iter()
        .filter(|s| !s.outcome.tag.is_routine_skip() && !matches!(s.outcome.tag, StepTag::WouldWrite | StepTag::NotSelected))
        .collect();
    if !attention.is_empty() {
        egui::CollapsingHeader::new(format!("Skipped, needs attention ({})", attention.len())).id_salt("rs_attention").default_open(true).show(ui, |ui| {
            for s in attention {
                ui.colored_label(WARN_COLOR, s.line(&p.source));
            }
        });
    }
    let routine: Vec<&RestoreStepJson> = plan.steps.iter().filter(|s| s.outcome.tag.is_routine_skip()).collect();
    if !routine.is_empty() {
        egui::CollapsingHeader::new(format!("Skipped as a matter of course ({})", routine.len())).id_salt("rs_routine").show(ui, |ui| {
            for s in routine {
                ui.label(s.line(&p.source));
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
/// equivalent command to copy. Shared by the transplant and restore results.
pub(crate) fn draw_restore_point(
    ui: &mut egui::Ui,
    restore_point: &RestorePointJson,
    offer: Option<&UndoOffer>,
    undo_set: &[String],
    busy: bool,
) -> Option<RestoreActionLike> {
    let mut chosen = None;
    ui.separator();
    ui.label(egui::RichText::new("Restore point").strong());
    ui.label("The camera as it was just before this apply:");
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(&restore_point.file).monospace());
        if ui.small_button("View").clicked() {
            chosen = Some(RestoreActionLike::View(restore_point.file.clone()));
        }
    });
    let command = undo_command(&restore_point.file, undo_set);
    match offer {
        Some(offer) => {
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    if ui
                        .button("Undo…")
                        .on_hover_text("Plan putting back what this changed. You review the plan and confirm it before anything is written.")
                        .clicked()
                    {
                        chosen = Some(RestoreActionLike::Undo(offer.clone()));
                    }
                });
                ui.label(format!("puts back {} propert(ies) from the restore point.", offer.undo_set.len()));
            });
        }
        None => {
            ui.label(egui::RichText::new("Nothing to put back.").weak());
        }
    }
    ui.label("The same from a terminal:");
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(&command).monospace());
        if ui.small_button("Copy").clicked() {
            chosen = Some(RestoreActionLike::Copy(command.clone()));
        }
    });
    if !undo_set.is_empty() {
        egui::CollapsingHeader::new(format!("Properties an undo puts back ({})", undo_set.len()))
            .id_salt("undo_set")
            .show(ui, |ui| ui.label(names_list(undo_set)));
    }
    chosen
}

/// The clicks a result view can hand back, whichever window drew it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RestoreActionLike {
    View(String),
    Copy(String),
    Undo(UndoOffer),
}

fn draw_restore_done(ui: &mut egui::Ui, a: &AppliedRestore, busy: bool, action: &mut Option<RestoreAction>) {
    let t = &a.request.target;
    ui.label(egui::RichText::new(format!("{}  →  {} {}", a.request.source_file, t.model, t.serial)).weak());
    match &a.outcome {
        ApplyOutcome::Report { report, note, cli_log } => {
            let (level, headline) = status_headline(report.status);
            ui.heading(report.status.tag());
            colored(ui, level, headline);
            if let Some(note) = note {
                colored(ui, Level::Warn, note.clone());
            }
            for (level, line) in restore_report_lines(report) {
                colored(ui, level, line);
            }
            if let Some(rp) = &report.restore_point {
                let offer = undo_offer(t, &a.outcome);
                if let Some(c) = draw_restore_point(ui, rp, offer.as_ref(), &report.undo_set, busy) {
                    *action = Some(match c {
                        RestoreActionLike::View(f) => RestoreAction::View(f),
                        RestoreActionLike::Copy(c) => RestoreAction::Copy(c),
                        RestoreActionLike::Undo(o) => RestoreAction::Undo(o),
                    });
                }
            }
            if !report.written.is_empty() {
                egui::CollapsingHeader::new(format!("Properties written ({})", report.written.len())).id_salt("rs_written").show(ui, |ui| {
                    ui.label(names_list(&report.written));
                });
            }
            if !cli_log.trim().is_empty() {
                egui::CollapsingHeader::new("CLI messages").id_salt("rs_applied_log").show(ui, |ui| {
                    ui.label(egui::RichText::new(cli_log.trim_end()).monospace());
                });
            }
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
            if !cli_log.trim().is_empty() {
                ui.label(egui::RichText::new(cli_log.trim_end()).monospace());
            }
        }
        ApplyOutcome::Indeterminate { message, cli_log } => {
            ui.heading("indeterminate");
            colored(ui, Level::Error, "The camera may have been changed.");
            ui.label(message);
            ui.label("Take a snapshot of the camera and compare it with the one you started from before doing anything else.");
            if !cli_log.trim().is_empty() {
                ui.label(egui::RichText::new(cli_log.trim_end()).monospace());
            }
        }
    }
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
                    let refusal = r.target_camera(cameras).and_then(restore_target_refusal);
                    if let Some(why) = &refusal {
                        ui.colored_label(ERROR_COLOR, why);
                    }

                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("2. The snapshot: one of that body's own").strong());
                    let rows: Vec<&SnapRow> = match (listing, r.target_camera(cameras)) {
                        (Some(l), Some(c)) => l.rows_for(&c.model, &c.serial).filter(|row| !row.name_lossy).collect(),
                        _ => Vec::new(),
                    };
                    if r.target_camera(cameras).is_some() && rows.is_empty() {
                        ui.label(egui::RichText::new("This body has no stored snapshots.").italics().weak());
                    }
                    ui.add_enabled_ui(!busy, |ui| {
                        let text = r
                            .source_file
                            .as_ref()
                            .map(|f| rows.iter().find(|row| &row.file == f).map_or_else(|| f.clone(), |row| snapshot_label(row)))
                            .unwrap_or_else(|| "choose a snapshot…".to_string());
                        egui::ComboBox::from_id_salt("rs_source").selected_text(text).width(520.0).show_ui(ui, |ui| {
                            for row in &rows {
                                let picked = r.source_file.as_ref() == Some(&row.file);
                                if ui.selectable_label(picked, snapshot_label(row)).clicked() {
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
                        && !matches!(r.stage(), RestoreStage::Applying(_) | RestoreStage::Planning(_));
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
                        RestoreStage::Idle => {
                            ui.label(
                                egui::RichText::new("Choose a body and a snapshot, then plan. Nothing is sent to the camera until you have read the plan and confirmed it.")
                                    .weak(),
                            );
                        }
                        RestoreStage::Planning(_) => {
                            ui.label("Planning… the camera is being read.");
                        }
                        RestoreStage::Planned(p) => {
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
                        RestoreStage::ConfirmApply(p) => {
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
                        RestoreStage::Applying(req) => {
                            ui.colored_label(
                                WARN_COLOR,
                                format!("Writing to {} {}… keep it connected and do not close this program.", req.target.model, req.target.serial),
                            );
                        }
                        RestoreStage::Done(a) => {
                            draw_restore_done(ui, a, busy, &mut action);
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
            RestoreAction::View(file) => self.open_snapshot(ctx, &file, false, false),
            RestoreAction::Copy(text) => ctx.copy_text(text),
            RestoreAction::Undo(offer) => self.start_undo(offer),
        }
    }

    /// Hand a restore command to the worker. If the worker is gone the command was not delivered,
    /// and the window must not wait for an answer that cannot come.
    pub(crate) fn send_restore(&mut self, cmd: Cmd) {
        if !self.send(cmd) {
            self.restore.not_sent();
        }
    }

    /// Undo an apply: the restore window opens on its restore point with the apply's undo set, and
    /// the plan is made (read-only). The person reviews it and confirms it as any restore.
    pub(crate) fn start_undo(&mut self, offer: UndoOffer) {
        if self.busy() {
            self.status = Status::new(Level::Warn, "Another camera operation is running; try the undo when it has finished.");
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
        self.restore.open = true;
        self.restore.target_key = Some(body);
        self.restore.source_file = Some(file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{argv_of, cli_error, library_apply_json, recording_cli, row, scan_of, test_app, usb};
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
        PlannedRestore::new(request(None), plan_of(&world()), SourceView::default(), String::new())
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
            seen.insert(o.tag());
            let _ = parsed.tag.words();
        }
        assert_eq!(seen.len(), outcomes.len(), "nine different tags");
        let junk = serde_json::from_value::<StepOutcomeJson>(serde_json::json!({"tag": "a_new_kind_of_step"}));
        assert!(junk.is_err(), "a tag the GUI does not know fails loudly");
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
        assert_eq!(r.verification.as_ref().map(|v| v.verdict), Some(VerdictJson::Verified));

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
        let never_ran = classify(cli_error(CliFailure::Spawn, None, ""));
        assert!(matches!(never_ran, ApplyOutcome::NotApplied { .. }));
        for e in [
            cli_error(CliFailure::TimedOut, None, ""),
            cli_error(CliFailure::Wait, None, ""),
            cli_error(CliFailure::Exit, None, ""),
            cli_error(CliFailure::Exit, Some(1), ""),
            cli_error(CliFailure::Exit, Some(2), ""),
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
        let Cmd::RestorePlan { request } = ui.begin_plan(&cameras).unwrap() else { panic!("a plan command") };
        ui.plan_arrived(request, Ok((plan_of(&world()), String::new(), SourceView::default())));
        assert!(matches!(ui.stage(), RestoreStage::Planned(_)));
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
        assert_eq!(*ui.stage(), RestoreStage::Idle);
        assert!(matches!(ui.begin_plan(&[row("Z 9", "T1", 2)]), Err(_)), "a body that shares its model cannot be aimed at");
        assert!(matches!(ui.begin_plan(&[target()]), Ok(Cmd::RestorePlan { .. })));
        assert!(ui.begin_plan(&[target()]).is_err(), "one plan at a time");
    }

    #[test]
    fn a_plan_that_comes_back_for_another_request_is_dropped_and_a_failed_one_is_a_notice() {
        let (mut ui, cameras) = ui_ready();
        ui.sync(&cameras);
        let Cmd::RestorePlan { request: asked } = ui.begin_plan(&cameras).unwrap() else { panic!() };
        let mut other = asked.clone();
        other.source_file = "another.json".into();
        ui.plan_arrived(other, Ok((plan_of(&world()), String::new(), SourceView::default())));
        assert!(matches!(ui.stage(), RestoreStage::Planning(_)), "still waiting for the plan that was asked for");
        ui.plan_arrived(asked, Err("Planning failed: camera busy".into()));
        assert_eq!(*ui.stage(), RestoreStage::Idle);
        assert!(ui.notice.contains("camera busy"));
        let stray = request(None);
        ui.plan_arrived(stray, Ok((plan_of(&world()), String::new(), SourceView::default())));
        assert_eq!(*ui.stage(), RestoreStage::Idle, "a plan nobody asked for is not shown");
    }

    #[test]
    fn apply_is_confirmed_in_two_steps_and_sends_the_plan_that_was_shown() {
        let (mut ui, _) = ui_with_plan();
        let shown = plan_of(&world());
        assert!(ui.confirm_apply().is_none(), "no confirmation was asked for");
        ui.ask_apply();
        assert!(matches!(ui.stage(), RestoreStage::ConfirmApply(_)));
        ui.cancel_confirmation();
        assert!(matches!(ui.stage(), RestoreStage::Planned(_)));
        assert!(ui.confirm_apply().is_none(), "cancelled");
        ui.ask_apply();
        let Some(Cmd::RestoreApply { request: sent, digest, summary }) = ui.confirm_apply() else { panic!("an apply") };
        assert_eq!((sent, digest, summary), (request(None), shown.digest, shown.summary));
        assert!(matches!(ui.stage(), RestoreStage::Applying(_)));
        assert!(ui.confirm_apply().is_none(), "no second apply while one is on the wire");
        assert!(ui.begin_plan(&[target()]).is_err(), "nor a plan");
    }

    #[test]
    fn a_plan_that_would_change_nothing_or_names_what_cannot_be_restored_cannot_be_applied() {
        let (mut ui, _) = ui_ready();
        let cameras = vec![target()];
        ui.sync(&cameras);
        let Cmd::RestorePlan { request } = ui.begin_plan(&cameras).unwrap() else { panic!() };
        ui.plan_arrived(request.clone(), Ok((plan_of(&world_with(&[0, 1, 2], &[], None)), String::new(), SourceView::default())));
        ui.ask_apply();
        assert!(matches!(ui.stage(), RestoreStage::Planned(_)), "nothing to restore");
        // one change is enough (the boundary)
        let one = plan_of(&world_with(&[0, 1], &[], None));
        assert_eq!(one.would_change, 1);
        assert!(PlannedRestore::new(request.clone(), one, SourceView::default(), String::new()).applicable());
        // a change count of zero is not
        assert!(!PlannedRestore::new(request.clone(), plan_of(&world_with(&[0, 1, 2], &[], None)), SourceView::default(), String::new()).applicable());
        let mut named = plan_of(&world());
        named.unrestorable = vec![("kNkMAIDCapability_X".into(), "read_only".into())];
        assert!(!PlannedRestore::new(request, named, SourceView::default(), String::new()).applicable());
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
                    assert!(matches!(ui.stage(), RestoreStage::ConfirmApply(_)));
                }
                ui.sync(&cameras);
                assert!(!matches!(ui.stage(), RestoreStage::Idle), "{what}: nothing changed yet");
                change(&mut ui, &mut cameras);
                ui.sync(&cameras);
                assert_eq!(*ui.stage(), RestoreStage::Idle, "{what} (confirming: {confirming})");
                assert!(ui.notice.contains("withdrawn"), "{what}: {}", ui.notice);
                assert!(ui.confirm_apply().is_none(), "{what}: nothing is left to confirm");
            }
        }
        // a plan still being made is withdrawn too, and its answer is then ignored
        let (mut ui, mut cameras) = ui_ready();
        ui.sync(&cameras);
        let Cmd::RestorePlan { request } = ui.begin_plan(&cameras).unwrap() else { panic!() };
        cameras[0].firmware = "5.40".into();
        ui.sync(&cameras);
        assert_eq!(*ui.stage(), RestoreStage::Idle);
        ui.plan_arrived(request, Ok((plan_of(&world()), String::new(), SourceView::default())));
        assert_eq!(*ui.stage(), RestoreStage::Idle);
    }

    #[test]
    fn an_apply_in_flight_is_not_withdrawn_by_a_change_and_its_answer_is_kept() {
        let (mut ui, mut cameras) = ui_with_plan();
        ui.ask_apply();
        let Some(Cmd::RestoreApply { request, .. }) = ui.confirm_apply() else { panic!() };
        cameras[0].firmware = "5.40".into();
        ui.source_file = Some("another.json".into());
        ui.sync(&cameras);
        assert!(matches!(ui.stage(), RestoreStage::Applying(_)), "the answer must still find it");
        // an answer for another request is not this apply's
        let mut elsewhere = request.clone();
        elsewhere.source_file = "another.json".into();
        ui.applied(elsewhere, ApplyOutcome::Indeterminate { message: "x".into(), cli_log: String::new() });
        assert!(matches!(ui.stage(), RestoreStage::Applying(_)));
        ui.applied(request, ApplyOutcome::NotApplied { why: NotAppliedWhy::PlanStale, message: "m".into(), cli_log: String::new() });
        assert!(matches!(ui.stage(), RestoreStage::Done(_)));
    }

    #[test]
    fn a_change_of_data_directory_withdraws_a_plan_and_each_confirmation_but_not_an_apply_on_the_wire() {
        for confirming in [false, true] {
            let (mut ui, _) = ui_with_plan();
            if confirming {
                ui.ask_apply();
            }
            ui.data_dir_changed();
            assert_eq!(*ui.stage(), RestoreStage::Idle, "confirming: {confirming}");
            assert!(ui.notice.contains("data directory"));
        }
        let (mut ui, _) = ui_with_plan();
        ui.ask_apply();
        ui.confirm_apply().unwrap();
        ui.data_dir_changed();
        assert!(matches!(ui.stage(), RestoreStage::Applying(_)));
    }

    #[test]
    fn a_dead_worker_ends_a_plan_as_lost_and_an_apply_as_unknown_never_as_unchanged() {
        let (mut ui, cameras) = ui_ready();
        ui.sync(&cameras);
        ui.begin_plan(&cameras).unwrap();
        ui.worker_lost();
        assert_eq!(*ui.stage(), RestoreStage::Idle);
        assert!(ui.notice.contains("Nothing was written"));

        let (mut ui, _) = ui_with_plan();
        ui.ask_apply();
        ui.confirm_apply().unwrap();
        ui.worker_lost();
        let RestoreStage::Done(done) = ui.stage() else { panic!("{:?}", ui.stage()) };
        assert!(matches!(done.outcome, ApplyOutcome::Indeterminate { .. }), "{:?}", done.outcome);
        assert!(applied_status_of("Restore", &done.outcome).text.contains("may have"), "said as unknown");

        // the command never reached the worker: then the camera was not asked
        let (mut ui, _) = ui_with_plan();
        ui.ask_apply();
        ui.confirm_apply().unwrap();
        ui.not_sent();
        let RestoreStage::Done(done) = ui.stage() else { panic!() };
        assert!(matches!(done.outcome, ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, .. }));
        // a plan being reviewed is untouched by a dead worker (nothing is in flight)
        let (mut ui, _) = ui_with_plan();
        ui.worker_lost();
        assert!(matches!(ui.stage(), RestoreStage::Planned(_)));
    }

    // ── narrowing the restore ────────────────────────────────────────────

    #[test]
    fn a_selection_starts_as_every_changing_property_and_only_those_can_be_ticked() {
        let (mut ui, _) = ui_with_plan();
        let RestoreStage::Planned(p) = ui.stage() else { panic!() };
        assert_eq!(p.selected.len(), 3);
        assert!(!p.narrowed(), "everything ticked is not a narrower restore");
        let first = p.changing()[0].name.clone();
        ui.toggle(&first);
        let RestoreStage::Planned(p) = ui.stage() else { panic!() };
        assert_eq!(p.selected.len(), 2);
        assert!(p.narrowed());
        ui.toggle("kNkMAIDCapability_NotInThePlan");
        let RestoreStage::Planned(p) = ui.stage() else { panic!() };
        assert_eq!(p.selected.len(), 2, "a property the plan does not change cannot be ticked");
    }

    #[test]
    fn narrowing_at_none_one_and_all_ticked() {
        let mk = |same: &[usize]| {
            let (mut ui, cameras) = ui_ready();
            ui.sync(&cameras);
            let Cmd::RestorePlan { request } = ui.begin_plan(&cameras).unwrap() else { panic!() };
            ui.plan_arrived(request, Ok((plan_of(&world_with(same, &[], None)), String::new(), SourceView::default())));
            (ui, cameras)
        };
        let (mut ui, cameras) = mk(&[]);
        let names: Vec<String> = ui.selected_for_test();
        for n in &names {
            ui.toggle(n);
        }
        let RestoreStage::Planned(none) = ui.stage() else { panic!() };
        assert!(none.selected.is_empty() && !none.narrowed(), "nothing ticked is not a narrower restore");
        assert!(ui.plan_selection(&cameras).is_err(), "nothing ticked is not a restore of everything");
        assert!(matches!(ui.stage(), RestoreStage::Planned(_)) && ui.only().is_none(), "and nothing was changed");
        ui.toggle(&names[1]);
        let RestoreStage::Planned(p) = ui.stage() else { panic!() };
        assert!(p.narrowed(), "one of three");
        let Ok(Cmd::RestorePlan { request }) = ui.plan_selection(&cameras) else { panic!("a new plan") };
        assert_eq!(request.only, Some(BTreeSet::from([names[1].clone()])));
        assert!(matches!(ui.stage(), RestoreStage::Planning(_)));
        // two of three, then everything ticked again
        let (mut ui, _) = mk(&[]);
        ui.toggle(&names[0]);
        let RestoreStage::Planned(p) = ui.stage() else { panic!() };
        assert!(p.narrowed());
        ui.toggle(&names[0]);
        let RestoreStage::Planned(p) = ui.stage() else { panic!() };
        assert!(!p.narrowed());
    }

    impl RestoreUi {
        fn selected_for_test(&self) -> Vec<String> {
            match &self.stage {
                RestoreStage::Planned(p) => p.selected.iter().cloned().collect(),
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
        let Cmd::RestorePlan { request } = ui.begin_plan(&cameras).unwrap() else { panic!() };
        ui.plan_arrived(request, Ok((plan_of(&world()), String::new(), SourceView::default())));
        ui.only = Some(BTreeSet::from(["kNkMAIDCapability_A".to_string()]));
        let Ok(Cmd::RestorePlan { request }) = ui.plan_everything(&cameras) else { panic!() };
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
        let text = restore_confirm_text(&PlannedRestore::new(req, limited, SourceView::default(), String::new()));
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
        let offer = undo_offer(&target(), &verified).expect("a verified apply can be undone");
        assert!(offer.restore_point.ends_with(".json") && !offer.undo_set.is_empty(), "{offer:?}");
        assert_eq!(offer.target, target());
        // a failed apply may have written some: its undo is offered too
        assert!(undo_offer(&target(), &report(&apply_text(&world_with(&[], &[], Some(1)), None).0)).is_some());
        // nothing was changed: no restore point, nothing to put back
        assert!(undo_offer(&target(), &report(&apply_text(&world_with(&[0, 1, 2], &[], None), None).0)).is_none());
        // an answer without a report names no restore point
        for outcome in [
            ApplyOutcome::<RestoreApplyJson>::Indeterminate { message: "m".into(), cli_log: String::new() },
            ApplyOutcome::<RestoreApplyJson>::NotApplied { why: NotAppliedWhy::Refused, message: "m".into(), cli_log: String::new() },
        ] {
            assert!(undo_offer(&target(), &outcome).is_none(), "{outcome:?}");
        }
        // an empty undo set (nothing the restore point can put back) offers nothing: the boundary is one name
        let ApplyOutcome::Report { report: mut r, .. } = verified else { panic!() };
        r.undo_set = vec!["kNkMAIDCapability_A".into()];
        let one = ApplyOutcome::Report { report: r.clone(), note: None, cli_log: String::new() };
        assert!(undo_offer(&target(), &one).is_some());
        r.undo_set.clear();
        let none = ApplyOutcome::Report { report: r.clone(), note: None, cli_log: String::new() };
        assert!(undo_offer(&target(), &none).is_none());
        // a status that says nothing was written offers none even with a restore point named
        r.undo_set = vec!["kNkMAIDCapability_A".into()];
        r.status = ApplyStatus::NotApplied;
        assert!(undo_offer(&target(), &ApplyOutcome::Report { report: r, note: None, cli_log: String::new() }).is_none());
    }

    #[test]
    fn a_transplants_apply_offers_the_same_undo() {
        let (json, status) = library_apply_json("failed");
        assert_eq!(status, ApplyStatus::Failed);
        let outcome = ApplyOutcome::Report { report: Box::new(serde_json::from_str::<ApplyJson>(&json).unwrap()), note: None, cli_log: String::new() };
        let offer = undo_offer(&target(), &outcome).expect("a failed transplant may have written");
        assert!(!offer.undo_set.is_empty());
    }

    #[test]
    fn an_undo_opens_the_restore_point_limited_to_the_undo_set_and_plans_it_without_writing() {
        let offer = UndoOffer {
            restore_point: "2026-10-27T172600Z_Z_9_T1_pre-restore.json".into(),
            undo_set: vec!["kNkMAIDCapability_B".into(), "kNkMAIDCapability_A".into()],
            target: target(),
        };
        let mut ui = RestoreUi::default();
        let cameras = vec![target()];
        let Ok(Cmd::RestorePlan { request }) = ui.start_undo(&offer, &cameras) else { panic!("a plan") };
        assert_eq!(request.source_file, offer.restore_point);
        assert_eq!(request.only, Some(BTreeSet::from(["kNkMAIDCapability_A".to_string(), "kNkMAIDCapability_B".to_string()])));
        assert_eq!(request.target, target());
        assert!(ui.open && matches!(ui.stage(), RestoreStage::Planning(_)));
        // the next frame does not take the selection away: it was made for this snapshot and body
        ui.sync(&cameras);
        assert!(ui.only().is_some() && matches!(ui.stage(), RestoreStage::Planning(_)));
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
        assert!(matches!(busy.stage(), RestoreStage::Applying(_)), "and the one in flight is left alone");
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
        let Ok(Cmd::RestorePlan { request }) = cmd_rx.try_recv() else { panic!("a plan") };
        assert!(app.busy());
        let plan = plan_of(&world());
        evt_tx.send(Evt::RestorePlanned { request: request.clone(), result: Ok((plan.clone(), String::new(), SourceView::default())) }).unwrap();
        app.poll();
        assert!(app.status.text.starts_with("Restore plan:"), "{}", app.status.text);
        // applying without having been asked the question sends nothing
        app.restore_act(&ctx, RestoreAction::ConfirmApply);
        assert!(cmd_rx.try_recv().is_err());
        app.restore_act(&ctx, RestoreAction::AskApply);
        assert!(cmd_rx.try_recv().is_err(), "asking is not applying");
        app.restore_act(&ctx, RestoreAction::ConfirmApply);
        let Ok(Cmd::RestoreApply { request: sent, digest, summary }) = cmd_rx.try_recv() else { panic!("an apply") };
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
        evt_tx.send(Evt::RestoreApplied { request: sent, outcome }).unwrap();
        app.poll();
        assert_eq!(app.status.level, Level::Error);
        assert!(app.status.text.starts_with("Restore failed:") && app.status.text.contains("may have landed"), "{}", app.status.text);
        assert!(matches!(cmd_rx.try_recv(), Ok(Cmd::RefreshListing)), "the restore point is a new file");
        assert!(matches!(app.restore.stage(), RestoreStage::Done(_)));
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
        let Some(Cmd::RestoreApply { request, .. }) = ui.confirm_apply() else { panic!() };
        app.restore = ui;
        app.pending = 1;
        let stale = ApplyOutcome::NotApplied { why: NotAppliedWhy::PlanStale, message: "stale".into(), cli_log: String::new() };
        evt_tx.send(Evt::RestoreApplied { request, outcome: stale }).unwrap();
        app.poll();
        assert_eq!(app.status.level, Level::Warn);
        assert!(app.status.text.contains("nothing was written") && app.status.text.contains("Plan again"), "{}", app.status.text);
        assert!(cmd_rx.try_recv().is_err(), "nothing was written, so nothing is re-listed");
        let _ = ctx;
    }

    #[test]
    fn a_transplants_undo_button_plans_the_undo_of_the_restore_point_it_made() {
        let (mut app, cmd_rx, _evt_tx, _dir) = test_app();
        let ctx = egui::Context::default();
        let (json, _) = library_apply_json("failed");
        let applied: ApplyJson = serde_json::from_str(&json).unwrap();
        let outcome = ApplyOutcome::Report { report: Box::new(applied.clone()), note: None, cli_log: String::new() };
        let offer = undo_offer(&target(), &outcome).expect("offered");
        app.transplant_act(&ctx, TransplantAction::Undo(offer));
        let Ok(Cmd::RestorePlan { request }) = cmd_rx.try_recv() else { panic!("a plan, not an apply") };
        assert_eq!(request.source_file, applied.restore_point.unwrap().file);
        assert_eq!(request.only, Some(applied.undo_set.iter().cloned().collect()));
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn the_undo_button_plans_through_the_worker_and_waits_while_the_camera_is_busy() {
        let (mut app, cmd_rx, _evt_tx, _dir) = test_app();
        let offer = UndoOffer { restore_point: "rp.json".into(), undo_set: vec!["kNkMAIDCapability_A".into()], target: target() };
        app.pending = 1;
        app.start_undo(offer.clone());
        assert!(cmd_rx.try_recv().is_err(), "a camera operation is running");
        assert_eq!(app.status.level, Level::Warn);
        app.pending = 0;
        app.start_undo(offer);
        let Ok(Cmd::RestorePlan { request }) = cmd_rx.try_recv() else { panic!("a plan") };
        assert_eq!(request.source_file, "rp.json");
        assert!(app.restore.open);
        assert!(cmd_rx.try_recv().is_err(), "a plan, not an apply: the person still confirms");
    }
}

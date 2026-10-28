//! Text for the CLI's `transplant` and `restore`, rendered from the `ops` values that the MCP
//! worker also returns (`TransplantOutcome`, `restore::Counts`). Pure, so the wording and the
//! counting are testable without a camera; `main.rs` decides where each part is printed.

use nikon_fleet::ops::apply::{ApplyReport, ApplyStatus, Readback};
use nikon_fleet::ops::restore::{Counts, StepOutcome, StepResult};
use nikon_fleet::ops::restore_flow::{PlanStep, RestoreApplyReport};
use nikon_fleet::transplant::{Severity, SkipReason, Strategy, TransplantOutcome};

/// What printing a transplant run produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TransplantText {
    /// Per-property lines and the summary block, for stdout (or stderr under `--json`).
    pub stdout: String,
    /// One line per warn- or error-severity skip, for stderr.
    pub warnings: Vec<String>,
    pub written: usize,
    pub errors: usize,
}

/// Render the outcomes of a dry run (`dry_run`, "would write" lines) or a real run.
pub fn render_transplant(outcomes: &[TransplantOutcome], dry_run: bool) -> TransplantText {
    let mut out = TransplantText::default();
    let (mut literal, mut enum_label, mut enum_raw, mut range) = (0usize, 0usize, 0usize, 0usize);
    let (mut absent, mut read_only, mut unsupported_type, mut no_match) = (0usize, 0usize, 0usize, 0usize);
    let (mut type_mismatch, mut sdk_decode, mut name_mismatch, mut ambiguous) = (0usize, 0usize, 0usize, 0usize);
    let (mut never, mut other) = (0usize, 0usize);
    let mut raw_unconfirmed: Vec<(String, String)> = Vec::new();
    let mut literal_unconfirmed: Vec<(String, String)> = Vec::new();

    for o in outcomes {
        match &o.result {
            Ok(strategy) => {
                if dry_run {
                    match &o.choice {
                        Some(c) => out.stdout.push_str(&format!("  would write  {} [{:#x}]  via {strategy}: {c}\n", o.name, o.code)),
                        None => out.stdout.push_str(&format!("  would write  {} [{:#x}]  via {strategy}\n", o.name, o.code)),
                    }
                }
                match strategy {
                    Strategy::Literal => literal += 1,
                    Strategy::EnumByLabel => enum_label += 1,
                    Strategy::EnumByRawValue => enum_raw += 1,
                    Strategy::RangeClamped => range += 1,
                }
            }
            Err(reason) => {
                match reason {
                    SkipReason::AbsentOnTarget => absent += 1,
                    SkipReason::ReadOnly => read_only += 1,
                    SkipReason::UnsupportedWriteType => unsupported_type += 1,
                    SkipReason::NoMatchingOption => no_match += 1,
                    SkipReason::WriteFailed(_) => out.errors += 1,
                    SkipReason::EnumTypeMismatch { .. } => type_mismatch += 1,
                    SkipReason::EnumUndecodedBySdk { .. } => sdk_decode += 1,
                    SkipReason::CapabilityNameMismatch { .. } => name_mismatch += 1,
                    SkipReason::AmbiguousLabel { .. } => ambiguous += 1,
                    // Policy skips, not problems: routine and numerous, so counted, not warned about one by one.
                    SkipReason::NeverCopied { .. } => never += 1,
                    SkipReason::RawCodeUnconfirmed { proposed } => raw_unconfirmed.push((o.name.clone(), proposed.clone())),
                    SkipReason::LiteralUnconfirmed { proposed } => literal_unconfirmed.push((o.name.clone(), proposed.clone())),
                    SkipReason::UnsupportedValueShape | SkipReason::TargetReadFailed(_) | SkipReason::MalformedSource => other += 1,
                }
                // Whether to warn is the reason's own severity, not a second table here.
                if matches!(reason.severity(), Severity::Warn | Severity::Error) {
                    out.warnings.push(format!("  warn: {} [{:#x}]: {reason}", o.name, o.code));
                }
            }
        }
    }

    out.written = literal + enum_label + enum_raw + range;
    out.stdout.push_str(&format!(
        "{} written={} (literal={literal} enum-by-label={enum_label} \
         enum-by-raw-value={enum_raw} range-clamped={range})\n  \
         skipped(absent)={absent} skipped(read-only)={read_only} \
         skipped(unsupported-type)={unsupported_type} skipped(no-matching-option)={no_match} \
         skipped(type-mismatch)={type_mismatch} skipped(sdk-decode-failure)={sdk_decode} \
         skipped(name-mismatch)={name_mismatch} skipped(ambiguous-label)={ambiguous} \
         skipped(never-copied)={never} skipped(raw-code-unconfirmed)={} \
         skipped(literal-unconfirmed)={} \
         skipped(other)={other}  errors={}\n",
        if dry_run { "Dry run:" } else { "Transplant complete:" },
        out.written,
        raw_unconfirmed.len(),
        literal_unconfirmed.len(),
        out.errors,
    ));
    if !raw_unconfirmed.is_empty() {
        out.stdout.push_str(&format!(
            "\n{} propert{} matched only by raw numeric code and would change the target; not written \
             (nothing guarantees both bodies number the same option the same way). Review, then opt in \
             per property with --allow-raw-code <name>:\n",
            raw_unconfirmed.len(),
            if raw_unconfirmed.len() == 1 { "y was" } else { "ies were" }
        ));
        for (name, proposed) in &raw_unconfirmed {
            out.stdout.push_str(&format!("  {name}: would set {proposed}\n"));
        }
    }
    if !literal_unconfirmed.is_empty() {
        out.stdout.push_str(&format!(
            "\n{} numeric propert{} copied as-is would change the target; not written \
             (an integer is no more self-describing than a raw code, so nothing says both bodies \
             give it the same meaning). Review, then opt in per property with --allow-raw-code <name>:\n",
            literal_unconfirmed.len(),
            if literal_unconfirmed.len() == 1 { "y" } else { "ies" }
        ));
        for (name, proposed) in &literal_unconfirmed {
            out.stdout.push_str(&format!("  {name}: would set {proposed}\n"));
        }
    }
    out
}

/// The `would write` lines of a restore plan, then the tally line.
pub fn render_restore_dry_run(steps: &[PlanStep], counts: &Counts, only_selected: bool) -> String {
    let mut s = String::new();
    for step in steps {
        if step.outcome == StepOutcome::WouldWrite {
            s.push_str(&format!("  would write  {} [{:#x}]\n", step.name, step.code));
        }
    }
    s.push_str(&restore_tally("Dry run:", counts, only_selected));
    s
}

/// The tally line shared by a dry run and a real restore.
pub fn restore_tally(heading: &str, c: &Counts, only_selected: bool) -> String {
    let Counts { written, skipped_absent, skipped_read_only, skipped_type, skipped_never, not_selected, errors } = *c;
    let not_selected_note = if only_selected { format!("  skipped(not-selected)={not_selected}") } else { String::new() };
    format!(
        "{heading} written={written}  skipped(absent)={skipped_absent}  \
         skipped(read-only)={skipped_read_only}  \
         skipped(unsupported-type)={skipped_type}  skipped(never-copied)={skipped_never}{not_selected_note}  errors={errors}\n"
    )
}

/// One warning per failed write of a restore run.
pub fn restore_write_warnings(steps: &[StepResult]) -> Vec<String> {
    steps
        .iter()
        .filter_map(|s| match &s.outcome {
            StepOutcome::WriteFailed(msg) => Some(format!("  warn: {} [{:#x}]: {msg}", s.name, s.code)),
            _ => None,
        })
        .collect()
}

/// How many names an undo command line may carry before it is replaced by a pointer to `--json`.
const MAX_UNDO_NAMES: usize = 12;

/// The command that undoes an apply: a targeted restore of its restore point.
pub fn undo_command(restore_point_file: &str, undo_set: &[String]) -> String {
    if undo_set.is_empty() {
        return "nothing to undo".into();
    }
    if undo_set.len() > MAX_UNDO_NAMES {
        return format!(
            "{} properties to put back from {restore_point_file}; list them with --json (undo_set) and pass each as --only",
            undo_set.len()
        );
    }
    let only: String = undo_set.iter().map(|n| format!(" --only {}", n.trim_start_matches("kNkMAIDCapability_"))).collect();
    format!("nikon-fleet restore {restore_point_file}{only}")
}

fn status_word(s: ApplyStatus) -> &'static str {
    match s {
        ApplyStatus::AppliedVerified => "applied_verified",
        ApplyStatus::AppliedUnverified => "applied_unverified",
        ApplyStatus::Failed => "failed",
        ApplyStatus::Indeterminate => "indeterminate",
    }
}

/// What a finished apply says about itself, and whether the command must exit non-zero.
#[derive(Debug, PartialEq, Eq)]
pub struct ApplyText {
    pub stdout: String,
    pub failing: Option<String>,
}

fn readback_lines(rb: &Readback, s: &mut String) {
    if !rb.not_taken.is_empty() {
        s.push_str(&format!("Not taken: {} not read back as written: {}\n", rb.not_taken.len(), rb.not_taken.join(", ")));
    }
    if !rb.unverifiable.is_empty() {
        s.push_str(&format!("Unverifiable: {} could not be read back: {}\n", rb.unverifiable.len(), rb.unverifiable.join(", ")));
    }
    match &rb.side_effects {
        Some(se) if !se.is_empty() => {
            let mut names: Vec<String> = se.changed.iter().map(|n| format!("{n} (changed)")).collect();
            names.extend(se.vanished.iter().map(|n| format!("{n} (vanished)")));
            names.extend(se.appeared.iter().map(|n| format!("{n} (appeared)")));
            s.push_str(&format!("Side effects: {}\n", names.join(", ")));
        }
        Some(_) => {}
        None => s.push_str("Side effects: unknown (the read-back was not complete enough to compare)\n"),
    }
}

/// The closing report of `transplant`: restore point, read-back, undo.
pub fn render_transplant_apply(r: &ApplyReport) -> ApplyText {
    let mut s = String::new();
    if r.no_op {
        s.push_str("Nothing to write; no restore point was taken.\n");
        return ApplyText { stdout: s, failing: None };
    }
    if let Some(rp) = &r.restore_point {
        s.push_str(&format!("Restore point: {}\n", rp.file));
    }
    if r.restore_point_gate_waived {
        s.push_str("note: the restore point did not pass the model's completeness check (or the model has no baseline); continuing because --allow-partial was given.\n");
    }
    s.push_str(&format!("Status: {}\n", status_word(r.status)));
    if let Some(rb) = &r.readback {
        readback_lines(rb, &mut s);
    }
    if let Some(rp) = &r.restore_point {
        s.push_str(&format!("To undo: {}\n", undo_command(&rp.file, &r.undo_set)));
    }
    let not_taken = r.readback.as_ref().map(|rb| rb.not_taken.len()).unwrap_or(0);
    let failing = if r.status == ApplyStatus::Failed {
        Some(format!("{} capability write(s) failed (see warnings above)", r.write_errors.len()))
    } else if not_taken > 0 {
        Some("the transplant did not take effect as reported; see the read-back above".to_string())
    } else {
        None
    };
    ApplyText { stdout: s, failing }
}

/// The closing report of `restore` after the tally line: restore point and status.
/// The read-back line itself is rendered by the caller.
pub fn render_restore_apply(r: &RestoreApplyReport) -> String {
    let mut s = String::new();
    if r.no_op {
        s.push_str("Nothing to write; every selected property already holds the stored value. No restore point was taken.\n");
        return s;
    }
    if let Some(rp) = &r.restore_point {
        s.push_str(&format!("Restore point: {}\n", rp.file));
    }
    if r.restore_point_gate_waived {
        s.push_str("note: the restore point did not pass the model's completeness check (or the model has no baseline); continuing because --allow-partial was given.\n");
    }
    s.push_str(&format!("Status: {}\n", status_word(r.status)));
    if let Some(se) = &r.side_effects {
        if !se.is_empty() {
            let mut names: Vec<String> = se.changed.iter().map(|n| format!("{n} (changed)")).collect();
            names.extend(se.vanished.iter().map(|n| format!("{n} (vanished)")));
            names.extend(se.appeared.iter().map(|n| format!("{n} (appeared)")));
            s.push_str(&format!("Side effects: {}\n", names.join(", ")));
        }
    }
    if let Some(rp) = &r.restore_point {
        s.push_str(&format!("To undo: {}\n", undo_command(&rp.file, &r.undo_set)));
    }
    s
}

/// The error for a targeted restore that did not write everything it was
/// asked to: `named` is how many properties `--only` selected (`None` for a
/// full restore, which skips many properties routinely and never fails for
/// that), `unrestored` each named property that was not written (or, in a dry
/// run, would not be) with a closed reason tag. `None` when everything named
/// was (or would be) written.
pub fn unrestored_error(named: Option<usize>, unrestored: &[(String, &'static str)]) -> Option<String> {
    let named = named?;
    if unrestored.is_empty() {
        return None;
    }
    let list: Vec<String> = unrestored.iter().map(|(n, why)| format!("{n} ({why})")).collect();
    Some(format!(
        "targeted restore incomplete: {} of {named} named propert{} not written: {}",
        unrestored.len(),
        if unrestored.len() == 1 { "y was" } else { "ies were" },
        list.join(", ")
    ))
}

/// Why a finished restore must make the command exit non-zero, if it must: a rejected write, a
/// named property that was not written, or a read-back that found the camera did not keep a write.
pub fn restore_failure(r: &RestoreApplyReport, named: Option<usize>, readback_failed: bool) -> Option<String> {
    if !r.write_errors.is_empty() {
        return Some(format!(
            "{} capability write(s) failed (see warnings above); the rest were written",
            r.write_errors.len()
        ));
    }
    if let Some(msg) = unrestored_error(named, &r.unrestored) {
        return Some(msg);
    }
    readback_failed.then(|| "the restore did not take effect as reported; see the read-back above".to_string())
}

/// A hint to append to an apply refusal, keyed by its closed tag.
pub fn refusal_hint(tag: &str) -> Option<&'static str> {
    match tag {
        "no_baseline" | "restore_point_incomplete" => Some(
            "Pass --allow-partial to go ahead anyway: the restore point is still taken and the read-back is still compared, \
             but the camera's state could not be judged complete.",
        ),
        "data_dir_full" => Some("Clear old mcp-restore-* snapshots from the snapshots directory, then retry."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nikon_fleet::transplant::ResolvedChoice;

    fn copied(name: &str, code: u32, s: Strategy) -> TransplantOutcome {
        TransplantOutcome { name: name.into(), code, result: Ok(s), choice: None, planned: None }
    }
    fn skipped(name: &str, code: u32, r: SkipReason) -> TransplantOutcome {
        TransplantOutcome { name: name.into(), code, result: Err(r), choice: None, planned: None }
    }

    fn text(o: &[TransplantOutcome], dry: bool) -> TransplantText {
        render_transplant(o, dry)
    }

    #[test]
    fn written_counts_each_strategy_and_a_dry_run_lists_would_write_lines() {
        let o = vec![
            copied("A", 1, Strategy::Literal),
            copied("B", 2, Strategy::EnumByLabel),
            copied("C", 3, Strategy::EnumByLabel),
            copied("D", 4, Strategy::EnumByRawValue),
            copied("E", 5, Strategy::RangeClamped),
        ];
        let dry = text(&o, true);
        assert_eq!(dry.written, 5);
        assert!(dry.stdout.contains("  would write  A [0x1]  via "));
        assert!(dry.stdout.contains("Dry run: written=5 (literal=1 enum-by-label=2 enum-by-raw-value=1 range-clamped=1)"));
        let real = text(&o, false);
        assert!(!real.stdout.contains("would write"), "a real run lists nothing per property");
        assert!(real.stdout.starts_with("Transplant complete: written=5"));
    }

    #[test]
    fn an_empty_run_is_all_zeros() {
        let t = text(&[], false);
        assert_eq!((t.written, t.errors), (0, 0));
        assert!(t.warnings.is_empty());
        assert!(t.stdout.contains("written=0") && t.stdout.contains("errors=0"));
        assert!(!t.stdout.contains("Review"), "no opt-in blocks without unconfirmed properties");
    }

    #[test]
    fn write_failures_are_errors_and_warned_but_routine_skips_are_neither() {
        let o = vec![
            skipped("W", 1, SkipReason::WriteFailed("SDK error -125".into())),
            skipped("R", 2, SkipReason::ReadOnly),
            skipped("A", 3, SkipReason::AbsentOnTarget),
            skipped("N", 4, SkipReason::NoMatchingOption),
        ];
        let t = text(&o, false);
        assert_eq!(t.errors, 1);
        assert_eq!(t.written, 0);
        assert_eq!(t.warnings.len(), 2, "{:?}", t.warnings);
        assert!(t.warnings.iter().any(|w| w.contains("W [0x1]") && w.contains("-125")));
        assert!(t.warnings.iter().all(|w| !w.contains("R [0x2]") && !w.contains("A [0x3]")));
        assert!(t.stdout.contains("skipped(absent)=1") && t.stdout.contains("skipped(read-only)=1") && t.stdout.contains("skipped(no-matching-option)=1"));
    }

    #[test]
    fn unconfirmed_opt_ins_are_listed_with_singular_and_plural_wording() {
        let raw = |n: &str, c| skipped(n, c, SkipReason::RawCodeUnconfirmed { proposed: "code 3".into() });
        let lit = |n: &str, c| skipped(n, c, SkipReason::LiteralUnconfirmed { proposed: "7".into() });
        let one = text(&[raw("MeteringMode", 1), lit("WBAutoType", 2)], true);
        assert!(one.stdout.contains("1 property was matched only by raw numeric code"), "{}", one.stdout);
        assert!(one.stdout.contains("  MeteringMode: would set code 3"));
        assert!(one.stdout.contains("1 numeric property copied as-is"));
        assert!(one.stdout.contains("skipped(raw-code-unconfirmed)=1") && one.stdout.contains("skipped(literal-unconfirmed)=1"));
        let two = text(&[raw("A", 1), raw("B", 2), lit("C", 3), lit("D", 4)], true);
        assert!(two.stdout.contains("2 properties were matched only by raw numeric code"));
        assert!(two.stdout.contains("2 numeric properties copied as-is"));
        assert_eq!(two.written, 0, "unconfirmed properties are not written");
    }

    #[test]
    fn a_choice_is_shown_on_the_would_write_line() {
        let mut o = copied("E", 9, Strategy::EnumByLabel);
        o.choice = Some(ResolvedChoice { target_index: 2, choice: "Auto".into(), current_index: Some(1), current: Some("Manual".into()) });
        let t = text(&[o], true);
        let first = t.stdout.lines().next().unwrap();
        assert!(first.contains("via ") && first.contains(": "), "{first}");
    }

    #[test]
    fn restore_tally_mentions_not_selected_only_for_a_targeted_restore() {
        let c = Counts { written: 2, not_selected: 190, errors: 1, ..Counts::default() };
        assert!(restore_tally("Restore complete:", &c, true).contains("skipped(not-selected)=190"));
        assert!(!restore_tally("Restore complete:", &c, false).contains("not-selected"));
        assert!(restore_tally("Dry run:", &c, false).starts_with("Dry run: written=2"));
    }

    #[test]
    fn restore_dry_run_lists_only_the_would_write_steps() {
        let step = |n: &str, o: StepOutcome| PlanStep { name: n.into(), code: 16, outcome: o, changes: Some(true) };
        let steps = vec![step("A", StepOutcome::WouldWrite), step("B", StepOutcome::Written), step("C", StepOutcome::WouldWrite)];
        let s = render_restore_dry_run(&steps, &Counts { written: 2, ..Counts::default() }, false);
        assert_eq!(s.lines().filter(|l| l.contains("would write")).count(), 2);
        assert!(s.contains("  would write  A [0x10]") && !s.contains("B ["));
    }

    #[test]
    fn restore_write_failures_are_warned_one_per_failed_step() {
        let step = |n: &str, o: StepOutcome| StepResult { name: n.into(), code: 255, outcome: o };
        let w = restore_write_warnings(&[step("A", StepOutcome::Written), step("B", StepOutcome::WriteFailed("boom".into())), step("C", StepOutcome::UnsupportedWriteType)]);
        assert_eq!(w, vec!["  warn: B [0xff]: boom".to_string()]);
    }

    fn apply_report(status: ApplyStatus, written: &[&str], rb: Option<Readback>) -> ApplyReport {
        ApplyReport {
            status,
            no_op: false,
            plan_digest: "d".into(),
            restore_point: Some(nikon_fleet::ops::apply::RestorePoint { file: "rp.json".into(), label: "mcp-restore-x".into() }),
            restore_point_gate_waived: false,
            written: written.iter().map(|s| s.to_string()).collect(),
            write_errors: vec![],
            outcomes: vec![],
            readback: rb,
            undo_set: written.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn readback(not_taken: &[&str], unverifiable: &[&str]) -> Readback {
        Readback {
            gate: None,
            stats: Default::default(),
            diff: None,
            side_effects: Some(Default::default()),
            not_taken: not_taken.iter().map(|s| s.to_string()).collect(),
            unverifiable: unverifiable.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_verified_transplant_names_its_restore_point_and_how_to_undo_it() {
        let t = render_transplant_apply(&apply_report(ApplyStatus::AppliedVerified, &["kNkMAIDCapability_WBTuneAuto"], Some(readback(&[], &[]))));
        assert!(t.failing.is_none());
        assert!(t.stdout.contains("Restore point: rp.json") && t.stdout.contains("Status: applied_verified"));
        assert!(t.stdout.contains("To undo: nikon-fleet restore rp.json --only WBTuneAuto"), "{}", t.stdout);
    }

    #[test]
    fn a_property_that_did_not_take_fails_the_command_but_unverifiable_alone_does_not() {
        let bad = render_transplant_apply(&apply_report(ApplyStatus::AppliedUnverified, &["A"], Some(readback(&["A"], &[]))));
        assert!(bad.failing.as_deref().unwrap().contains("did not take effect"));
        assert!(bad.stdout.contains("Not taken: 1 not read back as written: A"));
        let soft = render_transplant_apply(&apply_report(ApplyStatus::AppliedUnverified, &["A"], Some(readback(&[], &["A"]))));
        assert!(soft.failing.is_none(), "an incomplete read-back claims nothing either way");
        assert!(soft.stdout.contains("Unverifiable: 1"));
    }

    #[test]
    fn a_failed_status_fails_the_command_with_the_error_count() {
        let mut r = apply_report(ApplyStatus::Failed, &["A"], Some(readback(&[], &[])));
        r.write_errors = vec![nikon_fleet::ops::apply::WriteError { name: "B".into(), message: "m".into() }];
        assert_eq!(render_transplant_apply(&r).failing.as_deref(), Some("1 capability write(s) failed (see warnings above)"));
    }

    #[test]
    fn a_no_op_apply_takes_no_restore_point_and_does_not_fail() {
        let mut r = apply_report(ApplyStatus::AppliedVerified, &[], None);
        r.no_op = true;
        r.restore_point = None;
        let t = render_transplant_apply(&r);
        assert!(t.failing.is_none() && t.stdout.contains("Nothing to write") && !t.stdout.contains("Restore point:"));
    }

    #[test]
    fn a_waived_gate_is_said_out_loud() {
        let mut r = apply_report(ApplyStatus::AppliedUnverified, &["A"], Some(readback(&[], &[])));
        r.restore_point_gate_waived = true;
        assert!(render_transplant_apply(&r).stdout.contains("continuing because --allow-partial was given"));
    }

    #[test]
    fn unknown_side_effects_are_not_reported_as_none() {
        let mut rb = readback(&[], &[]);
        rb.side_effects = None;
        let t = render_transplant_apply(&apply_report(ApplyStatus::AppliedUnverified, &["A"], Some(rb)));
        assert!(t.stdout.contains("Side effects: unknown"));
    }

    #[test]
    fn the_undo_command_strips_the_prefix_and_gives_up_past_the_limit() {
        let names = |n: usize| (0..n).map(|i| format!("kNkMAIDCapability_P{i}")).collect::<Vec<_>>();
        assert_eq!(undo_command("rp.json", &[]), "nothing to undo");
        assert_eq!(undo_command("rp.json", &names(1)), "nikon-fleet restore rp.json --only P0");
        assert!(undo_command("rp.json", &names(MAX_UNDO_NAMES)).matches("--only").count() == MAX_UNDO_NAMES);
        let over = undo_command("rp.json", &names(MAX_UNDO_NAMES + 1));
        assert!(over.contains("13 properties") && over.contains("--json"), "{over}");
        assert!(!over.contains("--only P0"));
    }

    #[test]
    fn refusal_hints_exist_only_for_the_refusals_allow_partial_can_help_with() {
        assert!(refusal_hint("no_baseline").unwrap().contains("--allow-partial"));
        assert!(refusal_hint("restore_point_incomplete").unwrap().contains("--allow-partial"));
        assert!(refusal_hint("stale_plan").is_none());
        assert!(refusal_hint("nothing_readable").is_none(), "--allow-partial does not help when nothing could be read");
        assert!(refusal_hint("data_dir_full").is_some());
    }

    fn restore_report() -> RestoreApplyReport {
        RestoreApplyReport {
            status: ApplyStatus::AppliedVerified,
            no_op: false,
            plan_digest: "d".into(),
            restore_point: None,
            restore_point_gate_waived: false,
            written: vec![],
            write_errors: vec![],
            unrestored: vec![],
            counts: Counts::default(),
            steps: vec![],
            verification: None,
            side_effects: None,
            undo_set: vec![],
        }
    }

    #[test]
    fn a_restore_fails_the_command_for_a_rejected_write_an_unwritten_named_property_or_a_failed_read_back_only() {
        let ok = restore_report();
        assert_eq!(restore_failure(&ok, None, false), None);
        assert_eq!(restore_failure(&ok, Some(2), false), None, "everything named was written");
        let mut w = restore_report();
        w.write_errors = vec![nikon_fleet::ops::apply::WriteError { name: "A".into(), message: "m".into() }];
        assert!(restore_failure(&w, None, false).unwrap().contains("1 capability write(s) failed"));
        let mut u = restore_report();
        u.unrestored = vec![("B".into(), "read_only")];
        assert!(restore_failure(&u, Some(2), false).unwrap().contains("targeted restore incomplete: 1 of 2"));
        assert_eq!(restore_failure(&u, None, false), None, "a full restore never fails for routine skips");
        assert!(restore_failure(&ok, None, true).unwrap().contains("did not take effect"));
    }

    #[test]
    fn unrestored_error_pluralises_and_lists_reasons() {
        assert_eq!(unrestored_error(None, &[("A".into(), "absent_on_target")]), None);
        assert_eq!(unrestored_error(Some(3), &[]), None);
        let one = unrestored_error(Some(3), &[("A".into(), "read_only")]).unwrap();
        assert_eq!(one, "targeted restore incomplete: 1 of 3 named property was not written: A (read_only)");
        let two = unrestored_error(Some(3), &[("A".into(), "read_only"), ("B".into(), "absent_on_target")]).unwrap();
        assert!(two.contains("2 of 3 named properties were not written") && two.contains("A (read_only), B (absent_on_target)"));
    }
}

//! Device-free parts of `restore`: which snapshot properties a restore touches.
//!
//! `restore` writes every snapshotted property that is not on the never-copy
//! table, so anything changed on the camera after the snapshot was taken is
//! overwritten. A snapshot of *another* body (the CLI's `restore --serial` onto a second
//! body of one model) also leaves out the settings that read as a different position per
//! body ([`transplant::NEVER_TRANSLATED`], i.e. `ShootingLimit`); see [`Source`]. A range
//! is written back only onto the scale it was read on. A targeted restore (`--only`) writes back just the named
//! properties and leaves everything else alone; that is the undo of a
//! transplant (docs/mcp-plan.md §5.6): restore the properties the apply wrote
//! (and the ones it changed indirectly) from the restore point, and nothing
//! the user changed since.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Value, json};

use crate::ops::baseline::{GateOutcome, GatePolicy, gate};
use crate::ops::capture::{CaptureStats, capture};
use crate::ops::port::CameraPort;
use crate::ops::readback::{Residual, residual};
use crate::sdk::{CapabilityInfo, OP_SET, SdkError};
use crate::snapshot::{Camera, PropertyEntry, Snapshot, Transport};
use crate::transplant::{self, MAX_ERROR_MESSAGE_LEN, truncate_with_marker};
use crate::value_shape::{is_range_shape, is_stepped_range, range_scale, range_value};

/// Why a snapshot property is not written back by `restore`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreSkip {
    /// On [`transplant::NEVER_TRANSPLANT`] (it reports body/session state, or locks it, rather
    /// than holding a setting), or, for a snapshot of another body ([`Source::OtherBody`]), on
    /// [`transplant::NEVER_TRANSLATED`] (a setting that reads as a different position per body).
    /// The same tables `transplant` uses, so the two commands cannot disagree about what is state. A
    /// restore point must not generate errors for properties the camera
    /// refuses to set (`CurrentItemID` and `LiveViewStatus` failed -125 on
    /// every full restore of a Z6III), or `errors == 0` stops meaning anything.
    NeverCopied,
    /// Capability code not present on this camera.
    Absent,
    /// Present but `OP_SET` is not allowed.
    ReadOnly,
    /// A range whose scale on the body now differs from the one the snapshot read it on (a
    /// firmware change, or a setting such as the EV step that rescales it). Its stored position
    /// would mean a different value on the new scale, so it is left alone, as a transplant does.
    RangeScaleDiffers,
}

impl RestoreSkip {
    /// Closed machine-readable tag. Exhaustive match.
    pub fn tag(&self) -> &'static str {
        match self {
            RestoreSkip::NeverCopied => "never_copied",
            RestoreSkip::Absent => "absent_on_target",
            RestoreSkip::ReadOnly => "read_only",
            RestoreSkip::RangeScaleDiffers => "range_scale_differs",
        }
    }

    /// Every skip reason, for tests and for anything that must name them all.
    pub const ALL: [RestoreSkip; 4] =
        [RestoreSkip::NeverCopied, RestoreSkip::Absent, RestoreSkip::ReadOnly, RestoreSkip::RangeScaleDiffers];

    /// The skip a [`tag`](Self::tag) names, or `None` for a string that is not one (a tag from
    /// another build, an outcome that is not a skip, empty text).
    pub fn from_tag(tag: &str) -> Option<RestoreSkip> {
        Self::ALL.into_iter().find(|s| s.tag() == tag)
    }

    /// Whether the skip is part of how a restore normally goes, so a front end may list it as
    /// "skipped as a matter of course": the body does not hold or take the setting, and there is
    /// nothing for the person to do. `RangeScaleDiffers` is not: a setting the person asked to put
    /// back was deliberately left alone because its meaning moved, and they should see that. The one
    /// place this is decided; a new reason has to choose here (the match names every variant).
    pub fn is_routine(&self) -> bool {
        match self {
            RestoreSkip::NeverCopied | RestoreSkip::Absent | RestoreSkip::ReadOnly => true,
            RestoreSkip::RangeScaleDiffers => false,
        }
    }
}

/// Whose settings a restore is carrying: the body's own, or another body's.
///
/// [`transplant::NEVER_TRANSLATED`] settings read as different positions on different bodies, so
/// they come back only when the snapshot is of the very body being restored. A snapshot of another
/// body of the same model (the CLI's `restore --serial`) carries nothing the target can interpret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    SameBody,
    OtherBody,
}

impl Source {
    /// A snapshot of `snapshot_serial` restored onto `target_serial`. A serial that does not
    /// identify a body (empty, or the `id-N` fallback the CLI uses when no USB descriptor was
    /// read) cannot show that two records are of one body, so it is `OtherBody`: the settings
    /// that read differently per body are then left alone rather than written.
    pub fn of(snapshot_serial: &str, target_serial: &str) -> Source {
        let identifies = |s: &str| !s.is_empty() && !crate::ops::target::is_id_fallback(s);
        if identifies(snapshot_serial) && identifies(target_serial) && snapshot_serial == target_serial {
            Source::SameBody
        } else {
            Source::OtherBody
        }
    }
}

/// The device-free checks `restore` applies before writing a property, in
/// order: the policy skip first (so a state property is reported as such even
/// on a body that lacks it), then presence, then writability. For a snapshot of
/// the body itself; see [`restore_precheck_from`]. Tests only: a production caller must say whose
/// snapshot it is restoring, never get `SameBody` by default.
#[cfg(test)]
pub fn restore_precheck<'a>(
    cap_map: &HashMap<u32, &'a CapabilityInfo>,
    code: u32,
) -> Result<&'a CapabilityInfo, RestoreSkip> {
    restore_precheck_from(cap_map, code, Source::SameBody)
}

/// [`restore_precheck`] for a snapshot of `source`'s body.
pub fn restore_precheck_from<'a>(
    cap_map: &HashMap<u32, &'a CapabilityInfo>,
    code: u32,
    source: Source,
) -> Result<&'a CapabilityInfo, RestoreSkip> {
    if transplant::never_skip(code, source == Source::OtherBody).is_some() {
        return Err(RestoreSkip::NeverCopied);
    }
    let cap = *cap_map.get(&code).ok_or(RestoreSkip::Absent)?;
    if cap.operations & OP_SET == 0 {
        return Err(RestoreSkip::ReadOnly);
    }
    Ok(cap)
}

/// What an undo can put back, and what it cannot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UndoSet {
    /// Sorted names a targeted restore of the restore point accepts as `only` and writes.
    pub names: Vec<String>,
    /// Sorted names the apply touched or changed that an undo cannot restore: absent from the
    /// restore point (they appeared only after the write), read-only or state-like on the body,
    /// or never copied. Passing them as `only` would refuse the whole undo or fail it.
    pub not_restorable: Vec<String>,
}

/// The `only` for undoing an apply, from the restore point it made.
///
/// `touched` is what the apply wrote or may have written; `side_effects` is what changed that it
/// did not write. A name goes in `names` only if the restore point holds it and a restore of it
/// would be written (the same check a restore makes, for the body's own snapshot); every other
/// candidate is reported in `not_restorable`, never silently dropped and never passed to restore.
pub fn undo_set(
    restore_point: &Snapshot,
    capabilities: &[CapabilityInfo],
    touched: &BTreeSet<String>,
    side_effects: Option<&crate::ops::readback::SideEffects>,
    readback: Option<&Snapshot>,
) -> UndoSet {
    let cap_map: HashMap<u32, &CapabilityInfo> = capabilities.iter().map(|c| (c.id, c)).collect();
    // The undo restores the restore point onto the body it was taken from, so whether that counts as
    // "the body's own snapshot" is decided the way the undo will decide it: from the serial the
    // restore point carries (an empty or `id-N` serial identifies no body, so a setting that reads
    // differently per body would be skipped by the undo and must not be listed).
    let source = Source::of(&restore_point.camera.serial, &restore_point.camera.serial);
    let mut candidates: BTreeSet<String> = touched.clone();
    if let Some(se) = side_effects {
        candidates.extend(se.names());
    }
    let mut out = UndoSet::default();
    for name in candidates {
        let restorable = restore_point.properties.get(&name).is_some_and(|prop| {
            restore_precheck_from(&cap_map, prop.code, source).is_ok()
                // A range whose scale the apply itself changed (a setting that rescales it) cannot be
                // written back until that setting is: the undo would skip it as `range_scale_differs`.
                && !readback
                    .and_then(|after| after.properties.get(&name))
                    .is_some_and(|now| range_scale(&prop.value) != range_scale(&now.value) && is_range_shape(&prop.value))
        });
        if restorable { out.names.push(name) } else { out.not_restorable.push(name) }
    }
    out
}

/// Why a `--only` selection was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnlyError {
    /// No names were given. A selection of nothing would silently restore
    /// nothing; "no restriction" is expressed by not passing a selection.
    Empty,
    /// The name is not a property of the snapshot (a typo would otherwise
    /// look like a restore that simply had nothing to do).
    NoSuchProperty { requested: String },
}

impl OnlyError {
    pub fn tag(&self) -> &'static str {
        match self {
            OnlyError::Empty => "empty_selection",
            OnlyError::NoSuchProperty { .. } => "no_such_property",
        }
    }
}

impl std::fmt::Display for OnlyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OnlyError::Empty => f.write_str("--only was given no property names"),
            OnlyError::NoSuchProperty { requested } => {
                write!(f, "--only {requested:?}: no property by that name in the snapshot")
            }
        }
    }
}

impl std::error::Error for OnlyError {}


/// Turn `--only` names into full property names, checked against the
/// snapshot. A name is taken as written if the snapshot has it, else with the
/// `kNkMAIDCapability_` prefix added; an exact match wins over the prefixed
/// form. Case and whitespace are not normalised. Duplicates collapse.
pub fn resolve_only(
    requested: &[String],
    properties: &BTreeMap<String, PropertyEntry>,
) -> Result<BTreeSet<String>, OnlyError> {
    if requested.is_empty() {
        return Err(OnlyError::Empty);
    }
    let mut out = BTreeSet::new();
    for r in requested {
        let full = if properties.contains_key(r.as_str()) { r.clone() } else { format!("{}{r}", transplant::PROPERTY_PREFIX) };
        if !properties.contains_key(&full) {
            return Err(OnlyError::NoSuchProperty { requested: r.clone() });
        }
        out.insert(full);
    }
    Ok(out)
}

/// What a restore does with one property.
#[derive(Debug, Clone, Copy)]
pub enum Decision<'a> {
    /// Excluded by `--only`. Never written, whatever its state on the camera.
    NotSelected,
    Skip(RestoreSkip),
    Write(&'a CapabilityInfo),
}

#[derive(Debug, Clone, Copy)]
pub struct PlannedRestore<'a> {
    pub name: &'a str,
    pub code: u32,
    pub decision: Decision<'a>,
}

/// Decide every property of the snapshot, in the snapshot's (name) order.
/// With `only = None` every property is a candidate; with `Some`, a property
/// outside the set is `NotSelected` before any camera check. Nothing is
/// dropped: the result has one entry per property. For a snapshot of the body
/// itself; see [`plan_from`]. Tests only, for the same reason as [`restore_precheck`].
#[cfg(test)]
pub fn plan<'a>(
    properties: &'a BTreeMap<String, PropertyEntry>,
    only: Option<&BTreeSet<String>>,
    cap_map: &HashMap<u32, &'a CapabilityInfo>,
) -> Vec<PlannedRestore<'a>> {
    plan_from(properties, only, cap_map, Source::SameBody)
}

/// [`plan`] for a snapshot of `source`'s body.
pub fn plan_from<'a>(
    properties: &'a BTreeMap<String, PropertyEntry>,
    only: Option<&BTreeSet<String>>,
    cap_map: &HashMap<u32, &'a CapabilityInfo>,
    source: Source,
) -> Vec<PlannedRestore<'a>> {
    properties
        .iter()
        .map(|(name, prop)| {
            let decision = if only.is_some_and(|set| !set.contains(name)) {
                Decision::NotSelected
            } else {
                match restore_precheck_from(cap_map, prop.code, source) {
                    Ok(cap) => Decision::Write(cap),
                    Err(skip) => Decision::Skip(skip),
                }
            };
            PlannedRestore { name: name.as_str(), code: prop.code, decision }
        })
        .collect()
}

/// What happened to one property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    NotSelected,
    Skipped(RestoreSkip),
    /// Dry run: it would be written.
    WouldWrite,
    Written,
    /// The SDK rejected the capability kind for writing.
    UnsupportedWriteType,
    /// The write failed; carries the SDK's error text for a human.
    WriteFailed(String),
}

impl StepOutcome {
    /// Closed machine-readable tag. Exhaustive match.
    pub fn tag(&self) -> &'static str {
        match self {
            StepOutcome::NotSelected => "not_selected",
            StepOutcome::Skipped(s) => s.tag(),
            StepOutcome::WouldWrite => "would_write",
            StepOutcome::Written => "written",
            StepOutcome::UnsupportedWriteType => "unsupported_write_type",
            StepOutcome::WriteFailed(_) => "write_failed",
        }
    }
}

impl Serialize for StepOutcome {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = serde_json::Map::new();
        m.insert("tag".into(), json!(self.tag()));
        if let StepOutcome::WriteFailed(msg) = self {
            m.insert("detail".into(), json!({ "message": msg }));
        }
        Value::Object(m).serialize(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StepResult {
    pub name: String,
    pub code: u32,
    pub outcome: StepOutcome,
}

/// Tallies over a run. `written` counts would-writes in a dry run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Counts {
    pub written: usize,
    pub skipped_absent: usize,
    pub skipped_read_only: usize,
    pub skipped_type: usize,
    pub skipped_never: usize,
    pub skipped_scale: usize,
    pub not_selected: usize,
    pub errors: usize,
}

/// One restore, property by property, in the snapshot's name order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestoreRun {
    pub steps: Vec<StepResult>,
}

impl RestoreRun {
    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for s in &self.steps {
            match &s.outcome {
                StepOutcome::NotSelected => c.not_selected += 1,
                StepOutcome::Skipped(RestoreSkip::NeverCopied) => c.skipped_never += 1,
                StepOutcome::Skipped(RestoreSkip::Absent) => c.skipped_absent += 1,
                StepOutcome::Skipped(RestoreSkip::ReadOnly) => c.skipped_read_only += 1,
                StepOutcome::Skipped(RestoreSkip::RangeScaleDiffers) => c.skipped_scale += 1,
                StepOutcome::WouldWrite | StepOutcome::Written => c.written += 1,
                StepOutcome::UnsupportedWriteType => c.skipped_type += 1,
                StepOutcome::WriteFailed(_) => c.errors += 1,
            }
        }
        c
    }

    /// Properties actually written (not would-writes): what `readback::residual`
    /// should judge and what an undo of this restore would have to put back.
    pub fn written_names(&self) -> BTreeSet<String> {
        self.steps
            .iter()
            .filter(|s| s.outcome == StepOutcome::Written)
            .map(|s| s.name.clone())
            .collect()
    }

    /// Selected properties that were not (or, in a dry run, would not be)
    /// written, with a closed reason tag. Meaningful for a targeted restore.
    pub fn unrestored(&self) -> Vec<(String, &'static str)> {
        self.steps
            .iter()
            .filter(|s| {
                !matches!(s.outcome, StepOutcome::NotSelected | StepOutcome::Written | StepOutcome::WouldWrite)
            })
            .map(|s| (s.name.clone(), s.outcome.tag()))
            .collect()
    }
}

/// The struct to hand to the SDK for `stored`. A stepped range's `value` field is
/// `0.0` in every snapshot (F18), so writing it back as stored would send a
/// position of `value_index` and a number of 0; which of the two the camera honours
/// is not known. The number is therefore rewritten to the one the index means, so
/// the struct is coherent either way. Everything else is written as stored.
pub(crate) fn coherent_for_write(stored: &Value) -> Value {
    if !is_stepped_range(stored) {
        return stored.clone();
    }
    let mut out = stored.clone();
    if let Some(v) = crate::value_shape::range_value(stored) {
        out["value"] = json!(v);
    }
    out
}

/// Run a restore of `properties` onto `port`: [`plan`], then write each
/// `Write` decision (or only report it, if `dry_run`). A failed write does not
/// stop the rest. Tests only, for the same reason as [`restore_precheck`].
#[cfg(test)]
pub fn execute<P: CameraPort + ?Sized>(
    port: &P,
    properties: &BTreeMap<String, PropertyEntry>,
    only: Option<&BTreeSet<String>>,
    dry_run: bool,
) -> RestoreRun {
    execute_from(port, properties, only, dry_run, Source::SameBody)
}

/// [`execute`] for a snapshot of `source`'s body.
pub fn execute_from<P: CameraPort + ?Sized>(
    port: &P,
    properties: &BTreeMap<String, PropertyEntry>,
    only: Option<&BTreeSet<String>>,
    dry_run: bool,
    source: Source,
) -> RestoreRun {
    let cap_map: HashMap<u32, &CapabilityInfo> = port.capabilities().iter().map(|c| (c.id, c)).collect();
    let steps = plan_from(properties, only, &cap_map, source)
        .into_iter()
        .map(|step| {
            let outcome = match step.decision {
                Decision::NotSelected => StepOutcome::NotSelected,
                Decision::Skip(skip) => StepOutcome::Skipped(skip),
                Decision::Write(cap) => {
                    let value = &properties[step.name].value;
                    // A stored range with no usable position (a stepped range whose index is missing
                    // or past its last step) is a corrupt snapshot: writing it would hand the body a
                    // struct whose `value` is 0.0. A transplant refuses the same shape as malformed.
                    if is_range_shape(value) && range_value(value).is_none() {
                        return StepResult {
                            name: step.name.to_string(),
                            code: step.code,
                            outcome: StepOutcome::WriteFailed(
                                "the snapshot's range has no usable position (a stepped range with no valid index); not written".to_string(),
                            ),
                        };
                    }
                    // A range is read first, for two reasons. A body ignores a blind write of a
                    // stepped range and honours one made after a read (F18), so the read is the
                    // priming read (its result is not used for that). And a stored position only
                    // means the same value on the scale it was read on: if the body's scale for
                    // this range differs now, the range is left alone, as a transplant does.
                    let scale_differs = is_range_shape(value) && {
                        let live = port.read_capability(step.code).ok();
                        match (range_scale(value), live.as_ref().and_then(range_scale)) {
                            (Some(stored), Some(now)) => stored != now,
                            // No scale on one side (the read failed, or a malformed value): nothing
                            // to compare, so nothing to refuse on; the write and read-back judge it.
                            _ => false,
                        }
                    };
                    if scale_differs {
                        StepOutcome::Skipped(RestoreSkip::RangeScaleDiffers)
                    } else if dry_run {
                        StepOutcome::WouldWrite
                    } else {
                        match port.write_capability(step.code, cap.kind, &coherent_for_write(value)) {
                            Ok(()) => StepOutcome::Written,
                            Err(SdkError::UnsupportedWrite(_)) => StepOutcome::UnsupportedWriteType,
                            Err(e) => StepOutcome::WriteFailed(truncate_with_marker(e.to_string(), MAX_ERROR_MESSAGE_LEN)),
                        }
                    }
                }
            };
            StepResult { name: step.name.to_string(), code: step.code, outcome }
        })
        .collect();
    RestoreRun { steps }
}

/// What a read-back after a restore showed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every written property reads back as the snapshot says.
    Verified,
    /// At least one written property does not read back as restored: the body
    /// accepted the write and did not keep it (F18), or changed it.
    ResidualFound,
    /// The read-back was too incomplete to judge, so nothing is claimed.
    ReadbackIncomplete,
    /// Nothing was written, so there was nothing to check.
    NothingWritten,
    /// Something was written but none of it could be compared (`Residual::judged` is 0: every
    /// written name is volatile-listed, never-copy, or absent from the snapshot). Nothing was
    /// checked, so nothing is claimed. (Some written properties being unjudged among compared ones
    /// is reported in `Residual::unjudged` and does not change the verdict.)
    Unjudged,
}

impl Verdict {
    pub fn tag(&self) -> &'static str {
        match self {
            Verdict::Verified => "verified",
            Verdict::ResidualFound => "residual_found",
            Verdict::ReadbackIncomplete => "readback_incomplete",
            Verdict::NothingWritten => "nothing_written",
            Verdict::Unjudged => "unjudged",
        }
    }
}

impl Verdict {
    /// Every verdict, for tests and for anything that must name them all.
    pub const ALL: [Verdict; 5] =
        [Verdict::Verified, Verdict::ResidualFound, Verdict::ReadbackIncomplete, Verdict::NothingWritten, Verdict::Unjudged];

    /// Whether the read-back found that the camera did not keep a write (the restore failed).
    /// Everything else claims nothing, or claims success.
    pub fn is_failure(&self) -> bool {
        match self {
            Verdict::ResidualFound => true,
            Verdict::Verified | Verdict::ReadbackIncomplete | Verdict::NothingWritten | Verdict::Unjudged => false,
        }
    }
}

/// The one-line report of what the read-back showed, for the CLI and the GUI alike. `residual` names
/// the properties of a `ResidualFound`.
pub fn readback_line(verdict: Verdict, residual: Option<&crate::ops::readback::Residual>) -> String {
    match verdict {
        Verdict::Verified => "Read-back: verified — every written property reads back as restored".to_string(),
        Verdict::NothingWritten => "Read-back: nothing was written, nothing to verify".to_string(),
        Verdict::Unjudged => "Read-back: UNVERIFIED — none of the written properties could be compared (volatile-listed, \
             never-copy or absent from the snapshot); nothing is claimed"
            .to_string(),
        Verdict::ReadbackIncomplete => "Read-back: INCOMPLETE — the camera could not be read back well enough to verify the writes; \
             nothing is claimed"
            .to_string(),
        Verdict::ResidualFound => {
            let r = residual.cloned().unwrap_or_default();
            let mut names: Vec<String> = r.changed.iter().map(|n| format!("{n} (differs)")).collect();
            names.extend(r.missing.iter().map(|n| format!("{n} (not readable)")));
            format!(
                "Read-back: UNVERIFIED — {} not read back as restored: {}. The camera accepted the write and did not keep it.",
                if names.len() == 1 { "1 written property does".to_string() } else { format!("{} written properties do", names.len()) },
                names.join(", ")
            )
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Verification {
    pub verdict: Verdict,
    /// The read-back held to the baseline gate. `None` if it was not taken or read nothing.
    pub gate: Option<GateOutcome>,
    pub stats: Option<CaptureStats>,
    /// Written properties that did not end up as the snapshot says. `None` when
    /// the read-back was not good enough to compare.
    pub residual: Option<Residual>,
    /// The read-back itself, when it was complete enough to compare. Not serialised.
    #[serde(skip)]
    pub readback: Option<Snapshot>,
}

/// Read the body back after a restore and compare what was written with the
/// snapshot it was restored from. A write call that returned success proves
/// nothing (F18): only this does. `written` is [`RestoreRun::written_names`].
pub fn verify_with<P: CameraPort + ?Sized>(
    port: &P,
    name_lookup: &HashMap<u32, String>,
    camera: &Camera,
    source: &Snapshot,
    written: &BTreeSet<String>,
    captured_at: &str,
    gate_policy: GatePolicy,
) -> Verification {
    if written.is_empty() {
        return Verification { verdict: Verdict::NothingWritten, gate: None, stats: None, residual: None, readback: None };
    }
    let mut after = Snapshot::new(camera.clone(), Transport::Usb, captured_at.to_string());
    after.label = Some("readback".into());
    let stats = capture(port, name_lookup, &mut after);
    if stats.read_ok == 0 {
        return Verification { verdict: Verdict::ReadbackIncomplete, gate: None, stats: Some(stats), residual: None, readback: None };
    }
    let verdict_gate = gate(&after);
    // The same completeness rule as a transplant's read-back: a read-back that cannot be held to a
    // baseline is judged only if the caller waived that judgement (`AllowPartial`).
    let incomplete = match (&verdict_gate, gate_policy) {
        (GateOutcome::Failed { .. }, _) => true,
        (GateOutcome::NoBaseline(_), GatePolicy::Strict) => true,
        _ => false,
    };
    if incomplete {
        return Verification { verdict: Verdict::ReadbackIncomplete, gate: Some(verdict_gate), stats: Some(stats), residual: None, readback: None };
    }
    match residual(source, &after, written) {
        Ok(r) => Verification {
            verdict: if !r.is_empty() {
                Verdict::ResidualFound
            } else if r.judged == 0 {
                Verdict::Unjudged
            } else {
                Verdict::Verified
            },
            gate: Some(verdict_gate),
            stats: Some(stats),
            residual: Some(r),
            readback: Some(after),
        },
        Err(_) => Verification { verdict: Verdict::ReadbackIncomplete, gate: Some(verdict_gate), stats: Some(stats), residual: None, readback: None },
    }
}

/// [`verify_with`] without a baseline requirement, for tests.
#[cfg(test)]
pub fn verify<P: CameraPort + ?Sized>(
    port: &P,
    name_lookup: &HashMap<u32, String>,
    camera: &Camera,
    source: &Snapshot,
    written: &BTreeSet<String>,
    captured_at: &str,
) -> Verification {
    verify_with(port, name_lookup, camera, source, written, captured_at, GatePolicy::AllowPartial)
}

/// How many names an undo command line may carry before [`undo_command`] replaces it with a pointer
/// to the `--json` report's `undo_set`: a command line that long is not something to copy and paste.
/// The boundary is inclusive: exactly this many are listed, one more is not.
const MAX_UNDO_NAMES: usize = 12;

/// The command that undoes an apply, a targeted restore of its restore point. The CLI prints it and
/// the GUI shows it next to the restore point, so both say the same thing.
pub fn undo_command(restore_point_file: &str, undo_set: &[String]) -> String {
    if undo_set.is_empty() {
        return "nothing to undo".into();
    }
    match undo_command_exact(restore_point_file, undo_set, None) {
        Some(command) => command,
        None => format!(
            "{} properties to put back from {restore_point_file}; list them with --json (undo_set) and pass each as --only",
            undo_set.len()
        ),
    }
}

/// The undo as a command that can be pasted and run: `None` when there is nothing to undo or too
/// many properties to name on a command line (then [`undo_command`] is prose, which is not a
/// command). `data_dir` is named when the restore point is not in the CLI's default directory.
pub fn undo_command_exact(restore_point_file: &str, undo_set: &[String], data_dir: Option<&str>) -> Option<String> {
    if undo_set.is_empty() || undo_set.len() > MAX_UNDO_NAMES {
        return None;
    }
    let only: String = undo_set.iter().map(|n| format!(" --only {}", n.trim_start_matches(crate::transplant::PROPERTY_PREFIX))).collect();
    let dir = data_dir
        .map(|d| if d.contains(char::is_whitespace) { format!(" --data-dir \"{d}\"") } else { format!(" --data-dir {d}") })
        .unwrap_or_default();
    Some(format!("nikon-fleet{dir} restore {restore_point_file}{only}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdk::OP_GET;
    use serde_json::json;

    fn cap(id: u32, operations: u32) -> CapabilityInfo {
        CapabilityInfo { id, kind: 0, visibility: 0, operations, description: String::new() }
    }

    fn props(entries: &[(&str, u32)]) -> BTreeMap<String, PropertyEntry> {
        entries.iter().map(|(n, c)| (n.to_string(), PropertyEntry { code: *c, value: json!(1) })).collect()
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ── restore_precheck (moved from main.rs; shares NEVER_TRANSPLANT) ───

    #[test]
    fn precheck_skips_every_never_transplant_code_even_when_writable() {
        for (code, name, _) in transplant::NEVER_TRANSPLANT {
            let c = cap(*code, OP_GET | OP_SET);
            let caps: HashMap<u32, &CapabilityInfo> = HashMap::from([(*code, &c)]);
            assert_eq!(restore_precheck(&caps, *code).err(), Some(RestoreSkip::NeverCopied), "{name}");
        }
    }

    #[test]
    fn precheck_skips_shoot_no_card() {
        let c = cap(33108, OP_GET | OP_SET);
        let caps: HashMap<u32, &CapabilityInfo> = HashMap::from([(33108, &c)]);
        assert_eq!(restore_precheck(&caps, 33108).err(), Some(RestoreSkip::NeverCopied));
        let neighbour = cap(33109, OP_GET | OP_SET);
        let caps: HashMap<u32, &CapabilityInfo> = HashMap::from([(33109, &neighbour)]);
        assert_eq!(restore_precheck(&caps, 33109).unwrap().id, 33109);
    }

    #[test]
    fn precheck_restores_shooting_limit_like_its_neighbours() {
        // A restore returns a body to what the same body stored, so there is nothing to interpret and
        // the setting comes back (user decision, 2026-10-28). Only a transplant between bodies refuses it
        // (`NEVER_TRANSLATED`). At, below and above the code.
        for code in [33085, 33086, 33087] {
            let c = cap(code, OP_GET | OP_SET);
            let caps: HashMap<u32, &CapabilityInfo> = HashMap::from([(code, &c)]);
            assert_eq!(restore_precheck(&caps, code).unwrap().id, code, "{code}");
        }
        // It is still skipped when the body cannot write it, as any property would be.
        let ro = cap(33086, OP_GET);
        let caps: HashMap<u32, &CapabilityInfo> = HashMap::from([(33086, &ro)]);
        assert_eq!(restore_precheck(&caps, 33086).err(), Some(RestoreSkip::ReadOnly));
    }

    #[test]
    fn precheck_policy_skip_wins_over_absent_and_read_only() {
        let code = transplant::NEVER_TRANSPLANT[0].0;
        let empty: HashMap<u32, &CapabilityInfo> = HashMap::new();
        assert_eq!(restore_precheck(&empty, code).err(), Some(RestoreSkip::NeverCopied));
        let ro = cap(code, OP_GET);
        let caps: HashMap<u32, &CapabilityInfo> = HashMap::from([(code, &ro)]);
        assert_eq!(restore_precheck(&caps, code).err(), Some(RestoreSkip::NeverCopied));
    }

    #[test]
    fn precheck_ordinary_codes_keep_their_existing_behaviour() {
        let denied: BTreeSet<u32> = transplant::NEVER_TRANSPLANT.iter().map(|(c, _, _)| *c).collect();
        let near: Vec<u32> = denied.iter().flat_map(|c| [c - 1, c + 1]).filter(|c| !denied.contains(c)).collect();
        assert!(!near.is_empty());
        for code in near {
            let rw = cap(code, OP_GET | OP_SET);
            let caps: HashMap<u32, &CapabilityInfo> = HashMap::from([(code, &rw)]);
            assert_eq!(restore_precheck(&caps, code).unwrap().id, code, "{code} must still be restored");
        }
        let ro = cap(7, OP_GET);
        let caps: HashMap<u32, &CapabilityInfo> = HashMap::from([(7, &ro)]);
        assert_eq!(restore_precheck(&caps, 7).err(), Some(RestoreSkip::ReadOnly));
        assert_eq!(restore_precheck(&caps, 8).err(), Some(RestoreSkip::Absent));
    }

    #[test]
    fn skip_tags_are_distinct_and_closed() {
        let tags = [RestoreSkip::NeverCopied.tag(), RestoreSkip::Absent.tag(), RestoreSkip::ReadOnly.tag()];
        assert_eq!(tags, ["never_copied", "absent_on_target", "read_only"]);
    }

    #[test]
    fn every_skip_is_listed_has_a_distinct_tag_that_round_trips_and_has_a_decided_severity() {
        // This match names every variant: a new one fails to compile here, then `ALL` and the table.
        fn every(s: RestoreSkip) {
            match s {
                RestoreSkip::NeverCopied | RestoreSkip::Absent | RestoreSkip::ReadOnly | RestoreSkip::RangeScaleDiffers => {}
            }
        }
        let expected = [
            (RestoreSkip::NeverCopied, "never_copied", true),
            (RestoreSkip::Absent, "absent_on_target", true),
            (RestoreSkip::ReadOnly, "read_only", true),
            (RestoreSkip::RangeScaleDiffers, "range_scale_differs", false),
        ];
        assert_eq!(RestoreSkip::ALL.len(), expected.len());
        for (skip, tag, routine) in expected {
            every(skip);
            assert!(RestoreSkip::ALL.contains(&skip), "{tag} is in ALL");
            assert_eq!(skip.tag(), tag);
            assert_eq!(skip.is_routine(), routine, "{tag}");
            assert_eq!(RestoreSkip::from_tag(tag), Some(skip), "{tag} round-trips");
        }
        // not a skip: another outcome's tag, a tag from another build, empty text, a near miss
        for not_a_skip in ["would_write", "written", "not_selected", "a_new_kind_of_step", "", "NEVER_COPIED", "never_copied ", "absent"] {
            assert_eq!(RestoreSkip::from_tag(not_a_skip), None, "{not_a_skip:?}");
        }
    }

    // ── resolve_only ─────────────────────────────────────────────────────

    #[test]
    fn only_accepts_short_and_full_names_and_collapses_duplicates() {
        let p = props(&[("kNkMAIDCapability_Aperture", 1), ("kNkMAIDCapability_ISO", 2)]);
        let got = resolve_only(&names(&["Aperture", "kNkMAIDCapability_Aperture", "ISO", "Aperture"]), &p).unwrap();
        assert_eq!(
            got,
            BTreeSet::from(["kNkMAIDCapability_Aperture".to_string(), "kNkMAIDCapability_ISO".to_string()])
        );
    }

    #[test]
    fn only_exact_name_wins_over_the_prefixed_form() {
        // Both "Foo" and "kNkMAIDCapability_Foo" exist: "Foo" means the first.
        let p = props(&[("Foo", 1), ("kNkMAIDCapability_Foo", 2)]);
        assert_eq!(resolve_only(&names(&["Foo"]), &p).unwrap(), BTreeSet::from(["Foo".to_string()]));
    }

    #[test]
    fn only_unknown_names_are_refused_not_ignored() {
        let p = props(&[("kNkMAIDCapability_Aperture", 1)]);
        for bad in ["Apertur", "aperture", " Aperture", "Aperture ", "", "kNkMAIDCapability_", "kNkMAIDCapability_kNkMAIDCapability_Aperture"] {
            assert_eq!(
                resolve_only(&names(&[bad]), &p),
                Err(OnlyError::NoSuchProperty { requested: bad.to_string() }),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn only_one_bad_name_fails_the_whole_selection() {
        let p = props(&[("kNkMAIDCapability_Aperture", 1)]);
        assert!(resolve_only(&names(&["Aperture", "Nope"]), &p).is_err());
    }

    #[test]
    fn only_with_no_names_is_an_error_not_an_empty_selection() {
        let p = props(&[("kNkMAIDCapability_Aperture", 1)]);
        assert_eq!(resolve_only(&[], &p), Err(OnlyError::Empty));
        assert_eq!(OnlyError::Empty.tag(), "empty_selection");
        assert_eq!(OnlyError::NoSuchProperty { requested: String::new() }.tag(), "no_such_property");
    }

    #[test]
    fn only_on_an_empty_snapshot_finds_nothing() {
        assert!(matches!(resolve_only(&names(&["Aperture"]), &BTreeMap::new()), Err(OnlyError::NoSuchProperty { .. })));
    }

    // ── plan ─────────────────────────────────────────────────────────────

    /// Snapshot with: A writable, B writable, C read-only, D absent on the camera, L never-copied.
    fn fixture() -> (BTreeMap<String, PropertyEntry>, Vec<CapabilityInfo>) {
        let never = transplant::NEVER_TRANSPLANT[0].0;
        (
            props(&[("A", 100), ("B", 200), ("C", 300), ("D", 400), ("L", never)]),
            vec![cap(100, OP_GET | OP_SET), cap(200, OP_GET | OP_SET), cap(300, OP_GET), cap(never, OP_GET | OP_SET)],
        )
    }

    fn kinds<'a>(plan: &[PlannedRestore<'a>]) -> Vec<(&'a str, &'static str)> {
        plan.iter()
            .map(|p| {
                let k = match p.decision {
                    Decision::NotSelected => "not_selected",
                    Decision::Skip(s) => s.tag(),
                    Decision::Write(_) => "write",
                };
                (p.name, k)
            })
            .collect()
    }

    #[test]
    fn plan_without_a_selection_decides_every_property_like_the_full_restore() {
        let (p, caps) = fixture();
        let map: HashMap<u32, &CapabilityInfo> = caps.iter().map(|c| (c.id, c)).collect();
        let plan = plan(&p, None, &map);
        assert_eq!(
            kinds(&plan),
            [("A", "write"), ("B", "write"), ("C", "read_only"), ("D", "absent_on_target"), ("L", "never_copied")]
        );
    }

    #[test]
    fn plan_with_a_selection_writes_only_the_named_property() {
        // The targeted-undo contract: B is writable and in the snapshot, but not named, so it is left alone.
        let (p, caps) = fixture();
        let map: HashMap<u32, &CapabilityInfo> = caps.iter().map(|c| (c.id, c)).collect();
        let only = BTreeSet::from(["A".to_string()]);
        let plan = plan(&p, Some(&only), &map);
        assert_eq!(
            kinds(&plan),
            [("A", "write"), ("B", "not_selected"), ("C", "not_selected"), ("D", "not_selected"), ("L", "not_selected")]
        );
    }

    #[test]
    fn plan_a_selected_property_still_goes_through_every_camera_check() {
        let (p, caps) = fixture();
        let map: HashMap<u32, &CapabilityInfo> = caps.iter().map(|c| (c.id, c)).collect();
        let only: BTreeSet<String> = ["C", "D", "L"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            kinds(&plan(&p, Some(&only), &map)),
            [("A", "not_selected"), ("B", "not_selected"), ("C", "read_only"), ("D", "absent_on_target"), ("L", "never_copied")]
        );
    }

    #[test]
    fn plan_has_one_entry_per_property_in_name_order_and_keeps_codes() {
        let (p, caps) = fixture();
        let map: HashMap<u32, &CapabilityInfo> = caps.iter().map(|c| (c.id, c)).collect();
        let only = BTreeSet::from(["B".to_string(), "A".to_string()]);
        let plan = plan(&p, Some(&only), &map);
        assert_eq!(plan.len(), p.len());
        assert_eq!(plan.iter().map(|x| x.name).collect::<Vec<_>>(), ["A", "B", "C", "D", "L"]);
        assert_eq!(plan[1].code, 200);
        match plan[0].decision {
            Decision::Write(c) => assert_eq!(c.id, 100),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plan_with_a_selection_of_everything_equals_no_selection() {
        let (p, caps) = fixture();
        let map: HashMap<u32, &CapabilityInfo> = caps.iter().map(|c| (c.id, c)).collect();
        let all: BTreeSet<String> = p.keys().cloned().collect();
        assert_eq!(kinds(&plan(&p, Some(&all), &map)), kinds(&plan(&p, None, &map)));
    }

    #[test]
    fn plan_an_empty_selection_set_selects_nothing_which_is_why_resolve_refuses_it() {
        // Pin the hazard: Some(empty) writes nothing at all.
        let (p, caps) = fixture();
        let map: HashMap<u32, &CapabilityInfo> = caps.iter().map(|c| (c.id, c)).collect();
        let none = BTreeSet::new();
        assert!(plan(&p, Some(&none), &map).iter().all(|x| matches!(x.decision, Decision::NotSelected)));
    }
    // ── execute, on an in-memory body ────────────────────────────────────

    use crate::ops::capture::capture;
    use crate::ops::port::{Failure, FakeCamera};
    use crate::ops::readback::{residual, side_effects};
    use crate::snapshot::{Camera, Snapshot, Transport};

    const RW: u32 = OP_GET | OP_SET;

    fn snapshot_of(entries: &[(&str, u32, i64)]) -> Snapshot {
        let mut s = Snapshot::new(
            Camera { model: "Z 9".into(), serial: "1".into(), firmware: "5.31".into() },
            Transport::Usb,
            "2026-10-27T12:00:00Z".into(),
        );
        for (n, c, v) in entries {
            s.insert(*n, *c, json!(*v));
        }
        s
    }

    fn lookup(entries: &[(&str, u32)]) -> HashMap<u32, String> {
        entries.iter().map(|(n, c)| (*c, n.to_string())).collect()
    }

    fn outcomes(run: &RestoreRun) -> Vec<(&str, &'static str)> {
        run.steps.iter().map(|s| (s.name.as_str(), s.outcome.tag())).collect()
    }

    #[test]
    fn a_full_restore_writes_every_writable_property_and_only_those() {
        let never = transplant::NEVER_TRANSPLANT[0].0;
        let cam = FakeCamera::new()
            .with(1, RW, json!(0))
            .with(2, RW, json!(0))
            .with(3, OP_GET, json!(0))
            .with(never, RW, json!(0));
        let snap = snapshot_of(&[("A", 1, 10), ("B", 2, 20), ("C", 3, 30), ("D", 4, 40), ("L", never, 50)]);
        let run = execute(&cam, &snap.properties, None, false);
        assert_eq!(
            outcomes(&run),
            [("A", "written"), ("B", "written"), ("C", "read_only"), ("D", "absent_on_target"), ("L", "never_copied")]
        );
        assert_eq!(cam.writes(), [(1, json!(10)), (2, json!(20))]);
        assert_eq!(cam.value(never), Some(json!(0)), "state property untouched");
        let c = run.counts();
        assert_eq!((c.written, c.skipped_read_only, c.skipped_absent, c.skipped_never, c.not_selected, c.errors), (2, 1, 1, 1, 0, 0));
    }

    // ── the camera clock (light-review 1f6c#07): ClockDateTime is not on the never-copy table ──

    const CLOCK: u32 = 33060;
    /// How a snapshot holds the clock: a Z 9 or Z 5 capture reads `{"_type": "datetime"}`; an early
    /// Z 5 capture, `{"_unknown_type": 12}`.
    fn clock_shapes() -> [Value; 2] {
        [json!({"_type": "datetime"}), json!({"_unknown_type": 12})]
    }

    #[test]
    fn no_capability_kind_writes_a_datetime_placeholder() {
        // The one thing that keeps a snapshot's time off the camera: `prepare_write` builds the data
        // from the value, and a placeholder object is not a bool, number, string, enum (no
        // value_index) or range (no value) for any kind, nor is it a kind it writes at all. If a
        // DateTime kind is ever made writable, this fails and the clock needs a never-copy entry.
        for shape in clock_shapes() {
            for kind in (0..=40).chain([u32::MAX]) {
                assert!(crate::sdk::prepare_write(kind, &shape).is_err(), "kind {kind} would write {shape}");
            }
        }
    }

    #[test]
    fn a_restore_attempts_the_clock_and_the_write_is_refused_by_kind_never_made() {
        // A full restore, a restore of another body's snapshot and the undo (`only` = the clock) all
        // reach the clock as a planned write (it is not on the table) and all end the same way:
        // `unsupported_write_type`, counted as skipped_type, nothing written, the clock untouched.
        for shape in clock_shapes() {
            let only = BTreeSet::from(["kNkMAIDCapability_ClockDateTime".to_string()]);
            for (only, source) in [(None, Source::SameBody), (None, Source::OtherBody), (Some(&only), Source::SameBody)] {
                let cam = FakeCamera::new().with(1, RW, json!(0)).with(CLOCK, RW, json!({"_type": "datetime"}));
                let mut snap = snapshot_of(&[("kNkMAIDCapability_A", 1, 10)]);
                snap.insert("kNkMAIDCapability_ClockDateTime", CLOCK, shape.clone());
                let run = execute_from(&cam, &snap.properties, only, false, source);
                let clock = run.steps.iter().find(|s| s.code == CLOCK).unwrap();
                assert_eq!(clock.outcome, StepOutcome::UnsupportedWriteType, "{shape} only={only:?} {source:?}");
                assert_eq!(clock.outcome.tag(), "unsupported_write_type");
                assert!(!cam.codes_written().contains(&CLOCK), "the clock was written");
                assert_eq!(cam.value(CLOCK), Some(json!({"_type": "datetime"})), "the clock is untouched");
                assert_eq!(run.counts().skipped_type, 1);
                assert_eq!(run.counts().errors, 0, "refused by kind is routine, not an error");
                assert!(!run.written_names().contains(&clock.name), "never reported as restored");
            }
        }
    }

    #[test]
    fn a_targeted_restore_leaves_every_other_value_exactly_as_it_is() {
        let cam = FakeCamera::new().with(1, RW, json!(0)).with(2, RW, json!(0));
        let snap = snapshot_of(&[("A", 1, 10), ("B", 2, 20)]);
        let only = BTreeSet::from(["A".to_string()]);
        let run = execute(&cam, &snap.properties, Some(&only), false);
        assert_eq!(cam.writes(), [(1, json!(10))]);
        assert_eq!(cam.value(2), Some(json!(0)), "B differs from the snapshot and is left alone");
        assert_eq!(run.counts().not_selected, 1);
        assert!(run.unrestored().is_empty());
    }

    #[test]
    fn a_dry_run_writes_nothing_counts_would_writes_and_has_no_written_names() {
        let cam = FakeCamera::new().with(1, RW, json!(0));
        let snap = snapshot_of(&[("A", 1, 10)]);
        let run = execute(&cam, &snap.properties, None, true);
        assert!(cam.writes().is_empty() && cam.reads().is_empty());
        assert_eq!(outcomes(&run), [("A", "would_write")]);
        assert_eq!(run.counts().written, 1);
        assert!(run.written_names().is_empty(), "nothing was actually written");
    }

    #[test]
    fn a_failed_write_is_an_error_but_the_rest_still_run() {
        let cam = FakeCamera::new()
            .with(1, RW, json!(0))
            .with(2, RW, json!(0))
            .with(3, RW, json!(0))
            .failing_write(1, Failure::Call(-125))
            .failing_write(2, Failure::UnsupportedWrite);
        let snap = snapshot_of(&[("A", 1, 10), ("B", 2, 20), ("C", 3, 30)]);
        let run = execute(&cam, &snap.properties, None, false);
        assert_eq!(outcomes(&run), [("A", "write_failed"), ("B", "unsupported_write_type"), ("C", "written")]);
        let c = run.counts();
        assert_eq!((c.written, c.errors, c.skipped_type), (1, 1, 1));
        assert_eq!(run.written_names(), BTreeSet::from(["C".to_string()]), "a failed write is not written");
        assert_eq!(run.unrestored(), [("A".to_string(), "write_failed"), ("B".to_string(), "unsupported_write_type")]);
    }

    #[test]
    fn unrestored_lists_selected_skips_but_not_unselected_ones() {
        let cam = FakeCamera::new().with(1, RW, json!(0)).with(3, OP_GET, json!(0));
        let snap = snapshot_of(&[("A", 1, 10), ("C", 3, 30), ("D", 4, 40)]);
        let only = BTreeSet::from(["A".to_string(), "C".to_string()]);
        let run = execute(&cam, &snap.properties, Some(&only), false);
        assert_eq!(run.unrestored(), [("C".to_string(), "read_only")], "D was not asked for");
    }

    #[test]
    fn step_outcome_json_shapes() {
        let ok = serde_json::to_value(StepOutcome::Written).unwrap();
        assert_eq!(ok, json!({"tag": "written"}));
        let failed = serde_json::to_value(StepOutcome::WriteFailed("-125".into())).unwrap();
        assert_eq!(failed, json!({"tag": "write_failed", "detail": {"message": "-125"}}));
        assert_eq!(serde_json::to_value(StepOutcome::Skipped(RestoreSkip::Absent)).unwrap(), json!({"tag": "absent_on_target"}));
    }

    /// The plan's apply and undo sequence (docs/mcp-plan.md §5.5-5.6) on an
    /// in-memory body in which writing A also changes B.
    #[test]
    fn apply_then_targeted_undo_restores_the_written_and_indirect_changes_but_not_later_edits() {
        let names = [("A", 1u32), ("B", 2), ("C", 3)];
        let cam = FakeCamera::new()
            .with(1, RW, json!(1))
            .with(2, RW, json!(2))
            .with(3, RW, json!(3))
            .with_dependent(1, 2, json!(222)); // writing A moves B as a side effect
        let lk = lookup(&names);

        // 1. Restore point.
        let mut restore_point = snapshot_of(&[]);
        capture(&cam, &lk, &mut restore_point);

        // 2. The apply: set A to 100.
        let source = snapshot_of(&[("A", 1, 100)]);
        let only = BTreeSet::from(["A".to_string()]);
        let run = execute(&cam, &source.properties, Some(&only), false);
        let written = run.written_names();
        assert_eq!(written, BTreeSet::from(["A".to_string()]));

        // 3. Read back: B changed though nobody wrote it.
        let mut readback = snapshot_of(&[]);
        capture(&cam, &lk, &mut readback);
        let se = side_effects(&restore_point, &readback, &written).unwrap();
        assert_eq!(se.changed, ["B"]);

        // 4. The user changes C after the apply.
        cam.write_capability(3, 0, &json!(333)).unwrap();

        // 5. Undo A alone: B stays at its side-effect value. This is why the
        //    default undo set is written + side effects.
        let undo_a_only = execute(&cam, &restore_point.properties, Some(&written), false);
        assert_eq!(undo_a_only.written_names(), written);
        assert_eq!(cam.value(1), Some(json!(1)));
        assert_ne!(cam.value(2), Some(json!(2)), "the dependent change would have been left behind");

        // 6. Undo with written + side effects: A and B return, C (the user's edit) is left alone.
        let mut undo_set = written.clone();
        undo_set.extend(se.names());
        let undo = execute(&cam, &restore_point.properties, Some(&undo_set), false);
        assert_eq!(cam.value(1), Some(json!(1)));
        assert_eq!(cam.value(2), Some(json!(2)));
        assert_eq!(cam.value(3), Some(json!(333)), "a later edit outside the undo set is not overwritten");

        // 7. The undo reports no residual.
        let mut after_undo = snapshot_of(&[]);
        capture(&cam, &lk, &mut after_undo);
        assert!(residual(&restore_point, &after_undo, &undo.written_names()).unwrap().is_empty());
    }

    #[test]
    fn residual_catches_a_restore_that_did_not_take() {
        // The body writes A, but a dependent override puts it back: the write "succeeded".
        let cam = FakeCamera::new().with(1, RW, json!(1)).with_dependent(1, 1, json!(1));
        let lk = lookup(&[("A", 1)]);
        let snap = snapshot_of(&[("A", 1, 100)]);
        let run = execute(&cam, &snap.properties, None, false);
        assert_eq!(run.counts().errors, 0, "the write call reported success");
        let mut readback = snapshot_of(&[]);
        capture(&cam, &lk, &mut readback);
        assert_eq!(residual(&snap, &readback, &run.written_names()).unwrap().changed, ["A"]);
    }
    // ── ranges are written coherently (F18) ──────────────────────────────

    fn stepped(index: u64) -> Value {
        json!({"value": 0.0, "default": 0.0, "value_index": index, "default_index": 612,
               "lower": 0.0, "upper": 1224.0, "steps": 1225})
    }

    #[test]
    fn a_stepped_range_is_written_with_the_value_its_index_means() {
        let cam = FakeCamera::new().with(1, RW, stepped(100));
        let snap = {
            let mut s = snapshot_of(&[]);
            s.insert("WBTuneAuto", 1, stepped(612));
            s
        };
        let run = execute(&cam, &snap.properties, None, false);
        assert_eq!(run.counts().written, 1);
        let written = &cam.writes()[0].1;
        assert_eq!(written["value_index"], json!(612), "the position is untouched");
        assert_eq!(written["value"], json!(612.0), "the junk 0.0 is replaced by the value at that index");
        assert_eq!(written["lower"], json!(0.0));
    }

    #[test]
    fn coherent_for_write_leaves_everything_else_as_stored() {
        let cont = json!({"value": 3.5, "value_index": 0, "lower": 0.0, "upper": 10.0, "steps": 0});
        let one = json!({"value": 3.5, "value_index": 0, "lower": 0.0, "upper": 10.0, "steps": 1});
        let en = json!({"value_index": 1, "default_index": 0, "elem_count": 2, "elem_type": 2, "elem_bytes": 4, "values": [5, 3]});
        for v in [cont, one, en, json!(7), json!("text"), json!(true), json!(null)] {
            assert_eq!(coherent_for_write(&v), v);
        }
        // steps 2 is the first stepped case.
        let two = json!({"value": 0.0, "value_index": 1, "lower": 0.0, "upper": 10.0, "steps": 2});
        assert_eq!(coherent_for_write(&two)["value"], json!(10.0));
        // A stepped range already coherent stays so; the input is never mutated.
        let ok = json!({"value": 612.0, "value_index": 612, "lower": 0.0, "upper": 1224.0, "steps": 1225});
        assert_eq!(coherent_for_write(&ok), ok);
        let before = stepped(5);
        let _ = coherent_for_write(&before);
        assert_eq!(before["value"], json!(0.0));
    }
    #[test]
    fn a_stepped_range_is_read_before_it_is_written_but_nothing_else_is() {
        // F18: a blind write of a stepped range is accepted and ignored by the body; a read first makes it take.
        let cam = FakeCamera::new().with(1, RW, stepped(100)).with(2, RW, json!("old")).with(3, RW, json!(5));
        let mut snap = snapshot_of(&[]);
        snap.insert("A_Range", 1, stepped(612));
        snap.insert("B_String", 2, json!("new"));
        snap.insert("C_Number", 3, json!(6));
        let run = execute(&cam, &snap.properties, None, false);
        assert_eq!(run.counts().written, 3);
        assert_eq!(cam.reads(), [1], "only the stepped range is read, and only once");
        assert_eq!(cam.codes_written(), BTreeSet::from([1, 2, 3]));
    }

    #[test]
    fn the_read_happens_before_the_write_not_after() {
        // Use a body that refuses reads after the first write: the range read must come first.
        let cam = FakeCamera::new().with(1, RW, stepped(100)).failing_read_after_write(1, Failure::Call(-126));
        let mut snap = snapshot_of(&[]);
        snap.insert("A_Range", 1, stepped(612));
        let run = execute(&cam, &snap.properties, None, false);
        assert_eq!(run.counts().written, 1);
        assert_eq!(cam.events(), [crate::ops::port::Event::Read(1), crate::ops::port::Event::Write(1)]);
    }

    #[test]
    fn source_is_same_body_only_when_both_serials_identify_one_body_and_match() {
        // Equal and different real serials.
        assert_eq!(Source::of("0000003023668", "0000003023668"), Source::SameBody);
        assert_eq!(Source::of("0000003023668", "0000003023669"), Source::OtherBody);
        assert_eq!(Source::of("A", "a"), Source::OtherBody, "no case folding");
        // Empty: nothing identifies a body, so two empties are not 'the same'.
        assert_eq!(Source::of("", ""), Source::OtherBody);
        assert_eq!(Source::of("", "0000003023668"), Source::OtherBody);
        assert_eq!(Source::of("0000003023668", ""), Source::OtherBody);
        // The id-N fallback is a handle for one session, not a serial.
        assert_eq!(Source::of("id-3", "id-3"), Source::OtherBody);
        assert_eq!(Source::of("id-3", "0000003023668"), Source::OtherBody);
        assert_eq!(Source::of("0000003023668", "id-3"), Source::OtherBody);
        // A serial that merely starts like the fallback is a serial.
        assert_eq!(Source::of("id-3x", "id-3x"), Source::SameBody);
        assert_eq!(Source::of("id-", "id-"), Source::SameBody);
        // A bare number is a plausible serial (see target::require_usb_serial).
        assert_eq!(Source::of("7", "7"), Source::SameBody);
    }

    // ── undo_set: what an undo can put back ──────────────────────────────

    #[test]
    fn the_undo_set_holds_only_what_the_restore_point_has_and_the_body_will_take() {
        // A restore point that holds 1 (writable), 2 (read-only) and 33108 (never copied).
        let rp = snap_z5(&[("Setting", 1, json!(1)), ("ReadOnly", 2, json!(2)), ("ShootNoCard", 33108, json!(0))]);
        let caps = [cap(1, RW), cap(2, OP_GET), cap(33108, RW)];
        let touched: BTreeSet<String> = ["Setting", "Appeared"].iter().map(|s| s.to_string()).collect();
        // The side effects: ReadOnly changed, ShootNoCard changed, Appeared is only in the read-back.
        let se = crate::ops::readback::SideEffects {
            changed: vec!["ReadOnly".into(), "ShootNoCard".into()],
            vanished: vec![],
            appeared: vec!["Appeared".into()],
        };
        let u = undo_set(&rp, &caps, &touched, Some(&se), None);
        assert_eq!(u.names, ["Setting"]);
        assert_eq!(u.not_restorable, ["Appeared", "ReadOnly", "ShootNoCard"], "reported, not silently dropped");
        // Everything in `names` is accepted by `resolve_only` against the restore point (this was the
        // whole undo being refused as `no_such_property` before).
        assert!(resolve_only(&u.names, &rp.properties).is_ok());
    }

    #[test]
    fn an_undo_set_name_the_restore_point_lacks_would_have_refused_the_whole_undo() {
        let rp = snap_z5(&[("Setting", 1, json!(1))]);
        let both = vec!["Setting".to_string(), "Appeared".to_string()];
        assert_eq!(resolve_only(&both, &rp.properties), Err(OnlyError::NoSuchProperty { requested: "Appeared".into() }));
    }

    #[test]
    fn a_setting_that_reads_differently_per_body_is_not_in_the_undo_set_when_the_undo_cannot_tell_it_is_the_same_body() {
        // ShootingLimit (33086) is restored onto the body's own snapshot, but an empty or `id-N`
        // serial identifies no body, so the undo would skip it as never_copied: listing it would make
        // the undo fail by construction. With a real serial it is listed.
        let touched: BTreeSet<String> = ["Limit".to_string()].into();
        let caps = [cap(33086, RW)];
        for (serial, listed) in [("0000003023668", true), ("", false), ("id-3", false)] {
            let mut rp = snap_z5(&[("Limit", 33086, json!(1))]);
            rp.camera.serial = serial.to_string();
            let u = undo_set(&rp, &caps, &touched, None, None);
            assert_eq!(u.names.contains(&"Limit".to_string()), listed, "serial {serial:?}: {u:?}");
            assert_eq!(u.not_restorable.contains(&"Limit".to_string()), !listed, "serial {serial:?}");
        }
    }

    #[test]
    fn a_range_the_apply_rescaled_is_not_in_the_undo_set_until_its_scale_is_back() {
        // The apply changed a setting that rescales ExposureComp (EV step): the read-back holds the
        // new scale, the restore point the old. Writing the old index onto the new scale is refused
        // by the undo (range_scale_differs), so listing it would fail the undo.
        let stepped = |steps: u64| json!({"value": 0.0, "default": 0.0, "value_index": 3, "default_index": 0, "lower": 0.0, "upper": 10.0, "steps": steps});
        let rp = snap_z5(&[("Comp", 1, stepped(11))]);
        let touched: BTreeSet<String> = ["Comp".to_string()].into();
        let caps = [cap(1, RW)];
        let same = snap_z5(&[("Comp", 1, stepped(11))]);
        assert_eq!(undo_set(&rp, &caps, &touched, None, Some(&same)).names, ["Comp"], "scale unchanged");
        let rescaled = snap_z5(&[("Comp", 1, stepped(21))]);
        let u = undo_set(&rp, &caps, &touched, None, Some(&rescaled));
        assert!(u.names.is_empty() && u.not_restorable == ["Comp"], "{u:?}");
        // A scalar is never judged by scale.
        let rp2 = snap_z5(&[("S", 1, json!(1))]);
        let after2 = snap_z5(&[("S", 1, json!(9))]);
        assert_eq!(undo_set(&rp2, &caps, &["S".to_string()].into(), None, Some(&after2)).names, ["S"]);
    }

    #[test]
    fn a_stored_range_with_no_usable_position_is_not_written_and_is_an_error() {
        // Stepped, 11 steps, index 11 is one past the end (or missing): its `value` is 0.0, so
        // writing it as stored would put the property at the bottom of the scale.
        let cam = FakeCamera::new().with(1, RW, stepped(100)).with(2, RW, stepped(100));
        let mut snap = snapshot_of(&[]);
        let mut past = stepped(612);
        past["value_index"] = json!(1225);
        let mut missing = stepped(612);
        missing.as_object_mut().unwrap().remove("value_index");
        snap.insert("A_Past", 1, past);
        snap.insert("B_Missing", 2, missing);
        let run = execute(&cam, &snap.properties, None, false);
        assert!(cam.writes().is_empty(), "{:?}", cam.writes());
        assert_eq!(run.counts().errors, 2);
        assert!(matches!(&run.steps[0].outcome, StepOutcome::WriteFailed(m) if m.contains("no usable position")));
    }

    #[test]
    fn an_empty_undo_set_stays_empty() {
        let rp = snap_z5(&[("Setting", 1, json!(1))]);
        let u = undo_set(&rp, &[cap(1, RW)], &BTreeSet::new(), None, None);
        assert_eq!(u, UndoSet::default());
    }

    #[test]
    fn a_read_back_with_no_baseline_is_judged_only_when_the_caller_waived_that() {
        // Z 5 has no baseline: the read-back cannot be held to one. A transplant's read-back treats
        // that as incomplete under Strict and complete under AllowPartial; a restore's must too.
        let cam = FakeCamera::new().with(1, RW, json!(0));
        let source = snap_z5(&[("A", 1, json!(10))]);
        let run = execute(&cam, &source.properties, None, false);
        let w = run.written_names();
        let strict = verify_with(&cam, &lookup_for(&[("A", 1)]), &cam_id(), &source, &w, "t", GatePolicy::Strict);
        assert_eq!(strict.verdict, Verdict::ReadbackIncomplete);
        let partial = verify_with(&cam, &lookup_for(&[("A", 1)]), &cam_id(), &source, &w, "t", GatePolicy::AllowPartial);
        assert_eq!(partial.verdict, Verdict::Verified);
    }

    #[test]
    fn writing_only_properties_that_cannot_be_compared_is_not_verified() {
        // `AngleLevel` is on the volatile list: written, but not compared. With nothing else compared
        // the read-back checked nothing, however empty the residual looks.
        let cam = FakeCamera::new().with(1, RW, json!(0));
        let source = snap_z5(&[("kNkMAIDCapability_AngleLevel", 1, json!(10))]);
        let run = execute(&cam, &source.properties, None, false);
        let v = verify(&cam, &lookup_for(&[("kNkMAIDCapability_AngleLevel", 1)]), &cam_id(), &source, &run.written_names(), "t");
        assert_eq!(v.verdict, Verdict::Unjudged);
        assert_eq!(v.residual.as_ref().unwrap().unjudged, ["kNkMAIDCapability_AngleLevel"]);
    }

    #[test]
    fn a_failed_priming_read_does_not_stop_the_write() {
        let cam = FakeCamera::new().with(1, RW, stepped(100)).failing_read(1, Failure::Call(-106));
        let mut snap = snapshot_of(&[]);
        snap.insert("A_Range", 1, stepped(612));
        let run = execute(&cam, &snap.properties, None, false);
        assert_eq!(run.counts().written, 1, "the read is a nudge, not a gate");
    }

    #[test]
    fn a_dry_run_writes_nothing_and_a_range_that_will_not_be_written_is_not_read() {
        let cam = FakeCamera::new().with(1, RW, stepped(100)).with(2, OP_GET, stepped(100));
        let mut snap = snapshot_of(&[]);
        snap.insert("A_Range", 1, stepped(612));
        snap.insert("B_ReadOnlyRange", 2, stepped(612));
        let dry = execute(&cam, &snap.properties, None, true);
        assert!(cam.writes().is_empty(), "a dry run writes nothing");
        // The writable range is read (to compare its scale with the stored one); the read-only one
        // is skipped before any read.
        assert_eq!(cam.reads(), [1]);
        assert_eq!(dry.counts().written, 1);
        // A range not selected by --only is not read either.
        let probe = FakeCamera::new().with(1, RW, stepped(100)).with(2, OP_GET, stepped(100));
        let only = BTreeSet::from(["B_ReadOnlyRange".to_string()]);
        let _ = execute(&probe, &snap.properties, Some(&only), false);
        assert!(probe.reads().is_empty(), "read-only: skipped, so neither read nor written");
    }

    #[test]
    fn a_continuous_range_is_read_for_its_scale_and_then_written() {
        let cont = json!({"value": 3.5, "value_index": 0, "lower": 0.0, "upper": 10.0, "steps": 0});
        let cam = FakeCamera::new().with(1, RW, cont.clone());
        let mut snap = snapshot_of(&[]);
        snap.insert("A_Cont", 1, cont);
        let run = execute(&cam, &snap.properties, None, false);
        assert_eq!(cam.reads(), [1]);
        assert_eq!(run.counts().written, 1);
    }

    #[test]
    fn a_range_whose_scale_changed_since_the_snapshot_is_left_alone_not_written_onto_the_new_scale() {
        // Stored on a 1225-step scale, now 751 steps (a firmware change, or a rescaling setting such
        // as the EV step): index 612 would mean a different value, so the write is refused.
        let now = json!({"value": 0.0, "default": 0.0, "value_index": 100, "default_index": 0, "lower": 2500.0, "upper": 10000.0, "steps": 751});
        for dry_run in [true, false] {
            let cam = FakeCamera::new().with(1, RW, now.clone());
            let mut snap = snapshot_of(&[]);
            snap.insert("A_Range", 1, stepped(612));
            let run = execute(&cam, &snap.properties, None, dry_run);
            assert_eq!(run.steps[0].outcome, StepOutcome::Skipped(RestoreSkip::RangeScaleDiffers), "dry_run={dry_run}");
            assert!(cam.writes().is_empty(), "dry_run={dry_run}");
            assert_eq!(run.counts().skipped_scale, 1);
            assert_eq!(cam.value(1).unwrap()["value_index"], json!(100), "untouched");
        }
        // A targeted restore that names it cannot do what was named.
        let cam = FakeCamera::new().with(1, RW, now);
        let mut snap = snapshot_of(&[]);
        snap.insert("A_Range", 1, stepped(612));
        let only = BTreeSet::from(["A_Range".to_string()]);
        let run = execute(&cam, &snap.properties, Some(&only), false);
        assert_eq!(run.unrestored(), [("A_Range".to_string(), "range_scale_differs")]);
    }

    #[test]
    fn the_same_scale_is_written_and_an_unreadable_scale_does_not_block_the_write() {
        let cam = FakeCamera::new().with(1, RW, stepped(100));
        let mut snap = snapshot_of(&[]);
        snap.insert("A_Range", 1, stepped(612));
        assert_eq!(execute(&cam, &snap.properties, None, false).counts().written, 1);
        // The read failing leaves nothing to compare; the write and the read-back judge it.
        let blind = FakeCamera::new().with(1, RW, stepped(100)).failing_read(1, Failure::Call(-106));
        assert_eq!(execute(&blind, &snap.properties, None, false).counts().written, 1);
    }

    // ── verify: read the body back after a restore ───────────────────────

    fn lookup_for(entries: &[(&str, u32)]) -> HashMap<u32, String> {
        entries.iter().map(|(n, c)| (*c, n.to_string())).collect()
    }

    fn cam_id() -> Camera {
        Camera { model: "Z 5".into(), serial: "1".into(), firmware: "1.00".into() }
    }

    fn snap_z5(entries: &[(&str, u32, Value)]) -> Snapshot {
        let mut s = Snapshot::new(cam_id(), Transport::Usb, "2026-10-27T12:00:00Z".into());
        for (n, c, v) in entries {
            s.insert(*n, *c, v.clone());
        }
        s
    }

    fn written(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_restore_that_took_is_verified() {
        let cam = FakeCamera::new().with(1, RW, json!(0)).with(2, RW, json!(0));
        let source = snap_z5(&[("A", 1, json!(10)), ("B", 2, json!(20))]);
        let run = execute(&cam, &source.properties, None, false);
        let v = verify(&cam, &lookup_for(&[("A", 1), ("B", 2)]), &cam_id(), &source, &run.written_names(), "t");
        assert_eq!(v.verdict, Verdict::Verified);
        assert!(v.residual.as_ref().unwrap().is_empty());
        assert!(matches!(v.gate, Some(GateOutcome::NoBaseline(_))), "Z 5 has no baseline: compared, not judged for completeness");
    }

    #[test]
    fn a_body_that_accepts_and_ignores_the_write_is_reported_not_verified() {
        // The F18 failure: written=1 errors=0, and nothing changed.
        let cam = FakeCamera::new().with(1, RW, json!(0)).with(2, RW, json!(0)).ignoring_writes(2);
        let source = snap_z5(&[("A", 1, json!(10)), ("B", 2, json!(20))]);
        let run = execute(&cam, &source.properties, None, false);
        assert_eq!(run.counts().errors, 0, "the write call said success");
        assert_eq!(run.counts().written, 2);
        let v = verify(&cam, &lookup_for(&[("A", 1), ("B", 2)]), &cam_id(), &source, &run.written_names(), "t");
        assert_eq!(v.verdict, Verdict::ResidualFound);
        assert_eq!(v.residual.unwrap().changed, ["B"]);
    }

    #[test]
    fn a_stepped_range_survives_a_body_that_ignores_blind_range_writes_because_it_is_read_first() {
        let cam = FakeCamera::new().with(1, RW, stepped(613)).ignoring_blind_writes(1).with_sdk_range_quirk();
        let source = {
            let mut s = snap_z5(&[]);
            s.insert("WBTuneAuto", 1, stepped(612));
            s
        };
        let run = execute(&cam, &source.properties, None, false);
        assert_eq!(cam.ignored_writes().len(), 0, "the priming read means no write was blind");
        let v = verify(&cam, &lookup_for(&[("WBTuneAuto", 1)]), &cam_id(), &source, &run.written_names(), "t");
        assert_eq!(v.verdict, Verdict::Verified);
        assert_eq!(cam.value(1).unwrap()["value_index"], json!(612));
    }

    #[test]
    fn without_the_priming_read_the_same_body_is_caught_by_the_verification() {
        // Drive the blind write by hand (what restore did before 626950c) and verify.
        let cam = FakeCamera::new().with(1, RW, stepped(613)).ignoring_blind_writes(1).with_sdk_range_quirk();
        let source = {
            let mut s = snap_z5(&[]);
            s.insert("WBTuneAuto", 1, stepped(612));
            s
        };
        cam.write_capability(1, 0, &coherent_for_write(&source.properties["WBTuneAuto"].value)).unwrap();
        let v = verify(&cam, &lookup_for(&[("WBTuneAuto", 1)]), &cam_id(), &source, &written(&["WBTuneAuto"]), "t");
        assert_eq!(v.verdict, Verdict::ResidualFound, "success from the write call was not trusted");
        assert_eq!(v.residual.unwrap().changed, ["WBTuneAuto"]);
    }

    #[test]
    fn nothing_written_means_nothing_to_check_and_the_body_is_not_read() {
        let cam = FakeCamera::new().with(1, RW, json!(0));
        let source = snap_z5(&[("A", 1, json!(10))]);
        let v = verify(&cam, &lookup_for(&[("A", 1)]), &cam_id(), &source, &written(&[]), "t");
        assert_eq!(v.verdict, Verdict::NothingWritten);
        assert!(cam.reads().is_empty() && v.stats.is_none() && v.gate.is_none() && v.residual.is_none());
    }

    #[test]
    fn a_read_back_that_reads_nothing_claims_nothing() {
        let cam = FakeCamera::new().with_unreadable(1, OP_GET);
        let source = snap_z5(&[("A", 1, json!(10))]);
        let v = verify(&cam, &lookup_for(&[("A", 1)]), &cam_id(), &source, &written(&["A"]), "t");
        assert_eq!(v.verdict, Verdict::ReadbackIncomplete);
        assert!(v.residual.is_none() && v.gate.is_none());
        assert_eq!(v.stats.unwrap().read_ok, 0);
    }

    #[test]
    fn an_incomplete_read_back_on_a_baselined_model_claims_nothing() {
        // A Z 9 read back with most of its capabilities missing: comparing would be meaningless.
        let b = crate::ops::baseline::baseline_for("Z 9").unwrap();
        let mut cam = FakeCamera::new();
        let mut names = HashMap::new();
        for c in b.readable.iter().take(18) {
            cam = cam.with(c.code, RW, json!(1));
            names.insert(c.code, c.name.clone());
        }
        let z9 = Camera { model: "Z 9".into(), serial: "S".into(), firmware: "5.31".into() };
        let mut source = Snapshot::new(z9.clone(), Transport::Usb, "t".into());
        source.insert(b.readable[0].name.clone(), b.readable[0].code, json!(1));
        let v = verify(&cam, &names, &z9, &source, &written(&[&b.readable[0].name]), "t");
        assert_eq!(v.verdict, Verdict::ReadbackIncomplete);
        assert!(matches!(v.gate, Some(GateOutcome::Failed { .. })));
        assert!(v.residual.is_none(), "an incomplete read-back is never compared");
    }

    #[test]
    fn a_snapshot_of_another_model_cannot_be_judged_against_this_body() {
        let cam = FakeCamera::new().with(1, RW, json!(10));
        let mut source = snap_z5(&[("A", 1, json!(10))]);
        source.camera.model = "Z6_3".into();
        let v = verify(&cam, &lookup_for(&[("A", 1)]), &cam_id(), &source, &written(&["A"]), "t");
        assert_eq!(v.verdict, Verdict::ReadbackIncomplete, "never reported as verified");
        assert!(v.residual.is_none());
    }

    #[test]
    fn only_written_properties_are_judged() {
        // B differs from the snapshot but was not written (read-only on the body): not residual.
        let cam = FakeCamera::new().with(1, RW, json!(0)).with(2, OP_GET, json!(999));
        let source = snap_z5(&[("A", 1, json!(10)), ("B", 2, json!(20))]);
        let run = execute(&cam, &source.properties, None, false);
        let v = verify(&cam, &lookup_for(&[("A", 1), ("B", 2)]), &cam_id(), &source, &run.written_names(), "t");
        assert_eq!(v.verdict, Verdict::Verified);
    }

    #[test]
    fn verdict_tags_are_closed_and_distinct() {
        let all = [Verdict::Verified, Verdict::ResidualFound, Verdict::ReadbackIncomplete, Verdict::NothingWritten, Verdict::Unjudged];
        let tags: BTreeSet<_> = all.iter().map(Verdict::tag).collect();
        assert_eq!(tags.len(), 5);
        for v in all {
            assert_eq!(serde_json::to_value(v).unwrap(), json!(v.tag()));
        }
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
    fn the_undo_command_is_a_command_up_to_twelve_names_and_prose_after() {
        let names = |n: usize| (0..n).map(|i| format!("kNkMAIDCapability_P{i}")).collect::<Vec<_>>();
        assert_eq!(undo_command_exact("rp.json", &[], None), None, "nothing to undo");
        let one = undo_command_exact("rp.json", &names(1), None).unwrap();
        assert_eq!(one, "nikon-fleet restore rp.json --only P0");
        assert_eq!(undo_command("rp.json", &names(1)), one, "the printed command is the exact one");
        assert!(undo_command_exact("rp.json", &names(12), None).is_some(), "twelve is the last that is listed");
        assert_eq!(undo_command_exact("rp.json", &names(13), None), None, "thirteen is prose, not a command");
        assert!(undo_command("rp.json", &names(13)).starts_with("13 properties to put back"));
        assert_eq!(undo_command("rp.json", &[]), "nothing to undo");
        // the data directory is named when it is given, quoted when it has a space
        assert_eq!(undo_command_exact("rp.json", &names(1), Some("/d")).unwrap(), "nikon-fleet --data-dir /d restore rp.json --only P0");
        assert!(undo_command_exact("rp.json", &names(1), Some("/my data")).unwrap().contains("--data-dir \"/my data\" restore"));
    }

    #[test]
    fn every_verdict_has_a_line_and_only_a_residual_is_a_failure() {
        use crate::ops::readback::Residual;
        let residual = Residual { changed: vec!["A".into()], missing: vec!["B".into(), "C".into()], unjudged: vec![], judged: 3 };
        let mut lines = std::collections::BTreeSet::new();
        for v in Verdict::ALL {
            let line = readback_line(v, Some(&residual));
            assert!(line.starts_with("Read-back:"), "{line}");
            assert_eq!(v.is_failure(), v == Verdict::ResidualFound, "{}", v.tag());
            lines.insert(line);
        }
        assert_eq!(lines.len(), 5, "five different sentences");
        let found = readback_line(Verdict::ResidualFound, Some(&residual));
        assert!(found.contains("3 written properties do") && found.contains("A (differs)") && found.contains("C (not readable)"), "{found}");
        assert!(readback_line(Verdict::ResidualFound, Some(&Residual { changed: vec!["A".into()], ..Residual::default() })).contains("1 written property does"));
        // the verdicts the JSON carries are the ones named here
        for v in Verdict::ALL {
            assert_eq!(serde_json::from_value::<Verdict>(serde_json::to_value(v).unwrap()).unwrap(), v);
        }
    }

}

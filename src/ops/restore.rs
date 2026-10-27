//! Device-free parts of `restore`: which snapshot properties a restore touches.
//!
//! `restore` writes every snapshotted property that is not on the never-copy
//! table, so anything changed on the camera after the snapshot was taken is
//! overwritten. A targeted restore (`--only`) writes back just the named
//! properties and leaves everything else alone; that is the undo of a
//! transplant (docs/mcp-plan.md §5.6): restore the properties the apply wrote
//! (and the ones it changed indirectly) from the restore point, and nothing
//! the user changed since.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Serialize, Serializer};
use serde_json::{Value, json};

use crate::ops::port::CameraPort;
use crate::sdk::{CapabilityInfo, OP_SET, SdkError};
use crate::snapshot::PropertyEntry;
use crate::transplant;

/// Why a snapshot property is not written back by `restore`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreSkip {
    /// On [`transplant::NEVER_TRANSPLANT`]: it reports body/session state (or
    /// locks it) rather than holding a setting. The same table `transplant`
    /// uses, so the two commands cannot disagree about what is state. A
    /// restore point must not generate errors for properties the camera
    /// refuses to set (`CurrentItemID` and `LiveViewStatus` failed -125 on
    /// every full restore of a Z6III), or `errors == 0` stops meaning anything.
    NeverCopied,
    /// Capability code not present on this camera.
    Absent,
    /// Present but `OP_SET` is not allowed.
    ReadOnly,
}

impl RestoreSkip {
    /// Closed machine-readable tag. Exhaustive match.
    pub fn tag(&self) -> &'static str {
        match self {
            RestoreSkip::NeverCopied => "never_copied",
            RestoreSkip::Absent => "absent_on_target",
            RestoreSkip::ReadOnly => "read_only",
        }
    }
}

/// The device-free checks `restore` applies before writing a property, in
/// order: the policy skip first (so a state property is reported as such even
/// on a body that lacks it), then presence, then writability.
pub fn restore_precheck<'a>(
    cap_map: &HashMap<u32, &'a CapabilityInfo>,
    code: u32,
) -> Result<&'a CapabilityInfo, RestoreSkip> {
    if transplant::never_copied(code).is_some() {
        return Err(RestoreSkip::NeverCopied);
    }
    let cap = *cap_map.get(&code).ok_or(RestoreSkip::Absent)?;
    if cap.operations & OP_SET == 0 {
        return Err(RestoreSkip::ReadOnly);
    }
    Ok(cap)
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

const PREFIX: &str = "kNkMAIDCapability_";

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
        let full = if properties.contains_key(r.as_str()) { r.clone() } else { format!("{PREFIX}{r}") };
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
/// dropped: the result has one entry per property.
pub fn plan<'a>(
    properties: &'a BTreeMap<String, PropertyEntry>,
    only: Option<&BTreeSet<String>>,
    cap_map: &HashMap<u32, &'a CapabilityInfo>,
) -> Vec<PlannedRestore<'a>> {
    properties
        .iter()
        .map(|(name, prop)| {
            let decision = if only.is_some_and(|set| !set.contains(name)) {
                Decision::NotSelected
            } else {
                match restore_precheck(cap_map, prop.code) {
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
    let stepped = stored.get("lower").is_some() && stored.get("steps").and_then(Value::as_u64).unwrap_or(0) >= 2;
    if !stepped {
        return stored.clone();
    }
    let mut out = stored.clone();
    if let Some(v) = transplant::range_value(stored) {
        out["value"] = json!(v);
    }
    out
}

/// Run a restore of `properties` onto `port`: [`plan`], then write each
/// `Write` decision (or only report it, if `dry_run`). A failed write does not
/// stop the rest.
pub fn execute<P: CameraPort + ?Sized>(
    port: &P,
    properties: &BTreeMap<String, PropertyEntry>,
    only: Option<&BTreeSet<String>>,
    dry_run: bool,
) -> RestoreRun {
    let cap_map: HashMap<u32, &CapabilityInfo> = port.capabilities().iter().map(|c| (c.id, c)).collect();
    let steps = plan(properties, only, &cap_map)
        .into_iter()
        .map(|step| {
            let outcome = match step.decision {
                Decision::NotSelected => StepOutcome::NotSelected,
                Decision::Skip(skip) => StepOutcome::Skipped(skip),
                Decision::Write(_) if dry_run => StepOutcome::WouldWrite,
                Decision::Write(cap) => match port.write_capability(step.code, cap.kind, &coherent_for_write(&properties[step.name].value)) {
                    Ok(()) => StepOutcome::Written,
                    Err(SdkError::UnsupportedWrite(_)) => StepOutcome::UnsupportedWriteType,
                    Err(e) => StepOutcome::WriteFailed(e.to_string()),
                },
            };
            StepResult { name: step.name.to_string(), code: step.code, outcome }
        })
        .collect();
    RestoreRun { steps }
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
}

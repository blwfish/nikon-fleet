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

use crate::sdk::{CapabilityInfo, OP_SET};
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
}

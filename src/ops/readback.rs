//! Judging a write by reading the camera back.
//!
//! A process exit status cannot say what a write did (F3, F4, F10 in
//! docs/mcp-hardware-findings.md): the apply result comes from a read-back
//! capture compared with what was meant to happen. Two questions, both pure
//! functions of snapshots held in memory:
//!
//! - [`side_effects`]: after a transplant, what changed that the transplant did
//!   not write? Writing one property can change others (dependent options,
//!   locked settings); those are part of what an undo has to put back.
//! - [`residual`]: after a restore, which of the properties it wrote still
//!   differ from the snapshot it restored from?
//!
//! Both leave out what is expected to differ without meaning anything: the
//! volatile list (live sensors, clocks), the enum `default_index` the SDK
//! leaves uninitialised on some bodies, and the never-copy state properties.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::diff::{DiffError, DiffOptions, is_volatile, values_differ};
use crate::snapshot::Snapshot;
use crate::transplant::never_copied;

fn same_camera_model(a: &Snapshot, b: &Snapshot) -> Result<(), DiffError> {
    if a.camera.model != b.camera.model {
        return Err(DiffError::ModelMismatch { model_a: a.camera.model.clone(), model_b: b.camera.model.clone() });
    }
    Ok(())
}

/// Properties that changed without being written. All three lists are sorted
/// by name and exclude volatile and never-copy properties.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct SideEffects {
    /// In both snapshots, with a different value in the read-back.
    pub changed: Vec<String>,
    /// In the restore point but not readable in the read-back.
    pub vanished: Vec<String>,
    /// Readable in the read-back but not in the restore point.
    pub appeared: Vec<String>,
}

impl SideEffects {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.vanished.is_empty() && self.appeared.is_empty()
    }

    /// Every affected name, sorted and unique: what a targeted undo must
    /// include besides the properties the apply wrote.
    pub fn names(&self) -> BTreeSet<String> {
        self.changed.iter().chain(&self.vanished).chain(&self.appeared).cloned().collect()
    }
}

fn skipped(name: &str, code: u32) -> bool {
    is_volatile(name) || never_copied(code).is_some()
}

/// What differs between `restore_point` and `readback` besides `written`.
pub fn side_effects(
    restore_point: &Snapshot,
    readback: &Snapshot,
    written: &BTreeSet<String>,
) -> Result<SideEffects, DiffError> {
    same_camera_model(restore_point, readback)?;
    let opts = DiffOptions::default();
    let mut out = SideEffects::default();
    for (name, before) in &restore_point.properties {
        if written.contains(name) || skipped(name, before.code) {
            continue;
        }
        match readback.properties.get(name) {
            Some(after) => {
                if values_differ(&before.value, &after.value, &opts) {
                    out.changed.push(name.clone());
                }
            }
            None => out.vanished.push(name.clone()),
        }
    }
    for (name, after) in &readback.properties {
        if !restore_point.properties.contains_key(name) && !written.contains(name) && !skipped(name, after.code) {
            out.appeared.push(name.clone());
        }
    }
    Ok(out)
}

/// Written properties that did not end up as the snapshot says.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Residual {
    /// Read back with a different value.
    pub changed: Vec<String>,
    /// Not readable in the read-back at all.
    pub missing: Vec<String>,
    /// Written, but not compared: the name is on the volatile list (a substring match, so a real
    /// setting can land here), the property is never-copy, or the snapshot does not hold it.
    /// `verified` says nothing about these.
    pub unjudged: Vec<String>,
    /// How many written properties were compared at all. Zero means nothing was checked, however
    /// empty the other lists are.
    pub judged: usize,
}

impl Residual {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.missing.is_empty()
    }
}

/// Of the properties a restore reports it `written`, which still differ from
/// `source` (the snapshot restored from) in `readback`. Properties the
/// restore did not write are not judged: a full restore skips many read-only
/// and state properties that legitimately differ.
pub fn residual(
    source: &Snapshot,
    readback: &Snapshot,
    written: &BTreeSet<String>,
) -> Result<Residual, DiffError> {
    same_camera_model(source, readback)?;
    let opts = DiffOptions::default();
    let mut out = Residual::default();
    for name in written {
        let Some(want) = source.properties.get(name) else {
            out.unjudged.push(name.clone());
            continue;
        };
        if skipped(name, want.code) {
            out.unjudged.push(name.clone());
            continue;
        }
        out.judged += 1;
        match readback.properties.get(name) {
            Some(got) => {
                if values_differ(&want.value, &got.value, &opts) {
                    out.changed.push(name.clone());
                }
            }
            None => out.missing.push(name.clone()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Camera, Transport};
    use crate::transplant::NEVER_TRANSPLANT;
    use serde_json::{Value, json};

    fn snap(props: &[(&str, u32, Value)]) -> Snapshot {
        let mut s = Snapshot::new(
            Camera { model: "Z 9".into(), serial: "1".into(), firmware: "5.31".into() },
            Transport::Usb,
            "2026-10-27T12:00:00Z".into(),
        );
        for (n, c, v) in props {
            s.insert(*n, *c, v.clone());
        }
        s
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn enum_val(idx: u64, default: u64) -> Value {
        json!({"value_index": idx, "default_index": default, "elem_count": 2, "elem_type": 2, "elem_bytes": 4, "values": [0, 1]})
    }

    // ── side_effects ─────────────────────────────────────────────────────

    #[test]
    fn nothing_changed_means_no_side_effects() {
        let a = snap(&[("A", 1, json!(1)), ("B", 2, json!(2))]);
        assert!(side_effects(&a, &a.clone(), &set(&[])).unwrap().is_empty());
    }

    #[test]
    fn the_written_property_changing_is_expected_not_a_side_effect() {
        let before = snap(&[("A", 1, json!(1)), ("B", 2, json!(2))]);
        let after = snap(&[("A", 1, json!(9)), ("B", 2, json!(2))]);
        assert!(side_effects(&before, &after, &set(&["A"])).unwrap().is_empty());
    }

    #[test]
    fn an_unwritten_property_that_changed_is_reported() {
        let before = snap(&[("A", 1, json!(1)), ("B", 2, json!(2))]);
        let after = snap(&[("A", 1, json!(9)), ("B", 2, json!(3))]);
        let se = side_effects(&before, &after, &set(&["A"])).unwrap();
        assert_eq!(se.changed, ["B"]);
        assert!(se.vanished.is_empty() && se.appeared.is_empty());
    }

    #[test]
    fn written_but_unchanged_is_fine_and_names_not_in_the_snapshots_are_harmless() {
        let before = snap(&[("A", 1, json!(1))]);
        assert!(side_effects(&before, &before.clone(), &set(&["A", "NotThere"])).unwrap().is_empty());
    }

    #[test]
    fn vanished_and_appeared_are_reported_separately() {
        let before = snap(&[("A", 1, json!(1)), ("Gone", 2, json!(2))]);
        let after = snap(&[("A", 1, json!(1)), ("New", 3, json!(3))]);
        let se = side_effects(&before, &after, &set(&[])).unwrap();
        assert_eq!((se.vanished.as_slice(), se.appeared.as_slice()), (&["Gone".to_string()][..], &["New".to_string()][..]));
        assert_eq!(se.names(), set(&["Gone", "New"]));
    }

    #[test]
    fn a_written_property_that_only_appears_in_the_readback_is_not_a_side_effect() {
        // The transplant wrote it; it just was not readable before.
        let before = snap(&[("A", 1, json!(1))]);
        let after = snap(&[("A", 1, json!(1)), ("W", 2, json!(5))]);
        assert!(side_effects(&before, &after, &set(&["W"])).unwrap().is_empty());
        assert_eq!(side_effects(&before, &after, &set(&[])).unwrap().appeared, ["W"]);
    }

    #[test]
    fn is_empty_is_false_if_any_one_list_has_something() {
        let only = |c: &[&str], v: &[&str], a: &[&str]| SideEffects {
            changed: c.iter().map(|s| s.to_string()).collect(),
            vanished: v.iter().map(|s| s.to_string()).collect(),
            appeared: a.iter().map(|s| s.to_string()).collect(),
        };
        assert!(only(&[], &[], &[]).is_empty());
        assert!(!only(&["x"], &[], &[]).is_empty());
        assert!(!only(&[], &["x"], &[]).is_empty());
        assert!(!only(&[], &[], &["x"]).is_empty());
        assert!(Residual::default().is_empty());
        assert!(!Residual { changed: vec!["x".into()], ..Default::default() }.is_empty());
        assert!(!Residual { missing: vec!["x".into()], ..Default::default() }.is_empty());
    }

    #[test]
    fn volatile_never_copy_and_default_index_noise_are_not_side_effects() {
        let never = &NEVER_TRANSPLANT[0];
        let before = snap(&[
            ("kNkMAIDCapability_AngleLevel", 10, json!(1)),
            ("kNkMAIDCapability_DateTime", 11, json!("a")),
            (never.1, never.0, json!(0)),
            ("E", 12, enum_val(1, 7)),
        ]);
        let after = snap(&[
            ("kNkMAIDCapability_AngleLevel", 10, json!(2)),
            ("kNkMAIDCapability_DateTime", 11, json!("b")),
            (never.1, never.0, json!(1)),
            ("E", 12, enum_val(1, 99999)), // stale-heap default_index, same selection
        ]);
        assert!(side_effects(&before, &after, &set(&[])).unwrap().is_empty());
    }

    #[test]
    fn an_enum_whose_selection_changed_is_a_side_effect_even_with_the_same_default() {
        let before = snap(&[("E", 12, enum_val(0, 0))]);
        let after = snap(&[("E", 12, enum_val(1, 0))]);
        assert_eq!(side_effects(&before, &after, &set(&[])).unwrap().changed, ["E"]);
    }

    #[test]
    fn side_effects_are_sorted_and_names_merges_all_three() {
        let before = snap(&[("C", 3, json!(1)), ("A", 1, json!(1)), ("B", 2, json!(1)), ("Gone", 4, json!(1))]);
        let after = snap(&[("C", 3, json!(2)), ("A", 1, json!(2)), ("B", 2, json!(1)), ("New", 5, json!(1))]);
        let se = side_effects(&before, &after, &set(&[])).unwrap();
        assert_eq!(se.changed, ["A", "C"]);
        assert_eq!(se.names(), set(&["A", "C", "Gone", "New"]));
    }

    #[test]
    fn side_effects_refuse_two_different_models() {
        let a = snap(&[("A", 1, json!(1))]);
        let mut b = a.clone();
        b.camera.model = "Z6_3".into();
        assert!(matches!(side_effects(&a, &b, &set(&[])), Err(DiffError::ModelMismatch { .. })));
    }

    // ── residual ─────────────────────────────────────────────────────────

    #[test]
    fn a_restore_that_took_leaves_no_residual() {
        let source = snap(&[("A", 1, json!(1)), ("B", 2, json!(2))]);
        assert!(residual(&source, &source.clone(), &set(&["A", "B"])).unwrap().is_empty());
    }

    #[test]
    fn a_written_property_that_did_not_take_is_residual() {
        let source = snap(&[("A", 1, json!(1)), ("B", 2, json!(2))]);
        let readback = snap(&[("A", 1, json!(1)), ("B", 2, json!(99))]);
        let r = residual(&source, &readback, &set(&["A", "B"])).unwrap();
        assert_eq!((r.changed.as_slice(), r.missing.is_empty()), (&["B".to_string()][..], true));
    }

    #[test]
    fn a_written_property_missing_from_the_readback_is_missing_not_changed() {
        let source = snap(&[("A", 1, json!(1)), ("B", 2, json!(2))]);
        let readback = snap(&[("A", 1, json!(1))]);
        let r = residual(&source, &readback, &set(&["A", "B"])).unwrap();
        assert_eq!((r.changed.is_empty(), r.missing.as_slice()), (true, &["B".to_string()][..]));
    }

    #[test]
    fn properties_not_written_are_not_judged() {
        // A read-only property legitimately differs from the snapshot.
        let source = snap(&[("A", 1, json!(1)), ("ReadOnly", 2, json!(2))]);
        let readback = snap(&[("A", 1, json!(1)), ("ReadOnly", 2, json!(50))]);
        assert!(residual(&source, &readback, &set(&["A"])).unwrap().is_empty());
        assert!(residual(&source, &readback, &set(&[])).unwrap().is_empty(), "nothing written, nothing judged");
    }

    #[test]
    fn a_written_property_on_the_volatile_list_is_reported_unjudged_not_silently_verified() {
        // `AutoFocusPositionPreset` is a real setting that the substring list flags volatile
        // (diff.rs pins the false positive). If the body ignored its write, the verdict cannot
        // say so; it must at least say it did not look.
        let source = snap(&[("kNkMAIDCapability_AutoFocusPositionPreset", 1, json!(1)), ("A", 2, json!(1))]);
        let readback = snap(&[("kNkMAIDCapability_AutoFocusPositionPreset", 1, json!(9)), ("A", 2, json!(1))]);
        let r = residual(&source, &readback, &set(&["kNkMAIDCapability_AutoFocusPositionPreset", "A"])).unwrap();
        assert!(r.is_empty(), "unjudged is not a difference");
        assert_eq!(r.unjudged, ["kNkMAIDCapability_AutoFocusPositionPreset"]);
        assert_eq!(r.judged, 1, "A was compared");
    }

    #[test]
    fn a_written_name_the_source_does_not_have_is_unjudged_not_dropped() {
        let source = snap(&[("A", 1, json!(1))]);
        let r = residual(&source, &source.clone(), &set(&["Nope"])).unwrap();
        assert!(r.is_empty());
        assert_eq!((r.unjudged.as_slice(), r.judged), (&["Nope".to_string()][..], 0));
    }

    #[test]
    fn residual_ignores_default_index_noise_but_not_a_changed_selection() {
        let source = snap(&[("E", 12, enum_val(1, 7))]);
        assert!(residual(&source, &snap(&[("E", 12, enum_val(1, 4242))]), &set(&["E"])).unwrap().is_empty());
        assert_eq!(residual(&source, &snap(&[("E", 12, enum_val(0, 7))]), &set(&["E"])).unwrap().changed, ["E"]);
    }

    #[test]
    fn residual_skips_volatile_and_never_copy_even_if_named() {
        let never = &NEVER_TRANSPLANT[0];
        let source = snap(&[("kNkMAIDCapability_AngleLevel", 10, json!(1)), (never.1, never.0, json!(0))]);
        let readback = snap(&[("kNkMAIDCapability_AngleLevel", 10, json!(5)), (never.1, never.0, json!(1))]);
        let written = set(&["kNkMAIDCapability_AngleLevel", never.1]);
        assert!(residual(&source, &readback, &written).unwrap().is_empty());
    }

    #[test]
    fn residual_refuses_two_different_models() {
        let a = snap(&[("A", 1, json!(1))]);
        let mut b = a.clone();
        b.camera.model = "Z6_3".into();
        assert!(matches!(residual(&a, &b, &set(&["A"])), Err(DiffError::ModelMismatch { .. })));
    }

    #[test]
    fn json_shapes() {
        let se = SideEffects { changed: vec!["A".into()], vanished: vec![], appeared: vec!["N".into()] };
        assert_eq!(serde_json::to_value(&se).unwrap(), json!({"changed": ["A"], "vanished": [], "appeared": ["N"]}));
        let r = Residual { changed: vec![], missing: vec!["M".into()], ..Default::default() };
        assert_eq!(serde_json::to_value(&r).unwrap(), json!({"changed": [], "missing": ["M"], "unjudged": [], "judged": 0}));
    }
}

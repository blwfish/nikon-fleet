//! The one table of per-capability-code policy.
//!
//! What may be done with a setting is a fact about its capability code, and it used to live in
//! three parallel tables (`transplant::NEVER_TRANSPLANT`, `transplant::NEVER_TRANSLATED`,
//! `range_policy::VERIFIED_RANGES`) plus a name-alias list, joined by tests that each checked one
//! pair. Here every code has at most one [`CodePolicy`] record, so a code cannot be both "never
//! copied" and "verified for a range copy": there is nowhere to write the second one. Everything
//! that asks (transplant's precheck, restore's precheck, the read-back's volatile test, the range
//! gate, the name guard) reads this table through the functions below.
//!
//! Evidence is by name and by behaviour, not by anything in `MaidLayer.config` (it does not
//! classify state vs setting): `LiveViewStatus` and `CurrentItemID` already failed with
//! `SetCapability` -125 on a real Z6III restore. Judgement calls are marked in the reason; add a
//! record rather than special-casing elsewhere.
//!
//! The rules:
//!
//! - [`CopyRule::NeverCopy`]: reports body/session state (or locks it), or is a read-out, rather
//!   than holding a setting. Neither `transplant` nor `restore` writes it, with no override.
//! - [`CopyRule::NeverTranslate`]: a real setting that a `transplant` between bodies never
//!   writes, but a `restore` of the same body still brings back (a restore of another body's
//!   snapshot skips it too: `ops::restore::Source::OtherBody`). The trouble is only in carrying a
//!   value from one body to another, because this SDK reads the same menu setting as a different
//!   position on different bodies.
//! - [`CopyRule::RangeVerified`]: a numeric range that may be copied between two different models
//!   when both are listed. Default-deny: a code not here, or a model not captured yet, is skipped
//!   as `range_unverified`. What "verified" means, exactly, and no more: the range's `lower`,
//!   `upper`, `steps` and `default_index` were read identical in a capture from each listed model
//!   (the evidence tests in `range_policy` check all four). That rules out a different scale; it
//!   does **not** show that each position means the same thing, which only a person at the
//!   camera can show. To add a model, capture it, compare every range against the listed models
//!   (`docs/verify-a-model.md`), and add the model to each range that matches.
//! - [`CopyRule::Open`]: no restriction beyond the general gates; the record exists only to carry
//!   the name aliases.
//!
//! `aliases` is one equivalence class of symbolic names for the code that legitimately differs
//! between Z-series models while meaning the same setting (the name guard accepts a code whose
//! two models' names are both in the class).

/// What a transplant or restore may do with a capability code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyRule {
    /// No restriction beyond the general gates.
    Open,
    /// Never written by `transplant` or `restore`.
    NeverCopy { why: &'static str },
    /// Never carried from one body to another; a restore of the same body still writes it.
    NeverTranslate { why: &'static str },
    /// A range that may be copied between two different models when both are listed.
    RangeVerified { models: &'static [&'static str] },
}

/// One capability code's policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodePolicy {
    pub code: u32,
    /// The canonical symbolic name (`kNkMAIDCapability_...`), checked against the real config.
    pub name: &'static str,
    pub rule: CopyRule,
    /// Other names the code carries on some Z-series models (empty unless there are any).
    pub aliases: &'static [&'static str],
}

impl CodePolicy {
    pub const fn never_copy(code: u32, name: &'static str, why: &'static str) -> Self {
        CodePolicy { code, name, rule: CopyRule::NeverCopy { why }, aliases: &[] }
    }

    pub const fn never_translate(code: u32, name: &'static str, why: &'static str) -> Self {
        CodePolicy { code, name, rule: CopyRule::NeverTranslate { why }, aliases: &[] }
    }

    pub const fn range_verified(code: u32, name: &'static str, models: &'static [&'static str]) -> Self {
        CodePolicy { code, name, rule: CopyRule::RangeVerified { models }, aliases: &[] }
    }

    /// An otherwise unrestricted code whose symbolic name differs between models. `names` is the
    /// whole equivalence class (it includes `name`).
    pub const fn aliased(code: u32, name: &'static str, names: &'static [&'static str]) -> Self {
        CodePolicy { code, name, rule: CopyRule::Open, aliases: names }
    }
}

/// Models whose ranges were captured for the first table (2026-10-28, F25).
/// Names are the strings in a snapshot's `camera.model`.
const ALL_THREE: &[&str] = &["Z 5", "Z 9", "Z6_3"];
/// The six `MovieWBTunePreset*` ranges are not present on the Z5.
const Z9_AND_Z6III: &[&str] = &["Z 9", "Z6_3"];

/// The table. One record per code (a test enforces it). The 33200 alias: `BracketingType` on most
/// bodies, `AEBracketingType` on the Z 8 and Z 9. Confirmed 2026-10-27 against live Z 9 (fw 5.31),
/// Z6_3 (fw 2.00) and Z 5 (fw 1.43): the Z 9 and Z6_3 offer the same option set {4..8} in
/// different orders (so a value, not an index, must be matched, which `translate_enum` does), the
/// Z 5 offers {0..8}, all three default to code 4, and the config's own display label is
/// "AEBracketing Type" on the Z6_3, Z 8 and Z 9 alike. What each option code *means* in the menu
/// is not independently confirmed.
pub const CODE_POLICY: &[CodePolicy] = &[
    CodePolicy::never_copy(33089, "kNkMAIDCapability_LockCamera",
        "camera lock state; copying it could lock or unlock the target"),
    CodePolicy::never_copy(33342, "kNkMAIDCapability_LiveViewStatus",
        "whether live view is running; rejected by a real Z6III write (-125)"),
    CodePolicy::never_copy(33617, "kNkMAIDCapability_CurrentItemID",
        "the item currently selected in playback; rejected by a real Z6III write (-125)"),
    CodePolicy::never_copy(33213, "kNkMAIDCapability_ExternalFlashStatus",
        "what is attached to the hot shoe right now"),
    CodePolicy::never_copy(33876, "kNkMAIDCapability_RawJpegTransferStatus",
        "state of an in-progress transfer"),
    CodePolicy::never_copy(34080, "kNkMAIDCapability_ChangeMonitorOffStatus",
        "monitor power state (judgement call: status by name)"),
    CodePolicy::never_copy(33606, "kNkMAIDCapability_LiveViewPhotoShootingMode",
        "live-view session mode (judgement call: state by name)"),
    CodePolicy::never_copy(33165, "kNkMAIDCapability_ShootingMode",
        "exposure mode, set by the body's mode dial (judgement call)"),
    CodePolicy::never_copy(34147, "kNkMAIDCapability_Viewmode",
        "which display the body is showing (judgement call)"),
    CodePolicy::never_copy(33044, "kNkMAIDCapability_FlexibleProgram",
        "program shift: a transient exposure shift the body resets, not a stored setting (judgement call, like ShootingMode)"),
    CodePolicy::never_copy(33059, "kNkMAIDCapability_FocalLength",
        "the mounted lens's focal length, a read-out: a range with no span (0..0 in 0 steps on all three bodies), so there is nothing to set"),
    CodePolicy::never_copy(33108, "kNkMAIDCapability_ShootNoCard",
        "flipped on its own across a power cycle with nothing writing to the camera, so state-like or reset by the body (judgement call, hardware observation F10)"),
    CodePolicy::never_translate(33086, "kNkMAIDCapability_ShootingLimit",
        "max shots per burst (Custom Setting d2): the same setting reads as different positions on different bodies, so a copy to another body can leave 1 shot per burst instead of infinity (infinity is index 0 on a Z 9, index 1 on a Z6III where it cannot be told from 1, and a Z5 has no infinity; hardware observation F25). A restore of the same body still brings it back"),
    CodePolicy::aliased(33200, "kNkMAIDCapability_BracketingType",
        &["kNkMAIDCapability_BracketingType", "kNkMAIDCapability_AEBracketingType"]),
    CodePolicy::range_verified(33045, "kNkMAIDCapability_ExposureComp", ALL_THREE),
    CodePolicy::range_verified(33049, "kNkMAIDCapability_WBTuneAuto", ALL_THREE),
    CodePolicy::range_verified(33051, "kNkMAIDCapability_WBTuneFluorescent", ALL_THREE),
    CodePolicy::range_verified(33052, "kNkMAIDCapability_WBTuneSunny", ALL_THREE),
    CodePolicy::range_verified(33053, "kNkMAIDCapability_WBTuneFlash", ALL_THREE),
    CodePolicy::range_verified(33054, "kNkMAIDCapability_WBTuneShade", ALL_THREE),
    CodePolicy::range_verified(33055, "kNkMAIDCapability_WBTuneCloudy", ALL_THREE),
    CodePolicy::range_verified(33214, "kNkMAIDCapability_InternalFlashComp", ALL_THREE),
    CodePolicy::range_verified(33215, "kNkMAIDCapability_ExternalFlashComp", ALL_THREE),
    CodePolicy::range_verified(33352, "kNkMAIDCapability_MFDriveStep", ALL_THREE),
    CodePolicy::range_verified(33358, "kNkMAIDCapability_WBTuneColorAdjust", ALL_THREE),
    CodePolicy::range_verified(33359, "kNkMAIDCapability_WBTunePreset1", ALL_THREE),
    CodePolicy::range_verified(33360, "kNkMAIDCapability_WBTunePreset2", ALL_THREE),
    CodePolicy::range_verified(33361, "kNkMAIDCapability_WBTunePreset3", ALL_THREE),
    CodePolicy::range_verified(33362, "kNkMAIDCapability_WBTunePreset4", ALL_THREE),
    CodePolicy::range_verified(33363, "kNkMAIDCapability_WBTunePreset5", ALL_THREE),
    CodePolicy::range_verified(33573, "kNkMAIDCapability_WBTuneColorTempEx", ALL_THREE),
    CodePolicy::range_verified(33584, "kNkMAIDCapability_ISOAutoShutterTimeAutoValue", ALL_THREE),
    CodePolicy::range_verified(33586, "kNkMAIDCapability_MovieRecMicrophoneValue", ALL_THREE),
    CodePolicy::range_verified(33593, "kNkMAIDCapability_MovieExposureComp", ALL_THREE),
    CodePolicy::range_verified(33621, "kNkMAIDCapability_WBTunePreset6", ALL_THREE),
    CodePolicy::range_verified(33889, "kNkMAIDCapability_MovieWBTuneAuto", ALL_THREE),
    CodePolicy::range_verified(33893, "kNkMAIDCapability_MovieWBTuneFluorescent", ALL_THREE),
    CodePolicy::range_verified(33894, "kNkMAIDCapability_MovieWBTuneSunny", ALL_THREE),
    CodePolicy::range_verified(33896, "kNkMAIDCapability_MovieWBTuneShade", ALL_THREE),
    CodePolicy::range_verified(33897, "kNkMAIDCapability_MovieWBTuneCloudy", ALL_THREE),
    CodePolicy::range_verified(33898, "kNkMAIDCapability_MovieWBTuneColorTempEx", ALL_THREE),
    CodePolicy::range_verified(33899, "kNkMAIDCapability_MovieWBTuneColorAdjust", ALL_THREE),
    CodePolicy::range_verified(33900, "kNkMAIDCapability_MovieWBTunePreset1", Z9_AND_Z6III),
    CodePolicy::range_verified(33901, "kNkMAIDCapability_MovieWBTunePreset2", Z9_AND_Z6III),
    CodePolicy::range_verified(33902, "kNkMAIDCapability_MovieWBTunePreset3", Z9_AND_Z6III),
    CodePolicy::range_verified(33903, "kNkMAIDCapability_MovieWBTunePreset4", Z9_AND_Z6III),
    CodePolicy::range_verified(33904, "kNkMAIDCapability_MovieWBTunePreset5", Z9_AND_Z6III),
    CodePolicy::range_verified(33905, "kNkMAIDCapability_MovieWBTunePreset6", Z9_AND_Z6III),
    CodePolicy::range_verified(34044, "kNkMAIDCapability_WBTuneNatural", ALL_THREE),
    CodePolicy::range_verified(34046, "kNkMAIDCapability_MovieWBTuneNatural", ALL_THREE),
];

/// The record for `code`, if it has one.
pub fn policy(code: u32) -> Option<&'static CodePolicy> {
    CODE_POLICY.iter().find(|p| p.code == code)
}

/// Why `code` is never transplanted or restored, if its rule is [`CopyRule::NeverCopy`].
pub fn never_copied(code: u32) -> Option<&'static str> {
    match policy(code)?.rule {
        CopyRule::NeverCopy { why } => Some(why),
        _ => None,
    }
}

/// Why `code` is never translated between bodies, if its rule is [`CopyRule::NeverTranslate`].
pub fn never_translated(code: u32) -> Option<&'static str> {
    match policy(code)?.rule {
        CopyRule::NeverTranslate { why } => Some(why),
        _ => None,
    }
}

/// Why `code` must not be written from a snapshot, if it must not: a `NeverCopy` code always, and
/// a `NeverTranslate` code when the value would cross from one body to another (`between_bodies`).
/// The one question `transplant` and `restore` both ask before a write, so they cannot answer it
/// from two hand-combined tables.
pub fn never_skip(code: u32, between_bodies: bool) -> Option<&'static str> {
    never_copied(code).or_else(|| if between_bodies { never_translated(code) } else { None })
}

/// The models a range at `code` was verified identical on, if its rule is
/// [`CopyRule::RangeVerified`].
pub fn verified_range_models(code: u32) -> Option<&'static [&'static str]> {
    match policy(code)?.rule {
        CopyRule::RangeVerified { models } => Some(models),
        _ => None,
    }
}

/// The equivalence class of names `code` may carry across Z-series models, if it has one.
pub fn name_aliases(code: u32) -> Option<&'static [&'static str]> {
    policy(code).map(|p| p.aliases).filter(|a| !a.is_empty())
}

/// `(code, name, why)` of every `NeverCopy` record, in table order.
pub fn never_copy_table() -> Vec<(u32, &'static str, &'static str)> {
    CODE_POLICY
        .iter()
        .filter_map(|p| match p.rule {
            CopyRule::NeverCopy { why } => Some((p.code, p.name, why)),
            _ => None,
        })
        .collect()
}

/// `(code, name, why)` of every `NeverTranslate` record, in table order.
pub fn never_translate_table() -> Vec<(u32, &'static str, &'static str)> {
    CODE_POLICY
        .iter()
        .filter_map(|p| match p.rule {
            CopyRule::NeverTranslate { why } => Some((p.code, p.name, why)),
            _ => None,
        })
        .collect()
}

/// `(code, name, models)` of every `RangeVerified` record, in table order.
pub fn verified_ranges() -> Vec<(u32, &'static str, &'static [&'static str])> {
    CODE_POLICY
        .iter()
        .filter_map(|p| match p.rule {
            CopyRule::RangeVerified { models } => Some((p.code, p.name, models)),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_code_has_exactly_one_record_and_one_name() {
        let codes: BTreeSet<u32> = CODE_POLICY.iter().map(|p| p.code).collect();
        let names: BTreeSet<&str> = CODE_POLICY.iter().map(|p| p.name).collect();
        assert_eq!(codes.len(), CODE_POLICY.len(), "a code with two records could be both never-copied and range-verified");
        assert_eq!(names.len(), CODE_POLICY.len(), "duplicate name");
        for p in CODE_POLICY {
            assert!(p.name.starts_with("kNkMAIDCapability_"), "{}", p.name);
            match p.rule {
                CopyRule::NeverCopy { why } | CopyRule::NeverTranslate { why } => {
                    assert!(!why.is_empty(), "{} has no reason", p.name)
                }
                CopyRule::RangeVerified { models } => {
                    // One model has no pair to verify; a copy within one model needs no record.
                    assert!(models.len() >= 2, "{} ({}) lists {} model(s)", p.name, p.code, models.len());
                    let distinct: BTreeSet<&&str> = models.iter().collect();
                    assert_eq!(distinct.len(), models.len(), "{} repeats a model", p.name);
                    assert!(models.iter().all(|m| !m.is_empty()), "{} lists an empty model", p.name);
                }
                CopyRule::Open => assert!(!p.aliases.is_empty(), "{} is Open with no alias: it says nothing", p.name),
            }
            if !p.aliases.is_empty() {
                assert!(p.aliases.contains(&p.name), "{}: the alias class must include the canonical name", p.name);
            }
        }
    }

    #[test]
    fn the_tables_are_views_of_the_one_table_and_sum_to_it() {
        let open = CODE_POLICY.iter().filter(|p| p.rule == CopyRule::Open).count();
        assert_eq!(
            never_copy_table().len() + never_translate_table().len() + verified_ranges().len() + open,
            CODE_POLICY.len()
        );
        // The counts the earlier three tables held: 12 never-copy, 1 never-translate, 36 verified ranges.
        assert_eq!((never_copy_table().len(), never_translate_table().len(), verified_ranges().len()), (12, 1, 36));
    }

    #[test]
    fn a_rule_is_only_ever_reported_by_its_own_question() {
        let (state, limit, ranged) = (33089, 33086, 33049);
        assert!(never_copied(state).is_some() && never_translated(state).is_none() && verified_range_models(state).is_none());
        assert!(never_translated(limit).is_some() && never_copied(limit).is_none() && verified_range_models(limit).is_none());
        assert!(verified_range_models(ranged).is_some() && never_copied(ranged).is_none() && never_translated(ranged).is_none());
        for unknown in [0, 1, u32::MAX] {
            assert!(policy(unknown).is_none(), "{unknown}");
            assert!(never_skip(unknown, true).is_none() && never_skip(unknown, false).is_none());
        }
    }

    #[test]
    fn never_skip_combines_the_two_rules_by_whether_the_value_crosses_bodies() {
        let state = never_copy_table()[0].0;
        let limit = 33086; // ShootingLimit, NeverTranslate
        assert!(never_skip(state, false).is_some() && never_skip(state, true).is_some(), "state is never written");
        assert!(never_skip(limit, true).is_some(), "ShootingLimit does not cross bodies");
        assert_eq!(never_skip(limit, false), None, "and is restored onto its own body");
        assert_eq!(never_skip(33049, true), None, "a verified range is not a never-skip");
        assert_eq!(never_skip(33200, true), None, "nor is an alias-only record");
    }

    #[test]
    fn the_alias_record_is_the_bracketing_type_pair_and_only_that() {
        let aliased: Vec<u32> = CODE_POLICY.iter().filter(|p| !p.aliases.is_empty()).map(|p| p.code).collect();
        assert_eq!(aliased, [33200]);
        assert_eq!(
            name_aliases(33200),
            Some(&["kNkMAIDCapability_BracketingType", "kNkMAIDCapability_AEBracketingType"][..])
        );
        assert_eq!(name_aliases(33199), None, "a neighbour has no aliases");
        assert_eq!(name_aliases(33201), None, "nor the other neighbour");
        assert_eq!(name_aliases(33049), None, "a record with an empty alias list reports none");
    }
}

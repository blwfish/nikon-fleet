//! Which numeric ranges may be copied between two different camera models.
//!
//! A range is a position on a scale (`value_index` of `steps`), and the same
//! position need not mean the same thing on another body: `ShootingLimit`
//! (max shots per burst) has the same 0 to 200 bounds on a Z 9 and a Z6III yet
//! reads infinity as index 0 on one and index 1 on the other (F25). So copying
//! a range between two models is **default-deny**: it is allowed only when the
//! property is on [`VERIFIED_RANGES`] for both models, or the two models are
//! the same. A model nobody has captured is not on the table, so it is skipped
//! until someone reads it and adds it.
//!
//! What "verified" means here, exactly, and no more: the range's `lower`,
//! `upper`, `steps` and `default_index` were read identical in a capture from
//! each listed model. That rules out a different scale; it does **not** show
//! that each position means the same thing, which only a person at the camera
//! can show. The `ShootingLimit` case is why bounds alone are not trusted
//! elsewhere (it is on `NEVER_TRANSPLANT`). To add a model, capture it, compare
//! every range against the listed models, and add the model to each range that
//! matches.

/// Models whose ranges were captured for the first table (2026-10-28, F25).
/// Names are the strings in a snapshot's `camera.model`.
const ALL_THREE: &[&str] = &["Z 5", "Z 9", "Z6_3"];
/// The six `MovieWBTunePreset*` ranges are not present on the Z5.
const Z9_AND_Z6III: &[&str] = &["Z 9", "Z6_3"];

/// `(capability code, name, models on which the range's scale was read identical)`.
/// A range read on one model only has no pair to verify and is left out; a copy
/// within one model needs no entry.
pub const VERIFIED_RANGES: &[(u32, &str, &[&str])] = &[
    (33044, "kNkMAIDCapability_FlexibleProgram", ALL_THREE),
    (33045, "kNkMAIDCapability_ExposureComp", ALL_THREE),
    (33049, "kNkMAIDCapability_WBTuneAuto", ALL_THREE),
    (33051, "kNkMAIDCapability_WBTuneFluorescent", ALL_THREE),
    (33052, "kNkMAIDCapability_WBTuneSunny", ALL_THREE),
    (33053, "kNkMAIDCapability_WBTuneFlash", ALL_THREE),
    (33054, "kNkMAIDCapability_WBTuneShade", ALL_THREE),
    (33055, "kNkMAIDCapability_WBTuneCloudy", ALL_THREE),
    (33059, "kNkMAIDCapability_FocalLength", ALL_THREE),
    (33214, "kNkMAIDCapability_InternalFlashComp", ALL_THREE),
    (33215, "kNkMAIDCapability_ExternalFlashComp", ALL_THREE),
    (33352, "kNkMAIDCapability_MFDriveStep", ALL_THREE),
    (33358, "kNkMAIDCapability_WBTuneColorAdjust", ALL_THREE),
    (33359, "kNkMAIDCapability_WBTunePreset1", ALL_THREE),
    (33360, "kNkMAIDCapability_WBTunePreset2", ALL_THREE),
    (33361, "kNkMAIDCapability_WBTunePreset3", ALL_THREE),
    (33362, "kNkMAIDCapability_WBTunePreset4", ALL_THREE),
    (33363, "kNkMAIDCapability_WBTunePreset5", ALL_THREE),
    (33573, "kNkMAIDCapability_WBTuneColorTempEx", ALL_THREE),
    (33584, "kNkMAIDCapability_ISOAutoShutterTimeAutoValue", ALL_THREE),
    (33586, "kNkMAIDCapability_MovieRecMicrophoneValue", ALL_THREE),
    (33593, "kNkMAIDCapability_MovieExposureComp", ALL_THREE),
    (33621, "kNkMAIDCapability_WBTunePreset6", ALL_THREE),
    (33889, "kNkMAIDCapability_MovieWBTuneAuto", ALL_THREE),
    (33893, "kNkMAIDCapability_MovieWBTuneFluorescent", ALL_THREE),
    (33894, "kNkMAIDCapability_MovieWBTuneSunny", ALL_THREE),
    (33896, "kNkMAIDCapability_MovieWBTuneShade", ALL_THREE),
    (33897, "kNkMAIDCapability_MovieWBTuneCloudy", ALL_THREE),
    (33898, "kNkMAIDCapability_MovieWBTuneColorTempEx", ALL_THREE),
    (33899, "kNkMAIDCapability_MovieWBTuneColorAdjust", ALL_THREE),
    (33900, "kNkMAIDCapability_MovieWBTunePreset1", Z9_AND_Z6III),
    (33901, "kNkMAIDCapability_MovieWBTunePreset2", Z9_AND_Z6III),
    (33902, "kNkMAIDCapability_MovieWBTunePreset3", Z9_AND_Z6III),
    (33903, "kNkMAIDCapability_MovieWBTunePreset4", Z9_AND_Z6III),
    (33904, "kNkMAIDCapability_MovieWBTunePreset5", Z9_AND_Z6III),
    (33905, "kNkMAIDCapability_MovieWBTunePreset6", Z9_AND_Z6III),
    (34044, "kNkMAIDCapability_WBTuneNatural", ALL_THREE),
    (34046, "kNkMAIDCapability_MovieWBTuneNatural", ALL_THREE),
];

/// May the range at `code` be copied from `source_model` to `target_model`?
/// Same model: yes, the scale is the same by identity. Different models: only
/// when both are listed for the code.
pub fn range_copy_verified(code: u32, source_model: &str, target_model: &str) -> bool {
    if source_model.is_empty() || target_model.is_empty() {
        return false; // no model named: nothing to say the scale is the same
    }
    if source_model == target_model {
        return true;
    }
    VERIFIED_RANGES
        .iter()
        .find(|(c, _, _)| *c == code)
        .is_some_and(|(_, _, models)| models.contains(&source_model) && models.contains(&target_model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const WB_TUNE_AUTO: u32 = 33049; // on all three models
    const SHOOTING_LIMIT: u32 = 33086;

    fn code_of(name: &str) -> u32 {
        VERIFIED_RANGES
            .iter()
            .find(|(_, n, _)| *n == format!("kNkMAIDCapability_{name}"))
            .unwrap_or_else(|| panic!("{name} is not on the table"))
            .0
    }

    #[test]
    fn the_table_has_unique_codes_and_names_and_at_least_two_models_each() {
        let codes: BTreeSet<u32> = VERIFIED_RANGES.iter().map(|(c, _, _)| *c).collect();
        let names: BTreeSet<&str> = VERIFIED_RANGES.iter().map(|(_, n, _)| *n).collect();
        assert_eq!(codes.len(), VERIFIED_RANGES.len(), "duplicate code");
        assert_eq!(names.len(), VERIFIED_RANGES.len(), "duplicate name");
        for (code, name, models) in VERIFIED_RANGES {
            assert!(name.starts_with("kNkMAIDCapability_"), "{name}");
            // One model has no pair to verify; a copy within one model needs no entry.
            assert!(models.len() >= 2, "{name} ({code}) lists {} model(s)", models.len());
            let distinct: BTreeSet<&&str> = models.iter().collect();
            assert_eq!(distinct.len(), models.len(), "{name} repeats a model");
            assert!(models.iter().all(|m| !m.is_empty()), "{name} lists an empty model");
        }
    }

    #[test]
    fn the_first_table_is_what_the_f25_scan_found() {
        // 32 ranges on all three captured models, six (the movie WB presets) on the Z 9 and Z6III only.
        let on = |n| VERIFIED_RANGES.iter().filter(|(_, _, m)| m.len() == n).count();
        assert_eq!((on(3), on(2), VERIFIED_RANGES.len()), (32, 6, 38));
        assert!(VERIFIED_RANGES.iter().all(|(_, _, m)| m.iter().all(|x| ["Z 5", "Z 9", "Z6_3"].contains(x))));
    }

    #[test]
    fn the_range_that_started_this_is_not_on_the_table() {
        // ShootingLimit has the same bounds on a Z 9 and a Z6III and means different things at index 0 and 1 (F25).
        assert!(!VERIFIED_RANGES.iter().any(|(c, _, _)| *c == SHOOTING_LIMIT));
        assert!(!range_copy_verified(SHOOTING_LIMIT, "Z 9", "Z6_3"));
        // A range seen on one model only has no pair either.
        assert!(!VERIFIED_RANGES.iter().any(|(_, n, _)| n.ends_with("PixelShiftShootingInterval")));
    }

    #[test]
    fn two_listed_models_pass_in_both_directions_and_one_listed_model_does_not() {
        let c = WB_TUNE_AUTO;
        assert!(range_copy_verified(c, "Z 9", "Z6_3"));
        assert!(range_copy_verified(c, "Z6_3", "Z 9"));
        assert!(range_copy_verified(c, "Z 5", "Z 9"));
        // At the boundary: the first model listed, the second not (a model nobody has captured).
        assert!(!range_copy_verified(c, "Z 9", "Z 8"));
        assert!(!range_copy_verified(c, "Z 8", "Z 9"));
        assert!(!range_copy_verified(c, "Z 8", "Z 7_2"));
    }

    #[test]
    fn a_range_listed_for_two_models_is_not_verified_for_a_third() {
        // The movie WB presets are not on the Z5, so a Z5 pair is refused although the Z5 is listed elsewhere.
        let c = code_of("MovieWBTunePreset1");
        assert!(range_copy_verified(c, "Z 9", "Z6_3"));
        assert!(!range_copy_verified(c, "Z 5", "Z 9"));
        assert!(!range_copy_verified(c, "Z6_3", "Z 5"));
    }

    #[test]
    fn the_same_model_is_always_verified_even_for_a_code_on_no_list() {
        assert!(range_copy_verified(1, "Z 9", "Z 9"));
        assert!(range_copy_verified(SHOOTING_LIMIT, "Z6_3", "Z6_3"));
        assert!(range_copy_verified(u32::MAX, "Z 8", "Z 8")); // a model nobody has captured
    }

    #[test]
    fn a_code_on_no_list_is_refused_between_two_models() {
        for code in [0, 1, WB_TUNE_AUTO - 1000, u32::MAX] {
            if VERIFIED_RANGES.iter().any(|(c, _, _)| *c == code) {
                continue;
            }
            assert!(!range_copy_verified(code, "Z 9", "Z6_3"), "{code}");
        }
    }

    #[test]
    fn an_empty_model_is_never_verified_not_even_against_itself() {
        assert!(!range_copy_verified(WB_TUNE_AUTO, "", ""));
        assert!(!range_copy_verified(WB_TUNE_AUTO, "", "Z 9"));
        assert!(!range_copy_verified(WB_TUNE_AUTO, "Z 9", ""));
    }

    #[test]
    fn model_names_are_matched_exactly_not_by_case_or_prefix() {
        let c = WB_TUNE_AUTO;
        assert!(!range_copy_verified(c, "z 9", "Z6_3"), "case");
        assert!(!range_copy_verified(c, "Z 9 ", "Z6_3"), "trailing space");
        assert!(!range_copy_verified(c, "Z 9", "Z6"), "prefix of Z6_3");
        assert!(!range_copy_verified(c, "Z6_3x", "Z 9"), "extension of Z6_3");
    }

    #[test]
    fn no_verified_range_is_also_on_the_never_copy_table() {
        for (code, name, _) in VERIFIED_RANGES {
            assert!(crate::transplant::never_copied(*code).is_none(), "{name} is verified and never copied");
        }
    }
}

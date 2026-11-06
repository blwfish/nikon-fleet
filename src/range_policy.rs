//! Which numeric ranges may be copied between two different camera models.
//!
//! A range is a position on a scale (`value_index` of `steps`), and the same
//! position need not mean the same thing on another body: `ShootingLimit`
//! (max shots per burst) has the same 0 to 200 bounds on a Z 9 and a Z6III yet
//! reads infinity as index 0 on one and index 1 on the other (F25). So copying
//! a range between two models is **default-deny**: it is allowed only when the
//! property is a `RangeVerified` record in `code_policy::CODE_POLICY` for both models, or the two models are
//! the same. A model nobody has captured is not on the table, so it is skipped
//! until someone reads it and adds it.
//!
//! What "verified" means here, exactly, and no more: the range's `lower`,
//! `upper`, `steps` and `default_index` were read identical in a capture from
//! each listed model (the table's evidence tests check all four). That rules
//! out a different scale; it does **not** show that each position means the
//! same thing, which only a person at the camera can show, and for most of
//! the entries nobody has. `ShootingLimit` is the case that was meaning-checked
//! by hand and failed; it is a `NeverTranslate` record, not a `RangeVerified` one. The live
//! check a transplant makes against the target body (`check_range_scale`) compares
//! `lower`, `upper` and `steps`. A shared scale that is not a setting at all (a
//! read-out such as `FocalLength`, a transient such as `FlexibleProgram`) is on
//! `NeverCopy` record and deliberately not a `RangeVerified` one. To add a model, capture it, compare
//! every range against the listed models, and add the model to each range that
//! matches.

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
    crate::code_policy::verified_range_models(code)
        .is_some_and(|models| models.contains(&source_model) && models.contains(&target_model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_policy::verified_ranges;
    use std::collections::{BTreeMap, BTreeSet};

    const WB_TUNE_AUTO: u32 = 33049; // on all three models
    const SHOOTING_LIMIT: u32 = 33086;

    fn code_of(name: &str) -> u32 {
        verified_ranges()
            .iter()
            .find(|(_, n, _)| *n == format!("kNkMAIDCapability_{name}"))
            .unwrap_or_else(|| panic!("{name} is not on the table"))
            .0
    }

    #[test]
    fn the_first_table_is_what_the_f25_scan_found() {
        // 30 ranges on all three captured models, six (the movie WB presets) on the Z 9 and Z6III only.
        // The F25 scan found 32 on all three; FocalLength (a zero-span read-out) and FlexibleProgram
        // (a transient) are shared scales that are never copied, so they are on the never-copy table
        // instead (see code_policy).
        let on = |n| verified_ranges().iter().filter(|(_, _, m)| m.len() == n).count();
        assert_eq!((on(3), on(2), verified_ranges().len()), (30, 6, 36));
        assert!(verified_ranges().iter().all(|(_, _, m)| m.iter().all(|x| ["Z 5", "Z 9", "Z6_3"].contains(x))));
    }

    #[test]
    fn the_range_that_started_this_is_not_on_the_table() {
        // ShootingLimit has the same bounds on a Z 9 and a Z6III and means different things at index 0 and 1 (F25).
        assert!(!verified_ranges().iter().any(|(c, _, _)| *c == SHOOTING_LIMIT));
        assert!(!range_copy_verified(SHOOTING_LIMIT, "Z 9", "Z6_3"));
        // A range seen on one model only has no pair either.
        assert!(!verified_ranges().iter().any(|(_, n, _)| n.ends_with("PixelShiftShootingInterval")));
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
            if verified_ranges().iter().any(|(c, _, _)| *c == code) {
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

    // ── the table against its evidence: docs/range-scales/*.json ──────────

    /// One report per model, made by `scripts/range_scales.py dump` from a real capture.
    const REPORTS: [&str; 3] = [
        include_str!("../docs/range-scales/Z_5.json"),
        include_str!("../docs/range-scales/Z_9.json"),
        include_str!("../docs/range-scales/Z6_3.json"),
    ];

    /// `(lower, upper, steps, default_index)`, compared exactly as the script and the live check do.
    type Scale = (String, String, u64, u64);

    /// model -> code -> scale. Bounds are kept as their JSON text so no float comparison is involved.
    fn reports() -> BTreeMap<String, BTreeMap<u32, Scale>> {
        REPORTS
            .iter()
            .map(|text| {
                let v: serde_json::Value = serde_json::from_str(text).unwrap();
                let model = v["model"].as_str().unwrap().to_string();
                let ranges = v["ranges"]
                    .as_object()
                    .unwrap()
                    .values()
                    .map(|r| {
                        let scale = (
                            r["lower"].to_string(),
                            r["upper"].to_string(),
                            r["steps"].as_u64().unwrap(),
                            r["default_index"].as_u64().unwrap(),
                        );
                        (r["code"].as_u64().unwrap() as u32, scale)
                    })
                    .collect();
                (model, ranges)
            })
            .collect()
    }

    #[test]
    fn every_table_entry_is_backed_by_an_identical_scale_in_each_listed_models_report() {
        let reports = reports();
        for (code, name, models) in &verified_ranges() {
            let scales: Vec<&Scale> = models
                .iter()
                .map(|m| {
                    reports
                        .get(*m)
                        .unwrap_or_else(|| panic!("{name}: no report for model {m:?}"))
                        .get(code)
                        .unwrap_or_else(|| panic!("{name} ({code}) is not in {m}'s report"))
                })
                .collect();
            assert!(scales.windows(2).all(|w| w[0] == w[1]), "{name}: scales differ across {models:?}: {scales:?}");
        }
    }

    #[test]
    fn every_scale_shared_by_two_models_in_the_reports_is_on_the_table() {
        // The other direction: nothing the evidence supports is left off by accident.
        let reports = reports();
        let codes: BTreeSet<u32> = reports.values().flat_map(|r| r.keys().copied()).collect();
        for code in codes {
            let mut groups: BTreeMap<&Scale, Vec<&str>> = BTreeMap::new();
            for (model, ranges) in &reports {
                if let Some(scale) = ranges.get(&code) {
                    groups.entry(scale).or_default().push(model);
                }
            }
            let shared: Vec<Vec<&str>> = groups.into_values().filter(|g| g.len() >= 2).collect();
            let listed = verified_ranges().iter().find(|(c, _, _)| *c == code).map(|(_, _, m)| *m);
            match (shared.as_slice(), listed) {
                ([], None) => {}
                // A scale that is shared but belongs to a setting that is never copied at all
                // (a read-out such as FocalLength, a transient such as FlexibleProgram) is not a
                // range anyone may copy, so it is deliberately not on this table.
                ([_], None) if crate::transplant::never_skip(code, true).is_some() => {}
                ([group], Some(models)) => {
                    let mut want = group.clone();
                    let mut got = models.to_vec();
                    want.sort_unstable();
                    got.sort_unstable();
                    assert_eq!(got, want, "code {code}");
                }
                (other, listed) => panic!("code {code}: shared groups {other:?}, table {listed:?}"),
            }
        }
    }

    #[test]
    fn the_models_in_the_table_are_exactly_the_models_with_a_report() {
        let reported: BTreeSet<String> = reports().keys().cloned().collect();
        let listed: BTreeSet<String> =
            verified_ranges().iter().flat_map(|(_, _, m)| m.iter().map(|x| x.to_string())).collect();
        assert_eq!(listed, reported);
    }

    #[test]
    fn no_verified_range_is_also_on_the_never_copy_table() {
        // One record per code makes this structural (see code_policy's own test); this pins the
        // questions the transplant and restore actually ask.
        for (code, name, _) in &verified_ranges() {
            assert!(crate::transplant::never_copied(*code).is_none(), "{name} is verified and never copied");
            assert!(crate::transplant::never_translated(*code).is_none(), "{name} is verified and never translated");
        }
    }
}

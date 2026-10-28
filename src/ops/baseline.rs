//! Per-model capture baselines and the superset gate.
//!
//! A healthy capture of a model reads the same set of capabilities every time
//! (docs/mcp-hardware-findings.md F5: a Z6III always `ok=200 err=12`, a Z 9
//! `ok=196 err=11`), but "zero read errors" is not usable as a health check
//! and neither are counts: a USB reset mid-capture produced `ok=18 err=194`
//! that exited 0 (F2), and a capability that is *absent* rather than failing
//! never shows up as an error at all. So a capture is judged against the set
//! of capabilities that were readable in a known-good capture of that model:
//! every one of them must be readable now (a superset check).
//!
//! Baselines are files in `baselines/` (one per model, seeded from a real
//! snapshot with `nikon-fleet baseline seed`) embedded into the binary at
//! build time, so every front end sees the same set. The gate is a pure
//! function of a snapshot held in memory: it runs before anything is written.
//!
//! A capture the gate cannot judge is `NoBaseline`, not a pass and not a
//! failure: it may be written (a new model has to be capturable to be
//! baselined), but the apply tools refuse it (docs/mcp-plan.md §5.5).

use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::snapshot::Snapshot;

pub const FORMAT_VERSION: u32 = 1;

/// One readable capability. The code is what is compared; the name is for
/// humans reading the file and the failure message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineCapability {
    pub code: u32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    pub format_version: u32,
    /// The SDK model name, exactly as snapshots carry it (`"Z 9"`, `"Z6_3"`).
    pub model: String,
    /// Firmware of the body the baseline was seeded from.
    pub firmware: String,
    /// File name and capture time of the seeding snapshot, for provenance.
    pub seeded_from: String,
    pub seeded_captured_at: String,
    /// Sorted by code, unique.
    pub readable: Vec<BaselineCapability>,
}

#[derive(Debug, Error)]
pub enum BaselineError {
    #[error("baseline JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported baseline format_version {found}; this build expects {expected}")]
    UnsupportedVersion { found: u32, expected: u32 },
    #[error("baseline has an empty model name")]
    EmptyModel,
    #[error("baseline for {model:?} has no firmware recorded; it could not tell a firmware change from a failure")]
    NoFirmware { model: String },
    #[error("baseline for {model:?} lists no readable capabilities; it would pass every capture")]
    EmptyReadable { model: String },
    #[error("baseline for {model:?} lists capability code {code} more than once")]
    DuplicateCode { model: String, code: u32 },
}

impl Baseline {
    /// Check the invariants every consumer relies on.
    pub fn validate(&self) -> Result<(), BaselineError> {
        if self.format_version != FORMAT_VERSION {
            return Err(BaselineError::UnsupportedVersion { found: self.format_version, expected: FORMAT_VERSION });
        }
        if self.model.is_empty() {
            return Err(BaselineError::EmptyModel);
        }
        if self.firmware.is_empty() {
            return Err(BaselineError::NoFirmware { model: self.model.clone() });
        }
        if self.readable.is_empty() {
            return Err(BaselineError::EmptyReadable { model: self.model.clone() });
        }
        let mut seen = BTreeSet::new();
        for c in &self.readable {
            if !seen.insert(c.code) {
                return Err(BaselineError::DuplicateCode { model: self.model.clone(), code: c.code });
            }
        }
        Ok(())
    }

    pub fn from_json(text: &str) -> Result<Self, BaselineError> {
        let b: Baseline = serde_json::from_str(text)?;
        b.validate()?;
        Ok(b)
    }

    pub fn to_pretty_json(&self) -> Result<String, BaselineError> {
        Ok(serde_json::to_string_pretty(self)? + "\n")
    }

    /// Seed a baseline from a snapshot of a known-good capture: its properties
    /// are exactly the capabilities that were readable. Refuses a snapshot that
    /// would make a useless baseline (nothing readable, no firmware).
    pub fn from_snapshot(snap: &Snapshot, seeded_from: &str) -> Result<Self, BaselineError> {
        let mut readable: Vec<BaselineCapability> = snap
            .properties
            .iter()
            .map(|(name, p)| BaselineCapability { code: p.code, name: name.clone() })
            .collect();
        readable.sort_by_key(|c| c.code);
        let b = Baseline {
            format_version: FORMAT_VERSION,
            model: snap.camera.model.clone(),
            firmware: snap.camera.firmware.clone(),
            seeded_from: seeded_from.to_string(),
            seeded_captured_at: snap.captured_at.clone(),
            readable,
        };
        b.validate()?;
        Ok(b)
    }

    fn codes(&self) -> BTreeSet<u32> {
        self.readable.iter().map(|c| c.code).collect()
    }
}

/// A capability the baseline reads that the capture did not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissingCapability {
    pub code: u32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum NoBaselineReason {
    /// No baseline file for this model.
    UnknownModel,
    /// The capture's firmware differs from the baseline's and the readable
    /// set differs too, so the baseline may simply be out of date: re-seed it.
    FirmwareChanged { baseline: String, capture: String },
}

/// How strictly a restore point and a read-back are held to the model's baseline.
///
/// `Strict` is what the MCP uses: a restore point that misses a baseline capability, or a model
/// with no baseline, refuses the apply before any write. `AllowPartial` is the CLI's escape hatch
/// (`--allow-partial`, as for `snapshot`): the apply goes ahead, the report says the gate was
/// waived, and a read-back that cannot be judged for completeness is compared anyway. A restore
/// point that read nothing at all is refused under either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GatePolicy {
    #[default]
    Strict,
    AllowPartial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum GateOutcome {
    /// Every capability the baseline reads was read.
    Passed,
    /// Same firmware, but baseline capabilities are missing: an incomplete
    /// capture. All of them are listed, sorted by code.
    Failed { missing: Vec<MissingCapability> },
    /// The gate cannot judge this capture.
    NoBaseline(NoBaselineReason),
}

impl GateOutcome {
    /// Closed vocabulary for structured output.
    pub fn tag(&self) -> &'static str {
        match self {
            GateOutcome::Passed => "passed",
            GateOutcome::Failed { .. } => "failed",
            GateOutcome::NoBaseline(_) => "no_baseline",
        }
    }
}

/// Judge `snap` against `baseline` (which must be for `snap`'s model).
///
/// - Every baseline code readable ⇒ `Passed` (extra capabilities are fine on
///   the same firmware).
/// - Some missing and the same firmware ⇒ `Failed`.
/// - A different firmware and the readable set differs in either direction ⇒
///   `NoBaseline(FirmwareChanged)`: a firmware update can legitimately add or
///   remove capabilities, so the capture is neither passed nor blamed.
pub fn check_against(baseline: &Baseline, snap: &Snapshot) -> GateOutcome {
    let have = snap.readable_codes();
    let base = baseline.codes();
    let same_firmware = snap.camera.firmware == baseline.firmware;
    let missing: Vec<MissingCapability> = baseline
        .readable
        .iter()
        .filter(|c| !have.contains(&c.code))
        .map(|c| MissingCapability { code: c.code, name: c.name.clone() })
        .collect();
    let changed_firmware = || {
        GateOutcome::NoBaseline(NoBaselineReason::FirmwareChanged {
            baseline: baseline.firmware.clone(),
            capture: snap.camera.firmware.clone(),
        })
    };
    if !missing.is_empty() {
        return if same_firmware { GateOutcome::Failed { missing } } else { changed_firmware() };
    }
    if same_firmware || have == base {
        GateOutcome::Passed
    } else {
        changed_firmware()
    }
}

/// A human-readable account of a failed gate, for the CLI and the GUI. Lists
/// the first `SHOWN` missing capabilities and says how many more there are.
pub fn describe_failure(snap: &Snapshot, missing: &[MissingCapability]) -> String {
    const SHOWN: usize = 15;
    let list: Vec<String> = missing
        .iter()
        .take(SHOWN)
        .map(|m| format!("{} ({:#x})", m.name, m.code))
        .collect();
    let more = if missing.len() > SHOWN { format!(", and {} more", missing.len() - SHOWN) } else { String::new() };
    format!(
        "this capture is incomplete: {} capabilit{} readable in the {} baseline (firmware {}) could not be read: {}{more}. \
         An interrupted capture or a USB reset looks like this.",
        missing.len(),
        if missing.len() == 1 { "y" } else { "ies" },
        snap.camera.model,
        snap.camera.firmware,
        list.join(", "),
    )
}

const BUILTIN_SOURCES: &[(&str, &str)] = &[
    ("Z6_3", include_str!("../../baselines/Z6_3.json")),
    ("Z 9", include_str!("../../baselines/Z_9.json")),
];

/// The baselines compiled into this build. Parsing them cannot fail in a
/// released build: `builtin_baselines_are_valid_and_keyed_correctly` fails the
/// test run first.
pub fn builtin_baselines() -> &'static [Baseline] {
    static CELL: OnceLock<Vec<Baseline>> = OnceLock::new();
    CELL.get_or_init(|| {
        BUILTIN_SOURCES
            .iter()
            .map(|(key, text)| {
                let b = Baseline::from_json(text).unwrap_or_else(|e| panic!("embedded baseline {key:?}: {e}"));
                assert_eq!(&b.model, key, "embedded baseline registered under the wrong model");
                b
            })
            .collect()
    })
}

/// The baseline for exactly this SDK model name. No case folding and no slug
/// form: `"Z 9"` finds it, `"z 9"` and `"Z_9"` do not.
pub fn baseline_for(model: &str) -> Option<&'static Baseline> {
    builtin_baselines().iter().find(|b| b.model == model)
}

/// Judge a capture against its model's built-in baseline.
pub fn gate(snap: &Snapshot) -> GateOutcome {
    match baseline_for(&snap.camera.model) {
        Some(b) => check_against(b, snap),
        None => GateOutcome::NoBaseline(NoBaselineReason::UnknownModel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Camera, Transport};
    use serde_json::json;

    fn snap(model: &str, firmware: &str, codes: &[u32]) -> Snapshot {
        let mut s = Snapshot::new(
            Camera { model: model.into(), serial: "1".into(), firmware: firmware.into() },
            Transport::Usb,
            "2026-10-27T12:00:00Z".into(),
        );
        for c in codes {
            s.insert(format!("kNkMAIDCapability_C{c}"), *c, json!(1));
        }
        s
    }

    /// A baseline of codes 10, 20, 30 on firmware "1.00".
    fn base() -> Baseline {
        Baseline::from_snapshot(&snap("Z 9", "1.00", &[10, 20, 30]), "seed.json").unwrap()
    }

    fn missing_codes(o: &GateOutcome) -> Vec<u32> {
        match o {
            GateOutcome::Failed { missing } => missing.iter().map(|m| m.code).collect(),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ── the superset check: at / below / above ───────────────────────────

    #[test]
    fn exactly_the_baseline_set_passes() {
        assert_eq!(check_against(&base(), &snap("Z 9", "1.00", &[10, 20, 30])), GateOutcome::Passed);
    }

    #[test]
    fn one_below_the_baseline_fails_and_names_the_missing_code() {
        for (have, missing) in [(&[10, 20][..], 30), (&[10, 30][..], 20), (&[20, 30][..], 10)] {
            let o = check_against(&base(), &snap("Z 9", "1.00", have));
            assert_eq!(missing_codes(&o), [missing], "have {have:?}");
        }
    }

    #[test]
    fn one_above_the_baseline_still_passes_on_the_same_firmware() {
        assert_eq!(check_against(&base(), &snap("Z 9", "1.00", &[10, 20, 30, 40])), GateOutcome::Passed);
    }

    #[test]
    fn a_capability_that_is_absent_not_failing_is_caught() {
        // An unreadable capability simply never appears in the snapshot, which
        // is why counts of read errors cannot find it. Same count, different set.
        let o = check_against(&base(), &snap("Z 9", "1.00", &[10, 20, 99]));
        assert_eq!(missing_codes(&o), [30]);
    }

    #[test]
    fn an_empty_or_nearly_empty_capture_fails_with_everything_missing() {
        assert_eq!(missing_codes(&check_against(&base(), &snap("Z 9", "1.00", &[]))), [10, 20, 30]);
        assert_eq!(missing_codes(&check_against(&base(), &snap("Z 9", "1.00", &[20]))), [10, 30]);
    }

    #[test]
    fn missing_list_is_complete_sorted_and_carries_names() {
        // 200 capabilities, nothing read: all 200, never truncated.
        let codes: Vec<u32> = (1..=200).rev().collect();
        let big = Baseline::from_snapshot(&snap("Z 9", "1.00", &codes), "big.json").unwrap();
        match check_against(&big, &snap("Z 9", "1.00", &[])) {
            GateOutcome::Failed { missing } => {
                assert_eq!(missing.len(), 200);
                assert!(missing.windows(2).all(|w| w[0].code < w[1].code), "sorted by code");
                assert_eq!(missing[0].name, "kNkMAIDCapability_C1");
            }
            other => panic!("{other:?}"),
        }
    }

    // ── firmware rule ────────────────────────────────────────────────────

    #[test]
    fn firmware_matrix() {
        let b = base();
        let fw_changed = |baseline: &str, capture: &str| {
            GateOutcome::NoBaseline(NoBaselineReason::FirmwareChanged { baseline: baseline.into(), capture: capture.into() })
        };
        // same firmware, missing  -> a real failure
        assert!(matches!(check_against(&b, &snap("Z 9", "1.00", &[10, 20])), GateOutcome::Failed { .. }));
        // other firmware, missing -> cannot blame the capture
        assert_eq!(check_against(&b, &snap("Z 9", "2.00", &[10, 20])), fw_changed("1.00", "2.00"));
        // other firmware, identical set -> nothing changed that matters
        assert_eq!(check_against(&b, &snap("Z 9", "2.00", &[10, 20, 30])), GateOutcome::Passed);
        // other firmware, extras -> the baseline is out of date
        assert_eq!(check_against(&b, &snap("Z 9", "2.00", &[10, 20, 30, 40])), fw_changed("1.00", "2.00"));
        // same firmware, extras -> fine
        assert_eq!(check_against(&b, &snap("Z 9", "1.00", &[10, 20, 30, 40])), GateOutcome::Passed);
    }

    #[test]
    fn unknown_capture_firmware_is_a_different_firmware_not_a_match() {
        let b = base();
        assert_eq!(check_against(&b, &snap("Z 9", "", &[10, 20, 30])), GateOutcome::Passed);
        assert!(matches!(
            check_against(&b, &snap("Z 9", "", &[10, 20])),
            GateOutcome::NoBaseline(NoBaselineReason::FirmwareChanged { .. })
        ));
    }

    #[test]
    fn firmware_comparison_is_exact() {
        // "1.0" is not "1.00": the same strictness as the rest of the code's version handling.
        assert!(matches!(
            check_against(&base(), &snap("Z 9", "1.0", &[10, 20])),
            GateOutcome::NoBaseline(_)
        ));
    }

    // ── lookup ───────────────────────────────────────────────────────────

    #[test]
    fn lookup_is_by_exact_sdk_model_name() {
        assert!(baseline_for("Z 9").is_some());
        assert!(baseline_for("Z6_3").is_some());
        for wrong in ["z 9", "Z_9", "Z9", "Z 9 ", "", "Z 5", "Z6III", "z6_3"] {
            assert!(baseline_for(wrong).is_none(), "{wrong:?}");
        }
    }

    #[test]
    fn a_model_without_a_baseline_is_no_baseline_not_a_pass_or_a_failure() {
        assert_eq!(
            gate(&snap("Z 5", "1.43", &[1, 2, 3])),
            GateOutcome::NoBaseline(NoBaselineReason::UnknownModel)
        );
        // Even an empty capture of an unknown model is not judged.
        assert_eq!(gate(&snap("Z 5", "1.43", &[])), GateOutcome::NoBaseline(NoBaselineReason::UnknownModel));
    }

    // ── file validation ──────────────────────────────────────────────────

    fn raw(f: impl Fn(&mut serde_json::Value)) -> String {
        let mut v = serde_json::to_value(base()).unwrap();
        f(&mut v);
        v.to_string()
    }

    #[test]
    fn validation_rejects_every_baseline_that_would_mislead() {
        assert!(matches!(
            Baseline::from_json(&raw(|v| v["readable"] = json!([]))),
            Err(BaselineError::EmptyReadable { .. })
        ));
        assert!(matches!(
            Baseline::from_json(&raw(|v| v["model"] = json!(""))),
            Err(BaselineError::EmptyModel)
        ));
        assert!(matches!(
            Baseline::from_json(&raw(|v| v["firmware"] = json!(""))),
            Err(BaselineError::NoFirmware { .. })
        ));
        assert!(matches!(
            Baseline::from_json(&raw(|v| v["format_version"] = json!(2))),
            Err(BaselineError::UnsupportedVersion { found: 2, .. })
        ));
        assert!(matches!(
            Baseline::from_json(&raw(|v| v["readable"][1]["code"] = json!(10))),
            Err(BaselineError::DuplicateCode { code: 10, .. })
        ));
        assert!(matches!(Baseline::from_json("{ nope"), Err(BaselineError::Json(_))));
        assert!(Baseline::from_json(&raw(|_| {})).is_ok());
    }

    #[test]
    fn a_single_capability_baseline_is_valid_boundary() {
        let one = Baseline::from_snapshot(&snap("Z 9", "1.00", &[7]), "one.json").unwrap();
        assert_eq!(check_against(&one, &snap("Z 9", "1.00", &[7])), GateOutcome::Passed);
        assert!(matches!(check_against(&one, &snap("Z 9", "1.00", &[])), GateOutcome::Failed { .. }));
    }

    // ── seeding ──────────────────────────────────────────────────────────

    #[test]
    fn from_snapshot_sorts_by_code_and_round_trips() {
        let b = Baseline::from_snapshot(&snap("Z 9", "1.00", &[30, 10, 20]), "seed.json").unwrap();
        assert_eq!(b.readable.iter().map(|c| c.code).collect::<Vec<_>>(), [10, 20, 30]);
        assert_eq!((b.seeded_from.as_str(), b.seeded_captured_at.as_str()), ("seed.json", "2026-10-27T12:00:00Z"));
        assert_eq!(Baseline::from_json(&b.to_pretty_json().unwrap()).unwrap(), b);
    }

    #[test]
    fn seeding_refuses_an_empty_capture_and_an_unknown_firmware() {
        assert!(matches!(
            Baseline::from_snapshot(&snap("Z 9", "1.00", &[]), "x"),
            Err(BaselineError::EmptyReadable { .. })
        ));
        assert!(matches!(
            Baseline::from_snapshot(&snap("Z 9", "", &[1]), "x"),
            Err(BaselineError::NoFirmware { .. })
        ));
    }

    #[test]
    fn a_baseline_seeded_from_a_snapshot_passes_that_snapshot() {
        let s = snap("Z 9", "1.00", &[5, 6, 7]);
        let b = Baseline::from_snapshot(&s, "s.json").unwrap();
        assert_eq!(check_against(&b, &s), GateOutcome::Passed);
    }

    // ── shipped baselines ────────────────────────────────────────────────

    #[test]
    fn builtin_baselines_are_valid_and_keyed_correctly() {
        let all = builtin_baselines();
        assert_eq!(all.len(), BUILTIN_SOURCES.len());
        let models: BTreeSet<&str> = all.iter().map(|b| b.model.as_str()).collect();
        assert_eq!(models.len(), all.len(), "one baseline per model");
        for (key, _) in BUILTIN_SOURCES {
            assert!(models.contains(key), "{key}");
        }
    }

    #[test]
    fn shipped_baselines_have_the_sizes_measured_in_f5() {
        // docs/mcp-hardware-findings.md F5: ok=200 (Z6III) and ok=196 (Z 9).
        assert_eq!(baseline_for("Z6_3").unwrap().readable.len(), 200);
        assert_eq!(baseline_for("Z 9").unwrap().readable.len(), 196);
        assert_eq!(baseline_for("Z6_3").unwrap().firmware, "2.00");
        assert_eq!(baseline_for("Z 9").unwrap().firmware, "5.31");
    }

    #[test]
    fn shipped_baselines_exclude_the_capabilities_that_cannot_be_read() {
        // F5: Picture Controls and these others fail with -126 on both bodies,
        // so they must not be in a set every capture has to satisfy.
        for model in ["Z6_3", "Z 9"] {
            let b = baseline_for(model).unwrap();
            for unreadable in [
                "PictureControlDataEx2",
                "MoviePictureControlDataEx2",
                "PortraitImpressionBalanceDataSet",
                "SpecialShootingSettingInfo",
                "GetStorageInfoDataSet",
            ] {
                assert!(
                    !b.readable.iter().any(|c| c.name.ends_with(unreadable)),
                    "{model} baseline lists {unreadable}"
                );
            }
        }
    }

    #[test]
    fn shipped_baseline_names_match_the_real_config_at_their_codes() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk-runtime/MaidLayer.config");
        let Ok(cfg) = crate::maid_layer::MaidLayerConfig::parse_file(&path) else { return };
        for b in builtin_baselines() {
            let names = cfg.name_sets_for_model(&b.model);
            let mut checked = 0;
            for c in b.readable.iter().filter(|c| !c.name.starts_with("cap_0x")) {
                let ok = names.get(&c.code).is_some_and(|set| set.contains(&c.name));
                assert!(ok, "{}: {} is not named at code {} in the config", b.model, c.name, c.code);
                checked += 1;
            }
            assert!(checked > 150, "{}: only {checked} named entries checked", b.model);
        }
    }

    // ── output shapes ────────────────────────────────────────────────────

    #[test]
    fn gate_outcome_json_shapes_and_tags() {
        let failed = check_against(&base(), &snap("Z 9", "1.00", &[10]));
        assert_eq!(failed.tag(), "failed");
        assert_eq!(
            serde_json::to_value(&failed).unwrap(),
            json!({"outcome": "failed", "missing": [
                {"code": 20, "name": "kNkMAIDCapability_C20"}, {"code": 30, "name": "kNkMAIDCapability_C30"}]})
        );
        assert_eq!(serde_json::to_value(GateOutcome::Passed).unwrap(), json!({"outcome": "passed"}));
        assert_eq!(GateOutcome::Passed.tag(), "passed");
        let unknown = GateOutcome::NoBaseline(NoBaselineReason::UnknownModel);
        assert_eq!(unknown.tag(), "no_baseline");
        assert_eq!(serde_json::to_value(&unknown).unwrap(), json!({"outcome": "no_baseline", "reason": "unknown_model"}));
        let fw = GateOutcome::NoBaseline(NoBaselineReason::FirmwareChanged { baseline: "1.00".into(), capture: "2.00".into() });
        assert_eq!(
            serde_json::to_value(&fw).unwrap(),
            json!({"outcome": "no_baseline", "reason": "firmware_changed", "baseline": "1.00", "capture": "2.00"})
        );
    }

    #[test]
    fn describe_failure_lists_fifteen_then_counts_the_rest() {
        let m = |n: u32| MissingCapability { code: n, name: format!("N{n}") };
        let s = snap("Z 9", "5.31", &[]);
        let one = describe_failure(&s, &[m(1)]);
        assert!(one.contains("1 capability ") && one.contains("N1 (0x1)") && !one.contains("more"), "{one}");
        let fifteen: Vec<_> = (1..=15).map(m).collect();
        assert!(!describe_failure(&s, &fifteen).contains("more"));
        let sixteen: Vec<_> = (1..=16).map(m).collect();
        let t = describe_failure(&s, &sixteen);
        assert!(t.contains("16 capabilities") && t.contains("and 1 more") && !t.contains("N16"), "{t}");
        assert!(t.contains("Z 9") && t.contains("5.31"), "{t}");
    }
}

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
//! Baselines are files in `baselines/` (one per model, `{model_slug}.json`,
//! seeded from a real snapshot with `nikon-fleet baseline seed`) embedded into
//! the binary at build time, so every front end sees the same set. The
//! registry is a hand-kept list ([`BUILTIN_SOURCES`]): a file that is not in it
//! is never read, which is why `every_file_in_baselines_is_registered` fails
//! the test run, and why `baseline seed` says whether the file it wrote is
//! registered and embedded. The gate is a pure function of a snapshot held in
//! memory: it runs before anything is written.
//!
//! A capture the gate cannot judge is `NoBaseline`, not a pass and not a
//! failure: it may be written (a new model has to be capturable to be
//! baselined), and the apply tools refuse it unless the caller waived the gate
//! ([`GatePolicy::AllowPartial`], the CLI's `--allow-partial`; docs/mcp-plan.md
//! §5.5). A capture whose firmware is unknown (empty) is never judged
//! "firmware changed": there is no evidence it changed, so missing
//! capabilities fail it.
//!
//! ## Not yet measured: state-dependent capabilities
//!
//! A baseline is whatever one known-good capture read. It was measured on one
//! Z 9 and one Z6III, each in one state (a card in, a lens on, still mode); the
//! second Z 9 body was never attached. A capability that is readable only in
//! some states (no card, movie mode, no lens, a menu open) would make a
//! healthy capture in another state fail this gate. That cannot be settled
//! offline. It needs hardware captures of each model in the states a user
//! reaches (card in/out, still and movie mode, lens on/off, USB-powered), each
//! compared with the baseline: the capabilities that differ between healthy
//! states come off the baseline (seed from the intersection), or the gate
//! learns a per-state set. Until then `--allow-partial` is the escape hatch
//! and a failing gate on a healthy body is evidence to collect, not to dismiss.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::firmware::model_slug;
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
    /// Strictly ascending by code (so unique); [`Baseline::validate`] enforces it.
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
    #[error("baseline for {model:?} is not sorted by capability code: {code} comes after {after}")]
    NotSorted { model: String, code: u32, after: u32 },
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
        // Unique already; what is left to check is the order the file promises, which
        // `gate`'s sorted `missing` list and a readable diff of a re-seed rely on.
        if let Some(w) = self.readable.windows(2).find(|w| w[0].code > w[1].code) {
            return Err(BaselineError::NotSorted { model: self.model.clone(), code: w[1].code, after: w[0].code });
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

/// A capability the baseline reads that the capture did not: the baseline's own entry for it.
pub type MissingCapability = BaselineCapability;

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

/// The caller asked to judge a capture against another model's baseline. A
/// superset of the wrong model's capabilities would otherwise read as `Passed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelMismatch {
    pub baseline: String,
    pub capture: String,
}

impl ModelMismatch {
    /// Closed vocabulary for structured output.
    pub fn tag(&self) -> &'static str {
        "model_mismatch"
    }
}

impl std::fmt::Display for ModelMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the capture is of {:?} but the baseline is for {:?}; a capture is judged only against its own model's baseline",
            self.capture, self.baseline
        )
    }
}

impl std::error::Error for ModelMismatch {}

/// Judge `snap` against `baseline`; refuses (`ModelMismatch`) when they are not the same model
/// (exact string, as everywhere else).
///
/// - Every baseline code readable ⇒ `Passed` (extra capabilities are fine on
///   the same firmware).
/// - Some missing and the same firmware ⇒ `Failed`.
/// - Some missing and **unknown firmware** (empty: the body's USB record was not read, and the
///   SDK's own version string is empty on every body surveyed) ⇒ `Failed`. Nothing shows the
///   firmware changed, so a short capture is an incomplete one, not an out-of-date baseline.
/// - A different (known) firmware and the readable set differs in either direction ⇒
///   `NoBaseline(FirmwareChanged)`: a firmware update can legitimately add or
///   remove capabilities, so the capture is neither passed nor blamed. A front end that must not
///   keep an interrupted capture on that ground checks [`missing_capabilities`] too.
pub fn check_against(baseline: &Baseline, snap: &Snapshot) -> Result<GateOutcome, ModelMismatch> {
    if snap.camera.model != baseline.model {
        return Err(ModelMismatch { baseline: baseline.model.clone(), capture: snap.camera.model.clone() });
    }
    Ok(judge(baseline, snap))
}

/// [`check_against`] for a baseline already known to be `snap`'s model's.
fn judge(baseline: &Baseline, snap: &Snapshot) -> GateOutcome {
    let have = snap.readable_codes();
    let base = baseline.codes();
    let firmware_differs = !snap.camera.firmware.is_empty() && snap.camera.firmware != baseline.firmware;
    let missing = missing_capabilities(baseline, snap);
    let changed_firmware = || {
        GateOutcome::NoBaseline(NoBaselineReason::FirmwareChanged {
            baseline: baseline.firmware.clone(),
            capture: snap.camera.firmware.clone(),
        })
    };
    if !missing.is_empty() {
        return if firmware_differs { changed_firmware() } else { GateOutcome::Failed { missing } };
    }
    if !firmware_differs || have == base {
        GateOutcome::Passed
    } else {
        changed_firmware()
    }
}

/// The capabilities `baseline` reads that `snap` did not, sorted by code (the baseline's order).
/// Independent of firmware: it is the raw shortfall `check_against` then interprets.
pub fn missing_capabilities(baseline: &Baseline, snap: &Snapshot) -> Vec<MissingCapability> {
    let have = snap.readable_codes();
    baseline.readable.iter().filter(|c| !have.contains(&c.code)).cloned().collect()
}

/// The capabilities `old` reads that `new` does not: what replacing `old` with `new` would stop
/// requiring of a capture. Sorted by code (`old`'s order).
pub fn lost_capabilities(old: &Baseline, new: &Baseline) -> Vec<MissingCapability> {
    let kept = new.codes();
    old.readable.iter().filter(|c| !kept.contains(&c.code)).cloned().collect()
}

/// "name (0xcode)" for the first 15 of `missing`, then how many more: the list inside every
/// message about a shortfall.
pub fn describe_missing_list(missing: &[MissingCapability]) -> String {
    const SHOWN: usize = 15;
    let list: Vec<String> = missing.iter().take(SHOWN).map(|m| format!("{} ({:#x})", m.name, m.code)).collect();
    let more = if missing.len() > SHOWN { format!(", and {} more", missing.len() - SHOWN) } else { String::new() };
    format!("{}{more}", list.join(", "))
}

/// "1 capability" / "16 capabilities": the one phrasing every message that counts them uses.
pub fn capabilities_phrase(n: usize) -> String {
    format!("{n} capabilit{}", if n == 1 { "y" } else { "ies" })
}

/// A human-readable account of a failed gate, for the CLI and the GUI. Lists
/// the first `SHOWN` missing capabilities and says how many more there are.
pub fn describe_failure(snap: &Snapshot, missing: &[MissingCapability]) -> String {
    format!(
        "this capture is incomplete: {} readable in the {} baseline (firmware {}) could not be read: {}. \
         An interrupted capture or a USB reset looks like this.",
        capabilities_phrase(missing.len()),
        snap.camera.model,
        if snap.camera.firmware.is_empty() { "unknown" } else { &snap.camera.firmware },
        describe_missing_list(missing),
    )
}

/// The baselines compiled into this build, as `(file name, text)`. The file name is
/// `{model_slug}.json`, which is what `baseline seed` writes. **Add a line here for every new
/// file in `baselines/`**: nothing else reads that directory, and
/// `every_file_in_baselines_is_registered` fails until you do.
macro_rules! builtin_sources {
    ($($file:literal),+ $(,)?) => {
        &[$(($file, include_str!(concat!("../../baselines/", $file)))),+]
    };
}

const BUILTIN_SOURCES: &[(&str, &str)] = builtin_sources!("Z6_3.json", "Z_9.json");

/// Where a baseline file written by `baseline seed` stands in this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    /// Registered, and this binary embeds exactly this text.
    Embedded,
    /// Registered, but this binary embeds different text: rebuild to use it.
    RebuildNeeded,
    /// Not in [`BUILTIN_SOURCES`]: rebuilding alone will never read it.
    NotRegistered,
}

/// Whether the baseline file `file_name` with contents `text` is the one this build embeds.
pub fn registration_of(file_name: &str, text: &str) -> Registration {
    match BUILTIN_SOURCES.iter().find(|(f, _)| *f == file_name) {
        Some((_, embedded)) if *embedded == text => Registration::Embedded,
        Some(_) => Registration::RebuildNeeded,
        None => Registration::NotRegistered,
    }
}

/// The baselines compiled into this build. Parsing them cannot fail in a
/// released build: `builtin_baselines_are_valid_and_keyed_correctly` fails the
/// test run first.
pub fn builtin_baselines() -> &'static [Baseline] {
    static CELL: OnceLock<Vec<Baseline>> = OnceLock::new();
    CELL.get_or_init(|| {
        BUILTIN_SOURCES
            .iter()
            .map(|(file, text)| {
                let b = Baseline::from_json(text).unwrap_or_else(|e| panic!("embedded baseline {file:?}: {e}"));
                assert_eq!(
                    format!("{}.json", model_slug(&b.model)),
                    *file,
                    "embedded baseline registered under the wrong file name"
                );
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
        Some(b) => judge(b, snap),
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

    /// `check_against` for a capture of the baseline's own model (the refusal has its own test).
    fn check(b: &Baseline, s: &Snapshot) -> GateOutcome {
        check_against(b, s).expect("same model")
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
        assert_eq!(check(&base(), &snap("Z 9", "1.00", &[10, 20, 30])), GateOutcome::Passed);
    }

    #[test]
    fn one_below_the_baseline_fails_and_names_the_missing_code() {
        for (have, missing) in [(&[10, 20][..], 30), (&[10, 30][..], 20), (&[20, 30][..], 10)] {
            let o = check(&base(), &snap("Z 9", "1.00", have));
            assert_eq!(missing_codes(&o), [missing], "have {have:?}");
        }
    }

    #[test]
    fn one_above_the_baseline_still_passes_on_the_same_firmware() {
        assert_eq!(check(&base(), &snap("Z 9", "1.00", &[10, 20, 30, 40])), GateOutcome::Passed);
    }

    #[test]
    fn a_capability_that_is_absent_not_failing_is_caught() {
        // An unreadable capability simply never appears in the snapshot, which
        // is why counts of read errors cannot find it. Same count, different set.
        let o = check(&base(), &snap("Z 9", "1.00", &[10, 20, 99]));
        assert_eq!(missing_codes(&o), [30]);
    }

    #[test]
    fn an_empty_or_nearly_empty_capture_fails_with_everything_missing() {
        assert_eq!(missing_codes(&check(&base(), &snap("Z 9", "1.00", &[]))), [10, 20, 30]);
        assert_eq!(missing_codes(&check(&base(), &snap("Z 9", "1.00", &[20]))), [10, 30]);
    }

    #[test]
    fn missing_list_is_complete_sorted_and_carries_names() {
        // 200 capabilities, nothing read: all 200, never truncated.
        let codes: Vec<u32> = (1..=200).rev().collect();
        let big = Baseline::from_snapshot(&snap("Z 9", "1.00", &codes), "big.json").unwrap();
        match check(&big, &snap("Z 9", "1.00", &[])) {
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
        assert!(matches!(check(&b, &snap("Z 9", "1.00", &[10, 20])), GateOutcome::Failed { .. }));
        // other firmware, missing -> cannot blame the capture
        assert_eq!(check(&b, &snap("Z 9", "2.00", &[10, 20])), fw_changed("1.00", "2.00"));
        // other firmware, identical set -> nothing changed that matters
        assert_eq!(check(&b, &snap("Z 9", "2.00", &[10, 20, 30])), GateOutcome::Passed);
        // other firmware, extras -> the baseline is out of date
        assert_eq!(check(&b, &snap("Z 9", "2.00", &[10, 20, 30, 40])), fw_changed("1.00", "2.00"));
        // same firmware, extras -> fine
        assert_eq!(check(&b, &snap("Z 9", "1.00", &[10, 20, 30, 40])), GateOutcome::Passed);
    }

    #[test]
    fn unknown_capture_firmware_is_not_evidence_of_a_firmware_change() {
        let b = base();
        // Complete, or a superset: nothing to blame and nothing out of date to claim.
        assert_eq!(check(&b, &snap("Z 9", "", &[10, 20, 30])), GateOutcome::Passed);
        assert_eq!(check(&b, &snap("Z 9", "", &[10, 20, 30, 40])), GateOutcome::Passed);
        // Incomplete: an interrupted capture of a body whose firmware was not read (the CLI's
        // fallback to the SDK's always-empty version string) FAILS. It used to be NoBaseline,
        // so it was written with only a stderr note.
        assert_eq!(missing_codes(&check(&b, &snap("Z 9", "", &[10, 20]))), [30]);
        assert_eq!(missing_codes(&check(&b, &snap("Z 9", "", &[]))), [10, 20, 30]);
        // A *known* different firmware is still the legitimate-change case.
        assert!(matches!(
            check(&b, &snap("Z 9", "1.01", &[10, 20])),
            GateOutcome::NoBaseline(NoBaselineReason::FirmwareChanged { .. })
        ));
    }

    #[test]
    fn a_capture_is_never_judged_against_another_models_baseline() {
        // A Z6_3 capture that happens to read every code a Z 9 baseline lists is a superset
        // and, before the model check, `Passed`.
        let refused = check_against(&base(), &snap("Z6_3", "1.00", &[10, 20, 30, 40]));
        assert_eq!(refused, Err(ModelMismatch { baseline: "Z 9".into(), capture: "Z6_3".into() }));
        assert_eq!(refused.unwrap_err().tag(), "model_mismatch");
        let text = check_against(&base(), &snap("Z6_3", "1.00", &[])).unwrap_err().to_string();
        assert!(text.contains("Z6_3") && text.contains("Z 9"), "{text}");
        // Exact string: no case folding, no slug form, no whitespace tolerance.
        for wrong in ["z 9", "Z_9", "Z 9 ", ""] {
            assert!(check_against(&base(), &snap(wrong, "1.00", &[10, 20, 30])).is_err(), "{wrong:?}");
        }
        assert!(check_against(&base(), &snap("Z 9", "1.00", &[10, 20, 30])).is_ok());
    }

    #[test]
    fn missing_capabilities_is_the_raw_shortfall_whatever_the_firmware() {
        let b = base();
        let codes = |m: Vec<MissingCapability>| m.into_iter().map(|c| c.code).collect::<Vec<_>>();
        assert_eq!(codes(missing_capabilities(&b, &snap("Z 9", "1.00", &[10, 20, 30]))), Vec::<u32>::new());
        assert_eq!(codes(missing_capabilities(&b, &snap("Z 9", "9.99", &[20]))), [10, 30]);
        assert_eq!(codes(missing_capabilities(&b, &snap("Z 9", "", &[]))), [10, 20, 30]);
    }

    #[test]
    fn firmware_comparison_is_exact() {
        // "1.0" is not "1.00": the same strictness as the rest of the code's version handling.
        assert!(matches!(
            check(&base(), &snap("Z 9", "1.0", &[10, 20])),
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
    fn validation_enforces_the_sort_order_the_file_promises() {
        // Swap the first two entries of [10, 20, 30]: unique, but not sorted.
        let swap_first_two = |v: &mut serde_json::Value| {
            let r = v["readable"].as_array_mut().unwrap();
            r.swap(0, 1);
        };
        assert!(matches!(
            Baseline::from_json(&raw(swap_first_two)),
            Err(BaselineError::NotSorted { code: 10, after: 20, .. })
        ));
        // Descending throughout, and a single out-of-place entry at the end.
        assert!(matches!(
            Baseline::from_json(&raw(|v| v["readable"].as_array_mut().unwrap().reverse())),
            Err(BaselineError::NotSorted { .. })
        ));
        assert!(matches!(
            Baseline::from_json(&raw(|v| v["readable"][2]["code"] = json!(15))),
            Err(BaselineError::NotSorted { code: 15, after: 20, .. })
        ));
        // At the boundary: adjacent codes (19 then 20) are sorted; equal is a duplicate, not unsorted.
        assert!(Baseline::from_json(&raw(|v| v["readable"][0]["code"] = json!(19))).is_ok());
        assert!(matches!(
            Baseline::from_json(&raw(|v| v["readable"][0]["code"] = json!(20))),
            Err(BaselineError::DuplicateCode { code: 20, .. })
        ));
    }

    #[test]
    fn the_shipped_files_validate_as_sorted() {
        // validate() already ran on them to build the registry; this names the property.
        for b in builtin_baselines() {
            assert!(b.readable.windows(2).all(|w| w[0].code < w[1].code), "{}", b.model);
        }
    }

    #[test]
    fn a_single_capability_baseline_is_valid_boundary() {
        let one = Baseline::from_snapshot(&snap("Z 9", "1.00", &[7]), "one.json").unwrap();
        assert_eq!(check(&one, &snap("Z 9", "1.00", &[7])), GateOutcome::Passed);
        assert!(matches!(check(&one, &snap("Z 9", "1.00", &[])), GateOutcome::Failed { .. }));
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
        assert_eq!(check(&b, &s), GateOutcome::Passed);
    }

    // ── shipped baselines ────────────────────────────────────────────────

    #[test]
    fn builtin_baselines_are_valid_and_keyed_correctly() {
        let all = builtin_baselines();
        assert_eq!(all.len(), BUILTIN_SOURCES.len());
        let models: BTreeSet<&str> = all.iter().map(|b| b.model.as_str()).collect();
        assert_eq!(models.len(), all.len(), "one baseline per model");
        // Registered under the name `baseline seed` writes: `{model_slug}.json`.
        for (file, _) in BUILTIN_SOURCES {
            let stem = file.strip_suffix(".json").expect("registered as a .json file");
            assert!(all.iter().any(|b| model_slug(&b.model) == stem), "{file}");
        }
    }

    /// `*.json` files in `dir` that are not in `registered` (by file name), sorted.
    fn unregistered_files(dir: &std::path::Path, registered: &[(&str, &str)]) -> Vec<String> {
        let mut missing: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|f| f.ends_with(".json") && !registered.iter().any(|(r, _)| r == f))
            .collect();
        missing.sort();
        missing
    }

    #[test]
    fn every_file_in_baselines_is_registered() {
        // `baseline seed` writes baselines/{slug}.json, and nothing reads that directory: only
        // BUILTIN_SOURCES is embedded. A seeded file that is not added there is silently unused.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("baselines");
        assert_eq!(
            unregistered_files(&dir, BUILTIN_SOURCES),
            Vec::<String>::new(),
            "add each to BUILTIN_SOURCES in src/ops/baseline.rs and rebuild"
        );
    }

    #[test]
    fn an_unregistered_file_is_what_the_registry_check_reports() {
        // The check itself, on a directory it can be wrong about: a stray file, a registered one,
        // and non-baseline files that must be ignored.
        let d = tempfile::tempdir().unwrap();
        for f in ["Z_9.json", "Z_5.json", "notes.txt", "Z6_3.json.bak"] {
            std::fs::write(d.path().join(f), "{}").unwrap();
        }
        assert_eq!(unregistered_files(d.path(), BUILTIN_SOURCES), ["Z_5.json"]);
        assert_eq!(unregistered_files(d.path(), &[("Z_9.json", ""), ("Z_5.json", "")]), Vec::<String>::new());
    }

    #[test]
    fn registration_says_whether_a_seeded_file_is_embedded_stale_or_unread() {
        let (file, text) = BUILTIN_SOURCES[0];
        assert_eq!(registration_of(file, text), Registration::Embedded);
        // Same registered name, different bytes: rebuild. One byte is enough.
        assert_eq!(registration_of(file, &format!("{text} ")), Registration::RebuildNeeded);
        // A name nobody registered: rebuilding will not help.
        assert_eq!(registration_of("Z_5.json", text), Registration::NotRegistered);
        assert_eq!(registration_of("", ""), Registration::NotRegistered);
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

    /// Whether this run must have the gitignored `sdk-runtime/`: the repo's own switch
    /// (`REQUIRE_SDK_RUNTIME`, as in `transplant.rs`) or a CI run. Set to anything but empty,
    /// `0` or `false`.
    fn sdk_runtime_required(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
        ["REQUIRE_SDK_RUNTIME", "CI"].iter().any(|name| {
            var(name).is_some_and(|v| !v.is_empty() && v != "0" && v != "false")
        })
    }

    #[test]
    fn the_sdk_runtime_requirement_is_decided_by_both_switches() {
        fn env<'a>(set: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
            move |name| set.iter().find(|(k, _)| *k == name).map(|(_, v)| std::ffi::OsString::from(*v))
        }
        assert!(!sdk_runtime_required(env(&[])));
        assert!(sdk_runtime_required(env(&[("REQUIRE_SDK_RUNTIME", "1")])));
        assert!(sdk_runtime_required(env(&[("CI", "true")])), "CI alone requires it");
        // Set but switched off: the boundary of "set".
        for off in ["", "0", "false"] {
            assert!(!sdk_runtime_required(env(&[("CI", off)])), "CI={off:?}");
            assert!(!sdk_runtime_required(env(&[("REQUIRE_SDK_RUNTIME", off)])), "REQUIRE_SDK_RUNTIME={off:?}");
        }
        assert!(sdk_runtime_required(env(&[("CI", "0"), ("REQUIRE_SDK_RUNTIME", "1")])));
    }

    #[test]
    fn shipped_baseline_names_match_the_real_config_at_their_codes() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk-runtime/MaidLayer.config");
        if !path.exists() {
            // `sdk-runtime/` is gitignored; absence is loud, and a failure where it must exist
            // (REQUIRE_SDK_RUNTIME, or any CI run), so a mutant that makes it look absent cannot
            // survive there.
            assert!(
                !sdk_runtime_required(|n| std::env::var_os(n)),
                "REQUIRE_SDK_RUNTIME or CI is set but {path:?} does not exist (run scripts/setup-sdk-runtime.sh)"
            );
            eprintln!("SKIPPED baseline-names sweep: {path:?} not present (run scripts/setup-sdk-runtime.sh)");
            return;
        }
        // Present but unparseable is a failure, never a quiet pass.
        let cfg = crate::maid_layer::MaidLayerConfig::parse_file(&path).expect("the real MaidLayer.config parses");
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
    fn gate_outcome_json_shapes() {
        let failed = check(&base(), &snap("Z 9", "1.00", &[10]));
        assert_eq!(
            serde_json::to_value(&failed).unwrap(),
            json!({"outcome": "failed", "missing": [
                {"code": 20, "name": "kNkMAIDCapability_C20"}, {"code": 30, "name": "kNkMAIDCapability_C30"}]})
        );
        assert_eq!(serde_json::to_value(GateOutcome::Passed).unwrap(), json!({"outcome": "passed"}));
        let unknown = GateOutcome::NoBaseline(NoBaselineReason::UnknownModel);
        assert_eq!(serde_json::to_value(&unknown).unwrap(), json!({"outcome": "no_baseline", "reason": "unknown_model"}));
        let fw = GateOutcome::NoBaseline(NoBaselineReason::FirmwareChanged { baseline: "1.00".into(), capture: "2.00".into() });
        assert_eq!(
            serde_json::to_value(&fw).unwrap(),
            json!({"outcome": "no_baseline", "reason": "firmware_changed", "baseline": "1.00", "capture": "2.00"})
        );
    }

    #[test]
    fn capabilities_phrase_singular_plural_and_zero() {
        assert_eq!(capabilities_phrase(0), "0 capabilities");
        assert_eq!(capabilities_phrase(1), "1 capability");
        assert_eq!(capabilities_phrase(2), "2 capabilities");
    }

    #[test]
    fn lost_capabilities_is_what_a_replacement_stops_requiring() {
        let old = base(); // 10, 20, 30
        let new = |codes: &[u32]| Baseline::from_snapshot(&snap("Z 9", "2.00", codes), "n.json").unwrap();
        let lost = |n: &Baseline| lost_capabilities(&old, n).into_iter().map(|c| c.code).collect::<Vec<_>>();
        assert_eq!(lost(&new(&[10, 20, 30])), Vec::<u32>::new());
        assert_eq!(lost(&new(&[10, 20, 30, 40])), Vec::<u32>::new(), "growing is not losing");
        assert_eq!(lost(&new(&[10, 30])), [20]);
        assert_eq!(lost(&new(&[99])), [10, 20, 30], "a different set of the same size still loses all");
    }

    #[test]
    fn describe_missing_list_boundaries() {
        let m = |n: u32| MissingCapability { code: n, name: format!("N{n}") };
        assert_eq!(describe_missing_list(&[]), "");
        assert_eq!(describe_missing_list(&[m(1), m(2)]), "N1 (0x1), N2 (0x2)");
        let fifteen: Vec<_> = (1..=15).map(m).collect();
        assert!(!describe_missing_list(&fifteen).contains("more"));
        let sixteen: Vec<_> = (1..=16).map(m).collect();
        let t = describe_missing_list(&sixteen);
        assert!(t.ends_with(", and 1 more") && !t.contains("N16"), "{t}");
    }

    #[test]
    fn describe_failure_says_unknown_for_an_unread_firmware_not_a_blank() {
        let t = describe_failure(&snap("Z 9", "", &[]), &[MissingCapability { code: 1, name: "N1".into() }]);
        assert!(t.contains("(firmware unknown)"), "{t}");
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

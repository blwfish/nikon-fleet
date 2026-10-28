//! A transplant plan: what copying a snapshot onto a body would attempt, a
//! digest that says whether that is still true later, and a one-line summary a
//! human can approve.
//!
//! A plan is **not a prediction** (docs/mcp-plan.md §5.4): a dry run finds
//! `no_matching_option` but cannot find a write that will fail (F4). The digest
//! is a **staleness guard**, not consent: it proves neither the snapshot nor
//! the body changed in a way that matters since the plan. The real consent is
//! `--enable-writes` and the client's approval prompt, which shows the
//! arguments; hence the echoed [`summary`](summary_text).
//!
//! What the digest covers, and what it must not:
//!
//! - covered: the source file's bytes, the target serial, the opt-in set, and
//!   per property `(name, code, tag)`, plus for a copied property the strategy,
//!   the value that would be written and whether it changes the target;
//! - never covered: SDK error text, the SDK's stale enum `default_index` (F6),
//!   or the target's current value of a property whose outcome does not depend
//!   on it. A current value enters only as the unchanged/changed bit.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::ops::port::CameraPort;
use crate::snapshot::{Camera, Snapshot};
use crate::transplant::{CodeNameGuard, Severity, TransplantOptions, TransplantOutcome, transplant};

/// Bump when the digest's input changes, so an old digest never matches a new one.
pub const DIGEST_FORMAT: u32 = 1;

pub struct PlanInput<'a> {
    pub source: &'a Snapshot,
    /// The source file exactly as stored; hashed, never re-serialised.
    pub source_bytes: &'a [u8],
    pub target: &'a Camera,
    pub options: &'a TransplantOptions,
}

/// Something the SDK cannot read on these bodies, so no snapshot holds it and
/// no transplant or undo can carry it. Invisible in a per-property list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct NotTransferable {
    pub what: &'static str,
    pub why: &'static str,
}

/// From F5: unreadable (-126) on both the Z6III and the Z 9.
pub const NOT_TRANSFERABLE: &[NotTransferable] = &[
    NotTransferable {
        what: "Picture Controls (PictureControlDataEx2, MoviePictureControlDataEx2)",
        why: "the SDK cannot read them on the Z6III or the Z 9, so they cannot be copied or restored",
    },
    NotTransferable { what: "PortraitImpressionBalanceDataSet", why: "unreadable through the SDK (F5)" },
    NotTransferable { what: "SpecialShootingSettingInfo", why: "unreadable through the SDK (F5)" },
    NotTransferable { what: "GetStorageInfoDataSet", why: "unreadable through the SDK (F5)" },
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanCounts {
    /// Properties that would be written.
    pub copied: usize,
    /// Of those, the ones that would change the target or whose current value
    /// is not known (unknown counts as a possible change).
    pub would_change: usize,
    /// Per outcome tag (`copied` and each skip tag).
    pub by_tag: BTreeMap<String, usize>,
}

/// The array in a serialised [`PlanReport`] a client pages through. The MCP server pages by this
/// name; a test pins it to the report's real field.
pub const PLAN_LIST_KEY: &str = "outcomes";

#[derive(Debug, Serialize)]
pub struct PlanReport {
    pub digest: String,
    /// What the caller echoes back to apply.
    pub summary: String,
    /// Source and target are the same model: `restore` would be simpler.
    pub same_model: bool,
    pub source: Camera,
    pub source_label: Option<String>,
    pub target: Camera,
    pub allow_raw_code: Vec<String>,
    pub counts: PlanCounts,
    /// Warn-worthy tags first, then policy, routine, and copied last; by name within a tag.
    pub outcomes: Vec<TransplantOutcome>,
    pub cannot_read: &'static [NotTransferable],
}

/// A copied property that would, or might, change the target.
pub fn would_change(o: &TransplantOutcome) -> bool {
    o.result.is_ok() && o.planned.as_ref().is_some_and(|p| p.changes != Some(false))
}

/// The line a human is shown and the caller must echo to apply.
pub fn summary_text(target: &Camera, would_change: usize, allow_raw_code: &BTreeSet<String>) -> String {
    let optins = if allow_raw_code.is_empty() {
        "none".to_string()
    } else {
        allow_raw_code.iter().cloned().collect::<Vec<_>>().join(", ")
    };
    format!(
        "{} {}: {} would change; opted in: {optins}",
        target.model,
        target.serial,
        property_count_phrase(would_change),
    )
}

/// A value as the plan digest sees it: without `default_index`, which the SDK leaves uninitialised
/// on some bodies, so it would make a digest depend on garbage. This is deliberately wider than
/// `diff`'s rule (which strips it from enum shapes only): a range's `default_index` is just as
/// uninitialised, and a digest that included it would differ between two reads of one body.
fn strip_default_index(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("default_index");
    }
    v
}

fn tag_of(o: &TransplantOutcome) -> &'static str {
    match &o.result {
        Ok(_) => "copied",
        Err(r) => r.tag(),
    }
}

/// The digest of a plan (hex SHA-256). See the module docs for what it covers.
pub fn digest(
    source_bytes: &[u8],
    target_serial: &str,
    allow_raw_code: &BTreeSet<String>,
    outcomes: &[TransplantOutcome],
) -> String {
    let props: Vec<Value> = outcomes
        .iter()
        .map(|o| {
            let mut m = serde_json::Map::new();
            m.insert("name".into(), json!(o.name));
            m.insert("code".into(), json!(o.code));
            m.insert("tag".into(), json!(tag_of(o)));
            if let (Ok(strategy), Some(p)) = (&o.result, &o.planned) {
                m.insert("strategy".into(), json!(strategy.tag()));
                m.insert("write".into(), strip_default_index(&p.value));
                m.insert("changes".into(), json!(p.changes));
            }
            Value::Object(m)
        })
        .collect();
    let doc = json!({
        "format": DIGEST_FORMAT,
        "source_sha256": hex(&Sha256::digest(source_bytes)),
        "target_serial": target_serial,
        "allow_raw_code": allow_raw_code,
        "properties": props,
    });
    // serde_json objects are key-sorted, so this serialisation is canonical.
    hex(&Sha256::digest(serde_json::to_vec(&doc).expect("a Value always serialises")))
}

/// Lower-case hex of `bytes`, for digests.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `1 property` / `3 properties`, as a plan's summary words it. The summary is echoed back to
/// confirm a plan, so every summary builds this phrase here: a pluralisation that differed between
/// two places would make apply refuse a plan as stale.
pub(crate) fn property_count_phrase(n: usize) -> String {
    format!("{n} propert{}", if n == 1 { "y" } else { "ies" })
}

fn rank(o: &TransplantOutcome) -> u8 {
    match &o.result {
        Err(r) => match r.severity() {
            Severity::Error => 0,
            Severity::Warn => 1,
            Severity::Policy => 2,
            Severity::Routine => 3,
        },
        Ok(_) => 4,
    }
}

/// Run the transplant as a dry run on `port` and build the report.
pub fn build_plan<P: CameraPort + ?Sized>(port: &P, guard: &CodeNameGuard, input: &PlanInput<'_>) -> PlanReport {
    let outcomes = transplant(port, &input.source.properties, guard, input.options, true);
    report_from(outcomes, input)
}

/// Build the report from outcomes already computed (the apply path reuses it).
pub fn report_from(outcomes: Vec<TransplantOutcome>, input: &PlanInput<'_>) -> PlanReport {
    let allow = &input.options.allow_raw_code;
    let d = digest(input.source_bytes, &input.target.serial, allow, &outcomes);
    let n_change = outcomes.iter().filter(|o| would_change(o)).count();
    let mut by_tag: BTreeMap<String, usize> = BTreeMap::new();
    for o in &outcomes {
        *by_tag.entry(tag_of(o).to_string()).or_insert(0) += 1;
    }
    let copied = by_tag.get("copied").copied().unwrap_or(0);
    let mut ordered = outcomes;
    ordered.sort_by(|a, b| (rank(a), tag_of(a), &a.name).cmp(&(rank(b), tag_of(b), &b.name)));
    PlanReport {
        digest: d,
        summary: summary_text(input.target, n_change, allow),
        same_model: input.source.camera.model == input.target.model,
        source: input.source.camera.clone(),
        source_label: input.source.label.clone(),
        target: input.target.clone(),
        allow_raw_code: allow.iter().cloned().collect(),
        counts: PlanCounts { copied, would_change: n_change, by_tag },
        outcomes: ordered,
        cannot_read: NOT_TRANSFERABLE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::port::{FakeCamera, Failure};
    use crate::sdk::{OP_GET, OP_SET};
    use crate::snapshot::Transport;
    use crate::test_support::parse_config;

    const RW: u32 = OP_GET | OP_SET;

    fn cam(model: &str, serial: &str) -> Camera {
        Camera { model: model.into(), serial: serial.into(), firmware: "1.00".into() }
    }

    fn source_of(model: &str, entries: &[(&str, u32, Value)]) -> (Snapshot, Vec<u8>) {
        let mut s = Snapshot::new(cam(model, "SRC"), Transport::Usb, "2026-10-27T12:00:00Z".into());
        s.label = Some("source".into());
        for (n, c, v) in entries {
            s.insert(format!("kNkMAIDCapability_{n}"), *c, v.clone());
        }
        let bytes = s.to_pretty_json().unwrap().into_bytes();
        (s, bytes)
    }

    fn guard(entries: &[(&str, u32, Value)]) -> CodeNameGuard {
        let caps: Vec<(&str, u32)> = entries.iter().map(|(n, c, _)| (*n, *c)).collect();
        let cfg = parse_config(&[("Z 9", &caps), ("Z 8", &caps)]);
        CodeNameGuard::new(&cfg, "Z 9", "Z 8").unwrap()
    }

    fn opts(allow: &[&str]) -> TransplantOptions {
        TransplantOptions { allow_raw_code: allow.iter().map(|n| format!("kNkMAIDCapability_{n}")).collect() }
    }

    fn plan_with(
        camera: &FakeCamera,
        entries: &[(&str, u32, Value)],
        serial: &str,
        allow: &[&str],
    ) -> PlanReport {
        let (src, bytes) = source_of("Z 9", entries);
        let target = cam("Z 8", serial);
        let options = opts(allow);
        build_plan(camera, &guard(entries), &PlanInput { source: &src, source_bytes: &bytes, target: &target, options: &options })
    }

    // A small world: a numeric literal, a string, an enum-less read-only property and an absent one.
    fn world(n: i64, s: &str) -> (FakeCamera, Vec<(&'static str, u32, Value)>) {
        (
            FakeCamera::new().with(1, RW, json!(n)).with(2, RW, json!(s)).with(3, OP_GET, json!(0)),
            vec![("Num", 1, json!(10)), ("Str", 2, json!("source text")), ("RO", 3, json!(1)), ("Absent", 4, json!(1))],
        )
    }

    // ── digest stability ─────────────────────────────────────────────────

    #[test]
    fn the_same_state_gives_the_same_digest_and_it_is_hex_sha256() {
        let (cam1, entries) = world(3, "old");
        let a = plan_with(&cam1, &entries, "T1", &["Num"]);
        let (cam2, _) = world(3, "old");
        let b = plan_with(&cam2, &entries, "T1", &["Num"]);
        assert_eq!(a.digest, b.digest);
        assert_eq!(a.digest.len(), 64);
        assert!(a.digest.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn each_covered_input_changes_the_digest() {
        let (c, entries) = world(3, "old");
        let base = plan_with(&c, &entries, "T1", &["Num"]).digest;
        // target serial
        assert_ne!(base, plan_with(&c, &entries, "T2", &["Num"]).digest);
        // opt-in set
        assert_ne!(base, plan_with(&c, &entries, "T1", &[]).digest);
        // source bytes (one byte of the file)
        let (src, mut bytes) = source_of("Z 9", &entries);
        bytes.push(b' ');
        let target = cam("Z 8", "T1");
        let o = opts(&["Num"]);
        let changed = build_plan(&c, &guard(&entries), &PlanInput { source: &src, source_bytes: &bytes, target: &target, options: &o });
        assert_ne!(base, changed.digest);
        // a copied property's written value
        let mut other = entries.clone();
        other[1].2 = json!("different source text");
        assert_ne!(base, plan_with(&c, &other, "T1", &["Num"]).digest);
    }

    #[test]
    fn the_unchanged_bit_is_covered_a_literal_that_stops_changing_alters_the_digest() {
        let entries = vec![("Num", 1, json!(10))];
        let differs = plan_with(&FakeCamera::new().with(1, RW, json!(3)), &entries, "T", &["Num"]);
        let equal = plan_with(&FakeCamera::new().with(1, RW, json!(10)), &entries, "T", &["Num"]);
        assert_ne!(differs.digest, equal.digest);
        assert_eq!((differs.counts.would_change, equal.counts.would_change), (1, 0));
    }

    #[test]
    fn a_current_value_that_does_not_move_the_outcome_does_not_move_the_digest() {
        // Opted-in numeric literal: current 3 and current 4 both differ from the source 10.
        let entries = vec![("Num", 1, json!(10))];
        let a = plan_with(&FakeCamera::new().with(1, RW, json!(3)), &entries, "T", &["Num"]);
        let b = plan_with(&FakeCamera::new().with(1, RW, json!(4)), &entries, "T", &["Num"]);
        assert_eq!(a.digest, b.digest);
        // A string is written without a read at all: whatever the body holds is irrelevant.
        let s = vec![("Str", 2, json!("new"))];
        let a = plan_with(&FakeCamera::new().with(2, RW, json!("old")), &s, "T", &[]);
        let b = plan_with(&FakeCamera::new().with(2, RW, json!("something else entirely")), &s, "T", &[]);
        assert_eq!(a.digest, b.digest);
    }

    #[test]
    fn the_target_current_value_of_a_state_like_property_does_not_cause_a_stale_plan() {
        // The motivating case (F10, ShootNoCard before it joined the table): a writable boolean that
        // flips on its own. It is written without a read, so its current value is not in the digest.
        let entries = vec![("StateLike", 5, json!(false))];
        let a = plan_with(&FakeCamera::new().with(5, RW, json!(false)), &entries, "T", &[]);
        let b = plan_with(&FakeCamera::new().with(5, RW, json!(true)), &entries, "T", &[]);
        assert_eq!(a.digest, b.digest);
    }

    #[test]
    fn sdk_error_text_is_not_hashed() {
        let entries = vec![("Num", 1, json!(10))];
        let a = plan_with(&FakeCamera::new().with(1, RW, json!(3)).failing_read(1, Failure::Call(-106)), &entries, "T", &[]);
        let b = plan_with(&FakeCamera::new().with(1, RW, json!(3)).failing_read(1, Failure::Call(-999)), &entries, "T", &[]);
        // A failed numeric read is "unknown", so the outcome is literal_unconfirmed with no error text in play.
        assert_eq!(a.digest, b.digest);
        // And an enum whose read fails carries the SDK text in target_read_failed: still not hashed.
        let en = json!({"value_index": 0, "default_index": 0, "elem_count": 2, "elem_type": 2, "elem_bytes": 4, "values": [1, 2]});
        let e = vec![("En", 6, en)];
        let a = plan_with(&FakeCamera::new().with(6, RW, json!(0)).failing_read(6, Failure::Call(-106)), &e, "T", &[]);
        let b = plan_with(&FakeCamera::new().with(6, RW, json!(0)).failing_read(6, Failure::Call(-107)), &e, "T", &[]);
        assert_eq!(a.counts.by_tag.get("target_read_failed"), Some(&1));
        assert_eq!(a.digest, b.digest);
    }

    #[test]
    fn the_sdks_stale_enum_default_index_is_not_hashed() {
        let en = |idx: u64, default: u64| json!({"value_index": idx, "default_index": default, "elem_count": 2, "elem_type": 2, "elem_bytes": 4, "values": [7, 9]});
        let entries = vec![("En", 6, en(1, 0))];
        let a = plan_with(&FakeCamera::new().with(6, RW, en(0, 26)), &entries, "T", &["En"]);
        let b = plan_with(&FakeCamera::new().with(6, RW, en(0, 72350)), &entries, "T", &["En"]);
        assert_eq!(a.digest, b.digest);
    }

    #[test]
    fn a_different_enum_selection_on_the_target_does_not_move_the_digest_unless_it_flips_the_bit() {
        let en = |idx: u64| json!({"value_index": idx, "default_index": 0, "elem_count": 3, "elem_type": 2, "elem_bytes": 4, "values": [1, 2, 3]});
        // Source wants code 3 (index 2 of [1,2,3]); target on index 0 or index 1: both change.
        let entries = vec![("En", 6, en(2))];
        let a = plan_with(&FakeCamera::new().with(6, RW, en(0)), &entries, "T", &["En"]);
        let b = plan_with(&FakeCamera::new().with(6, RW, en(1)), &entries, "T", &["En"]);
        assert_eq!(a.digest, b.digest);
        // Target already on code 3: unchanged, so the bit flips.
        let c = plan_with(&FakeCamera::new().with(6, RW, en(2)), &entries, "T", &["En"]);
        assert_ne!(a.digest, c.digest);
    }

    #[test]
    fn a_skip_reason_change_moves_the_digest() {
        let (c, entries) = world(3, "old");
        let blocked = plan_with(&c, &entries, "T", &[]); // Num -> literal_unconfirmed
        let allowed = plan_with(&c, &entries, "T", &["Num"]); // Num -> copied
        assert_ne!(blocked.digest, allowed.digest);
    }

    // ── summary ──────────────────────────────────────────────────────────

    #[test]
    fn the_summary_names_target_count_and_opt_ins() {
        let (c, entries) = world(3, "old");
        let p = plan_with(&c, &entries, "T1", &["Num"]);
        // Num (changes), Str (unknown: written without a read) = 2 would change.
        assert_eq!(p.summary, "Z 8 T1: 2 properties would change; opted in: kNkMAIDCapability_Num");
        let q = plan_with(&c, &entries, "T1", &[]);
        assert_eq!(q.summary, "Z 8 T1: 1 property would change; opted in: none");
    }

    #[test]
    fn summary_text_pluralisation_and_opt_in_order() {
        let t = cam("Z 9", "S");
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
        assert!(summary_text(&t, 0, &set(&[])).contains("0 properties"));
        assert!(summary_text(&t, 1, &set(&[])).contains("1 property would"));
        assert!(summary_text(&t, 2, &set(&[])).contains("2 properties"));
        assert!(summary_text(&t, 1, &set(&["b", "a"])).ends_with("opted in: a, b"), "sorted");
    }

    // ── report ───────────────────────────────────────────────────────────

    #[test]
    fn counts_by_tag_cover_every_property_and_copied_counts_writes() {
        let (c, entries) = world(3, "old");
        let p = plan_with(&c, &entries, "T", &["Num"]);
        assert_eq!(p.counts.copied, 2);
        assert_eq!(p.counts.by_tag.values().sum::<usize>(), entries.len());
        assert_eq!(p.counts.by_tag.get("read_only"), Some(&1));
        assert_eq!(p.counts.by_tag.get("absent_on_target"), Some(&1));
    }

    #[test]
    fn warn_worthy_skips_come_first_and_copied_last() {
        let en = json!({"value_index": 0, "default_index": 0, "elem_count": 1, "elem_type": 2, "elem_bytes": 4, "values": [1]});
        let cam = FakeCamera::new()
            .with(1, RW, json!(3))
            .with(2, RW, json!("old"))
            .with(3, OP_GET, json!(0))
            .with(6, RW, json!(0)); // target's live value for an enum is not an enum: shape mismatch
        let entries = vec![("Num", 1, json!(10)), ("Str", 2, json!("x")), ("RO", 3, json!(1)), ("Absent", 4, json!(1)), ("Z", 6, en)];
        let p = plan_with(&cam, &entries, "T", &[]);
        let tags: Vec<&str> = p.outcomes.iter().map(tag_of).collect();
        let first_copied = tags.iter().position(|t| *t == "copied").unwrap();
        assert!(tags[first_copied..].iter().all(|t| *t == "copied"), "{tags:?}");
        let last_routine = tags.iter().rposition(|t| matches!(*t, "read_only" | "absent_on_target")).unwrap();
        let first_policy = tags.iter().position(|t| *t == "literal_unconfirmed").unwrap();
        assert!(first_policy < last_routine, "policy before routine: {tags:?}");
        // The enum whose target holds a non-enum is a warn-severity skip (`no_matching_option`); it
        // must exist, and come before the policy skips. (Error severity, `write_failed`, cannot occur
        // in a plan: nothing is written.)
        let first_warn = tags
            .iter()
            .position(|t| *t == "target_read_failed" || *t == "no_matching_option" || *t == "enum_undecoded_by_sdk")
            .unwrap_or_else(|| panic!("the fixture must produce a warn-severity skip: {tags:?}"));
        assert!(first_warn < first_policy, "warn before policy: {tags:?}");
    }

    #[test]
    fn same_model_is_flagged_and_the_plan_still_proceeds() {
        let entries = vec![("Str", 2, json!("x"))];
        let (src, bytes) = source_of("Z 9", &entries);
        let target = cam("Z 9", "T");
        let o = opts(&[]);
        let cfg = parse_config(&[("Z 9", &[("Str", 2)])]);
        let g = CodeNameGuard::new(&cfg, "Z 9", "Z 9").unwrap();
        let p = build_plan(&FakeCamera::new().with(2, RW, json!("o")), &g, &PlanInput { source: &src, source_bytes: &bytes, target: &target, options: &o });
        assert!(p.same_model);
        assert_eq!(p.counts.copied, 1);
        assert!(!plan_with(&FakeCamera::new().with(2, RW, json!("o")), &entries, "T", &[]).same_model);
    }

    #[test]
    fn the_report_states_what_the_sdk_cannot_read() {
        let (c, entries) = world(3, "old");
        let p = plan_with(&c, &entries, "T", &[]);
        assert!(p.cannot_read.iter().any(|n| n.what.contains("Picture Controls")));
        let v = serde_json::to_value(&p).unwrap();
        assert!(v["cannot_read"][0]["what"].as_str().unwrap().contains("Picture Controls"));
    }

    #[test]
    fn an_all_skipped_plan_reports_zero_changes() {
        let cam = FakeCamera::new().with(3, OP_GET, json!(0));
        let entries = vec![("RO", 3, json!(1)), ("Absent", 4, json!(1))];
        let p = plan_with(&cam, &entries, "T", &[]);
        assert_eq!((p.counts.copied, p.counts.would_change), (0, 0));
        assert!(p.summary.contains("0 properties would change"));
    }

    #[test]
    fn the_plan_never_writes() {
        let (c, entries) = world(3, "old");
        let _ = plan_with(&c, &entries, "T", &["Num"]);
        assert!(c.writes().is_empty());
    }

    #[test]
    fn the_list_a_client_pages_is_a_real_array_in_the_report() {
        let (c, entries) = world(3, "old");
        let v = serde_json::to_value(plan_with(&c, &entries, "T", &["Num"])).unwrap();
        assert!(v[PLAN_LIST_KEY].is_array(), "{PLAN_LIST_KEY} must name an array in the serialised plan");
    }

    #[test]
    fn the_report_serialises_with_the_documented_top_level_keys() {
        let (c, entries) = world(3, "old");
        let v = serde_json::to_value(plan_with(&c, &entries, "T", &["Num"])).unwrap();
        let keys: BTreeSet<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            BTreeSet::from([
                "digest", "summary", "same_model", "source", "source_label", "target", "allow_raw_code",
                "counts", "outcomes", "cannot_read"
            ])
        );
        assert_eq!(v["counts"]["would_change"], json!(2));
    }
    #[test]
    fn the_opt_in_set_is_covered_even_when_it_changes_no_outcome() {
        // Num equals the target's value, so it is copied with or without the opt-in:
        // the set differs, every outcome is the same, and apply must still call it stale.
        let entries = vec![("Num", 1, json!(10))];
        let c = FakeCamera::new().with(1, RW, json!(10));
        let with = plan_with(&c, &entries, "T", &["Num"]);
        let without = plan_with(&c, &entries, "T", &[]);
        assert_eq!(with.counts.by_tag, without.counts.by_tag);
        assert_ne!(with.digest, without.digest);
    }

    /// A plan between two bodies of one model, so only the range's scale decides (not the model pair).
    fn same_model_plan(camera: &FakeCamera, entries: &[(&str, u32, Value)]) -> PlanReport {
        let (src, bytes) = source_of("Z 9", entries);
        let target = cam("Z 9", "T");
        let caps: Vec<(&str, u32)> = entries.iter().map(|(n, c, _)| (*n, *c)).collect();
        let g = CodeNameGuard::new(&parse_config(&[("Z 9", &caps)]), "Z 9", "Z 9").unwrap();
        let options = opts(&[]);
        build_plan(camera, &g, &PlanInput { source: &src, source_bytes: &bytes, target: &target, options: &options })
    }

    #[test]
    fn a_target_whose_range_has_a_different_scale_is_skipped_and_never_clamped() {
        // Source 8 of 0..10 used to clamp to a target's 0..5 or 0..6 and write 5 or 6. Now the scale
        // not matching is the skip, whatever the target's bounds are.
        let range = |upper: f64, v: f64| json!({"value": v, "default": 0.0, "value_index": 0, "default_index": 0, "lower": 0.0, "upper": upper, "steps": 0});
        let entries = vec![("R", 9, range(10.0, 8.0))];
        for target_upper in [5.0, 6.0, 11.0] {
            let p = same_model_plan(&FakeCamera::new().with(9, RW, range(target_upper, 1.0)), &entries);
            assert_eq!(p.counts.by_tag.get("range_scale_differs"), Some(&1), "target 0..{target_upper}");
            assert_eq!((p.counts.copied, p.counts.would_change), (0, 0), "target 0..{target_upper}");
        }
        // The same scale on both sides is copied.
        let same = same_model_plan(&FakeCamera::new().with(9, RW, range(10.0, 1.0)), &entries);
        assert_eq!((same.counts.copied, same.counts.would_change), (1, 1));
    }

    #[test]
    fn a_range_between_two_models_that_are_not_both_on_the_verified_list_is_skipped() {
        let range = json!({"value": 8.0, "default": 0.0, "value_index": 0, "default_index": 0, "lower": 0.0, "upper": 10.0, "steps": 0});
        let entries = vec![("R", 9, range.clone())];
        // plan_with is Z 9 to Z 8, and code 9 is on no list.
        let p = plan_with(&FakeCamera::new().with(9, RW, range), &entries, "T", &[]);
        assert_eq!(p.counts.by_tag.get("range_unverified"), Some(&1));
        assert_eq!((p.counts.copied, p.counts.would_change), (0, 0));
    }

    #[test]
    fn two_different_skip_reasons_give_different_digests() {
        let entries = vec![("P", 3, json!(1))];
        let read_only = plan_with(&FakeCamera::new().with(3, OP_GET, json!(0)), &entries, "T", &[]);
        let absent = plan_with(&FakeCamera::new(), &entries, "T", &[]);
        assert_ne!(read_only.digest, absent.digest);
    }

    #[test]
    fn the_summary_counts_changes_not_copies() {
        // Num equals the target (copied, unchanged); Str is copied with an unknown current value.
        let entries = vec![("Num", 1, json!(10)), ("Str", 2, json!("x"))];
        let c = FakeCamera::new().with(1, RW, json!(10)).with(2, RW, json!("y"));
        let p = plan_with(&c, &entries, "T", &[]);
        assert_eq!(p.counts.copied, 2);
        assert_eq!(p.counts.would_change, 1);
        assert!(p.summary.contains("1 property would change"), "{}", p.summary);
    }

    /// Pins the digest's composition: any change to what is hashed or how must show up here and
    /// bump DIGEST_FORMAT, because a digest from one format must never match another's.
    #[test]
    fn golden_digest_for_a_fixed_plan() {
        let entries = vec![("Num", 1, json!(10)), ("Str", 2, json!("x")), ("RO", 3, json!(1))];
        let c = FakeCamera::new().with(1, RW, json!(3)).with(2, RW, json!("y")).with(3, OP_GET, json!(0));
        let (src, _) = source_of("Z 9", &entries);
        let target = cam("Z 8", "T1");
        let o = opts(&["Num"]);
        let report = build_plan(&c, &guard(&entries), &PlanInput { source: &src, source_bytes: b"fixed source bytes", target: &target, options: &o });
        assert_eq!(report.digest, "5a41695ba4fcfeade8091fd65a4229b7c494f201de6f091a469593a033de63da");
    }
}

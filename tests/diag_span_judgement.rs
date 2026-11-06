//! The diagnostics spans of the workflows end with the outcome their contents earn, through the real
//! entry points (`capture`, `execute_from`, `build_plan`, `build_restore_plan`, `apply_*`).
//!
//! The judging functions have their own unit tests; this file checks the wiring: that each span is ended by
//! its judge. It needs the process-wide recorder, which the library's unit tests must never install (one test
//! there relies on it being unset), so it is an integration test, a process of its own. One test function, so
//! no other test's spans interleave in the recorder.

use std::collections::{BTreeSet, HashMap};

use nikon_fleet::diag::{self, Class, Config, Entry, Kind, Outcome, Subsystem, Val};
use nikon_fleet::maid_layer::MaidLayerConfig;
use nikon_fleet::ops::apply::{ApplyError, ApplyRequest, apply_transplant};
use nikon_fleet::ops::baseline::GatePolicy;
use nikon_fleet::ops::capture::capture;
use nikon_fleet::ops::plan::{PlanInput, build_plan};
use nikon_fleet::ops::port::{FakeCamera, Failure};
use nikon_fleet::ops::restore::{Source, execute_from};
use nikon_fleet::ops::restore_flow::{
    BodyPolicy, RestoreApplyError, RestoreApplyRequest, RestorePlanInput, apply_restore, build_restore_plan,
};
use nikon_fleet::sdk::{OP_GET, OP_SET};
use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
use nikon_fleet::transplant::{CodeNameGuard, TransplantOptions};
use serde_json::json;

const RW: u32 = OP_GET | OP_SET;
const AT: &str = "2026-10-27T12:00:00Z";

fn camera(model: &str, serial: &str) -> Camera {
    Camera { model: model.into(), serial: serial.into(), firmware: "1.00".into() }
}

fn snapshot(model: &str, serial: &str, entries: &[(&str, u32, serde_json::Value)]) -> Snapshot {
    let mut s = Snapshot::new(camera(model, serial), Transport::Usb, AT.into());
    for (n, c, v) in entries {
        s.insert(format!("kNkMAIDCapability_{n}"), *c, v.clone());
    }
    s
}

/// The newest completed span of `op` in `subsystem`.
fn latest(subsystem: Subsystem, op: &str) -> Entry {
    let d = diag::global().expect("the recorder is installed");
    d.snapshot()
        .into_iter()
        .rev()
        .find(|e| e.kind == Kind::Span && e.subsystem == subsystem && e.op == op)
        .unwrap_or_else(|| panic!("no {op} span"))
}

fn get(e: &Entry, key: &str) -> Option<Val> {
    e.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

fn config_for(models: &[&str], caps: &[(&str, u32)]) -> MaidLayerConfig {
    let mut out = String::from("<ProfileVersion>9.0.3000</ProfileVersion>\n");
    for model in models {
        out.push_str(&format!("<model:{model}>\n<version>common</version>\n<caplist>\n"));
        for (name, code) in caps {
            out.push_str(&format!(
                "<capability:kNkMAIDCapability_{name}-{code}>\n<description>0,0,\"{name}\"</description>\n\
                 <allowedoperation:14></allowedoperation>\n<DeviceCommand>1,0,0,0,0</DeviceCommand>\n</capability>\n"
            ));
        }
        out.push_str("</caplist>\n</model>\n");
    }
    MaidLayerConfig::parse(&out).expect("fixture config parses")
}

#[test]
fn each_workflow_span_ends_with_the_outcome_its_contents_earn() {
    let dir = tempfile::tempdir().unwrap();
    diag::init(Config::new(dir.path().to_path_buf(), Subsystem::Cli)).expect("recorder starts");

    // ── capture_reads ────────────────────────────────────────────────────
    let all_unreadable = FakeCamera::new().with_unreadable(1, OP_GET).with_unreadable(2, OP_GET);
    let mut snap = Snapshot::new(camera("Z 9", "S"), Transport::Usb, AT.into());
    capture(&all_unreadable, &HashMap::new(), &mut snap);
    let e = latest(Subsystem::Snapshot, "capture_reads");
    assert_eq!(e.outcome, Some(Outcome::Failed), "every read failed: {e:?}");
    assert_eq!((get(&e, "read_ok"), get(&e, "read_err")), (Some(Val::U64(0)), Some(Val::U64(2))));

    let some_unreadable = FakeCamera::new().with(1, OP_GET, json!(1)).with_unreadable(2, OP_GET);
    capture(&some_unreadable, &HashMap::new(), &mut snap);
    let e = latest(Subsystem::Snapshot, "capture_reads");
    assert_eq!(e.outcome, Some(Outcome::Ok), "a partial read is a normal capture: {e:?}");
    assert_eq!((get(&e, "read_ok"), get(&e, "read_err")), (Some(Val::U64(1)), Some(Val::U64(1))));

    // ── execute_writes ───────────────────────────────────────────────────
    let src = snapshot("Z 9", "S", &[("A", 1, json!(10))]);
    let failing = FakeCamera::new().with(1, RW, json!(0)).failing_write(1, Failure::Call(-125));
    execute_from(&failing, &src.properties, None, false, Source::SameBody);
    let e = latest(Subsystem::Restore, "execute_writes");
    assert_eq!(e.outcome, Some(Outcome::Failed), "{e:?}");
    assert_eq!((get(&e, "written"), get(&e, "write_errors")), (Some(Val::U64(0)), Some(Val::U64(1))));
    assert_eq!(get(&e, "dry_run"), Some(Val::Bool(false)));

    let healthy = FakeCamera::new().with(1, RW, json!(0));
    execute_from(&healthy, &src.properties, None, false, Source::SameBody);
    let e = latest(Subsystem::Restore, "execute_writes");
    assert_eq!(e.outcome, Some(Outcome::Ok), "{e:?}");
    assert_eq!((get(&e, "written"), get(&e, "write_errors")), (Some(Val::U64(1)), Some(Val::U64(0))));

    // ── transplant plan ──────────────────────────────────────────────────
    let en = json!({"value_index": 0, "default_index": 0, "elem_count": 2, "elem_type": 2, "elem_bytes": 4, "values": [1, 2]});
    let tsrc = snapshot("Z 9", "SRC", &[("En", 6, en)]);
    let bytes = tsrc.to_pretty_json().unwrap().into_bytes();
    let target = camera("Z 8", "T");
    let guard = CodeNameGuard::new(&config_for(&["Z 9", "Z 8"], &[("En", 6)]), "Z 9", "Z 8").expect("both Z-series");
    let options = TransplantOptions::default();
    let input = PlanInput { source: &tsrc, source_bytes: &bytes, target: &target, options: &options };
    let unreadable_body = FakeCamera::new().with(6, RW, json!(0)).failing_read(6, Failure::Call(-106));
    let report = build_plan(&unreadable_body, &guard, &input);
    assert_eq!(report.counts.by_tag.get("target_read_failed"), Some(&1));
    let e = latest(Subsystem::Transplant, "plan");
    assert_eq!(e.outcome, Some(Outcome::Failed), "the only property could not be read on the body: {e:?}");

    // ── apply refusals (no body needed): failed, with the closed tag ─────
    let names: HashMap<u32, String> = HashMap::new();
    let r = apply_transplant(
        &unreadable_body,
        &ApplyRequest {
            plan: PlanInput { source: &tsrc, source_bytes: &bytes, target: &target, options: &options },
            guard: &guard,
            snapshots_dir: dir.path(),
            name_lookup: &names,
            expected_digest: "",
            expected_summary: "",
            captured_at: AT.into(),
            gate_policy: GatePolicy::Strict,
        },
        &mut |_| {},
    );
    assert!(matches!(r, Err(ApplyError::MissingConfirmation { .. })));
    let e = latest(Subsystem::Transplant, "apply");
    assert_eq!(e.outcome, Some(Outcome::Failed), "{e:?}");
    assert_eq!(get(&e, "error"), Some(Val::text("missing_confirmation")));

    // ── restore plan, restore apply ──────────────────────────────────────
    let same = snapshot("Z 9", "SER", &[("A", 1, json!(10))]);
    let sbytes = same.to_pretty_json().unwrap().into_bytes();
    let live = camera("Z 9", "SER");
    let body = FakeCamera::new().with(1, RW, json!(0));
    let plan_input = |target: &'static Camera| RestorePlanInput {
        source: Box::leak(Box::new(same.clone())),
        source_name: "s.json",
        source_bytes: Box::leak(sbytes.clone().into_boxed_slice()),
        target,
        only: None,
        body_policy: BodyPolicy::SameBody,
    };
    let live: &'static Camera = Box::leak(Box::new(live));
    let plan = build_restore_plan(&body, &plan_input(live)).expect("plans");
    let e = latest(Subsystem::Restore, "plan");
    assert_eq!(e.outcome, Some(Outcome::Ok), "{e:?}");
    assert_eq!(get(&e, "steps"), Some(Val::U64(plan.steps.len() as u64)));
    assert_eq!(get(&e, "would_change"), Some(Val::U64(plan.would_change as u64)));

    let other: &'static Camera = Box::leak(Box::new(camera("Z 9", "OTHER")));
    assert!(build_restore_plan(&body, &plan_input(other)).is_err());
    let e = latest(Subsystem::Restore, "plan");
    assert_eq!(e.outcome, Some(Outcome::Failed), "{e:?}");
    assert_eq!(get(&e, "error"), Some(Val::text("serial_mismatch")));

    let only: Option<&BTreeSet<String>> = None;
    let r = apply_restore(
        &body,
        &RestoreApplyRequest {
            plan: RestorePlanInput { only, ..plan_input(live) },
            snapshots_dir: dir.path(),
            name_lookup: &names,
            expected_digest: "",
            expected_summary: "",
            captured_at: AT.into(),
            gate_policy: GatePolicy::Strict,
        },
        &mut |_| {},
    );
    assert!(matches!(r, Err(RestoreApplyError::Apply(ApplyError::MissingConfirmation { .. }))));
    let e = latest(Subsystem::Restore, "apply");
    assert_eq!(e.outcome, Some(Outcome::Failed), "{e:?}");
    assert_eq!(get(&e, "error"), Some(Val::text("missing_confirmation")));
    assert_eq!(e.class, Class::Top);
}

//! The camera as the workflows see it: a list of capabilities that can be read
//! and written. `Device` implements it for a real body; [`FakeCamera`] is an
//! in-memory body for tests, so plan / apply / restore-point logic runs
//! without hardware (docs/mcp-plan.md §6).
//!
//! The trait is exactly the three things the workflows do with a `Device`.
//! It adds no behaviour: the properties that matter (what a failed read
//! returns, that a write may change other values) are the fake's to model.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::sdk::{
    CAP_TYPE_BOOLEAN, CAP_TYPE_ENUM, CAP_TYPE_FLOAT, CAP_TYPE_INTEGER, CAP_TYPE_RANGE, CAP_TYPE_STRING, CapabilityInfo, Device,
    SdkError, prepare_write,
};
use crate::value_shape::{is_enum_shape, is_range_shape, is_stepped_range};

pub trait CameraPort {
    /// Every capability the body reports.
    fn capabilities(&self) -> &[CapabilityInfo];
    fn read_capability(&self, id: u32) -> Result<Value, SdkError>;
    fn write_capability(&self, id: u32, kind: u32, value: &Value) -> Result<(), SdkError>;
}

impl CameraPort for Device<'_> {
    fn capabilities(&self) -> &[CapabilityInfo] {
        &self.capabilities
    }
    // Inherent methods win over trait methods in resolution, so these call
    // `Device::read_capability` / `Device::write_capability`, not themselves.
    fn read_capability(&self, id: u32) -> Result<Value, SdkError> {
        Device::read_capability(self, id)
    }
    fn write_capability(&self, id: u32, kind: u32, value: &Value) -> Result<(), SdkError> {
        Device::write_capability(self, id, kind, value)
    }
}

/// How a fake operation fails. (`SdkError` is not `Clone`, so the fake stores
/// this and builds the error each time.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// `SdkError::SdkCall` with this code (e.g. -125 for a state property, -126 unreadable).
    Call(i32),
    /// `SdkError::UnsupportedWrite`.
    UnsupportedWrite,
}

impl Failure {
    fn error(self, call: &'static str, kind: u32) -> SdkError {
        match self {
            Failure::Call(code) => SdkError::SdkCall { call, code },
            Failure::UnsupportedWrite => SdkError::UnsupportedWrite(kind),
        }
    }
}

/// An in-memory body. Reads return the stored value; a write stores the new
/// value, then applies any configured dependent change (writing one property
/// can change others on a real body). Everything is recorded.
#[derive(Default)]
pub struct FakeCamera {
    caps: Vec<CapabilityInfo>,
    values: RefCell<BTreeMap<u32, Value>>,
    read_failures: BTreeMap<u32, Failure>,
    /// Reads of these codes fail once any write has succeeded (a body that stops
    /// answering after a change).
    read_failures_after_write: BTreeMap<u32, Failure>,
    write_failures: BTreeMap<u32, Failure>,
    /// code written -> (other code, value it takes) applied after that write succeeds.
    dependents: BTreeMap<u32, Vec<(u32, Value)>>,
    /// Writes to these codes are accepted and have no effect.
    ignoring_all: BTreeSet<u32>,
    /// Writes to these codes are accepted and have no effect unless the code was
    /// read earlier (a read attempt, successful or not): the Z 9's behaviour for
    /// stepped ranges (F18).
    ignoring_blind: BTreeSet<u32>,
    primed: RefCell<BTreeSet<u32>>,
    /// Behave like the real SDK on stepped ranges: store the position but read
    /// `value` back as 0.0 (F18).
    zero_range_value: bool,
    ignored: RefCell<Vec<(u32, Value)>>,
    reads: RefCell<Vec<u32>>,
    writes: RefCell<Vec<(u32, Value)>>,
    /// Every read attempt and every write attempt, in the order they happened.
    events: RefCell<Vec<Event>>,
}

/// One call on a [`FakeCamera`], for tests that care about order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Read(u32),
    Write(u32),
}

impl FakeCamera {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a capability with the given operation bits and current value.
    pub fn with(mut self, id: u32, operations: u32, value: Value) -> Self {
        self.caps.push(CapabilityInfo { id, kind: 0, visibility: 0, operations, description: String::new() });
        self.values.borrow_mut().insert(id, value);
        self
    }

    /// A capability with no stored value (reading it fails unless configured otherwise).
    pub fn with_unreadable(mut self, id: u32, operations: u32) -> Self {
        self.caps.push(CapabilityInfo { id, kind: 0, visibility: 0, operations, description: String::new() });
        self
    }

    pub fn failing_read(mut self, id: u32, f: Failure) -> Self {
        self.read_failures.insert(id, f);
        self
    }

    pub fn failing_read_after_write(mut self, id: u32, f: Failure) -> Self {
        self.read_failures_after_write.insert(id, f);
        self
    }

    pub fn failing_write(mut self, id: u32, f: Failure) -> Self {
        self.write_failures.insert(id, f);
        self
    }

    /// Writes to `id` return success and change nothing.
    pub fn ignoring_writes(mut self, id: u32) -> Self {
        self.ignoring_all.insert(id);
        self
    }

    /// Writes to `id` return success and change nothing unless `id` was read first.
    pub fn ignoring_blind_writes(mut self, id: u32) -> Self {
        self.ignoring_blind.insert(id);
        self
    }

    /// Read stepped ranges back with `value` 0.0 whatever was written, as the real SDK does.
    pub fn with_sdk_range_quirk(mut self) -> Self {
        self.zero_range_value = true;
        self
    }

    /// After a successful write of `id`, `other` takes `value`.
    pub fn with_dependent(mut self, id: u32, other: u32, value: Value) -> Self {
        self.dependents.entry(id).or_default().push((other, value));
        self
    }

    /// Current stored value, for assertions.
    pub fn value(&self, id: u32) -> Option<Value> {
        self.values.borrow().get(&id).cloned()
    }

    /// Every write that succeeded, in order.
    pub fn writes(&self) -> Vec<(u32, Value)> {
        self.writes.borrow().clone()
    }

    /// Every read attempted, in order (including failed ones).
    pub fn reads(&self) -> Vec<u32> {
        self.reads.borrow().clone()
    }

    /// Writes that returned success but were ignored, in order.
    pub fn ignored_writes(&self) -> Vec<(u32, Value)> {
        self.ignored.borrow().clone()
    }

    /// Reads and writes in the order they were attempted (failed ones included).
    pub fn events(&self) -> Vec<Event> {
        self.events.borrow().clone()
    }

    pub fn codes_written(&self) -> BTreeSet<u32> {
        self.writes.borrow().iter().map(|(c, _)| *c).collect()
    }
}

impl CameraPort for FakeCamera {
    fn capabilities(&self) -> &[CapabilityInfo] {
        &self.caps
    }

    fn read_capability(&self, id: u32) -> Result<Value, SdkError> {
        self.reads.borrow_mut().push(id);
        self.primed.borrow_mut().insert(id);
        self.events.borrow_mut().push(Event::Read(id));
        if let Some(f) = self.read_failures.get(&id) {
            return Err(f.error("GetCapability", 0));
        }
        if !self.writes.borrow().is_empty() {
            if let Some(f) = self.read_failures_after_write.get(&id) {
                return Err(f.error("GetCapability", 0));
            }
        }
        self.values.borrow().get(&id).cloned().ok_or(SdkError::SdkCall { call: "GetCapability", code: -126 })
    }

    fn write_capability(&self, id: u32, kind: u32, value: &Value) -> Result<(), SdkError> {
        self.events.borrow_mut().push(Event::Write(id));
        if let Some(f) = self.write_failures.get(&id) {
            return Err(f.error("SetCapability", kind));
        }
        if self.ignoring_all.contains(&id) || (self.ignoring_blind.contains(&id) && !self.primed.borrow().contains(&id)) {
            self.ignored.borrow_mut().push((id, value.clone()));
            return Ok(());
        }
        // The real write builds its data from the capability's kind and refuses a value that does
        // not fit it (a float for an integer, a missing index, a 256-byte string...). The fake's
        // capabilities carry kind 0, which the real path would refuse outright, so the kind is
        // taken from the value the body holds, and then the same `prepare_write` decides.
        let held = self.values.borrow().get(&id).cloned();
        let effective_kind = if kind != 0 { kind } else { held.as_ref().map_or(0, kind_of) };
        prepare_write(effective_kind, value)?;
        // A body keeps its own scale and option list: a write sets the position, not the scale.
        let merged = match &held {
            Some(h) if is_range_shape(h) && is_range_shape(value) => {
                let mut v = h.clone();
                v["value"] = value["value"].clone();
                v["value_index"] = value["value_index"].clone();
                v
            }
            Some(h) if is_enum_shape(h) && is_enum_shape(value) => {
                let mut v = h.clone();
                v["value_index"] = value["value_index"].clone();
                v
            }
            _ => value.clone(),
        };
        let stored = if self.zero_range_value && is_stepped_range(&merged) {
            let mut v = merged;
            v["value"] = serde_json::json!(0.0);
            v
        } else {
            merged
        };
        self.values.borrow_mut().insert(id, stored);
        self.writes.borrow_mut().push((id, value.clone()));
        if let Some(deps) = self.dependents.get(&id) {
            for (other, v) in deps {
                self.values.borrow_mut().insert(*other, v.clone());
            }
        }
        Ok(())
    }
}

/// The kind a capability holding `v` has on a real body (kNkMAIDCapType_*), or 0 if none writes.
fn kind_of(v: &Value) -> u32 {
    if is_enum_shape(v) {
        CAP_TYPE_ENUM
    } else if is_range_shape(v) {
        CAP_TYPE_RANGE
    } else if v.is_boolean() {
        CAP_TYPE_BOOLEAN
    } else if v.is_i64() || v.is_u64() {
        CAP_TYPE_INTEGER
    } else if v.is_f64() {
        CAP_TYPE_FLOAT
    } else if v.is_string() {
        CAP_TYPE_STRING
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdk::{OP_GET, OP_SET};
    use serde_json::json;

    fn cam() -> FakeCamera {
        FakeCamera::new().with(1, OP_GET | OP_SET, json!(10)).with(2, OP_GET, json!("x"))
    }

    #[test]
    fn reads_return_the_stored_value_and_are_recorded() {
        let c = cam();
        assert_eq!(c.read_capability(1).unwrap(), json!(10));
        assert_eq!(c.read_capability(2).unwrap(), json!("x"));
        assert_eq!(c.reads(), [1, 2]);
        assert_eq!(c.capabilities().len(), 2);
    }

    // ── the fake refuses what the real write refuses ─────────────────────

    fn range(value: f64, index: u64) -> Value {
        json!({"value": value, "default": 0.0, "value_index": index, "default_index": 0, "lower": 0.0, "upper": 10.0, "steps": 11})
    }

    fn enum_of(index: u64) -> Value {
        json!({"value_index": index, "default_index": 0, "elem_count": 3, "elem_type": 2, "elem_bytes": 4, "values": [1, 2, 3]})
    }

    #[test]
    fn the_fake_and_the_real_write_path_accept_and_refuse_the_same_values() {
        // (what the body holds, what is written): both go through `prepare_write`, so the fake can
        // never accept a write the real path would refuse.
        let long = Value::String("x".repeat(256));
        let cases: Vec<(Value, Value)> = vec![
            (json!(10), json!(11)),
            (json!(10), json!(11.5)),
            (json!(10), json!("11")),
            (json!(10.5), json!(2)),
            (json!("old"), json!("new")),
            (json!("old"), long.clone()),
            (json!("old"), json!(5)),
            (json!(true), json!(false)),
            (json!(true), json!(1)),
            (range(3.0, 3), range(4.0, 4)),
            (range(3.0, 3), json!({"value_index": 4})),
            (range(3.0, 3), json!(4)),
            (enum_of(0), enum_of(2)),
            (enum_of(0), json!({"values": [1]})),
            (enum_of(0), json!(2)),
        ];
        for (held, written) in cases {
            let cam = FakeCamera::new().with(1, OP_GET | OP_SET, held.clone());
            let fake = cam.write_capability(1, 0, &written);
            let real = prepare_write(kind_of(&held), &written);
            assert_eq!(fake.is_ok(), real.is_ok(), "held {held}, written {written}: fake {fake:?}");
            if let (Err(f), Err(r)) = (&fake, &real) {
                assert_eq!(std::mem::discriminant(f), std::mem::discriminant(r), "same refusal for {held} <- {written}");
            }
            if fake.is_err() {
                assert_eq!(cam.value(1), Some(held), "a refused write changes nothing");
                assert!(cam.writes().is_empty(), "and is not recorded as a write");
            }
        }
    }

    #[test]
    fn the_kind_the_fake_infers_from_a_held_value_is_the_kind_a_body_reports_for_it() {
        // The parity test above feeds the same inferred kind to both sides, so this pins the inference
        // itself to the SDK's kind numbers (kNkMAIDCapType_*: Boolean 1, Integer 2, Float 4, String 8,
        // Enum 12, Range 13).
        assert_eq!(kind_of(&json!(true)), 1);
        assert_eq!(kind_of(&json!(7)), 2);
        assert_eq!(kind_of(&json!(7.5)), 4);
        assert_eq!(kind_of(&json!("s")), 8);
        assert_eq!(kind_of(&enum_of(0)), 12);
        assert_eq!(kind_of(&range(1.0, 1)), 13);
        assert_eq!(kind_of(&json!({"_type": "datetime"})), 0, "nothing writes it");
    }

    #[test]
    fn a_string_of_256_bytes_is_too_long_on_the_fake_as_on_the_body() {
        let cam = FakeCamera::new().with(1, OP_GET | OP_SET, json!("old"));
        assert!(matches!(cam.write_capability(1, 0, &Value::String("x".repeat(256))), Err(SdkError::StringTooLong { len: 256 })));
        assert!(cam.write_capability(1, 0, &Value::String("x".repeat(255))).is_ok());
    }

    #[test]
    fn a_write_to_a_range_or_enum_sets_the_position_and_keeps_the_bodys_own_scale() {
        let cam = FakeCamera::new().with(1, OP_GET | OP_SET, range(3.0, 3)).with(2, OP_GET | OP_SET, enum_of(0));
        // A stale scale in what is written (another firmware, a hand-edited snapshot): the body keeps
        // its own lower/upper/steps, so the read-back reports the position on ITS scale.
        let stale = json!({"value": 7.0, "default": 0.0, "value_index": 7, "default_index": 9, "lower": 100.0, "upper": 2000.0, "steps": 5});
        cam.write_capability(1, 0, &stale).unwrap();
        let held = cam.value(1).unwrap();
        assert_eq!((held["lower"].clone(), held["upper"].clone(), held["steps"].clone()), (json!(0.0), json!(10.0), json!(11)));
        assert_eq!(held["value_index"], json!(7));
        let other = json!({"value_index": 2, "default_index": 0, "elem_count": 1, "elem_type": 2, "elem_bytes": 4, "values": [9]});
        cam.write_capability(2, 0, &other).unwrap();
        assert_eq!(cam.value(2).unwrap()["values"], json!([1, 2, 3]), "the option list is the body's own");
        assert_eq!(cam.value(2).unwrap()["value_index"], json!(2));
    }

    #[test]
    fn a_capability_that_holds_nothing_writable_is_refused_by_kind_like_the_real_write() {
        // A datetime or array placeholder: no kind writes it.
        let cam = FakeCamera::new().with(1, OP_GET | OP_SET, json!({"_type": "datetime"}));
        assert!(matches!(cam.write_capability(1, 0, &json!("2026")), Err(SdkError::UnsupportedWrite(0))));
    }

    #[test]
    fn a_write_changes_what_the_next_read_returns_and_is_recorded() {
        let c = cam();
        c.write_capability(1, 0, &json!(11)).unwrap();
        assert_eq!(c.read_capability(1).unwrap(), json!(11));
        assert_eq!(c.writes(), [(1, json!(11))]);
        assert_eq!(c.codes_written(), BTreeSet::from([1]));
    }

    #[test]
    fn an_unknown_or_valueless_capability_reads_as_the_sdk_does_for_an_unreadable_one() {
        let c = cam().with_unreadable(3, OP_GET);
        for id in [3, 99] {
            match c.read_capability(id) {
                Err(SdkError::SdkCall { code: -126, .. }) => {}
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn configured_failures_apply_to_reads_and_writes_separately() {
        let c = cam().failing_read(1, Failure::Call(-106)).failing_write(2, Failure::UnsupportedWrite);
        assert!(matches!(c.read_capability(1), Err(SdkError::SdkCall { code: -106, .. })));
        assert!(c.write_capability(1, 0, &json!(1)).is_ok(), "a read failure does not block a write");
        assert!(matches!(c.write_capability(2, 7, &json!(1)), Err(SdkError::UnsupportedWrite(7))));
        assert_eq!(c.value(2), Some(json!("x")), "a failed write stores nothing");
        assert_eq!(c.writes().len(), 1);
    }

    #[test]
    fn a_failed_read_is_still_recorded_as_attempted() {
        let c = cam().failing_read(1, Failure::Call(-1));
        let _ = c.read_capability(1);
        assert_eq!(c.reads(), [1]);
    }

    #[test]
    fn a_write_can_change_another_property() {
        let c = cam().with_dependent(1, 2, json!("changed-by-1"));
        c.write_capability(1, 0, &json!(11)).unwrap();
        assert_eq!(c.value(2), Some(json!("changed-by-1")));
        assert_eq!(c.codes_written(), BTreeSet::from([1]), "the dependent change is not a write");
    }

    #[test]
    fn a_failed_write_applies_no_dependent_change() {
        let c = cam().with_dependent(1, 2, json!("nope")).failing_write(1, Failure::Call(-125));
        let _ = c.write_capability(1, 0, &json!(11));
        assert_eq!(c.value(2), Some(json!("x")));
    }

    #[test]
    fn a_read_can_start_failing_only_after_a_write_has_happened() {
        let c = cam().failing_read_after_write(2, Failure::Call(-126));
        assert!(c.read_capability(2).is_ok(), "before any write");
        c.write_capability(1, 0, &json!(11)).unwrap();
        assert!(matches!(c.read_capability(2), Err(SdkError::SdkCall { code: -126, .. })));
        assert!(c.read_capability(1).is_ok(), "other codes unaffected");
    }
    #[test]
    fn a_body_can_accept_and_ignore_every_write_to_a_code() {
        let c = cam().ignoring_writes(1);
        c.write_capability(1, 0, &json!(99)).unwrap();
        assert_eq!(c.value(1), Some(json!(10)), "unchanged");
        assert!(c.writes().is_empty());
        assert_eq!(c.ignored_writes(), [(1, json!(99))]);
        c.write_capability(2, 0, &json!("y")).unwrap();
        assert_eq!(c.value(2), Some(json!("y")), "other codes are unaffected");
    }

    #[test]
    fn a_blind_write_is_ignored_but_a_write_after_a_read_takes() {
        let c = cam().ignoring_blind_writes(1).with_dependent(1, 2, json!("dep"));
        c.write_capability(1, 0, &json!(11)).unwrap();
        assert_eq!(c.value(1), Some(json!(10)), "blind: accepted and ignored");
        assert_eq!(c.value(2), Some(json!("x")), "an ignored write has no dependent effect either");
        let _ = c.read_capability(1);
        c.write_capability(1, 0, &json!(12)).unwrap();
        assert_eq!(c.value(1), Some(json!(12)), "after a read it takes");
        assert_eq!(c.value(2), Some(json!("dep")));
        assert_eq!(c.ignored_writes(), [(1, json!(11))]);
        assert_eq!(c.writes(), [(1, json!(12))]);
    }

    #[test]
    fn a_failed_read_still_primes_the_code() {
        let c = cam().ignoring_blind_writes(1).failing_read(1, Failure::Call(-106));
        let _ = c.read_capability(1);
        c.write_capability(1, 0, &json!(13)).unwrap();
        assert_eq!(c.value(1), Some(json!(13)));
    }

    #[test]
    fn the_sdk_range_quirk_reads_a_stepped_range_back_with_a_zero_value_but_keeps_the_position() {
        let st = |v: f64, i: u64| json!({"value": v, "value_index": i, "lower": 0.0, "upper": 10.0, "steps": 11});
        let c = FakeCamera::new().with(1, OP_GET | OP_SET, st(0.0, 0)).with(2, OP_GET | OP_SET, json!({"value": 2.5, "value_index": 0, "lower": 0.0, "upper": 10.0, "steps": 0})).with_sdk_range_quirk();
        c.write_capability(1, 0, &st(7.0, 7)).unwrap();
        assert_eq!(c.value(1), Some(st(0.0, 7)));
        // Continuous ranges, enums and scalars are stored exactly as written.
        let cont = json!({"value": 3.5, "value_index": 0, "lower": 0.0, "upper": 10.0, "steps": 0});
        c.write_capability(2, 0, &cont).unwrap();
        assert_eq!(c.value(2), Some(cont));
    }
}

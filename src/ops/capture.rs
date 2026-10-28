//! Reading every readable capability of a body into a snapshot, without
//! printing anything.
//!
//! One implementation for `snapshot`, `check`, the GUI and (planned) the MCP,
//! including the restore point and the read-back of an apply. A capture
//! inserts a property only when its read succeeded, so the snapshot's codes are
//! the readable set the baseline gate compares (`ops::baseline`). Failures are
//! returned, not printed: the CLI decides how to show them.

use std::collections::HashMap;

use serde::Serialize;

use crate::ops::port::CameraPort;
use crate::sdk::OP_GET;
use crate::snapshot::Snapshot;

/// One capability that has a get bit but could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailedRead {
    pub code: u32,
    pub name: String,
    /// The SDK's error text, for a human. Nothing branches on it.
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct CaptureStats {
    pub read_ok: usize,
    pub read_err: usize,
    /// Capabilities without a get bit: never attempted.
    pub skipped_no_get: usize,
    /// Readable capabilities missing from the schema, recorded as `cap_0x..`:
    /// counted separately because a stale `MaidLayer.config` degrades
    /// readability with no other signal.
    pub unknown_schema: usize,
    /// Every failed read, in capability order. Never truncated.
    pub failed: Vec<FailedRead>,
}

/// The key a capability is stored under in a snapshot: the schema's name for the body's model, or
/// `cap_0x...` when the schema does not know the code. Everything that looks a property up in a
/// capture (the restore point, the read-back) must key it the same way.
pub fn property_key(name_lookup: &HashMap<u32, String>, code: u32) -> String {
    match name_lookup.get(&code) {
        Some(n) => n.clone(),
        None => format!("cap_{code:#x}"),
    }
}

/// Read every get-capable capability of `port` into `snap`, naming each from
/// `name_lookup` (`cap_{:#x}` on a schema miss).
pub fn capture<P: CameraPort + ?Sized>(
    port: &P,
    name_lookup: &HashMap<u32, String>,
    snap: &mut Snapshot,
) -> CaptureStats {
    let mut stats = CaptureStats::default();
    for cap in port.capabilities() {
        if cap.operations & OP_GET == 0 {
            stats.skipped_no_get += 1;
            continue;
        }
        let name = property_key(name_lookup, cap.id);
        match port.read_capability(cap.id) {
            Ok(value) => {
                if !name_lookup.contains_key(&cap.id) {
                    stats.unknown_schema += 1;
                }
                snap.insert(name, cap.id, value);
                stats.read_ok += 1;
            }
            Err(e) => {
                stats.read_err += 1;
                stats.failed.push(FailedRead { code: cap.id, name, error: e.to_string() });
            }
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::port::{FakeCamera, Failure};
    use crate::sdk::OP_SET;
    use crate::snapshot::{Camera, Transport};
    use serde_json::json;

    fn empty_snap() -> Snapshot {
        Snapshot::new(
            Camera { model: "Z 9".into(), serial: "1".into(), firmware: "5.31".into() },
            Transport::Usb,
            "2026-10-27T12:00:00Z".into(),
        )
    }

    fn names(pairs: &[(u32, &str)]) -> HashMap<u32, String> {
        pairs.iter().map(|(c, n)| (*c, n.to_string())).collect()
    }

    #[test]
    fn readable_capabilities_land_in_the_snapshot_under_their_schema_names() {
        let cam = FakeCamera::new().with(1, OP_GET | OP_SET, json!(10)).with(2, OP_GET, json!("x"));
        let mut s = empty_snap();
        let stats = capture(&cam, &names(&[(1, "A"), (2, "B")]), &mut s);
        assert_eq!((stats.read_ok, stats.read_err, stats.skipped_no_get, stats.unknown_schema), (2, 0, 0, 0));
        assert_eq!(s.properties["A"].value, json!(10));
        assert_eq!(s.properties["B"].code, 2);
        assert!(stats.failed.is_empty());
    }

    #[test]
    fn a_schema_miss_is_recorded_by_code_and_counted_separately_from_a_read_error() {
        let cam = FakeCamera::new().with(0x8561, OP_GET, json!(1));
        let mut s = empty_snap();
        let stats = capture(&cam, &HashMap::new(), &mut s);
        assert!(s.properties.contains_key("cap_0x8561"));
        assert_eq!((stats.read_ok, stats.unknown_schema, stats.read_err), (1, 1, 0));
    }

    #[test]
    fn a_failed_read_is_counted_listed_and_leaves_no_property() {
        let cam = FakeCamera::new()
            .with(1, OP_GET, json!(1))
            .with(2, OP_GET, json!(2))
            .failing_read(2, Failure::Call(-126))
            .with_unreadable(3, OP_GET);
        let mut s = empty_snap();
        let stats = capture(&cam, &names(&[(1, "A"), (2, "B"), (3, "C")]), &mut s);
        assert_eq!((stats.read_ok, stats.read_err), (1, 2));
        assert_eq!(s.readable_codes().into_iter().collect::<Vec<_>>(), [1], "only successful reads");
        let failed: Vec<(u32, &str)> = stats.failed.iter().map(|f| (f.code, f.name.as_str())).collect();
        assert_eq!(failed, [(2, "B"), (3, "C")], "every failure, in capability order, named");
        assert!(stats.failed[0].error.contains("-126"));
    }

    #[test]
    fn capabilities_without_a_get_bit_are_skipped_and_never_read() {
        let cam = FakeCamera::new().with(1, OP_SET, json!(1)).with(2, OP_GET, json!(2));
        let mut s = empty_snap();
        let stats = capture(&cam, &HashMap::new(), &mut s);
        assert_eq!((stats.skipped_no_get, stats.read_ok), (1, 1));
        assert_eq!(cam.reads(), [2]);
    }

    #[test]
    fn an_empty_body_yields_an_empty_snapshot_and_zero_counts() {
        let mut s = empty_snap();
        let stats = capture(&FakeCamera::new(), &HashMap::new(), &mut s);
        assert_eq!(stats, CaptureStats::default());
        assert!(s.properties.is_empty());
    }

    #[test]
    fn counts_always_account_for_every_capability() {
        let cam = FakeCamera::new()
            .with(1, OP_GET, json!(1))
            .with(2, OP_SET, json!(1))
            .with_unreadable(3, OP_GET)
            .with(4, OP_GET, json!(4));
        let mut s = empty_snap();
        let st = capture(&cam, &HashMap::new(), &mut s);
        assert_eq!(st.read_ok + st.read_err + st.skipped_no_get, cam.capabilities().len());
    }

    #[test]
    fn capture_stats_json_shape() {
        let cam = FakeCamera::new().with_unreadable(7, OP_GET);
        let mut s = empty_snap();
        let st = capture(&cam, &names(&[(7, "G")]), &mut s);
        assert_eq!(
            serde_json::to_value(&st).unwrap(),
            json!({"read_ok": 0, "read_err": 1, "skipped_no_get": 0, "unknown_schema": 0,
                   "failed": [{"code": 7, "name": "G", "error": "SDK call `GetCapability` returned error code -126"}]})
        );
    }
}

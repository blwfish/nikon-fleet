//! Paginated views of a snapshot and of a diff, for tools whose output has to
//! fit in a model's context (docs/mcp-plan.md §4: "paginated with
//! `truncated`/`next`, no silent cap").
//!
//! Rules shared by both views:
//! - `total` counts everything that matched the filter, before paging.
//! - `limit` is clamped to [`MAX_PAGE`]; `limit_clamped` says when it was.
//! - `truncated` is true exactly when matching entries remain after this page,
//!   and `next` is then the offset to ask for, **if asking for it would advance**.
//!   A `limit` of 0 returns no entries (a summary), is truncated whenever
//!   anything matched, and has no `next`: the cursor would be the offset just
//!   asked for, and a client that follows `next` would loop forever.
//! - An `offset` past the end returns an empty page that is not truncated.

use serde::Serialize;
use serde_json::Value;

use crate::diff::{Change, Diff, FirmwareAnnotation};
use crate::snapshot::{Camera, Snapshot, Transport};

/// The most entries one page may hold.
pub const MAX_PAGE: usize = 500;

/// Where a page sits in the matching entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Page {
    pub total: usize,
    pub offset: usize,
    pub returned: usize,
    pub limit_clamped: bool,
    pub truncated: bool,
    pub next: Option<usize>,
}

/// The `[start, end)` window of `total` matching entries for a request.
fn window(total: usize, offset: usize, limit: usize) -> (usize, usize, Page) {
    let clamped = limit > MAX_PAGE;
    let limit = limit.min(MAX_PAGE);
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    let truncated = end < total;
    let page = Page {
        total,
        offset,
        returned: end - start,
        limit_clamped: clamped,
        truncated,
        next: (truncated && end > start).then_some(end),
    };
    (start, end, page)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PropertyView {
    pub name: String,
    pub code: u32,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SnapshotView {
    pub camera: Camera,
    pub captured_at: String,
    pub label: Option<String>,
    pub transport: Transport,
    /// Every property in the snapshot, whatever the filter.
    pub property_count: usize,
    pub page: Page,
    pub properties: Vec<PropertyView>,
    /// The owner strings and FTP profiles, when the snapshot holds them. For an agent, so secrets are
    /// always masked here, whatever a person may reveal in a viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extended: Option<crate::ops::render_extended::RenderedExtended>,
}

/// A page of a snapshot's properties, in name order. `name_contains` is a
/// plain case-sensitive substring test on the property name (no pattern syntax).
pub fn snapshot_view(s: &Snapshot, name_contains: Option<&str>, offset: usize, limit: usize) -> SnapshotView {
    let matching: Vec<(&String, &crate::snapshot::PropertyEntry)> = s
        .properties
        .iter()
        .filter(|(n, _)| name_contains.is_none_or(|f| n.contains(f)))
        .collect();
    let (start, end, page) = window(matching.len(), offset, limit);
    SnapshotView {
        camera: s.camera.clone(),
        captured_at: s.captured_at.clone(),
        label: s.label.clone(),
        transport: s.transport,
        property_count: s.properties.len(),
        page,
        properties: matching[start..end]
            .iter()
            .map(|(n, e)| PropertyView { name: (*n).clone(), code: e.code, value: e.value.clone() })
            .collect(),
        extended: s.extended.as_ref().map(|e| crate::ops::render_extended::render_extended(e, false)),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffEntry {
    Changed(Change),
    OnlyInA { name: String },
    OnlyInB { name: String },
    /// A difference in the owner strings or FTP profiles; carries no secret.
    Extended(crate::extended_diff::ExtendedChange),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DiffCounts {
    pub changed: usize,
    pub only_in_a: usize,
    pub only_in_b: usize,
    pub extended: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiffView {
    pub counts: DiffCounts,
    pub page: Page,
    /// Changed first, then only-in-A, then only-in-B, each in name order; then the extended differences.
    pub entries: Vec<DiffEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware_annotation: Option<FirmwareAnnotation>,
}

pub fn diff_view(d: &Diff, offset: usize, limit: usize) -> DiffView {
    let total = d.changed.len() + d.only_in_a.len() + d.only_in_b.len() + d.extended.len();
    let (start, end, page) = window(total, offset, limit);
    let entry = |i: usize| -> DiffEntry {
        if i < d.changed.len() {
            DiffEntry::Changed(d.changed[i].clone())
        } else if i < d.changed.len() + d.only_in_a.len() {
            DiffEntry::OnlyInA { name: d.only_in_a[i - d.changed.len()].clone() }
        } else if i < d.changed.len() + d.only_in_a.len() + d.only_in_b.len() {
            DiffEntry::OnlyInB { name: d.only_in_b[i - d.changed.len() - d.only_in_a.len()].clone() }
        } else {
            DiffEntry::Extended(d.extended[i - d.changed.len() - d.only_in_a.len() - d.only_in_b.len()].clone())
        }
    };
    DiffView {
        counts: DiffCounts { changed: d.changed.len(), only_in_a: d.only_in_a.len(), only_in_b: d.only_in_b.len(), extended: d.extended.len() },
        page,
        entries: (start..end).map(entry).collect(),
        firmware_annotation: d.firmware_annotation.clone(),
    }
}

/// Page the list at `report[key]` of an already-serialised plan, so a plan of
/// hundreds of properties can be read a window at a time.
///
/// The list is replaced by its `[offset, offset + limit)` window and a `page`
/// object is added (the fields of [`Page`] plus `list`, the key that was paged).
/// Everything else in the report, the `digest` and `summary` included, is left
/// alone: they describe the whole plan, not the window, so every page of one
/// plan carries the same digest. Pages come from separate runs against the
/// body, so a digest that differs between pages means the plan changed under
/// the reader. The same window rules as the other views apply (see the module
/// docs). A report with no array at `key` is returned unchanged and without a
/// `page`, rather than guessed at.
pub fn page_list(mut report: Value, key: &str, offset: usize, limit: usize) -> Value {
    let Some(items) = report.get(key).and_then(Value::as_array) else {
        return report;
    };
    let (start, end, page) = window(items.len(), offset, limit);
    let window: Vec<Value> = items[start..end].to_vec();
    let mut page_json = serde_json::to_value(page).expect("a Page always serialises");
    page_json["list"] = Value::from(key);
    report[key] = Value::Array(window);
    report["page"] = page_json;
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snap(n: usize) -> Snapshot {
        let mut s = Snapshot::new(
            Camera { model: "Z 9".into(), serial: "S".into(), firmware: "5.31".into() },
            Transport::Usb,
            "t".into(),
        );
        for i in 0..n {
            s.insert(format!("kNkMAIDCapability_P{i:03}"), i as u32, json!(i));
        }
        s
    }

    fn diff_of(c: usize, a: usize, b: usize) -> Diff {
        Diff {
            changed: (0..c).map(|i| Change { name: format!("C{i}"), code: i as u32, value_a: json!(0), value_b: json!(1) }).collect(),
            only_in_a: (0..a).map(|i| format!("A{i}")).collect(),
            only_in_b: (0..b).map(|i| format!("B{i}")).collect(),
            extended: Vec::new(),
            firmware_annotation: None,
        }
    }

    #[test]
    fn a_page_that_ends_exactly_at_the_total_is_not_truncated() {
        for (n, limit, trunc) in [(10usize, 9usize, true), (10, 10, false), (10, 11, false)] {
            let v = snapshot_view(&snap(n), None, 0, limit);
            assert_eq!(v.page.truncated, trunc, "n={n} limit={limit}");
            assert_eq!(v.page.next, trunc.then_some(limit), "n={n} limit={limit}");
            assert_eq!(v.page.returned, limit.min(n));
        }
    }

    #[test]
    fn limit_zero_is_a_summary_that_says_more_exists() {
        let v = snapshot_view(&snap(3), None, 0, 0);
        assert!(v.properties.is_empty());
        assert_eq!((v.page.total, v.page.returned, v.page.truncated, v.page.next), (3, 0, true, None));
        assert_eq!(v.property_count, 3);
        let empty = snapshot_view(&snap(0), None, 0, 0);
        assert!(!empty.page.truncated && empty.page.next.is_none(), "nothing matched, nothing remains");
    }

    #[test]
    fn a_page_gets_a_next_cursor_only_when_asking_for_it_advances() {
        // limit 0 / 1 / 2 at the same offset: only a page that returns something moves the cursor.
        let s = snap(3);
        for (offset, limit, next) in [(0usize, 0usize, None), (0, 1, Some(1)), (2, 0, None), (2, 1, None), (1, 1, Some(2)), (3, 0, None)] {
            let v = snapshot_view(&s, None, offset, limit);
            assert_eq!(v.page.next, next, "offset={offset} limit={limit}");
            if let Some(n) = v.page.next {
                assert!(n > offset, "following `next` must move forward: offset={offset} limit={limit}");
            }
        }
        // A limit-0 client that follows `next` to the end terminates immediately instead of looping.
        let v = snapshot_view(&s, None, 1, 0);
        assert_eq!((v.page.truncated, v.page.next), (true, None), "still says more exists, without a loopable cursor");
        // The same on a diff and on a plan list, which share `window`.
        let d = diff_view(&diff_of(1, 1, 1), 1, 0);
        assert_eq!((d.page.truncated, d.page.next), (true, None));
        let p = page_list(json!({"steps": [0, 1, 2]}), "steps", 1, 0);
        assert_eq!((p["page"]["truncated"].as_bool(), p["page"]["next"].is_null()), (Some(true), true));
    }

    #[test]
    fn offset_at_below_and_above_the_total() {
        let s = snap(5);
        let below = snapshot_view(&s, None, 4, 10);
        assert_eq!((below.page.returned, below.page.truncated), (1, false));
        assert_eq!(below.properties[0].name, "kNkMAIDCapability_P004");
        let at = snapshot_view(&s, None, 5, 10);
        assert_eq!((at.page.returned, at.page.truncated), (0, false));
        let above = snapshot_view(&s, None, 6, 10);
        assert_eq!((above.page.returned, above.page.truncated, above.page.offset), (0, false, 6));
        let huge = snapshot_view(&s, None, usize::MAX, usize::MAX);
        assert_eq!((huge.page.returned, huge.page.truncated, huge.page.limit_clamped), (0, false, true));
    }

    #[test]
    fn paging_through_visits_every_entry_once_in_order() {
        let s = snap(23);
        let (mut offset, mut seen) = (0, Vec::new());
        for _ in 0..10 {
            // bounded: a page that never ends fails the assertion instead of hanging
            let v = snapshot_view(&s, None, offset, 10);
            seen.extend(v.properties.iter().map(|p| p.code));
            match v.page.next {
                Some(n) => offset = n,
                None => break,
            }
        }
        assert_eq!(seen, (0..23).collect::<Vec<u32>>());
    }

    #[test]
    fn the_limit_is_clamped_to_the_page_maximum_and_says_so() {
        let v = snapshot_view(&snap(MAX_PAGE + 1), None, 0, MAX_PAGE);
        assert!(!v.page.limit_clamped && v.page.truncated);
        let v = snapshot_view(&snap(MAX_PAGE + 1), None, 0, MAX_PAGE + 1);
        assert!(v.page.limit_clamped, "asked for one more than allowed");
        assert_eq!(v.page.returned, MAX_PAGE);
        assert_eq!(v.page.next, Some(MAX_PAGE));
    }

    #[test]
    fn the_name_filter_is_a_case_sensitive_substring_and_counts_matches() {
        let mut s = snap(0);
        s.insert("kNkMAIDCapability_ISO", 1, json!(1));
        s.insert("kNkMAIDCapability_AutoISO", 2, json!(1));
        s.insert("kNkMAIDCapability_Aperture", 3, json!(1));
        let v = snapshot_view(&s, Some("ISO"), 0, 10);
        assert_eq!((v.page.total, v.property_count), (2, 3));
        assert!(snapshot_view(&s, Some("iso"), 0, 10).properties.is_empty(), "no case folding");
        assert!(snapshot_view(&s, Some("kNk.*"), 0, 10).properties.is_empty(), "no pattern syntax");
        assert_eq!(snapshot_view(&s, Some(""), 0, 10).page.total, 3, "empty substring matches all");
    }

    #[test]
    fn the_snapshot_view_carries_the_extended_section_with_secrets_masked() {
        use crate::extended::{Extended, FtpProfileEntry, FtpState, FtpStore, ReadFrom, hex_encode};
        use crate::ptp_usb::{FtpProfile, encode_ftp_profile};
        let blob = encode_ftp_profile(&FtpProfile {
            profile_name: "rig".into(),
            ssid_24ghz: "net".into(),
            ssid_5ghz: "wifi-key-xyz".into(),
            host: "h".into(),
            username: "u".into(),
            password: "hunter2-pass".into(),
            port: 8080,
        })
        .unwrap();
        let mut s = snap(2);
        s.extended = Some(Extended {
            read_from: ReadFrom { model: "Z 9".into(), usb_serial: "1".into() },
            props: Default::default(),
            ftp: FtpState::Read(FtpStore {
                count: 1,
                limit: 20,
                active_slot: Some(0),
                device_name: String::new(),
                truncated: false,
                profiles: vec![FtpProfileEntry { slot: 0, hex: hex_encode(&blob) }],
            }),
        });
        let json = serde_json::to_string(&snapshot_view(&s, None, 0, 100)).unwrap();
        assert!(json.contains("\"extended\""), "{json}");
        assert!(json.contains("\"rig\""));
        assert!(!json.contains("hunter2-pass") && !json.contains("wifi-key-xyz"), "an agent must not receive the secrets: {json}");
        assert!(!serde_json::to_string(&snapshot_view(&snap(2), None, 0, 100)).unwrap().contains("extended"), "absent when the snapshot has none");
    }

    #[test]
    fn extended_differences_follow_the_property_ones_and_are_counted_and_paged_with_them() {
        use crate::extended_diff::{ExtendedChange, ExtendedChangeKind};
        let mut d = diff_of(1, 1, 1);
        d.extended = (0..2)
            .map(|i| ExtendedChange {
                item: format!("E{i}"),
                difference: ExtendedChangeKind::Changed,
                a: Some("x".into()),
                b: Some("y".into()),
            })
            .collect();
        let all = diff_view(&d, 0, 100);
        assert_eq!(all.counts.extended, 2);
        assert_eq!(all.page.total, 5, "the page counts every kind of entry");
        let last = diff_view(&d, 3, 100);
        assert!(matches!(&last.entries[..], [DiffEntry::Extended(a), DiffEntry::Extended(b)] if a.item == "E0" && b.item == "E1"));
        let json = serde_json::to_value(&last.entries[0]).unwrap();
        assert_eq!(json["kind"], "extended");
        assert_eq!(json["difference"], "changed", "the entry's own field does not collide with the tag");
    }

    #[test]
    fn diff_entries_run_changed_then_a_then_b_across_page_boundaries() {
        let d = diff_of(2, 2, 2);
        let names = |v: &DiffView| -> Vec<String> {
            v.entries
                .iter()
                .map(|e| match e {
                    DiffEntry::Changed(c) => c.name.clone(),
                    DiffEntry::OnlyInA { name } | DiffEntry::OnlyInB { name } => name.clone(),
                    DiffEntry::Extended(c) => c.item.clone(),
                })
                .collect()
        };
        let all = diff_view(&d, 0, 100);
        assert_eq!(names(&all), ["C0", "C1", "A0", "A1", "B0", "B1"]);
        assert_eq!(all.counts, DiffCounts { changed: 2, only_in_a: 2, only_in_b: 2, extended: 0 });
        let mid = diff_view(&d, 1, 3);
        assert_eq!(names(&mid), ["C1", "A0", "A1"]);
        assert_eq!(mid.page.next, Some(4));
        assert_eq!(names(&diff_view(&d, 4, 3)), ["B0", "B1"]);
    }

    #[test]
    fn an_empty_diff_is_an_empty_untruncated_page() {
        let v = diff_view(&Diff::default(), 0, 10);
        assert_eq!((v.page.total, v.page.returned, v.page.truncated), (0, 0, false));
        assert_eq!(v.counts, DiffCounts { changed: 0, only_in_a: 0, only_in_b: 0, extended: 0 });
    }

    #[test]
    fn entries_serialise_with_a_closed_kind_tag() {
        let v = serde_json::to_value(diff_view(&diff_of(1, 1, 1), 0, 10)).unwrap();
        let kinds: Vec<&str> = v["entries"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["changed", "only_in_a", "only_in_b"]);
        assert_eq!(v["entries"][0]["value_b"], json!(1));
    }
    #[test]
    fn page_list_windows_one_list_and_leaves_the_rest_alone() {
        let report = json!({"digest": "d", "summary": "s", "outcomes": [0, 1, 2, 3, 4]});
        let v = page_list(report, "outcomes", 1, 2);
        assert_eq!(v["outcomes"], json!([1, 2]));
        assert_eq!((v["digest"].as_str(), v["summary"].as_str()), (Some("d"), Some("s")));
        assert_eq!(v["page"]["total"], 5);
        assert_eq!(v["page"]["returned"], 2);
        assert_eq!(v["page"]["truncated"], true);
        assert_eq!(v["page"]["next"], 3);
        assert_eq!(v["page"]["list"], "outcomes");
    }

    #[test]
    fn page_list_at_below_and_above_the_total() {
        let items = || json!({"steps": [0, 1, 2, 3, 4]});
        let below = page_list(items(), "steps", 0, 4);
        assert_eq!((below["page"]["truncated"].as_bool(), below["steps"].as_array().map(Vec::len)), (Some(true), Some(4)));
        let at = page_list(items(), "steps", 0, 5);
        assert_eq!((at["page"]["truncated"].as_bool(), at["page"]["next"].is_null()), (Some(false), true));
        let above = page_list(items(), "steps", 0, 6);
        assert_eq!((above["page"]["truncated"].as_bool(), above["steps"].as_array().map(Vec::len)), (Some(false), Some(5)));
        let past = page_list(items(), "steps", 9, 3);
        assert_eq!((past["page"]["returned"].as_u64(), past["page"]["truncated"].as_bool()), (Some(0), Some(false)));
        assert_eq!(past["page"]["offset"], 9);
    }

    #[test]
    fn page_list_limit_zero_is_a_summary_and_over_max_is_clamped() {
        let zero = page_list(json!({"outcomes": [1, 2]}), "outcomes", 0, 0);
        assert_eq!(zero["outcomes"], json!([]));
        assert_eq!((zero["page"]["truncated"].as_bool(), zero["page"]["next"].as_u64()), (Some(true), None));
        let big: Vec<usize> = (0..MAX_PAGE + 1).collect();
        let clamped = page_list(json!({"outcomes": big}), "outcomes", 0, MAX_PAGE + 1);
        assert_eq!(clamped["outcomes"].as_array().map(Vec::len), Some(MAX_PAGE));
        assert_eq!((clamped["page"]["limit_clamped"].as_bool(), clamped["page"]["next"].as_u64()), (Some(true), Some(MAX_PAGE as u64)));
    }

    #[test]
    fn page_list_with_no_array_at_the_key_changes_nothing() {
        for report in [json!({"x": 1}), json!({"outcomes": "text"}), json!({"outcomes": null}), json!(null)] {
            assert_eq!(page_list(report.clone(), "outcomes", 0, 10), report);
        }
    }

    #[test]
    fn page_list_walks_a_plan_once_in_order() {
        let report = json!({"outcomes": (0..23).collect::<Vec<u32>>()});
        let (mut offset, mut seen) = (0, Vec::new());
        for _ in 0..10 {
            let v = page_list(report.clone(), "outcomes", offset, 10);
            seen.extend(v["outcomes"].as_array().unwrap().iter().map(|x| x.as_u64().unwrap()));
            match v["page"]["next"].as_u64() {
                Some(n) => offset = n as usize,
                None => break,
            }
        }
        assert_eq!(seen, (0..23).collect::<Vec<u64>>());
    }
}

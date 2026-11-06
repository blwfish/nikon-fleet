//! Comparing the extended sections (`crate::extended`) of two snapshots of one model.
//!
//! The result never contains a secret: a differing password or Wi-Fi key is reported as changed, with no
//! value on either side. Everything else is shown as it is stored.
//!
//! A snapshot with no extended section (one taken before it existed, or with `--no-extended`) is not the
//! same as one whose section says "nothing there": comparing the two reports the missing section as one
//! entry, never as a pile of "only in B" rows.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::extended::{Extended, FtpProfileEntry, FtpState, IptcState, IptcStore};
use crate::iptc::{IPTC_FIELDS, IptcRecord};
use crate::ops::render_extended::{ProfileField, profile_fields, prop_row};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtendedChangeKind {
    Changed,
    OnlyInA,
    OnlyInB,
}

/// One difference (`difference` is a name that cannot collide with a tagging field of a wrapper). `item` names it (`Artist`, `FTP profile "rig"`, `FTP profile "rig": Host`); `a` and `b`
/// are what each side shows, `None` where the side lacks the item. Never a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtendedChange {
    pub item: String,
    pub difference: ExtendedChangeKind,
    pub a: Option<String>,
    pub b: Option<String>,
}

fn change(item: impl Into<String>, kind: ExtendedChangeKind, a: Option<String>, b: Option<String>) -> ExtendedChange {
    ExtendedChange { item: item.into(), difference: kind, a, b }
}

/// A profile as it is compared: its name (a decode failure has none) and decoded fields.
struct Prof<'a> {
    entry: &'a FtpProfileEntry,
    name: Option<String>,
    fields: Vec<ProfileField>,
}

fn profs(list: &[FtpProfileEntry]) -> Vec<Prof<'_>> {
    list.iter()
        .map(|entry| match entry.decode() {
            Ok(p) => Prof { entry, name: Some(p.profile_name.clone()), fields: profile_fields(&p, false) },
            Err(_) => Prof { entry, name: None, fields: Vec::new() },
        })
        .collect()
}

fn label(p: &Prof<'_>) -> String {
    match &p.name {
        Some(n) => format!("FTP profile {n:?}"),
        None => format!("FTP profile in slot {} (not decodable)", p.entry.slot),
    }
}

fn clear(f: &ProfileField) -> &str {
    f.secret.as_ref().map_or(f.value.as_str(), |s| s.reveal())
}

/// Differences between two profiles that share a name.
fn field_changes(a: &Prof<'_>, b: &Prof<'_>, out: &mut Vec<ExtendedChange>) {
    let before = out.len();
    for (fa, fb) in a.fields.iter().zip(&b.fields) {
        if clear(fa) == clear(fb) {
            continue;
        }
        let secret = fa.secret.is_some() || fb.secret.is_some();
        let (va, vb) = if secret { (None, None) } else { (Some(fa.value.clone()), Some(fb.value.clone())) };
        let item = format!("{}: {}{}", label(a), fa.label, if secret { " (secret, value not shown)" } else { "" });
        out.push(change(item, ExtendedChangeKind::Changed, va, vb));
    }
    // Different bytes with every shown field equal: the blob holds something the table does not show.
    if out.len() == before && a.entry.hex != b.entry.hex {
        out.push(change(
            format!("{} (a part the table does not show)", label(a)),
            ExtendedChangeKind::Changed,
            Some(format!("{} byte(s)", a.entry.hex.len() / 2)),
            Some(format!("{} byte(s)", b.entry.hex.len() / 2)),
        ));
    }
}

fn ftp_changes(a: &FtpState, b: &FtpState, out: &mut Vec<ExtendedChange>) {
    let describe = |s: &FtpState| match s {
        FtpState::NotOnModel => "not on this model".to_string(),
        FtpState::ReadFailed { tag, .. } => format!("not read ({tag})"),
        FtpState::Read(store) => format!("{} stored", store.profiles.len()),
    };
    let (FtpState::Read(sa), FtpState::Read(sb)) = (a, b) else {
        if describe(a) != describe(b) {
            out.push(change("FTP profiles", ExtendedChangeKind::Changed, Some(describe(a)), Some(describe(b))));
        }
        return;
    };
    let (pa, pb) = (profs(&sa.profiles), profs(&sb.profiles));
    // Identical blobs cancel out first (a profile that only moved to another slot is not a change), as a
    // multiset: two identical profiles on one side and one on the other leave one over.
    let mut left: Vec<&Prof<'_>> = pa.iter().collect();
    let mut right: Vec<&Prof<'_>> = pb.iter().collect();
    left.retain(|x| match right.iter().position(|y| y.entry.hex == x.entry.hex) {
        Some(i) => {
            right.remove(i);
            false
        }
        None => true,
    });
    // What is left differs. Pair by name where a name is unique on both sides.
    let unique = |v: &[&Prof<'_>], n: &str| v.iter().filter(|p| p.name.as_deref() == Some(n)).count() == 1;
    let mut unpaired_right: Vec<&Prof<'_>> = right.clone();
    for x in &left {
        let paired = x.name.as_deref().filter(|n| unique(&left, n) && unique(&right, n));
        match paired.and_then(|n| right.iter().find(|y| y.name.as_deref() == Some(n))) {
            Some(y) => {
                unpaired_right.retain(|u| u.entry.hex != y.entry.hex);
                field_changes(x, y, out);
            }
            None => out.push(change(label(x), ExtendedChangeKind::OnlyInA, Some(format!("{} byte(s)", x.entry.hex.len() / 2)), None)),
        }
    }
    for y in unpaired_right {
        out.push(change(label(y), ExtendedChangeKind::OnlyInB, None, Some(format!("{} byte(s)", y.entry.hex.len() / 2))));
    }
    if sa.active_slot != sb.active_slot && pa.iter().all(|p| p.name.is_some()) {
        let act = |s: &crate::extended::FtpStore, ps: &[Prof<'_>]| {
            s.active_slot.and_then(|i| ps.get(i as usize)).map_or("none".to_string(), |p| label(p))
        };
        let (x, y) = (act(sa, &pa), act(sb, &pb));
        if x != y {
            out.push(change("FTP active profile", ExtendedChangeKind::Changed, Some(x), Some(y)));
        }
    }
}

fn iptc_changes(a: &IptcState, b: &IptcState, out: &mut Vec<ExtendedChange>) {
    let describe = |s: &IptcState| match s {
        IptcState::NotRead => "not read".to_string(),
        IptcState::NotOnModel => "not on this model".to_string(),
        IptcState::ReadFailed { tag, .. } => format!("not read ({tag})"),
        IptcState::Read(store) => format!("{} slot(s)", store.slots.len()),
    };
    let (IptcState::Read(sa), IptcState::Read(sb)) = (a, b) else {
        if describe(a) != describe(b) {
            out.push(change("IPTC presets", ExtendedChangeKind::Changed, Some(describe(a)), Some(describe(b))));
        }
        return;
    };
    fn by_slot(s: &IptcStore) -> BTreeMap<u32, &crate::extended::IptcSlotEntry> {
        s.slots.iter().map(|e| (e.slot, e)).collect()
    }
    let (ma, mb) = (by_slot(sa), by_slot(sb));
    let label = |slot: u32, r: &Result<IptcRecord, String>| match r {
        Ok(IptcRecord::Preset(p)) if !p.name.is_empty() => format!("IPTC preset {slot} {:?}", p.name),
        _ => format!("IPTC preset {slot}"),
    };
    for (slot, ea) in &ma {
        let Some(eb) = mb.get(slot) else {
            out.push(change(format!("IPTC slot {slot}"), ExtendedChangeKind::OnlyInA, Some("present".into()), None));
            continue;
        };
        if ea.hex == eb.hex {
            continue;
        }
        match (ea.decode(), eb.decode()) {
            (Ok(IptcRecord::Preset(pa)), Ok(IptcRecord::Preset(pb))) => {
                if pa.name != pb.name {
                    out.push(change(format!("IPTC preset {slot}: name"), ExtendedChangeKind::Changed, Some(format!("{:?}", pa.name)), Some(format!("{:?}", pb.name))));
                }
                for ((name, va), vb) in IPTC_FIELDS.iter().zip(&pa.fields).zip(&pb.fields) {
                    if va != vb {
                        out.push(change(
                            format!("{}: {name}", label(*slot, &Ok(IptcRecord::Preset(pb.clone())))),
                            ExtendedChangeKind::Changed,
                            Some(format!("{va:?}")),
                            Some(format!("{vb:?}")),
                        ));
                    }
                }
            }
            (ra, rb) => out.push(change(
                format!("{} (record layout not shown)", label(*slot, &ra).replace("IPTC preset", "IPTC slot")),
                ExtendedChangeKind::Changed,
                Some(format!("{} byte(s)", ea.hex.len() / 2)),
                Some(format!("{} byte(s) {}", eb.hex.len() / 2, if rb.is_err() { "(not decodable)" } else { "" }).trim().to_string()),
            )),
        }
    }
    for slot in mb.keys().filter(|s| !ma.contains_key(s)) {
        out.push(change(format!("IPTC slot {slot}"), ExtendedChangeKind::OnlyInB, None, Some("present".into())));
    }
}

/// The differences between the two extended sections, in a stable order: properties by code, then FTP.
pub fn diff_extended(a: Option<&Extended>, b: Option<&Extended>) -> Vec<ExtendedChange> {
    let mut out = Vec::new();
    let (a, b) = match (a, b) {
        (None, None) => return out,
        (Some(_), None) => {
            return vec![change(
                "extended section (owner strings, FTP profiles)",
                ExtendedChangeKind::OnlyInA,
                Some("present".into()),
                None,
            )];
        }
        (None, Some(_)) => {
            return vec![change(
                "extended section (owner strings, FTP profiles)",
                ExtendedChangeKind::OnlyInB,
                None,
                Some("present".into()),
            )];
        }
        (Some(a), Some(b)) => (a, b),
    };
    let rows = |e: &Extended| e.props.iter().map(|(k, p)| (k.clone(), prop_row(p))).collect::<BTreeMap<_, _>>();
    let (ra, rb) = (rows(a), rows(b));
    for (key, x) in &ra {
        match rb.get(key) {
            Some(y) if x.display == y.display => {}
            Some(y) => out.push(change(x.name.clone(), ExtendedChangeKind::Changed, Some(x.display.clone()), Some(y.display.clone()))),
            None => out.push(change(x.name.clone(), ExtendedChangeKind::OnlyInA, Some(x.display.clone()), None)),
        }
    }
    for (key, y) in &rb {
        if !ra.contains_key(key) {
            out.push(change(y.name.clone(), ExtendedChangeKind::OnlyInB, None, Some(y.display.clone())));
        }
    }
    ftp_changes(&a.ftp, &b.ftp, &mut out);
    iptc_changes(&a.iptc, &b.iptc, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extended::{ExtendedProp, FtpStore, PropState, ReadFrom, hex_encode};
    use crate::ptp_usb::{FtpProfile, encode_ftp_profile};

    fn blob(name: &str, host: &str, password: &str) -> Vec<u8> {
        encode_ftp_profile(&FtpProfile {
            profile_name: name.into(),
            ssid_24ghz: "net".into(),
            ssid_5ghz: "key-12345".into(),
            host: host.into(),
            username: "fleet".into(),
            password: password.into(),
            port: 8080,
        })
        .unwrap()
    }

    fn ext(artist: &str, blobs: &[Vec<u8>], active: Option<u32>) -> Extended {
        let mut props = BTreeMap::new();
        props.insert(
            "0xd072".into(),
            ExtendedProp { code: 0xd072, name: "Artist".into(), state: PropState::Text { text: artist.into() } },
        );
        Extended {
            read_from: ReadFrom { model: "Z 9".into(), usb_serial: "1".into() },
            props,
            iptc: Default::default(),
            ftp: FtpState::Read(FtpStore {
                count: blobs.len() as u32,
                limit: 20,
                active_slot: active,
                device_name: String::new(),
                truncated: false,
                profiles: blobs.iter().enumerate().map(|(i, b)| FtpProfileEntry { slot: i as u32, hex: hex_encode(b) }).collect(),
            }),
        }
    }

    #[test]
    fn identical_sections_have_no_differences() {
        let e = ext("Brian", &[blob("a", "10.0.0.1", "password-1")], Some(0));
        assert!(diff_extended(Some(&e), Some(&e.clone())).is_empty());
        assert!(diff_extended(None, None).is_empty());
    }

    #[test]
    fn a_changed_string_is_reported_with_both_values() {
        let (a, b) = (ext("Brian", &[], None), ext("Brian L Wong", &[], None));
        let d = diff_extended(Some(&a), Some(&b));
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].item.as_str(), d[0].difference), ("Artist", ExtendedChangeKind::Changed));
        assert_eq!((d[0].a.as_deref(), d[0].b.as_deref()), (Some("\"Brian\""), Some("\"Brian L Wong\"")));
    }

    #[test]
    fn a_missing_section_is_one_entry_not_a_pile() {
        let e = ext("Brian", &[blob("a", "h", "password-1")], Some(0));
        let d = diff_extended(Some(&e), None);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].difference, ExtendedChangeKind::OnlyInA);
        assert_eq!(diff_extended(None, Some(&e))[0].difference, ExtendedChangeKind::OnlyInB);
    }

    #[test]
    fn a_changed_password_is_reported_without_either_value() {
        let a = ext("x", &[blob("rig", "10.0.0.1", "old-password")], Some(0));
        let b = ext("x", &[blob("rig", "10.0.0.1", "new-password")], Some(0));
        let d = diff_extended(Some(&a), Some(&b));
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].item.contains("Password") && d[0].item.contains("value not shown"), "{:?}", d[0]);
        assert_eq!((d[0].a.as_deref(), d[0].b.as_deref()), (None, None));
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("old-password") && !json.contains("new-password"), "{json}");
    }

    #[test]
    fn a_changed_host_names_the_field_and_both_values() {
        let a = ext("x", &[blob("rig", "10.0.0.1", "password-1")], Some(0));
        let b = ext("x", &[blob("rig", "10.0.0.9", "password-1")], Some(0));
        let d = diff_extended(Some(&a), Some(&b));
        assert_eq!(d.len(), 1);
        assert!(d[0].item.ends_with("Host"), "{:?}", d[0]);
        assert_eq!((d[0].a.as_deref(), d[0].b.as_deref()), (Some("10.0.0.1"), Some("10.0.0.9")));
    }

    #[test]
    fn a_profile_that_only_moved_slot_is_not_a_change() {
        let (p, q) = (blob("p", "h1", "password-1"), blob("q", "h2", "password-2"));
        let a = ext("x", &[p.clone(), q.clone()], Some(0));
        let b = ext("x", &[q, p], Some(1)); // the same profile active, in the other position
        assert!(diff_extended(Some(&a), Some(&b)).is_empty(), "{:?}", diff_extended(Some(&a), Some(&b)));
    }

    #[test]
    fn an_added_and_a_removed_profile_are_each_one_entry() {
        let a = ext("x", &[blob("old", "h1", "password-1")], Some(0));
        let b = ext("x", &[blob("new", "h2", "password-2")], Some(0));
        let d = diff_extended(Some(&a), Some(&b));
        let kinds: Vec<_> = d.iter().map(|c| c.difference).collect();
        assert!(kinds.contains(&ExtendedChangeKind::OnlyInA) && kinds.contains(&ExtendedChangeKind::OnlyInB), "{d:?}");
        assert!(d.iter().any(|c| c.item.contains("\"old\"")) && d.iter().any(|c| c.item.contains("\"new\"")));
    }

    #[test]
    fn two_identical_profiles_against_one_leave_one_over() {
        let p = blob("dup", "h", "password-1");
        let a = ext("x", &[p.clone(), p.clone()], Some(0));
        let b = ext("x", &[p], Some(0));
        let d = diff_extended(Some(&a), Some(&b));
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].difference, ExtendedChangeKind::OnlyInA);
    }

    #[test]
    fn the_active_profile_changing_is_reported() {
        let (p, q) = (blob("p", "h1", "password-1"), blob("q", "h2", "password-2"));
        let a = ext("x", &[p.clone(), q.clone()], Some(0));
        let b = ext("x", &[p, q], Some(1));
        let d = diff_extended(Some(&a), Some(&b));
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].item, "FTP active profile");
    }

    #[test]
    fn a_model_without_ftp_against_one_with_it_is_one_entry() {
        let a = ext("x", &[blob("p", "h", "password-1")], Some(0));
        let mut b = a.clone();
        b.ftp = FtpState::NotOnModel;
        let d = diff_extended(Some(&a), Some(&b));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].item, "FTP profiles");
    }

    #[test]
    fn iptc_changes_name_the_preset_and_the_field() {
        use crate::extended::IptcSlotEntry;
        use crate::iptc::{IptcPreset, encode_iptc_preset};
        let preset = |name: &str, headline: &str| {
            let mut fields = vec![String::new(); 14];
            fields[2] = headline.into();
            hex_encode(&encode_iptc_preset(&IptcPreset { name: name.into(), fields }).unwrap())
        };
        let with = |hexes: &[(u32, String)]| {
            let mut e = ext("x", &[], None);
            e.iptc = IptcState::Read(IptcStore {
                count: hexes.len() as u32,
                truncated: false,
                slots: hexes.iter().map(|(slot, hex)| IptcSlotEntry { slot: *slot, hex: hex.clone() }).collect(),
            });
            e
        };
        let (a, b) = (with(&[(1, preset("A", "old"))]), with(&[(1, preset("A", "new"))]));
        let d = diff_extended(Some(&a), Some(&b));
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].item.ends_with("Headline") && d[0].item.contains("\"A\""), "{:?}", d[0]);
        assert_eq!((d[0].a.as_deref(), d[0].b.as_deref()), (Some("\"old\""), Some("\"new\"")));
        assert!(diff_extended(Some(&a), Some(&a.clone())).is_empty());
        let renamed = diff_extended(Some(&a), Some(&with(&[(1, preset("B", "old"))])));
        assert_eq!(renamed.len(), 1);
        assert!(renamed[0].item.ends_with("name"));
        let more = diff_extended(Some(&a), Some(&with(&[(1, preset("A", "old")), (2, preset("", ""))])));
        assert!(more.iter().any(|c| c.difference == ExtendedChangeKind::OnlyInB), "{more:?}");
        // A snapshot from before IPTC against one with it is one entry, not a pile.
        let mut old = a.clone();
        old.iptc = IptcState::NotRead;
        let d = diff_extended(Some(&old), Some(&a));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].item, "IPTC presets");
    }
}

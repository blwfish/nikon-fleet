//! Putting a snapshot's extended section (`crate::extended`) back on a body: the owner strings, the copyright and
//! IPTC-preset switches, the IPTC presets and the stored FTP profiles.
//!
//! It is its own operation, not part of the MAID restore: these items are written over raw PTP, which cannot share
//! the USB interface with an SDK session, and nothing here needs the SDK. One raw session reads the body, plans,
//! and (on apply) writes.
//!
//! The shape is the MAID restore's: a pure plan (what would change, what is skipped and why) with a digest and a
//! summary line the caller echoes back to apply; apply recomputes the plan on the body as it is now and refuses a
//! stale one, takes a restore point before the first write, writes each item with the read-back its write path has,
//! then reads the body again and checks that what it wrote now matches the snapshot.
//!
//! ## What is written, and how
//!
//! | category | item | write |
//! |---|---|---|
//! | `strings` | Artist, Copyright, Image comment | `SetDevicePropValue`, read back (`set_string_property`) |
//! | `switches` | EnableCopyright, IPTC preset | `SetDevicePropValue` of one byte, read back (`set_byte_property`) |
//! | `iptc` | each preset slot | `0x9413`, the slot read again and compared byte for byte |
//! | `ftp` | the whole list | `0x90EA` delete-all, then `0x90EE` adds in reverse order (an add lands at the FRONT), each read back |
//!
//! Order of apply: strings, IPTC, switches, FTP. The IPTC preset switch comes after the presets because a Z 9
//! refused to select a preset while all of them were empty.
//!
//! What it cannot do: choose which FTP profile is the active one (no op that selects one was found), write an IPTC
//! slot of another record kind, or restore a list the snapshot read only part of. Each of those is a typed skip in
//! the plan, never a silent omission.
//!
//! The FTP rebuild deletes every stored profile first. If an add then fails the list is partial; the restore point
//! (written before the first write, and a valid input to this same command) holds the original.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::extended::{
    Extended, ExtendedSink, FtpState, IptcState, PropState, ReadFrom, hex_decode, prop_key, read_extended,
};
use crate::iptc::{IptcRecord, decode_iptc_slot};
use crate::ops::apply::{ApplyError, ApplyStatus, RestorePoint, require_confirmation};
use crate::ptp_usb::{
    BYTE_PROP_WRITE_MODELS, BYTE_PROPS, IPTC_WRITE_CONFIRMED_MODELS, PtpUsbError, STRING_PROPS,
    STRING_PROP_WRITE_CONFIRMED_PROPS, STRING_PROP_WRITE_MODELS, check_string_write_gate, decode_ftp_profile,
    encode_ptp_string_value, ftp_write_confirmed,
};
use crate::snapshot::{Camera, Snapshot, SnapshotError, Transport};

/// Bump when the digest's vocabulary changes.
pub const DIGEST_FORMAT: u32 = 1;

/// A group of items, for `--only`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Strings,
    Iptc,
    Switches,
    Ftp,
}

impl Category {
    /// Apply order.
    pub const ALL: [Category; 4] = [Category::Strings, Category::Iptc, Category::Switches, Category::Ftp];

    pub fn tag(self) -> &'static str {
        match self {
            Category::Strings => "strings",
            Category::Iptc => "iptc",
            Category::Switches => "switches",
            Category::Ftp => "ftp",
        }
    }

    pub fn parse(text: &str) -> Result<Category, String> {
        Category::ALL
            .iter()
            .copied()
            .find(|c| c.tag() == text.trim().to_lowercase())
            .ok_or_else(|| format!("{text:?} is not a category; one of: strings, iptc, switches, ftp"))
    }
}

/// Why an item is not written. A closed vocabulary: nothing downstream parses the detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipKind {
    /// The snapshot has no value for it (a snapshot from before it was captured, a read that failed, a body without it).
    SnapshotHasNoValue,
    /// The body answers that it does not have it.
    BodyLacksIt,
    /// The body's current value could not be read, so a change cannot be judged.
    BodyUnreadable,
    /// The value is not one the write path accepts (text outside Latin-1, a byte out of range).
    ValueNotWritable,
    /// The write has not been confirmed on this model.
    ModelNotConfirmed,
    /// A record of a kind there is no layout to write, on either side.
    RecordNotWritable,
    /// The snapshot has a slot the body does not.
    SlotNotOnBody,
    /// The snapshot read only part of the list, so the rest cannot be rebuilt.
    ListTruncated,
    /// A stored profile does not decode, and an undecodable blob is never sent.
    ProfileNotDecodable,
    /// More profiles than the body holds.
    ListTooLong,
}

impl SkipKind {
    pub fn tag(self) -> &'static str {
        match self {
            SkipKind::SnapshotHasNoValue => "snapshot_has_no_value",
            SkipKind::BodyLacksIt => "body_lacks_it",
            SkipKind::BodyUnreadable => "body_unreadable",
            SkipKind::ValueNotWritable => "value_not_writable",
            SkipKind::ModelNotConfirmed => "model_not_confirmed",
            SkipKind::RecordNotWritable => "record_not_writable",
            SkipKind::SlotNotOnBody => "slot_not_on_body",
            SkipKind::ListTruncated => "list_truncated",
            SkipKind::ProfileNotDecodable => "profile_not_decodable",
            SkipKind::ListTooLong => "list_too_long",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ItemAction {
    /// Already what the snapshot says.
    Unchanged,
    /// Left out by `--only`.
    NotSelected,
    /// Would be written. `from` and `to` are for a person: never a password or key.
    Write { from: Option<String>, to: String },
    Skip { reason: SkipKind, #[serde(skip_serializing_if = "Option::is_none")] detail: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanItem {
    pub id: String,
    pub label: String,
    pub category: Category,
    #[serde(flatten)]
    pub action: ItemAction,
    /// Hash of what would be written, so the digest changes when the target changes without the display doing so.
    #[serde(skip)]
    pub fingerprint: String,
}

/// What apply sends for an item.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Work {
    Text { code: u16, text: String },
    Byte { code: u16, value: u8 },
    Iptc { slot: u32, bytes: Vec<u8> },
    Ftp { blobs: Vec<Vec<u8>> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub item: PlanItem,
    work: Option<Work>,
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn item(id: impl Into<String>, label: impl Into<String>, category: Category, action: ItemAction, fp: &str) -> PlanItem {
    PlanItem { id: id.into(), label: label.into(), category, action, fingerprint: sha(fp.as_bytes()) }
}

fn skip(reason: SkipKind, detail: impl Into<String>) -> ItemAction {
    ItemAction::Skip { reason, detail: Some(detail.into()) }
}

fn selected(only: Option<&BTreeSet<Category>>, c: Category) -> bool {
    only.is_none_or(|set| set.contains(&c))
}

fn quoted(text: &str) -> String {
    format!("{text:?}")
}

fn plan_strings(source: &Extended, live: &Extended, model: &str, only: Option<&BTreeSet<Category>>, out: &mut Vec<Planned>) {
    for &(code, name) in &STRING_PROPS {
        let id = prop_key(code);
        if !selected(only, Category::Strings) {
            out.push(Planned { item: item(&id, name, Category::Strings, ItemAction::NotSelected, ""), work: None });
            continue;
        }
        let (action, fp, work) = match (source.prop(code).map(|p| &p.state), live.prop(code).map(|p| &p.state)) {
            (Some(PropState::Text { text }), Some(PropState::Text { text: now })) if text == now => {
                (ItemAction::Unchanged, text.clone(), None)
            }
            (Some(PropState::Text { text }), Some(PropState::Text { text: now })) => {
                if let Err(e) = check_string_write_gate(model, code, STRING_PROP_WRITE_MODELS, STRING_PROP_WRITE_CONFIRMED_PROPS) {
                    (skip(SkipKind::ModelNotConfirmed, e.to_string()), text.clone(), None)
                } else if let Some(why) = string_not_writable(text) {
                    (skip(SkipKind::ValueNotWritable, why), text.clone(), None)
                } else {
                    (
                        ItemAction::Write { from: Some(quoted(now)), to: quoted(text) },
                        text.clone(),
                        Some(Work::Text { code, text: text.clone() }),
                    )
                }
            }
            (Some(PropState::Text { text }), Some(PropState::NotSupported)) => {
                (skip(SkipKind::BodyLacksIt, "the body answers DevicePropNotSupported"), text.clone(), None)
            }
            (Some(PropState::Text { text }), _) => {
                (skip(SkipKind::BodyUnreadable, "the body's current value was not read"), text.clone(), None)
            }
            (_, _) => (skip(SkipKind::SnapshotHasNoValue, "the snapshot holds no text for it"), String::new(), None),
        };
        out.push(Planned { item: item(&id, name, Category::Strings, action, &fp), work });
    }
}

/// Why `text` cannot be sent, or `None`. The same checks `plan_string_property` makes before it writes.
fn string_not_writable(text: &str) -> Option<String> {
    if let Some(c) = text.chars().find(|c| u32::from(*c) > 0xFF) {
        return Some(format!("{c:?} is outside Latin-1; the camera keeps one byte per character here"));
    }
    encode_ptp_string_value(text).err()
}

fn plan_switches(source: &Extended, live: &Extended, model: &str, only: Option<&BTreeSet<Category>>, out: &mut Vec<Planned>) {
    for &(code, name, max) in &BYTE_PROPS {
        let id = prop_key(code);
        if !selected(only, Category::Switches) {
            out.push(Planned { item: item(&id, name, Category::Switches, ItemAction::NotSelected, ""), work: None });
            continue;
        }
        let one_byte = |s: Option<&PropState>| match s {
            Some(PropState::Bytes { hex }) => hex_decode(hex).ok().filter(|b| b.len() == 1).map(|b| b[0]),
            _ => None,
        };
        let src = one_byte(source.prop(code).map(|p| &p.state));
        let now = one_byte(live.prop(code).map(|p| &p.state));
        let (action, fp, work) = match (src, now) {
            (None, _) => (skip(SkipKind::SnapshotHasNoValue, "the snapshot holds no one-byte value for it"), String::new(), None),
            (Some(v), Some(n)) if v == n => (ItemAction::Unchanged, v.to_string(), None),
            (Some(v), None) => {
                let lacks = matches!(live.prop(code).map(|p| &p.state), Some(PropState::NotSupported));
                let (kind, why) = if lacks {
                    (SkipKind::BodyLacksIt, "the body answers DevicePropNotSupported")
                } else {
                    (SkipKind::BodyUnreadable, "the body's current value was not read")
                };
                (skip(kind, why), v.to_string(), None)
            }
            (Some(v), Some(n)) => {
                let confirmed = BYTE_PROP_WRITE_MODELS.iter().any(|(c, models)| *c == code && models.contains(&model));
                if v > max {
                    (skip(SkipKind::ValueNotWritable, format!("{v} is above the highest value {name} takes, {max}")), v.to_string(), None)
                } else if !confirmed {
                    (skip(SkipKind::ModelNotConfirmed, format!("writing {name} is not confirmed on {model}")), v.to_string(), None)
                } else {
                    (
                        ItemAction::Write { from: Some(format!("0x{n:02x}")), to: format!("0x{v:02x}") },
                        v.to_string(),
                        Some(Work::Byte { code, value: v }),
                    )
                }
            }
        };
        out.push(Planned { item: item(&id, name, Category::Switches, action, &fp), work });
    }
}

fn preset_summary(bytes: &[u8]) -> String {
    match decode_iptc_slot(bytes) {
        Ok(IptcRecord::Preset(p)) if p.is_empty() => "empty".to_string(),
        Ok(IptcRecord::Preset(p)) => format!("{:?}, {} field(s) filled", p.name, p.fields.iter().filter(|f| !f.is_empty()).count()),
        Ok(IptcRecord::Other { kind, .. }) => format!("a kind {kind} record"),
        Err(_) => "not decodable".to_string(),
    }
}

fn plan_iptc(source: &Extended, live: &Extended, model: &str, only: Option<&BTreeSet<Category>>, out: &mut Vec<Planned>) {
    let whole = |action: ItemAction| Planned { item: item("iptc", "IPTC presets", Category::Iptc, action, ""), work: None };
    if !selected(only, Category::Iptc) {
        out.push(whole(ItemAction::NotSelected));
        return;
    }
    let IptcState::Read(src) = &source.iptc else {
        out.push(whole(skip(SkipKind::SnapshotHasNoValue, "the snapshot holds no IPTC presets")));
        return;
    };
    let live_store = match &live.iptc {
        IptcState::Read(s) => s,
        IptcState::NotOnModel => {
            out.push(whole(skip(SkipKind::BodyLacksIt, "this model has no IPTC presets")));
            return;
        }
        _ => {
            out.push(whole(skip(SkipKind::BodyUnreadable, "the body's IPTC presets were not read")));
            return;
        }
    };
    for entry in &src.slots {
        let id = format!("iptc:{}", entry.slot);
        let label = format!("IPTC preset {}", entry.slot);
        let Some(now) = live_store.slots.iter().find(|s| s.slot == entry.slot) else {
            out.push(Planned { item: item(&id, &label, Category::Iptc, skip(SkipKind::SlotNotOnBody, "the body has no such slot"), &entry.hex), work: None });
            continue;
        };
        let (action, work) = if now.hex == entry.hex {
            (ItemAction::Unchanged, None)
        } else {
            let (a, b) = (entry.decode(), now.decode());
            match (a, b, hex_decode(&entry.hex)) {
                (Ok(IptcRecord::Preset(_)), Ok(IptcRecord::Preset(_)), Ok(bytes)) => {
                    if IPTC_WRITE_CONFIRMED_MODELS.contains(&model) {
                        let from = preset_summary(&hex_decode(&now.hex).unwrap_or_default());
                        (ItemAction::Write { from: Some(from), to: preset_summary(&bytes) }, Some(Work::Iptc { slot: entry.slot, bytes }))
                    } else {
                        (skip(SkipKind::ModelNotConfirmed, format!("an IPTC write is not confirmed on {model}")), None)
                    }
                }
                _ => (skip(SkipKind::RecordNotWritable, "the slot is not an IPTC preset on one side"), None),
            }
        };
        out.push(Planned { item: item(&id, &label, Category::Iptc, action, &entry.hex), work });
    }
}

fn plan_ftp(source: &Extended, live: &Extended, model: &str, only: Option<&BTreeSet<Category>>, out: &mut Vec<Planned>) {
    let one = |action: ItemAction, fp: &str, work: Option<Work>| Planned { item: item("ftp", "FTP profiles", Category::Ftp, action, fp), work };
    if !selected(only, Category::Ftp) {
        out.push(one(ItemAction::NotSelected, "", None));
        return;
    }
    let FtpState::Read(src) = &source.ftp else {
        out.push(one(skip(SkipKind::SnapshotHasNoValue, "the snapshot holds no FTP profiles"), "", None));
        return;
    };
    let now = match &live.ftp {
        FtpState::Read(s) => s,
        FtpState::NotOnModel => {
            out.push(one(skip(SkipKind::BodyLacksIt, "this model has no FTP profiles"), "", None));
            return;
        }
        FtpState::ReadFailed { .. } => {
            out.push(one(skip(SkipKind::BodyUnreadable, "the body's FTP profiles were not read"), "", None));
            return;
        }
    };
    let fp: String = src.profiles.iter().map(|p| p.hex.as_str()).collect::<Vec<_>>().join(",");
    if now.profiles.len() == src.profiles.len() && now.profiles.iter().zip(&src.profiles).all(|(a, b)| a.hex == b.hex) {
        out.push(one(ItemAction::Unchanged, &fp, None));
        return;
    }
    let refuse = |reason: SkipKind, why: &str| one(skip(reason, why), &fp, None);
    if src.truncated {
        out.push(refuse(SkipKind::ListTruncated, "the snapshot read only part of the list"));
        return;
    }
    let mut blobs = Vec::new();
    for p in &src.profiles {
        match p.blob() {
            Ok(b) if decode_ftp_profile(&b).is_ok() => blobs.push(b),
            _ => {
                out.push(refuse(SkipKind::ProfileNotDecodable, &format!("the profile in slot {} does not decode", p.slot)));
                return;
            }
        }
    }
    if src.profiles.len() > now.limit as usize {
        out.push(refuse(SkipKind::ListTooLong, "more profiles than the body holds"));
        return;
    }
    if !ftp_write_confirmed(model) {
        out.push(refuse(SkipKind::ModelNotConfirmed, &format!("FTP profile writes are not confirmed on {model}")));
        return;
    }
    let names: Vec<String> = blobs
        .iter()
        .map(|b| decode_ftp_profile(b).map(|p| quoted(&p.profile_name)).unwrap_or_default())
        .collect();
    let from = if now.profiles.is_empty() {
        "none stored".to_string()
    } else {
        format!("{} stored, all deleted first", now.profiles.len())
    };
    out.push(one(
        ItemAction::Write { from: Some(from), to: format!("{} profile(s): {}", blobs.len(), names.join(", ")) },
        &fp,
        Some(Work::Ftp { blobs }),
    ));
}

/// The plan: one item per thing the snapshot could put back, in apply order. Pure.
pub fn plan_extended_restore(source: &Extended, live: &Extended, only: Option<&BTreeSet<Category>>) -> Vec<Planned> {
    let model = live.read_from.model.as_str();
    let mut out = Vec::new();
    plan_strings(source, live, model, only, &mut out);
    plan_iptc(source, live, model, only, &mut out);
    plan_switches(source, live, model, only, &mut out);
    plan_ftp(source, live, model, only, &mut out);
    out
}

/// What the plan says would be written.
pub fn would_change(plan: &[Planned]) -> usize {
    plan.iter().filter(|p| matches!(p.item.action, ItemAction::Write { .. })).count()
}

/// The line a person is shown and the caller must echo to apply.
pub fn summary_text(target: &Camera, source_name: &str, would_change: usize, only: Option<&BTreeSet<Category>>) -> String {
    let scope = match only {
        None => "all".to_string(),
        Some(set) => set.iter().map(|c| c.tag()).collect::<Vec<_>>().join(", "),
    };
    format!(
        "{} {}: restore owner strings, switches, IPTC and FTP profiles from {source_name}: {would_change} item(s) would change; only: {scope}",
        target.model, target.serial
    )
}

fn digest(source_bytes: &[u8], target_serial: &str, only: Option<&BTreeSet<Category>>, plan: &[Planned]) -> String {
    let items: Vec<_> = plan
        .iter()
        // The whole action, not just its tag: a plan that said "replace X with Y" must not apply once X has become Z.
        .map(|p| json!({"id": p.item.id, "action": p.item.action, "fingerprint": p.item.fingerprint}))
        .collect();
    let doc = json!({
        "format": DIGEST_FORMAT,
        "kind": "restore_extended",
        "source_sha256": sha(source_bytes),
        "target_serial": target_serial,
        "only": only.map(|s| s.iter().map(|c| c.tag()).collect::<Vec<_>>()),
        "items": items,
    });
    sha(&serde_json::to_vec(&doc).expect("a Value always serialises"))
}

/// The undo as a command that can be pasted and run: restore the restore point with the report's `undo_set` as
/// `--only`. `None` when there is nothing to undo. `data_dir` is named when the restore point is not in the default.
pub fn undo_command(restore_point_file: &str, undo_set: &[String], data_dir: Option<&str>) -> Option<String> {
    if undo_set.is_empty() {
        return None;
    }
    let only: String = undo_set.iter().map(|c| format!(" --only {c}")).collect();
    let dir = data_dir
        .map(|d| if d.contains(char::is_whitespace) { format!(" --data-dir \"{d}\"") } else { format!(" --data-dir {d}") })
        .unwrap_or_default();
    Some(format!("nikon-fleet{dir} restore-extended {restore_point_file}{only}"))
}

/// A refusal before anything is written.
#[derive(Debug)]
pub enum Refusal {
    /// The snapshot has no extended section (taken before it existed, or with `--no-extended`).
    SnapshotHasNoExtended,
    /// The raw session could not read the body.
    BodyNotReadable { tag: String, detail: String },
    /// The snapshot is of another model, or another body of this one.
    NotThisBody { why: String },
    /// The restore point could not be written, so nothing was.
    RestorePoint(SnapshotError),
}

impl Refusal {
    pub fn tag(&self) -> &'static str {
        match self {
            Refusal::SnapshotHasNoExtended => "snapshot_has_no_extended",
            Refusal::BodyNotReadable { .. } => "body_not_readable",
            Refusal::NotThisBody { .. } => "not_this_body",
            Refusal::RestorePoint(_) => "restore_point_not_written",
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::SnapshotHasNoExtended => write!(
                f,
                "this snapshot has no extended section (owner strings, IPTC, FTP profiles): it was taken before they were captured, or with --no-extended. Nothing was changed"
            ),
            Refusal::BodyNotReadable { tag, detail } => write!(f, "the body could not be read over raw USB ({tag}: {detail}). Nothing was changed"),
            Refusal::NotThisBody { why } => write!(f, "{why}. Nothing was changed"),
            Refusal::RestorePoint(e) => write!(f, "the restore point could not be written ({e}); nothing was changed"),
        }
    }
}

impl std::error::Error for Refusal {}

#[derive(Debug, Clone, Copy)]
pub struct ExtendedRestoreInput<'a> {
    pub source: &'a Snapshot,
    pub source_bytes: &'a [u8],
    pub source_name: &'a str,
    pub target: &'a Camera,
    pub only: Option<&'a BTreeSet<Category>>,
}

fn same_body(source: &Snapshot, live: &ReadFrom) -> Result<(), Refusal> {
    if live.model.is_empty() {
        return Ok(());
    }
    if live.model != source.camera.model {
        return Err(Refusal::NotThisBody {
            why: format!("the snapshot is of a {} but the body is a {}", source.camera.model, live.model),
        });
    }
    let (a, b) = (source.camera.serial.as_str(), live.usb_serial.as_str());
    if !a.is_empty() && !b.is_empty() && a != b {
        return Err(Refusal::NotThisBody { why: format!("the snapshot is of body {a} but the body is {b}") });
    }
    Ok(())
}

/// Read the live body and check it is the one the snapshot is of.
fn read_live<S: crate::extended::ExtendedSource + ?Sized>(session: &mut S, input: &ExtendedRestoreInput<'_>) -> Result<(Extended, Extended), Refusal> {
    let source = input.source.extended.clone().ok_or(Refusal::SnapshotHasNoExtended)?;
    let live = read_extended(session);
    if live.read_from.model.is_empty() {
        let (tag, detail) = match &live.ftp {
            FtpState::ReadFailed { tag, detail } => (tag.clone(), detail.clone()),
            _ => ("unknown".to_string(), String::new()),
        };
        return Err(Refusal::BodyNotReadable { tag, detail });
    }
    same_body(input.source, &live.read_from)?;
    Ok((source, live))
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtendedPlanReport {
    pub camera: Camera,
    pub source: String,
    pub would_change: usize,
    pub summary: String,
    pub digest: String,
    pub items: Vec<PlanItem>,
}

/// Dry-run: read the body and build the plan. Reads only.
pub fn build_extended_restore_plan<S: crate::extended::ExtendedSource + ?Sized>(
    session: &mut S,
    input: &ExtendedRestoreInput<'_>,
) -> Result<ExtendedPlanReport, Refusal> {
    let (source, live) = read_live(session, input)?;
    let plan = plan_extended_restore(&source, &live, input.only);
    let n = would_change(&plan);
    Ok(ExtendedPlanReport {
        camera: input.target.clone(),
        source: input.source_name.to_string(),
        would_change: n,
        summary: summary_text(input.target, input.source_name, n, input.only),
        digest: digest(input.source_bytes, &input.target.serial, input.only, &plan),
        items: plan.into_iter().map(|p| p.item).collect(),
    })
}

impl ExtendedPlanReport {
    /// The plan as the CLI prints it: the summary, then one line per item.
    pub fn to_text(&self) -> String {
        let mut out = format!("{}\n", self.summary);
        for i in &self.items {
            let line = match &i.action {
                ItemAction::Unchanged => format!("  same      {}", i.label),
                ItemAction::NotSelected => format!("  not selected  {}", i.label),
                ItemAction::Write { from, to } => format!("  WRITE     {}: {} -> {}", i.label, from.as_deref().unwrap_or("?"), to),
                ItemAction::Skip { reason, detail } => {
                    format!("  skip      {} ({}{})", i.label, reason.tag(), detail.as_ref().map(|d| format!(": {d}")).unwrap_or_default())
                }
            };
            out.push_str(&line);
            out.push('\n');
        }
        out.push_str(&format!("digest: {}\n", self.digest));
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ItemOutcome {
    /// The write was accepted. `verified`: its own read-back matched.
    Written { verified: bool },
    /// The camera refused, or the request failed before any data went out: nothing changed for this item.
    Failed { message: String },
    /// The data may have gone out: the item may or may not have changed.
    Indeterminate { message: String },
    /// Not tried, because an earlier step of the same item failed.
    NotAttempted { because: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ItemResult {
    pub id: String,
    pub label: String,
    pub category: Category,
    #[serde(flatten)]
    pub outcome: ItemOutcome,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtendedApplyReport {
    pub status: ApplyStatus,
    pub no_op: bool,
    pub plan_digest: String,
    /// The snapshot holding the body as it was before the first write; restore from it to undo. The same shape as the
    /// MAID restore's, so a client reads both alike.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore_point: Option<RestorePoint>,
    /// The categories an undo puts back: every one with a write that was tried (a write that failed part-way, an FTP
    /// rebuild above all, may have changed the body). Pass them as `only` when restoring from the restore point.
    pub undo_set: Vec<String>,
    pub results: Vec<ItemResult>,
    /// Items written that the body, read again afterwards, still shows as different from the snapshot.
    pub still_differing: Vec<String>,
    /// Set when the read after the writes failed, so nothing could be checked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_read_failed: Option<String>,
}

impl ExtendedApplyReport {
    /// The result as the CLI prints it.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        if self.no_op {
            out.push_str("nothing to change: the body already matches the snapshot\n");
            return out;
        }
        for r in &self.results {
            let line = match &r.outcome {
                ItemOutcome::Written { verified: true } => format!("  written, read back OK   {}", r.label),
                ItemOutcome::Written { verified: false } => format!("  written, NOT confirmed by its read-back   {}", r.label),
                ItemOutcome::Failed { message } => format!("  FAILED    {}: {message}", r.label),
                ItemOutcome::Indeterminate { message } => format!("  MAY HAVE CHANGED   {}: {message}", r.label),
                ItemOutcome::NotAttempted { because } => format!("  not attempted   {}: {because}", r.label),
            };
            out.push_str(&line);
            out.push('\n');
        }
        if !self.still_differing.is_empty() {
            out.push_str(&format!("after the writes the body still differs from the snapshot in: {}\n", self.still_differing.join(", ")));
        }
        if let Some(why) = &self.after_read_failed {
            out.push_str(&format!("{why}; nothing could be checked\n"));
        }
        if let Some(rp) = &self.restore_point {
            out.push_str(&format!("restore point: {} (restore from it with `restore-extended` to undo)\n", rp.file));
        }
        out.push_str(&format!("status: {:?}\n", self.status));
        out
    }
}

#[derive(Debug)]
pub enum ExtendedApplyError {
    Apply(ApplyError),
    Refused(Refusal),
}

impl From<ApplyError> for ExtendedApplyError {
    fn from(e: ApplyError) -> Self {
        ExtendedApplyError::Apply(e)
    }
}

impl From<Refusal> for ExtendedApplyError {
    fn from(e: Refusal) -> Self {
        ExtendedApplyError::Refused(e)
    }
}

impl ExtendedApplyError {
    pub fn tag(&self) -> &'static str {
        match self {
            ExtendedApplyError::Apply(e) => e.tag(),
            ExtendedApplyError::Refused(r) => r.tag(),
        }
    }
}

impl std::fmt::Display for ExtendedApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtendedApplyError::Apply(e) => write!(f, "{e}"),
            ExtendedApplyError::Refused(r) => write!(f, "{r}"),
        }
    }
}

impl std::error::Error for ExtendedApplyError {}

pub struct ExtendedApplyRequest<'a> {
    pub input: ExtendedRestoreInput<'a>,
    pub snapshots_dir: &'a Path,
    pub expected_digest: &'a str,
    pub expected_summary: &'a str,
    pub captured_at: String,
}

fn is_indeterminate(e: &PtpUsbError) -> bool {
    crate::exit_status::write_kind_of_ptp_usb_error(e) == crate::exit_status::ExitKind::WriteIndeterminate
}

fn outcome_of(r: Result<bool, PtpUsbError>) -> ItemOutcome {
    match r {
        Ok(verified) => ItemOutcome::Written { verified },
        Err(e) if is_indeterminate(&e) => ItemOutcome::Indeterminate { message: e.to_string() },
        Err(e) => ItemOutcome::Failed { message: e.to_string() },
    }
}

/// Run the writes in plan order. An FTP rebuild stops at its first failure.
fn execute<S: ExtendedSink + ?Sized>(sink: &mut S, plan: &[Planned], live_ftp_count: u32) -> Vec<ItemResult> {
    let mut results = Vec::new();
    for p in plan {
        let Some(work) = &p.work else { continue };
        let outcome = match work {
            Work::Text { code, text } => outcome_of(sink.write_string(*code, text)),
            Work::Byte { code, value } => outcome_of(sink.write_byte(*code, *value)),
            Work::Iptc { slot, bytes } => outcome_of(sink.write_iptc(*slot, bytes)),
            Work::Ftp { blobs } => execute_ftp(sink, blobs, live_ftp_count),
        };
        results.push(ItemResult { id: p.item.id.clone(), label: p.item.label.clone(), category: p.item.category, outcome });
    }
    results
}

/// Delete everything, then add the profiles last-first, because an add lands at the front of the list.
fn execute_ftp<S: ExtendedSink + ?Sized>(sink: &mut S, blobs: &[Vec<u8>], live_count: u32) -> ItemOutcome {
    if live_count > 0 {
        match sink.delete_all_ftp() {
            Ok(0) => {}
            Ok(n) => return ItemOutcome::Failed { message: format!("the delete left {n} profile(s) stored; nothing was added") },
            Err(e) if is_indeterminate(&e) => return ItemOutcome::Indeterminate { message: e.to_string() },
            Err(e) => return ItemOutcome::Failed { message: e.to_string() },
        }
    }
    let mut all_verified = true;
    let mut count = 0u32;
    for (done, blob) in blobs.iter().rev().enumerate() {
        match sink.add_ftp(blob, count) {
            Ok(verified) => {
                all_verified &= verified;
                count += 1;
            }
            Err(e) => {
                let list = format!("the list was emptied and {done} of {} profile(s) were added; the restore point holds the original list", blobs.len());
                return if is_indeterminate(&e) {
                    ItemOutcome::Indeterminate { message: format!("{e}; {list}") }
                } else {
                    ItemOutcome::Failed { message: format!("{e}; {list}") }
                };
            }
        }
    }
    ItemOutcome::Written { verified: all_verified }
}

/// The categories with a write that was tried, in apply order. One that was never attempted changed nothing.
fn undo_categories(results: &[ItemResult]) -> Vec<String> {
    Category::ALL
        .iter()
        .filter(|c| results.iter().any(|r| r.category == **c && !matches!(r.outcome, ItemOutcome::NotAttempted { .. })))
        .map(|c| c.tag().to_string())
        .collect()
}

fn status_of(results: &[ItemResult], still_differing: &[String], after_failed: bool) -> ApplyStatus {
    if results.iter().any(|r| matches!(r.outcome, ItemOutcome::Indeterminate { .. })) {
        return ApplyStatus::Indeterminate;
    }
    if results.iter().any(|r| matches!(r.outcome, ItemOutcome::Failed { .. } | ItemOutcome::NotAttempted { .. })) {
        return ApplyStatus::Failed;
    }
    let all_verified = results.iter().all(|r| matches!(r.outcome, ItemOutcome::Written { verified: true }));
    if all_verified && still_differing.is_empty() && !after_failed {
        ApplyStatus::AppliedVerified
    } else {
        ApplyStatus::AppliedUnverified
    }
}

/// Apply. Recomputes the plan on the body as it is now and refuses a stale one; writes the restore point; writes;
/// reads the body again and checks.
pub fn apply_extended_restore<S: ExtendedSink + ?Sized>(
    sink: &mut S,
    req: &ExtendedApplyRequest<'_>,
    progress: &mut dyn FnMut(&str),
) -> Result<ExtendedApplyReport, ExtendedApplyError> {
    require_confirmation(req.expected_digest, req.expected_summary)?;
    let input = &req.input;
    let (source, live) = read_live(sink, input)?;
    let plan = plan_extended_restore(&source, &live, input.only);
    let now_digest = digest(input.source_bytes, &input.target.serial, input.only, &plan);
    if now_digest != req.expected_digest.trim() {
        return Err(ApplyError::StalePlan { what: "digest" }.into());
    }
    let n = would_change(&plan);
    if summary_text(input.target, input.source_name, n, input.only) != req.expected_summary.trim() {
        return Err(ApplyError::StalePlan { what: "summary" }.into());
    }
    progress("plan_verified");
    if n == 0 {
        return Ok(ExtendedApplyReport {
            status: ApplyStatus::AppliedVerified,
            no_op: true,
            plan_digest: now_digest,
            restore_point: None,
            undo_set: Vec::new(),
            results: Vec::new(),
            still_differing: Vec::new(),
            after_read_failed: None,
        });
    }

    // The restore point: the body as it is, as a snapshot with no MAID properties and the live extended section.
    let mut rp = Snapshot::new(input.target.clone(), Transport::Usb, req.captured_at.clone());
    // A fixed label: the file name is built from it, and a name that embedded the source's would grow with every
    // restore from a restore point until it exceeded the file system's limit.
    rp.label = Some("restore-ext".to_string());
    rp.extended = Some(live.clone());
    let rp_file = rp.suggested_filename();
    std::fs::create_dir_all(req.snapshots_dir).map_err(|e| Refusal::RestorePoint(SnapshotError::Io(e)))?;
    rp.save_new(req.snapshots_dir.join(&rp_file)).map_err(Refusal::RestorePoint)?;
    progress("restore_point_written");

    progress("writing");
    let live_ftp = match &live.ftp {
        FtpState::Read(s) => s.profiles.len() as u32,
        _ => 0,
    };
    let results = execute(sink, &plan, live_ftp);

    progress("read_back");
    let after = read_extended(sink);
    let after_read_failed = (after.read_from.model.is_empty()).then(|| "the body could not be read after the writes".to_string());
    let still_differing: Vec<String> = if after_read_failed.is_some() {
        Vec::new()
    } else {
        let written: BTreeSet<&str> = results.iter().filter(|r| matches!(r.outcome, ItemOutcome::Written { .. })).map(|r| r.id.as_str()).collect();
        plan_extended_restore(&source, &after, input.only)
            .into_iter()
            .filter(|p| written.contains(p.item.id.as_str()) && !matches!(p.item.action, ItemAction::Unchanged))
            .map(|p| p.item.id)
            .collect()
    };
    let status = status_of(&results, &still_differing, after_read_failed.is_some());
    Ok(ExtendedApplyReport {
        status,
        no_op: false,
        plan_digest: now_digest,
        restore_point: Some(RestorePoint { file: rp_file, label: "restore-ext".to_string() }),
        undo_set: undo_categories(&results),
        results,
        still_differing,
        after_read_failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extended::{ExtendedSource, hex_encode};
    use crate::iptc::{IptcPreset, encode_iptc_preset};
    use crate::ptp_usb::{
        FtpListEntry, FtpListState, FtpProfile, FtpProfileInfo, PtpResponseCode, PtpString, encode_ftp_profile,
        encode_ptp_string_value as ptp_string,
    };
    use std::collections::BTreeMap;

    /// A body that keeps state and applies writes, so a plan can be run against it and read back.
    struct Body {
        model: String,
        serial: String,
        props: BTreeMap<u16, Vec<u8>>,
        iptc: Vec<Vec<u8>>,
        /// Front of the list first, as the camera orders it.
        ftp: Vec<Vec<u8>>,
        limit: u32,
        /// Writes to these ids fail (`string:0xd072`, `iptc:2`, `ftp_add:2`, `ftp_delete`).
        fail: Vec<String>,
        /// A failure of this kind is reported as indeterminate.
        indeterminate: Vec<String>,
        log: Vec<String>,
        unreadable: bool,
    }

    fn profile(name: &str, password: &str) -> Vec<u8> {
        encode_ftp_profile(&FtpProfile {
            profile_name: name.into(),
            ssid_24ghz: "net".into(),
            ssid_5ghz: "key-12345".into(),
            host: "10.0.0.2".into(),
            username: "fleet".into(),
            password: password.into(),
            port: 8080,
        })
        .unwrap()
    }

    fn preset(name: &str, headline: &str) -> Vec<u8> {
        let mut fields = vec![String::new(); 14];
        fields[2] = headline.into();
        encode_iptc_preset(&IptcPreset { name: name.into(), fields }).unwrap()
    }

    fn body(model: &str) -> Body {
        let mut props = BTreeMap::new();
        props.insert(0xD072, ptp_string("Brian").unwrap());
        props.insert(0xD073, ptp_string("(c)2026").unwrap());
        props.insert(0xD090, ptp_string("").unwrap());
        props.insert(0xD053, vec![1]);
        props.insert(0xD060, vec![0]);
        Body {
            model: model.into(),
            serial: "0000001".into(),
            props,
            iptc: vec![preset("", ""), preset("", ""), vec![1, 0, 0, 0, 0xC9, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]],
            ftp: vec![profile("one", "password-1"), profile("two", "password-2")],
            limit: 20,
            fail: vec![],
            indeterminate: vec![],
            log: vec![],
            unreadable: false,
        }
    }

    impl Body {
        fn extended(&mut self) -> Extended {
            read_extended(self)
        }

        fn err(&self, key: &str, op: u16) -> Result<(), PtpUsbError> {
            if self.fail.iter().any(|k| k == key) {
                let cause = PtpUsbError::OperationFailed { op, code: PtpResponseCode(0x2002) };
                return Err(if self.indeterminate.iter().any(|k| k == key) {
                    PtpUsbError::PropWriteIndeterminate { propcode: 0, cause: Box::new(cause) }
                } else {
                    cause
                });
            }
            Ok(())
        }
    }

    impl ExtendedSource for Body {
        fn identity(&mut self) -> Result<ReadFrom, PtpUsbError> {
            if self.unreadable {
                return Err(PtpUsbError::NoDevice);
            }
            Ok(ReadFrom { model: self.model.clone(), usb_serial: self.serial.clone() })
        }
        fn prop_bytes(&mut self, code: u16) -> Result<Vec<u8>, PtpUsbError> {
            self.props.get(&code).cloned().ok_or(PtpUsbError::OperationFailed { op: 0x1015, code: PtpResponseCode(0x200A) })
        }
        fn ftp_list(&mut self) -> Result<FtpListState, PtpUsbError> {
            Ok(FtpListState {
                info: FtpProfileInfo {
                    header: 200,
                    count: self.ftp.len() as u32,
                    limit: self.limit,
                    slot: if self.ftp.is_empty() { crate::ptp_usb::FTP_NO_ACTIVE_SLOT } else { 0 },
                    guid: [0; 16],
                    device_name: PtpString { text: String::new(), anomalies: vec![] },
                    trailer: 500,
                },
                entries: self.ftp.iter().map(|b| FtpListEntry { raw: b.clone(), decoded: None }).collect(),
                truncated: false,
            })
        }
        fn iptc_slots(&mut self, _max: u32) -> Result<(u32, Vec<(u32, Vec<u8>)>), PtpUsbError> {
            Ok((self.iptc.len() as u32, self.iptc.iter().enumerate().map(|(i, b)| (i as u32 + 1, b.clone())).collect()))
        }
    }

    impl ExtendedSink for Body {
        fn write_string(&mut self, code: u16, text: &str) -> Result<bool, PtpUsbError> {
            self.log.push(format!("string:{code:#06x}"));
            self.err(&format!("string:{code:#06x}"), 0x1016)?;
            self.props.insert(code, ptp_string(text).unwrap());
            Ok(true)
        }
        fn write_byte(&mut self, code: u16, value: u8) -> Result<bool, PtpUsbError> {
            self.log.push(format!("byte:{code:#06x}"));
            self.err(&format!("byte:{code:#06x}"), 0x1016)?;
            self.props.insert(code, vec![value]);
            Ok(true)
        }
        fn write_iptc(&mut self, slot: u32, data: &[u8]) -> Result<bool, PtpUsbError> {
            self.log.push(format!("iptc:{slot}"));
            self.err(&format!("iptc:{slot}"), 0x9413)?;
            self.iptc[slot as usize - 1] = data.to_vec();
            Ok(true)
        }
        fn delete_all_ftp(&mut self) -> Result<u32, PtpUsbError> {
            self.log.push("ftp_delete".into());
            self.err("ftp_delete", 0x90EA)?;
            self.ftp.clear();
            Ok(0)
        }
        fn add_ftp(&mut self, blob: &[u8], count_before: u32) -> Result<bool, PtpUsbError> {
            self.log.push(format!("ftp_add:{count_before}"));
            self.err(&format!("ftp_add:{count_before}"), 0x90EE)?;
            assert_eq!(count_before as usize, self.ftp.len(), "an add is sent with the stored count");
            self.ftp.insert(0, blob.to_vec());
            Ok(true)
        }
    }

    fn snapshot_of(b: &mut Body) -> Snapshot {
        let mut s = Snapshot::new(Camera { model: b.model.clone(), serial: b.serial.clone(), firmware: "1".into() }, Transport::Usb, "2026-11-06T00:00:00Z".into());
        s.extended = Some(b.extended());
        s
    }

    struct Fixture {
        snap: Snapshot,
        bytes: Vec<u8>,
        dir: tempfile::TempDir,
        /// The snapshot's file name, as the plan and the restore point see it.
        name: String,
    }

    fn fixture(source_body: &mut Body) -> Fixture {
        let snap = snapshot_of(source_body);
        let bytes = snap.to_pretty_json().unwrap().into_bytes();
        Fixture { snap, bytes, dir: tempfile::tempdir().unwrap(), name: "src.json".into() }
    }

    fn input<'a>(f: &'a Fixture, target: &'a Camera, only: Option<&'a BTreeSet<Category>>) -> ExtendedRestoreInput<'a> {
        ExtendedRestoreInput { source: &f.snap, source_bytes: &f.bytes, source_name: &f.name, target, only }
    }

    fn target(b: &Body) -> Camera {
        Camera { model: b.model.clone(), serial: b.serial.clone(), firmware: "1".into() }
    }

    fn plan_of(b: &mut Body, f: &Fixture, only: Option<&BTreeSet<Category>>) -> ExtendedPlanReport {
        let t = target(b);
        build_extended_restore_plan(b, &input(f, &t, only)).unwrap()
    }

    fn apply(b: &mut Body, f: &Fixture, only: Option<&BTreeSet<Category>>) -> Result<ExtendedApplyReport, ExtendedApplyError> {
        let plan = plan_of(b, f, only);
        apply_with(b, f, only, &plan.digest, &plan.summary)
    }

    fn apply_with(b: &mut Body, f: &Fixture, only: Option<&BTreeSet<Category>>, digest: &str, summary: &str) -> Result<ExtendedApplyReport, ExtendedApplyError> {
        let t = target(b);
        let req = ExtendedApplyRequest {
            input: input(f, &t, only),
            snapshots_dir: f.dir.path(),
            expected_digest: digest,
            expected_summary: summary,
            captured_at: "2026-11-06T01:00:00Z".into(),
        };
        apply_extended_restore(b, &req, &mut |_| {})
    }

    fn action<'a>(plan: &'a ExtendedPlanReport, id: &str) -> &'a ItemAction {
        &plan.items.iter().find(|i| i.id == id).unwrap_or_else(|| panic!("no item {id}")).action
    }

    /// A body, a snapshot of it, and the same body after someone changed every kind of item.
    fn drifted() -> (Body, Fixture) {
        drifted_on("Z 9")
    }

    fn drifted_on(model: &str) -> (Body, Fixture) {
        let mut b = body(model);
        let f = fixture(&mut b);
        b.props.insert(0xD072, ptp_string("Someone else").unwrap());
        b.iptc[0] = preset("X", "changed");
        b.ftp = vec![profile("new", "password-9")];
        (b, f)
    }

    #[test]
    fn a_body_that_matches_its_snapshot_plans_nothing_and_applies_as_a_no_op() {
        let mut b = body("Z 9");
        let f = fixture(&mut b);
        let plan = plan_of(&mut b, &f, None);
        assert_eq!(plan.would_change, 0);
        assert!(plan.items.iter().all(|i| matches!(i.action, ItemAction::Unchanged)), "{:?}", plan.items);
        let report = apply(&mut b, &f, None).unwrap();
        assert!(report.no_op && report.restore_point.is_none());
        assert_eq!(report.status, ApplyStatus::AppliedVerified);
        assert!(b.log.is_empty(), "nothing was written: {:?}", b.log);
    }

    #[test]
    fn the_plan_names_each_change_and_the_apply_puts_every_item_back_and_verifies_it() {
        let (mut b, f) = drifted();
        let plan = plan_of(&mut b, &f, None);
        assert!(matches!(action(&plan, "0xd072"), ItemAction::Write { from: Some(a), to } if a == "\"Someone else\"" && to == "\"Brian\""));
        assert!(matches!(action(&plan, "iptc:1"), ItemAction::Write { .. }));
        assert!(matches!(action(&plan, "iptc:2"), ItemAction::Unchanged));
        assert!(matches!(action(&plan, "ftp"), ItemAction::Write { from: Some(a), to } if a == "1 stored, all deleted first" && to.contains("\"one\"") && to.contains("\"two\"")));
        assert_eq!(plan.would_change, 3);
        let report = apply(&mut b, &f, None).unwrap();
        assert_eq!(report.status, ApplyStatus::AppliedVerified, "{report:?}");
        assert!(report.still_differing.is_empty());
        assert_eq!(b.ftp, [profile("one", "password-1"), profile("two", "password-2")], "the list is the snapshot's, in its order");
        assert_eq!(b.props[&0xD072], ptp_string("Brian").unwrap());
        assert_eq!(b.iptc[0], preset("", ""));
    }

    #[test]
    fn the_writes_go_in_the_documented_order_and_the_ftp_adds_go_last_first() {
        let (mut b, f) = drifted_on("Z6_3");
        b.props.insert(0xD060, vec![3]); // a switch change too (confirmed on a Z6III), so all four categories show
        apply(&mut b, &f, None).unwrap();
        assert_eq!(b.log, ["string:0xd072", "iptc:1", "byte:0xd060", "ftp_delete", "ftp_add:0", "ftp_add:1"].map(String::from));
        // The adds: "two" first (it ends second), then "one" (it ends at the front).
    }

    #[test]
    fn a_restore_point_is_written_first_holds_the_body_as_it_was_and_undoes_the_restore() {
        let (mut b, f) = drifted();
        let before = b.extended();
        let report = apply(&mut b, &f, None).unwrap();
        let name = report.restore_point.expect("a restore point").file;
        let rp = Snapshot::load_from_file(f.dir.path().join(&name)).unwrap();
        assert_eq!(rp.extended.as_ref(), Some(&before), "the extended section as it was before any write");
        assert_eq!(rp.label.as_deref(), Some("restore-ext"));
        // Restoring from the restore point puts the drift back.
        let undo = Fixture { bytes: std::fs::read(f.dir.path().join(&name)).unwrap(), snap: rp, dir: tempfile::tempdir().unwrap(), name: name.clone() };
        let back = apply(&mut b, &undo, None).unwrap();
        assert_eq!(back.status, ApplyStatus::AppliedVerified);
        assert_eq!(b.props[&0xD072], ptp_string("Someone else").unwrap());
        assert_eq!(b.ftp, [profile("new", "password-9")]);
    }

    #[test]
    fn restore_points_of_restore_points_do_not_grow_their_file_names() {
        let (mut b, f) = drifted();
        let first = apply(&mut b, &f, None).unwrap().restore_point.unwrap().file;
        let mut name = first.clone();
        let mut dir = f.dir;
        for round in 0..4 {
            let bytes = std::fs::read(dir.path().join(&name)).unwrap();
            let snap = Snapshot::load_from_file(dir.path().join(&name)).unwrap();
            let fx = Fixture { snap, bytes, dir: tempfile::tempdir().unwrap(), name: name.clone() };
            // Alternate the body so every round has something to restore.
            b.props.insert(0xD072, ptp_string(&format!("round {round}")).unwrap());
            let next = apply(&mut b, &fx, None).unwrap().restore_point.expect("a restore point").file;
            assert!(next.len() <= first.len() + 8, "round {round}: {} then {}", first.len(), next.len());
            name = next;
            dir = fx.dir;
        }
    }

    #[test]
    fn a_plan_that_no_longer_matches_the_body_is_refused_before_anything_is_written() {
        let (mut b, f) = drifted();
        let plan = plan_of(&mut b, &f, None);
        b.props.insert(0xD073, ptp_string("changed since the plan").unwrap());
        let err = apply_with(&mut b, &f, None, &plan.digest, &plan.summary).unwrap_err();
        assert_eq!(err.tag(), "stale_plan");
        assert!(b.log.is_empty() && std::fs::read_dir(f.dir.path()).unwrap().count() == 0, "no write, no restore point");
        let err = apply_with(&mut b, &f, None, "", &plan.summary).unwrap_err();
        assert_eq!(err.tag(), "missing_confirmation");
        let plan = plan_of(&mut b, &f, None);
        assert_eq!(apply_with(&mut b, &f, None, &plan.digest, "another summary").unwrap_err().tag(), "stale_plan");
    }

    #[test]
    fn a_value_that_changed_under_the_plan_makes_it_stale_even_when_the_count_of_changes_is_the_same() {
        let (mut b, f) = drifted();
        let plan = plan_of(&mut b, &f, None);
        // The string that the plan would overwrite is now something else: still one change, a different one.
        b.props.insert(0xD072, ptp_string("Yet another value").unwrap());
        let again = plan_of(&mut b, &f, None);
        assert_eq!(again.summary, plan.summary, "the same number of items would change");
        assert_ne!(again.digest, plan.digest);
        assert_eq!(apply_with(&mut b, &f, None, &plan.digest, &plan.summary).unwrap_err().tag(), "stale_plan");
        assert!(b.log.is_empty());
    }

    #[test]
    fn only_limits_the_plan_and_the_writes_to_the_named_categories() {
        let (mut b, f) = drifted();
        let only: BTreeSet<Category> = [Category::Strings].into();
        let plan = plan_of(&mut b, &f, Some(&only));
        assert_eq!(plan.would_change, 1);
        assert!(matches!(action(&plan, "ftp"), ItemAction::NotSelected));
        assert!(matches!(action(&plan, "iptc"), ItemAction::NotSelected));
        apply(&mut b, &f, Some(&only)).unwrap();
        assert_eq!(b.log, ["string:0xd072"]);
        assert_eq!(b.ftp.len(), 1, "the FTP list was left alone");
        assert_eq!(Category::parse(" FTP ").unwrap(), Category::Ftp);
        assert!(Category::parse("ftps").is_err());
    }

    #[test]
    fn the_digest_follows_the_target_and_the_scope_not_just_what_is_displayed() {
        let (mut b, f) = drifted();
        let all = plan_of(&mut b, &f, None).digest;
        assert_eq!(all, plan_of(&mut b, &f, None).digest, "stable");
        let only: BTreeSet<Category> = [Category::Strings].into();
        assert_ne!(all, plan_of(&mut b, &f, Some(&only)).digest, "the scope is in it");
        // Two snapshots that display the same change but write different bytes have different digests.
        let (mut b2, f2) = drifted();
        let mut s2 = f2.snap.clone();
        if let Some(FtpState::Read(store)) = s2.extended.as_mut().map(|e| &mut e.ftp) {
            store.profiles[0].hex = hex_encode(&profile("one", "password-X"));
        }
        let bytes2 = s2.to_pretty_json().unwrap().into_bytes();
        let f3 = Fixture { snap: s2, bytes: bytes2, dir: tempfile::tempdir().unwrap(), name: "src.json".into() };
        let p1 = plan_of(&mut b2, &f2, None);
        let p3 = plan_of(&mut b2, &f3, None);
        assert_eq!(p1.summary, p3.summary);
        assert_ne!(p1.digest, p3.digest);
    }

    #[test]
    fn no_plan_or_report_ever_carries_a_password_or_key() {
        let (mut b, f) = drifted();
        let plan = plan_of(&mut b, &f, None);
        let report = apply(&mut b, &f, None).unwrap();
        let text = format!("{}{}{plan:?}{report:?}", serde_json::to_string(&plan).unwrap(), serde_json::to_string(&report).unwrap());
        for secret in ["password-1", "password-2", "password-9", "key-12345"] {
            assert!(!text.contains(secret), "leaked {secret}");
        }
    }

    #[test]
    fn a_string_the_camera_cannot_hold_is_skipped_and_the_boundary_is_u00ff() {
        let mut b = body("Z 9");
        let mut f = fixture(&mut b);
        for (text, writable) in [("é", true), ("\u{ff}", true), ("\u{100}", false), ("日本", false), ("a\tb", false)] {
            if let Some(e) = f.snap.extended.as_mut() {
                e.props.get_mut("0xd072").unwrap().state = PropState::Text { text: text.into() };
            }
            let plan = plan_of(&mut b, &f, None);
            let a = action(&plan, "0xd072");
            assert_eq!(matches!(a, ItemAction::Write { .. }), writable, "{text:?}: {a:?}");
            if !writable {
                assert!(matches!(a, ItemAction::Skip { reason: SkipKind::ValueNotWritable, .. }), "{a:?}");
            }
        }
    }

    #[test]
    fn switches_are_written_only_on_models_they_are_confirmed_on_and_only_in_range() {
        for (model, writes) in [("Z 5", true), ("Z 9", false), ("Z6_3", false)] {
            let mut b = body(model);
            let f = fixture(&mut b);
            b.props.insert(0xD053, vec![0]);
            let plan = plan_of(&mut b, &f, None);
            let a = action(&plan, "0xd053");
            assert_eq!(matches!(a, ItemAction::Write { .. }), writes, "{model}: {a:?}");
            if !writes {
                assert!(matches!(a, ItemAction::Skip { reason: SkipKind::ModelNotConfirmed, .. }), "{model}: {a:?}");
            }
        }
        // IPTC preset: confirmed on the Z6III only; a value of 14 is out of range, 13 is in.
        for (value, writes) in [(13u8, true), (14, false)] {
            let mut b = body("Z6_3");
            let mut f = fixture(&mut b);
            f.snap.extended.as_mut().unwrap().props.get_mut("0xd060").unwrap().state = PropState::Bytes { hex: hex_encode(&[value]) };
            let a = action(&plan_of(&mut b, &f, None), "0xd060").clone();
            assert_eq!(matches!(a, ItemAction::Write { .. }), writes, "{value}: {a:?}");
        }
    }

    #[test]
    fn items_the_snapshot_or_the_body_cannot_support_are_typed_skips() {
        // A snapshot from before IPTC and FTP existed in it.
        let mut b = body("Z 9");
        let mut f = fixture(&mut b);
        let e = f.snap.extended.as_mut().unwrap();
        e.iptc = IptcState::NotRead;
        e.ftp = FtpState::ReadFailed { tag: "t".into(), detail: "d".into() };
        let plan = plan_of(&mut b, &f, None);
        assert!(matches!(action(&plan, "iptc"), ItemAction::Skip { reason: SkipKind::SnapshotHasNoValue, .. }));
        assert!(matches!(action(&plan, "ftp"), ItemAction::Skip { reason: SkipKind::SnapshotHasNoValue, .. }));
        // A body that has no FTP (a Z 5) against a snapshot that has a list: nothing to put it in.
        let mut z5 = body("Z 5");
        z5.props.remove(&0xD060);
        let mut f = fixture(&mut body("Z 9"));
        f.snap.camera.model = "Z 5".into();
        let z5_plan = plan_of(&mut z5, &f, None);
        assert!(matches!(action(&z5_plan, "ftp"), ItemAction::Skip { reason: SkipKind::BodyLacksIt, .. }), "{:?}", action(&z5_plan, "ftp"));
        // Slots: a snapshot slot the body does not have; a kind-2 record that differs; an undecodable one.
        let mut b = body("Z 9");
        let mut f = fixture(&mut b);
        if let IptcState::Read(store) = &mut f.snap.extended.as_mut().unwrap().iptc {
            store.slots.push(crate::extended::IptcSlotEntry { slot: 9, hex: hex_encode(&preset("n", "h")) });
            store.slots[2].hex = hex_encode(&[1, 0, 0, 0, 0xC9, 0, 2, 0, 0, 0, 0, 0, 7, 7, 7, 7]);
            store.slots[1].hex = "0102".into();
        }
        let plan = plan_of(&mut b, &f, None);
        assert!(matches!(action(&plan, "iptc:9"), ItemAction::Skip { reason: SkipKind::SlotNotOnBody, .. }));
        assert!(matches!(action(&plan, "iptc:3"), ItemAction::Skip { reason: SkipKind::RecordNotWritable, .. }));
        assert!(matches!(action(&plan, "iptc:2"), ItemAction::Skip { reason: SkipKind::RecordNotWritable, .. }));
    }

    #[test]
    fn ftp_skips_a_truncated_list_an_undecodable_blob_and_a_list_longer_than_the_limit_and_the_limit_itself_is_allowed() {
        let mut b = body("Z 9");
        let mut f = fixture(&mut b);
        b.ftp = vec![profile("other", "password-9")];
        let tweak = |f: &mut Fixture, g: &dyn Fn(&mut crate::extended::FtpStore)| {
            if let FtpState::Read(s) = &mut f.snap.extended.as_mut().unwrap().ftp {
                g(s);
            }
        };
        tweak(&mut f, &|s| s.truncated = true);
        assert!(matches!(action(&plan_of(&mut b, &f, None), "ftp"), ItemAction::Skip { reason: SkipKind::ListTruncated, .. }));
        tweak(&mut f, &|s| {
            s.truncated = false;
            s.profiles[0].hex = "0102".into();
        });
        assert!(matches!(action(&plan_of(&mut b, &f, None), "ftp"), ItemAction::Skip { reason: SkipKind::ProfileNotDecodable, .. }));
        // limit - the boundary: exactly `limit` profiles is allowed, one more is not.
        b.limit = 2;
        let mut g = fixture(&mut body("Z 9"));
        b.ftp = vec![];
        assert!(matches!(action(&plan_of(&mut b, &g, None), "ftp"), ItemAction::Write { .. }), "2 profiles into a limit of 2");
        b.limit = 1;
        assert!(matches!(action(&plan_of(&mut b, &g, None), "ftp"), ItemAction::Skip { reason: SkipKind::ListTooLong, .. }));
        g.snap.extended = None;
    }

    #[test]
    fn ftp_on_a_model_not_confirmed_for_writes_is_skipped() {
        let mut b = body("Z_NEW");
        let f = fixture(&mut b);
        b.ftp = vec![];
        let a = action(&plan_of(&mut b, &f, None), "ftp").clone();
        assert!(matches!(a, ItemAction::Skip { reason: SkipKind::ModelNotConfirmed, .. }), "{a:?}");
    }

    #[test]
    fn a_failed_write_is_reported_and_the_other_items_are_still_tried() {
        let (mut b, f) = drifted();
        b.fail.push("string:0xd072".into());
        let report = apply(&mut b, &f, None).unwrap();
        assert_eq!(report.status, ApplyStatus::Failed);
        let by_id = |id: &str| report.results.iter().find(|r| r.id == id).unwrap().outcome.clone();
        assert!(matches!(by_id("0xd072"), ItemOutcome::Failed { .. }));
        assert!(matches!(by_id("iptc:1"), ItemOutcome::Written { verified: true }), "one failure does not stop the rest");
        assert!(matches!(by_id("ftp"), ItemOutcome::Written { verified: true }));
    }

    #[test]
    fn a_write_whose_data_may_have_gone_out_makes_the_whole_apply_indeterminate() {
        let (mut b, f) = drifted();
        b.fail.push("iptc:1".into());
        b.indeterminate.push("iptc:1".into());
        assert_eq!(apply(&mut b, &f, None).unwrap().status, ApplyStatus::Indeterminate);
    }

    #[test]
    fn an_ftp_rebuild_that_fails_part_way_says_the_list_was_emptied_and_how_far_it_got() {
        let (mut b, f) = drifted();
        b.fail.push("ftp_add:1".into()); // the second add
        let report = apply(&mut b, &f, None).unwrap();
        assert_eq!(report.status, ApplyStatus::Failed);
        let ItemOutcome::Failed { message } = &report.results.iter().find(|r| r.id == "ftp").unwrap().outcome else { panic!() };
        assert!(message.contains("emptied") && message.contains("1 of 2") && message.contains("restore point"), "{message}");
        assert_eq!(b.ftp.len(), 1, "one profile was added before it stopped");
        // A failed delete adds nothing.
        let (mut b, f) = drifted();
        b.fail.push("ftp_delete".into());
        apply(&mut b, &f, None).unwrap();
        assert!(!b.log.iter().any(|l| l.starts_with("ftp_add")), "{:?}", b.log);
    }

    #[test]
    fn an_empty_body_list_is_filled_without_a_delete() {
        let mut b = body("Z 9");
        let f = fixture(&mut b);
        b.ftp.clear();
        apply(&mut b, &f, None).unwrap();
        assert_eq!(b.log, ["ftp_add:0", "ftp_add:1"]);
        assert_eq!(b.ftp.len(), 2);
    }

    #[test]
    fn a_body_that_does_not_end_up_as_the_snapshot_is_reported_unverified_not_verified() {
        // A body whose write is accepted and ignored: the read afterwards still differs.
        struct Ignoring(Body);
        impl ExtendedSource for Ignoring {
            fn identity(&mut self) -> Result<ReadFrom, PtpUsbError> { self.0.identity() }
            fn prop_bytes(&mut self, c: u16) -> Result<Vec<u8>, PtpUsbError> { self.0.prop_bytes(c) }
            fn ftp_list(&mut self) -> Result<FtpListState, PtpUsbError> { self.0.ftp_list() }
            fn iptc_slots(&mut self, m: u32) -> Result<(u32, Vec<(u32, Vec<u8>)>), PtpUsbError> { self.0.iptc_slots(m) }
        }
        impl ExtendedSink for Ignoring {
            fn write_string(&mut self, _: u16, _: &str) -> Result<bool, PtpUsbError> { Ok(true) }
            fn write_byte(&mut self, c: u16, v: u8) -> Result<bool, PtpUsbError> { self.0.write_byte(c, v) }
            fn write_iptc(&mut self, s: u32, d: &[u8]) -> Result<bool, PtpUsbError> { self.0.write_iptc(s, d) }
            fn delete_all_ftp(&mut self) -> Result<u32, PtpUsbError> { self.0.delete_all_ftp() }
            fn add_ftp(&mut self, b: &[u8], c: u32) -> Result<bool, PtpUsbError> { self.0.add_ftp(b, c) }
        }
        let (b, f) = drifted();
        let mut ign = Ignoring(b);
        let t = target(&ign.0);
        let plan = build_extended_restore_plan(&mut ign, &input(&f, &t, None)).unwrap();
        let req = ExtendedApplyRequest { input: input(&f, &t, None), snapshots_dir: f.dir.path(), expected_digest: &plan.digest, expected_summary: &plan.summary, captured_at: "x".into() };
        let report = apply_extended_restore(&mut ign, &req, &mut |_| {}).unwrap();
        assert_eq!(report.status, ApplyStatus::AppliedUnverified, "{report:?}");
        assert_eq!(report.still_differing, ["0xd072"]);
    }

    #[test]
    fn a_snapshot_without_the_section_a_body_that_cannot_be_read_and_another_body_are_refused() {
        let (mut b, mut f) = drifted();
        let t = target(&b);
        let mut bare = f.snap.clone();
        bare.extended = None;
        let bare_bytes = vec![];
        let input_bare = ExtendedRestoreInput { source: &bare, source_bytes: &bare_bytes, source_name: "x", target: &t, only: None };
        assert_eq!(build_extended_restore_plan(&mut b, &input_bare).unwrap_err().tag(), "snapshot_has_no_extended");
        b.unreadable = true;
        assert_eq!(build_extended_restore_plan(&mut b, &input(&f, &t, None)).unwrap_err().tag(), "body_not_readable");
        b.unreadable = false;
        b.serial = "0000002".into();
        assert_eq!(build_extended_restore_plan(&mut b, &input(&f, &t, None)).unwrap_err().tag(), "not_this_body");
        b.serial = "0000001".into();
        b.model = "Z6_3".into();
        assert_eq!(build_extended_restore_plan(&mut b, &input(&f, &t, None)).unwrap_err().tag(), "not_this_body");
        f.snap.camera.serial = String::new();
        b.model = "Z 9".into();
        b.serial = "anything".into();
        assert!(build_extended_restore_plan(&mut b, &input(&f, &t, None)).is_ok(), "a snapshot with no serial is not evidence of another body");
    }

    #[test]
    fn a_category_name_is_trimmed_and_case_folded_and_nothing_else_is_accepted() {
        // Pinned on purpose: unlike a MAID property name (` Copyright` is refused there), a category comes from a
        // closed list of four, so a stray space or capital is forgiven. Anything else is refused whole.
        for ok in ["strings", "Strings", " STRINGS ", "\tftp\n"] {
            assert!(Category::parse(ok).is_ok(), "{ok:?}");
        }
        assert_eq!(Category::parse(" Iptc ").unwrap(), Category::Iptc);
        for bad in ["", " ", "string", "switch", "all", "ftp,iptc", "Copyright", "0xd072"] {
            assert!(Category::parse(bad).is_err(), "{bad:?}");
        }
        for c in Category::ALL {
            assert_eq!(Category::parse(c.tag()).unwrap(), c, "every tag round-trips");
        }
    }

    #[test]
    fn the_report_names_the_restore_point_like_the_maid_restore_does_and_the_categories_an_undo_puts_back() {
        // Shape: `restore_point` is {file, label}, `undo_set` a list of category tags, both in the JSON a client reads.
        let (mut b, f) = drifted();
        let report = apply(&mut b, &f, None).unwrap();
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["restore_point"]["label"], "restore-ext");
        assert!(v["restore_point"]["file"].as_str().unwrap().ends_with(".json"), "{v}");
        // The fixture drifts a string, an IPTC preset and the FTP list, and no switch: `switches` must not be in the set.
        assert_eq!(v["undo_set"], serde_json::json!(["strings", "iptc", "ftp"]), "only the categories that were written, in apply order: {v}");

        // Nothing to change: no restore point, nothing to undo (the boundary: zero, not "all").
        let none = apply(&mut b, &f, None).unwrap();
        assert!(none.no_op && none.restore_point.is_none() && none.undo_set.is_empty());
        let v = serde_json::to_value(&none).unwrap();
        assert!(v.get("restore_point").is_none() && v["undo_set"] == serde_json::json!([]), "{v}");

        // A rebuild that failed part-way may have emptied the list: ftp is in the undo set although it did not succeed.
        let (mut b, f) = drifted();
        b.fail.push("ftp_add:0".to_string());
        let report = apply(&mut b, &f, None).unwrap();
        assert_eq!(report.status, ApplyStatus::Failed);
        assert!(report.undo_set.contains(&"ftp".to_string()), "{:?}", report.undo_set);
    }

    #[test]
    fn an_item_that_was_never_attempted_does_not_put_its_category_in_the_undo_set() {
        let r = |category, outcome| ItemResult { id: "x".into(), label: "x".into(), category, outcome };
        let results = vec![
            r(Category::Strings, ItemOutcome::Written { verified: true }),
            r(Category::Iptc, ItemOutcome::NotAttempted { because: "earlier".into() }),
            r(Category::Ftp, ItemOutcome::Failed { message: "m".into() }),
        ];
        assert_eq!(undo_categories(&results), ["strings", "ftp"]);
        assert!(undo_categories(&[]).is_empty());
    }

    #[test]
    fn the_undo_command_names_the_restore_point_and_each_category_and_is_none_for_an_empty_set() {
        let set = ["strings".to_string(), "ftp".to_string()];
        assert_eq!(undo_command("rp.json", &set, None).unwrap(), "nikon-fleet restore-extended rp.json --only strings --only ftp");
        assert_eq!(
            undo_command("rp.json", &set[..1], Some("/my data")).unwrap(),
            "nikon-fleet --data-dir \"/my data\" restore-extended rp.json --only strings"
        );
        assert_eq!(undo_command("rp.json", &[], None), None, "nothing to undo is not a command that restores everything");
        // The command it prints parses back to exactly the categories it names.
        for tag in set {
            assert!(Category::parse(&tag).is_ok());
        }
    }
}

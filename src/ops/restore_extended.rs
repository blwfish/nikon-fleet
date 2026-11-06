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
//! refused to select a preset while all of them were empty. A run STOPS at the first item that failed or may have
//! changed the body: every later item is [`ItemOutcome::NotAttempted`] and sends nothing.
//!
//! What it cannot do: write an IPTC slot of another record kind, or restore an FTP list the snapshot read only part of
//! (IPTC slots are independent, so a prefix of them is still restored slot by slot). Each of those is a typed skip in
//! the plan. It also cannot choose which FTP profile is the active one (no op that selects one was found); that is not
//! a plan item of its own: the FTP item's `to` text names the snapshot's active profile and says the camera chooses
//! its own, so it is stated, but not a `SkipKind`.
//!
//! Whether a string, a one-byte switch or an IPTC slot is a write or a skip is decided by the session's own gate
//! functions in `ptp_usb` (`check_string_property_write`, `check_byte_property_write`, `iptc_write_confirmed`), not by
//! copies; `ptp_usb`'s tests run the planner and a scripted session over the same inputs.
//!
//! The digest binds, per item, what would be written AND what the body held when the plan was made (the live value,
//! the live IPTC slot bytes, the live FTP list: hashed, never shown), plus the target serial and the scope.
//!
//! A run that wrote nothing is a verified no-op only if every difference was skipped for a routine reason
//! ([`SkipKind::is_routine`]); any other skip leaves the body different from the snapshot, which is `Failed`.
//!
//! The FTP rebuild deletes every stored profile first. If an add then fails the list is partial; the restore point
//! (written before the first write, and a valid input to this same command) holds the original. It counts toward the
//! data directory's per-camera restore-point cap, with the MCP's.

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
use crate::ops::retention;
use crate::ops::worker::{PHASE_PLAN_VERIFIED, PHASE_READ_BACK, PHASE_RESTORE_POINT_WRITTEN, PHASE_WRITING};
use crate::ptp_usb::{
    BYTE_PROPS, PtpUsbError, STRING_PROPS, check_byte_property_write, check_string_property_write, decode_ftp_profile,
    ftp_write_confirmed, iptc_write_confirmed,
};
use crate::snapshot::{Camera, Snapshot, SnapshotError, Transport};

/// Bump when the digest's vocabulary changes. 2: each item's fingerprint also binds what the body held when the plan
/// was made (the live FTP list, the live IPTC slot, the live value), not only what would be written.
pub const DIGEST_FORMAT: u32 = 2;

/// The label of the restore point an apply writes, and the one place it is spelled. A fixed label: the file name is
/// built from it, and a name that embedded the source's would grow with every restore from a restore point until it
/// exceeded the file system's limit.
pub const RESTORE_POINT_LABEL: &str = "restore-ext";

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

    /// Whether this skip only says the two bodies have nothing in common (the snapshot holds no value for the item, or
    /// the body does not have it), as opposed to something the caller asked for that could not be done. The one
    /// classification behind "a zero-write apply is a verified no-op" (routine skips only) versus a failed one: the
    /// MAID apply's `Severity::Routine` rule. No wildcard: a new reason must be classified here.
    pub fn is_routine(self) -> bool {
        match self {
            SkipKind::SnapshotHasNoValue | SkipKind::BodyLacksIt => true,
            SkipKind::BodyUnreadable
            | SkipKind::ValueNotWritable
            | SkipKind::ModelNotConfirmed
            | SkipKind::RecordNotWritable
            | SkipKind::SlotNotOnBody
            | SkipKind::ListTruncated
            | SkipKind::ProfileNotDecodable
            | SkipKind::ListTooLong => false,
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
#[derive(Clone, PartialEq, Eq)]
enum Work {
    Text { code: u16, text: String },
    Byte { code: u16, value: u8 },
    Iptc { slot: u32, bytes: Vec<u8> },
    Ftp { blobs: Vec<Vec<u8>> },
}

/// `Debug` prints no FTP blob: each holds a password and a Wi-Fi key, and `Planned` (hence the plan, hence anything
/// that formats one) carries this.
impl std::fmt::Debug for Work {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Work::Text { code, text } => f.debug_struct("Text").field("code", code).field("text", text).finish(),
            Work::Byte { code, value } => f.debug_struct("Byte").field("code", code).field("value", value).finish(),
            Work::Iptc { slot, bytes } => f.debug_struct("Iptc").field("slot", slot).field("bytes", &bytes.len()).finish(),
            Work::Ftp { blobs } => f
                .debug_struct("Ftp")
                .field("profiles", &blobs.len())
                .field("blobs", &format_args!("<{} byte(s) hidden>", blobs.iter().map(Vec::len).sum::<usize>()))
                .finish(),
        }
    }
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

/// The text a fingerprint is hashed from: each part length-prefixed, so ("ab", "c") and ("a", "bc") differ. A plan
/// item's parts are what would be written and what the body held when the plan was made, so the digest follows a
/// live value that changed to another one the display would render the same.
fn fp(parts: &[&str]) -> String {
    parts.iter().map(|p| format!("{}:{p}", p.len())).collect::<Vec<_>>().join("|")
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

/// Why a recorded property state is not a value, keeping what was recorded (the tag of a failed read, the reason a
/// value did not decode). `PropState` is a closed vocabulary, so this reads the variant, never a message.
fn state_words(state: Option<&PropState>) -> String {
    match state {
        None => "it is not recorded".to_string(),
        Some(PropState::Text { .. } | PropState::Bytes { .. }) => "it has a value".to_string(),
        Some(PropState::Undecodable { why, .. }) => format!("the stored bytes do not decode ({why})"),
        Some(PropState::NotSupported) => "the body answers DevicePropNotSupported".to_string(),
        Some(PropState::ReadFailed { tag, .. }) => format!("its read failed ({tag})"),
    }
}

/// The skip for a refusal of the session's own write gate ([`check_string_property_write`],
/// [`check_byte_property_write`]): the planner asks the same function the session asks, and files its answer under the
/// plan's vocabulary by the error's variant. A model or property the write is not confirmed on is a model skip; a value
/// the gate refuses (range, text) is a value skip; anything else keeps the error's own words under the value kind.
fn skip_of_gate_refusal(e: &PtpUsbError) -> ItemAction {
    let kind = match e {
        PtpUsbError::UnconfirmedModel(_) | PtpUsbError::PropWriteUnconfirmed { .. } => SkipKind::ModelNotConfirmed,
        _ => SkipKind::ValueNotWritable,
    };
    skip(kind, e.to_string())
}

fn plan_strings(source: &Extended, live: &Extended, model: &str, only: Option<&BTreeSet<Category>>, out: &mut Vec<Planned>) {
    for &(code, name) in &STRING_PROPS {
        let id = prop_key(code);
        if !selected(only, Category::Strings) {
            out.push(Planned { item: item(&id, name, Category::Strings, ItemAction::NotSelected, ""), work: None });
            continue;
        }
        let (src_state, live_state) = (source.prop(code).map(|p| &p.state), live.prop(code).map(|p| &p.state));
        let (action, fingerprint, work) = match (src_state, live_state) {
            (Some(PropState::Text { text }), Some(PropState::Text { text: now })) if text == now => {
                (ItemAction::Unchanged, fp(&[text, now]), None)
            }
            (Some(PropState::Text { text }), Some(PropState::Text { text: now })) => {
                // The session's own gate: model and property, then the text (Latin-1, no control characters, length).
                match check_string_property_write(model, code, text, false) {
                    Err(e) => (skip_of_gate_refusal(&e), fp(&[text, now]), None),
                    Ok(_) => (
                        ItemAction::Write { from: Some(quoted(now)), to: quoted(text) },
                        fp(&[text, now]),
                        Some(Work::Text { code, text: text.clone() }),
                    ),
                }
            }
            (Some(PropState::Text { text }), Some(PropState::NotSupported)) => {
                (skip(SkipKind::BodyLacksIt, "the body answers DevicePropNotSupported"), fp(&[text, "not supported"]), None)
            }
            (Some(PropState::Text { text }), other) => (
                skip(SkipKind::BodyUnreadable, format!("the body's current value was not read: {}", state_words(other))),
                fp(&[text, "unread"]),
                None,
            ),
            (other, _) => (
                skip(SkipKind::SnapshotHasNoValue, format!("the snapshot holds no text for it: {}", state_words(other))),
                String::new(),
                None,
            ),
        };
        out.push(Planned { item: item(&id, name, Category::Strings, action, &fingerprint), work });
    }
}

fn plan_switches(source: &Extended, live: &Extended, model: &str, only: Option<&BTreeSet<Category>>, out: &mut Vec<Planned>) {
    for &(code, name, _max) in &BYTE_PROPS {
        let id = prop_key(code);
        if !selected(only, Category::Switches) {
            out.push(Planned { item: item(&id, name, Category::Switches, ItemAction::NotSelected, ""), work: None });
            continue;
        }
        let one_byte = |s: Option<&PropState>| match s {
            Some(PropState::Bytes { hex }) => hex_decode(hex).ok().filter(|b| b.len() == 1).map(|b| b[0]),
            _ => None,
        };
        let (src_state, live_state) = (source.prop(code).map(|p| &p.state), live.prop(code).map(|p| &p.state));
        let (src, now) = (one_byte(src_state), one_byte(live_state));
        let (action, fingerprint, work) = match (src, now) {
            (None, _) => (
                skip(SkipKind::SnapshotHasNoValue, format!("the snapshot holds no one-byte value for it: {}", state_words(src_state))),
                String::new(),
                None,
            ),
            (Some(v), Some(n)) if v == n => (ItemAction::Unchanged, fp(&[&v.to_string(), &n.to_string()]), None),
            (Some(v), None) => {
                let (kind, why) = if matches!(live_state, Some(PropState::NotSupported)) {
                    (SkipKind::BodyLacksIt, "the body answers DevicePropNotSupported".to_string())
                } else {
                    (SkipKind::BodyUnreadable, format!("the body's current value was not read as one byte: {}", state_words(live_state)))
                };
                (skip(kind, why), fp(&[&v.to_string(), "unread"]), None)
            }
            (Some(v), Some(n)) => {
                let fingerprint = fp(&[&v.to_string(), &n.to_string()]);
                // The session's own gate (known property, range, model), in the session's order.
                match check_byte_property_write(model, code, v, false) {
                    Err(e) => (skip_of_gate_refusal(&e), fingerprint, None),
                    Ok(()) => (
                        ItemAction::Write { from: Some(format!("0x{n:02x}")), to: format!("0x{v:02x}") },
                        fingerprint,
                        Some(Work::Byte { code, value: v }),
                    ),
                }
            }
        };
        out.push(Planned { item: item(&id, name, Category::Switches, action, &fingerprint), work });
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

/// Slot numbers that appear more than once in a store. The camera never lists one twice; a hand-edited snapshot could,
/// and two entries under one number would make two items with one id (`iptc:N`) and an ambiguous "which one is the
/// slot".
fn duplicate_slots(slots: &[crate::extended::IptcSlotEntry]) -> BTreeSet<u32> {
    let mut seen = BTreeSet::new();
    slots.iter().filter(|e| !seen.insert(e.slot)).map(|e| e.slot).collect()
}

fn plan_iptc(source: &Extended, live: &Extended, model: &str, only: Option<&BTreeSet<Category>>, out: &mut Vec<Planned>) {
    let whole = |action: ItemAction, fingerprint: &str| Planned { item: item("iptc", "IPTC presets", Category::Iptc, action, fingerprint), work: None };
    if !selected(only, Category::Iptc) {
        out.push(whole(ItemAction::NotSelected, ""));
        return;
    }
    let src = match &source.iptc {
        IptcState::Read(store) => store,
        other => {
            let why = match other {
                IptcState::NotRead | IptcState::Read(_) => "the snapshot predates IPTC capture".to_string(),
                IptcState::NotOnModel => "the snapshot's model has no IPTC presets".to_string(),
                IptcState::ReadFailed { tag, .. } => format!("the snapshot's IPTC read failed ({tag})"),
            };
            out.push(whole(skip(SkipKind::SnapshotHasNoValue, format!("the snapshot holds no IPTC presets: {why}")), ""));
            return;
        }
    };
    let live_store = match &live.iptc {
        IptcState::Read(s) => s,
        IptcState::NotOnModel => {
            out.push(whole(skip(SkipKind::BodyLacksIt, "this model has no IPTC presets"), ""));
            return;
        }
        other => {
            let why = match other {
                IptcState::ReadFailed { tag, .. } => format!("its read failed ({tag})"),
                _ => "it was not read".to_string(),
            };
            out.push(whole(skip(SkipKind::BodyUnreadable, format!("the body's IPTC presets were not read: {why}")), ""));
            return;
        }
    };
    // Slots are independent of each other, so a snapshot that read only a prefix of them (`src.truncated`) still has
    // each slot it did read restored; unlike the FTP list there is no whole-list rebuild that a prefix would shorten.
    let (dup_src, dup_live) = (duplicate_slots(&src.slots), duplicate_slots(&live_store.slots));
    let mut planned_slots = BTreeSet::new();
    for entry in &src.slots {
        if !planned_slots.insert(entry.slot) {
            continue; // a duplicate: the one item for this slot was already emitted (as a skip)
        }
        let id = format!("iptc:{}", entry.slot);
        let label = format!("IPTC preset {}", entry.slot);
        if dup_src.contains(&entry.slot) || dup_live.contains(&entry.slot) {
            let side = if dup_src.contains(&entry.slot) { "the snapshot" } else { "the body" };
            out.push(Planned {
                item: item(&id, &label, Category::Iptc, skip(SkipKind::RecordNotWritable, format!("{side} lists slot {} more than once, so which record is the slot's is not known", entry.slot)), &fp(&[&entry.hex, "duplicate"])),
                work: None,
            });
            continue;
        }
        let Some(now) = live_store.slots.iter().find(|s| s.slot == entry.slot) else {
            out.push(Planned { item: item(&id, &label, Category::Iptc, skip(SkipKind::SlotNotOnBody, "the body has no such slot"), &fp(&[&entry.hex, "absent"])), work: None });
            continue;
        };
        let fingerprint = fp(&[&entry.hex, &now.hex]);
        let (action, work) = if now.hex == entry.hex {
            (ItemAction::Unchanged, None)
        } else {
            match (entry.decode(), now.decode(), hex_decode(&entry.hex)) {
                (Ok(IptcRecord::Preset(_)), Ok(IptcRecord::Preset(_)), Ok(bytes)) => {
                    if iptc_write_confirmed(model) {
                        let from = preset_summary(&hex_decode(&now.hex).unwrap_or_default());
                        (ItemAction::Write { from: Some(from), to: preset_summary(&bytes) }, Some(Work::Iptc { slot: entry.slot, bytes }))
                    } else {
                        (skip(SkipKind::ModelNotConfirmed, format!("an IPTC write is not confirmed on {model}")), None)
                    }
                }
                (a, b, _) => (skip(SkipKind::RecordNotWritable, iptc_not_writable_words(&a, &b)), None),
            }
        };
        out.push(Planned { item: item(&id, &label, Category::Iptc, action, &fingerprint), work });
    }
}

/// Why a slot pair is not two presets, per side: a record of another kind (no layout to write) is not the same thing as
/// bytes that do not decode (the decoder's own reason is kept).
fn iptc_not_writable_words(snapshot: &Result<IptcRecord, String>, body: &Result<IptcRecord, String>) -> String {
    let side = |who: &str, r: &Result<IptcRecord, String>| match r {
        Ok(IptcRecord::Preset(_)) => None,
        Ok(IptcRecord::Other { kind, .. }) => Some(format!("the {who}'s slot holds a record of kind {kind}, which has no layout to write")),
        Err(why) => Some(format!("the {who}'s slot does not decode ({why})")),
    };
    [side("snapshot", snapshot), side("body", body)].into_iter().flatten().collect::<Vec<_>>().join("; ")
}

fn plan_ftp(source: &Extended, live: &Extended, model: &str, only: Option<&BTreeSet<Category>>, out: &mut Vec<Planned>) {
    let one = |action: ItemAction, fingerprint: &str, work: Option<Work>| Planned { item: item("ftp", "FTP profiles", Category::Ftp, action, fingerprint), work };
    if !selected(only, Category::Ftp) {
        out.push(one(ItemAction::NotSelected, "", None));
        return;
    }
    let src = match &source.ftp {
        FtpState::Read(store) => store,
        other => {
            let why = match other {
                FtpState::NotOnModel => "the snapshot's model has no FTP profiles".to_string(),
                FtpState::ReadFailed { tag, .. } => format!("the snapshot's FTP read failed ({tag})"),
                FtpState::Read(_) => String::new(),
            };
            out.push(one(skip(SkipKind::SnapshotHasNoValue, format!("the snapshot holds no FTP profiles: {why}")), "", None));
            return;
        }
    };
    let now = match &live.ftp {
        FtpState::Read(s) => s,
        FtpState::NotOnModel => {
            out.push(one(skip(SkipKind::BodyLacksIt, "this model has no FTP profiles"), "", None));
            return;
        }
        FtpState::ReadFailed { tag, .. } => {
            out.push(one(skip(SkipKind::BodyUnreadable, format!("the body's FTP profiles were not read ({tag})")), "", None));
            return;
        }
    };
    let join = |profiles: &[crate::extended::FtpProfileEntry]| profiles.iter().map(|p| p.hex.as_str()).collect::<Vec<_>>().join(",");
    // What would be written AND what the body holds now: a live list that became another list of the same length is a
    // different plan. (Hashed with the rest of the item; the blobs hold passwords and never leave this function as text.)
    let fingerprint = fp(&[&join(&src.profiles), &join(&now.profiles)]);
    if now.profiles.len() == src.profiles.len() && now.profiles.iter().zip(&src.profiles).all(|(a, b)| a.hex == b.hex) {
        out.push(one(ItemAction::Unchanged, &fingerprint, None));
        return;
    }
    let refuse = |reason: SkipKind, why: &str| one(skip(reason, why), &fingerprint, None);
    // The rebuild deletes the body's whole list, so a live list read only in part would leave the rest undeleted and
    // un-snapshotted: refuse it before anything else. (The camera's limit is 20 and the read cap 32, so it does not
    // occur on a body seen so far.)
    if now.truncated || now.count as usize != now.profiles.len() {
        out.push(refuse(SkipKind::BodyUnreadable, "the body's list was read only in part, so it cannot be replaced whole"));
        return;
    }
    // `truncated` is the reader's flag; a count that disagrees with the profiles held is the same fact said by a file
    // (a hand-edited or damaged snapshot): writing the held profiles would delete the rest.
    if src.truncated || src.count as usize != src.profiles.len() {
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
    let what = if blobs.is_empty() { "0 profiles (the list is emptied)".to_string() } else { format!("{} profile(s): {}", blobs.len(), names.join(", ")) };
    // No operation that selects the active profile was found, so a rebuild cannot restore which one it was. Said in the
    // item itself (and so in the digest), not left out: the camera picks the active profile on its own.
    let active = match src.active_slot {
        Some(slot) => {
            let named = names.get(slot as usize).filter(|n| !n.is_empty()).map(|n| format!(" ({n})")).unwrap_or_default();
            format!("; the snapshot's active profile (slot {slot}{named}) cannot be set, the camera chooses its own")
        }
        None => "; the snapshot had no active profile, and the camera chooses its own".to_string(),
    };
    out.push(one(ItemAction::Write { from: Some(from), to: format!("{what}{active}") }, &fingerprint, Some(Work::Ftp { blobs })));
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

/// One shell word: left bare when it is made only of characters no POSIX shell treats specially, else single-quoted
/// (each `'` inside written `'\''`), so a path with a space, `"`, `$` or a backtick pastes back as the same argument.
pub fn shell_word(text: &str) -> String {
    let plain = !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || "_-./:,@%+=".contains(c));
    if plain { text.to_string() } else { format!("'{}'", text.replace('\'', "'\\''")) }
}

/// The undo as a command that can be pasted and run: restore the restore point with the report's `undo_set` as
/// `--only`. `None` when there is nothing to undo. `data_dir` is named when the restore point is not in the default.
/// The file and directory are shell-quoted ([`shell_word`]); a category is one of four fixed words and needs none.
pub fn undo_command(restore_point_file: &str, undo_set: &[String], data_dir: Option<&str>) -> Option<String> {
    if undo_set.is_empty() {
        return None;
    }
    let only: String = undo_set.iter().map(|c| format!(" --only {c}")).collect();
    let dir = data_dir.map(|d| format!(" --data-dir {}", shell_word(d))).unwrap_or_default();
    Some(format!("nikon-fleet{dir} restore-extended {}{only}", shell_word(restore_point_file)))
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

/// The USB serial the snapshot is of, when it says one: its camera serial; or, when that is the `id-N` handle of one
/// session (which names no body), the serial the extended section was read from. `None` when the snapshot gives no
/// serial to compare. (An EMPTY camera serial stays "no evidence" even if the section has one: pinned by
/// `a_snapshot_without_the_section_...`.)
fn source_usb_serial(source: &Snapshot) -> Option<&str> {
    let serial = source.camera.serial.as_str();
    if serial.is_empty() {
        return None;
    }
    if crate::ops::target::is_id_fallback(serial) {
        return source.extended.as_ref().map(|e| e.read_from.usb_serial.as_str()).filter(|s| !s.is_empty() && !crate::ops::target::is_id_fallback(s));
    }
    Some(serial)
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
    // A body with no readable USB serial gives nothing to compare against (and with several bodies of the model
    // attached, the session would not have opened); the digest also binds the target serial.
    if let (Some(a), b) = (source_usb_serial(source), live.usb_serial.as_str())
        && !b.is_empty()
        && a != b
    {
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
    /// The camera refused, or the request failed before any data went out: nothing changed for this item. The one
    /// exception is an FTP rebuild that failed AFTER its delete-all was acknowledged: the list was emptied (or is
    /// partial), its message says how far it got, and its category is in the report's `undo_set`.
    Failed { message: String },
    /// The data may have gone out: the item may or may not have changed.
    Indeterminate { message: String },
    /// Not tried, because an earlier item failed or may have changed the body: a run stops at its first such item
    /// rather than writing on top of a state it can no longer vouch for.
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
            // Nothing was written. Whether that is "nothing to do" or "nothing could be done" is the status's: a plan
            // whose every item was skipped for a reason (not a routine absence) is a failed no-op, never "matches".
            if self.status == ApplyStatus::AppliedVerified {
                out.push_str("nothing to change: every item the snapshot and the body both hold already matches\n");
            } else {
                out.push_str(
                    "nothing was written, and the body was NOT confirmed to match the snapshot: the plan skipped what differs \
                     (run with --dry-run to see each item and its reason)\n",
                );
            }
            out.push_str(&format!("status: {:?}\n", self.status));
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
            out.push_str(&format!("{why}\n"));
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

/// Run the writes in plan order, and stop at the first item that failed or may have changed the body: every later
/// item is [`ItemOutcome::NotAttempted`] and sends nothing. (The order is a dependency, not a convenience: the IPTC
/// preset switch comes after the presets because a Z 9 refused to select a preset while all of them were empty, and the
/// FTP rebuild is the most destructive item, so it is never run on top of an earlier failure.) An item that was
/// written but whose own read-back did not confirm it is not a failure and does not stop the run. An FTP rebuild
/// stops at its first failure too.
fn execute<S: ExtendedSink + ?Sized>(sink: &mut S, plan: &[Planned], live_ftp_count: u32) -> Vec<ItemResult> {
    let mut results = Vec::new();
    let mut stopped_by: Option<String> = None;
    for p in plan {
        let Some(work) = &p.work else { continue };
        let outcome = match &stopped_by {
            Some(label) => ItemOutcome::NotAttempted { because: format!("{label} did not complete, and nothing after it is tried") },
            None => {
                let outcome = match work {
                    Work::Text { code, text } => outcome_of(sink.write_string(*code, text)),
                    Work::Byte { code, value } => outcome_of(sink.write_byte(*code, *value)),
                    Work::Iptc { slot, bytes } => outcome_of(sink.write_iptc(*slot, bytes)),
                    Work::Ftp { blobs } => execute_ftp(sink, blobs, live_ftp_count),
                };
                if matches!(outcome, ItemOutcome::Failed { .. } | ItemOutcome::Indeterminate { .. }) {
                    stopped_by = Some(p.item.label.clone());
                }
                outcome
            }
        };
        results.push(ItemResult { id: p.item.id.clone(), label: p.item.label.clone(), category: p.item.category, outcome });
    }
    results
}

/// Delete everything, then add the profiles last-first, because an add lands at the front of the list.
fn execute_ftp<S: ExtendedSink + ?Sized>(sink: &mut S, blobs: &[Vec<u8>], live_count: u32) -> ItemOutcome {
    let mut emptied = false;
    if live_count > 0 {
        match sink.delete_all_ftp() {
            Ok(0) => emptied = true,
            // The camera acknowledged the delete and still reports profiles: the list was touched and its state is not
            // the one either side described. Not "nothing changed".
            Ok(n) => {
                return ItemOutcome::Indeterminate {
                    message: format!(
                        "the camera acknowledged the delete but reports {n} profile(s) still stored; nothing was added, and the list may be partly deleted; the restore point holds the original list"
                    ),
                };
            }
            // Includes an acknowledged delete whose re-read failed (`FtpWriteIndeterminate`).
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
                let list = if emptied {
                    format!("the list was emptied and {done} of {} profile(s) were added; the restore point holds the original list", blobs.len())
                } else {
                    format!("{done} of {} profile(s) were added", blobs.len())
                };
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

/// The restore points the data directory holds for `camera`: the MCP's (`mcp-restore-...`, counted by
/// [`retention::count_for_camera`]) plus the ones this operation writes, which carry [`RESTORE_POINT_LABEL`] and which
/// `retention::kind_of_label` does not know. So the one per-camera cap binds the files that hold plaintext FTP
/// passwords as well, and they cannot pile up unseen.
fn restore_points_held(snapshots_dir: &Path, camera: &Camera) -> Result<retention::Counts, crate::ops::list::ListError> {
    use crate::ops::list::{EntryState, list_dir};
    let mut counts = retention::count_for_camera(snapshots_dir, &camera.model, &camera.serial)?;
    let listing = list_dir(snapshots_dir, None, None)?;
    counts.restore_points += listing
        .entries
        .iter()
        .filter(|e| matches!(&e.state, EntryState::Ok { model, serial, label, .. }
            if *model == camera.model && *serial == camera.serial && label.as_deref() == Some(RESTORE_POINT_LABEL)))
        .count();
    Ok(counts)
}

/// The status of an apply that wrote nothing. A plan with nothing but matching items (and routine absences: a body
/// without the feature, a snapshot without the value) is a verified no-op. A plan that skipped something for a reason
/// ([`SkipKind::is_routine`] is false) did NOT leave the body matching the snapshot, and "applied, verified, nothing to
/// change" would say it did: that is `Failed`, as the MAID apply's no-op is when it copied nothing because of problems.
fn zero_write_status(plan: &[Planned]) -> ApplyStatus {
    let blocked = plan.iter().any(|p| matches!(&p.item.action, ItemAction::Skip { reason, .. } if !reason.is_routine()));
    if blocked { ApplyStatus::Failed } else { ApplyStatus::AppliedVerified }
}

/// How one written item looks in the plan made from the body read after the writes.
enum AfterWrite {
    Matches,
    Differs,
    /// The body could not be read for it afterwards, so nothing can be said.
    Unreadable,
}

fn judge_after_write(written: &ItemResult, after_plan: &[Planned]) -> AfterWrite {
    // The item itself; or, when the whole category could not be read (IPTC: one item for the lot, not one per slot), that.
    let own = after_plan.iter().find(|p| p.item.id == written.id);
    let whole = || {
        after_plan
            .iter()
            .find(|p| p.item.category == written.category && matches!(p.item.action, ItemAction::Skip { reason: SkipKind::BodyUnreadable, .. }))
    };
    match own.or_else(whole).map(|p| &p.item.action) {
        Some(ItemAction::Unchanged) => AfterWrite::Matches,
        Some(ItemAction::Skip { reason: SkipKind::BodyUnreadable, .. }) => AfterWrite::Unreadable,
        Some(_) | None => AfterWrite::Differs,
    }
}

/// Apply. Recomputes the plan on the body as it is now and refuses a stale one; writes the restore point; writes;
/// reads the body again and checks. `progress` is told the shared `PHASE_*` phases (`ops::worker`), which the MCP
/// supervisor and the worker key their cancel and "were writes possible" decisions on.
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
    progress(PHASE_PLAN_VERIFIED);
    if n == 0 {
        return Ok(ExtendedApplyReport {
            status: zero_write_status(&plan),
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
    // Refused first, like the MAID apply's, when the data directory is at its restore-point cap.
    let mut rp = Snapshot::new(input.target.clone(), Transport::Usb, req.captured_at.clone());
    rp.label = Some(RESTORE_POINT_LABEL.to_string());
    rp.extended = Some(live.clone());
    let rp_file = rp.suggested_filename();
    std::fs::create_dir_all(req.snapshots_dir).map_err(|e| Refusal::RestorePoint(SnapshotError::Io(e)))?;
    let counts = restore_points_held(req.snapshots_dir, input.target).map_err(ApplyError::List)?;
    retention::check_restore_point_cap(&counts).map_err(ApplyError::RestorePointCap)?;
    rp.save_new(req.snapshots_dir.join(&rp_file)).map_err(Refusal::RestorePoint)?;
    progress(PHASE_RESTORE_POINT_WRITTEN);

    progress(PHASE_WRITING);
    let live_ftp = match &live.ftp {
        FtpState::Read(s) => s.profiles.len() as u32,
        _ => 0,
    };
    let results = execute(sink, &plan, live_ftp);

    progress(PHASE_READ_BACK);
    let after = read_extended(sink);
    let mut after_read_failed = (after.read_from.model.is_empty()).then(|| "the body could not be read after the writes; nothing could be checked".to_string());
    let mut still_differing: Vec<String> = Vec::new();
    if after_read_failed.is_none() {
        let after_plan = plan_extended_restore(&source, &after, input.only);
        let mut unreadable: Vec<&str> = Vec::new();
        for r in results.iter().filter(|r| matches!(r.outcome, ItemOutcome::Written { .. })) {
            match judge_after_write(r, &after_plan) {
                AfterWrite::Matches => {}
                AfterWrite::Differs => still_differing.push(r.id.clone()),
                AfterWrite::Unreadable => unreadable.push(r.label.as_str()),
            }
        }
        if !unreadable.is_empty() {
            after_read_failed = Some(format!("after the writes the body could not be read for: {}; they could not be checked", unreadable.join(", ")));
        }
    }
    let status = status_of(&results, &still_differing, after_read_failed.is_some());
    Ok(ExtendedApplyReport {
        status,
        no_op: false,
        plan_digest: now_digest,
        restore_point: Some(RestorePoint { file: rp_file, label: RESTORE_POINT_LABEL.to_string() }),
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
        /// `delete_all_ftp` reports this many profiles left after an acknowledged delete.
        delete_leaves: u32,
        /// Writes to these ids are accepted but their own read-back did not match (`Ok(false)`).
        unverified: Vec<String>,
        /// After the first write, reading the IPTC slots / the FTP list fails.
        iptc_unreadable_after_write: bool,
        ftp_unreadable_after_write: bool,
        /// The FTP list header reports this count instead of the number of profiles held.
        ftp_count: Option<u32>,
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
            delete_leaves: 0,
            unverified: vec![],
            iptc_unreadable_after_write: false,
            ftp_unreadable_after_write: false,
            ftp_count: None,
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
                    // The library's own typed variant for each write path.
                    if key.starts_with("ftp") {
                        PtpUsbError::FtpWriteIndeterminate { cause: Box::new(cause) }
                    } else if key.starts_with("iptc") {
                        PtpUsbError::IptcWriteIndeterminate { cause: Box::new(cause) }
                    } else {
                        PtpUsbError::PropWriteIndeterminate { propcode: 0, cause: Box::new(cause) }
                    }
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
            if self.ftp_unreadable_after_write && !self.log.is_empty() {
                return Err(PtpUsbError::NoDevice);
            }
            Ok(FtpListState {
                info: FtpProfileInfo {
                    header: 200,
                    count: self.ftp_count.unwrap_or(self.ftp.len() as u32),
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
            if self.iptc_unreadable_after_write && !self.log.is_empty() {
                return Err(PtpUsbError::NoDevice);
            }
            Ok((self.iptc.len() as u32, self.iptc.iter().enumerate().map(|(i, b)| (i as u32 + 1, b.clone())).collect()))
        }
    }

    impl ExtendedSink for Body {
        fn write_string(&mut self, code: u16, text: &str) -> Result<bool, PtpUsbError> {
            self.log.push(format!("string:{code:#06x}"));
            self.err(&format!("string:{code:#06x}"), 0x1016)?;
            self.props.insert(code, ptp_string(text).unwrap());
            Ok(!self.unverified.contains(&format!("string:{code:#06x}")))
        }
        fn write_byte(&mut self, code: u16, value: u8) -> Result<bool, PtpUsbError> {
            self.log.push(format!("byte:{code:#06x}"));
            self.err(&format!("byte:{code:#06x}"), 0x1016)?;
            self.props.insert(code, vec![value]);
            Ok(!self.unverified.contains(&format!("byte:{code:#06x}")))
        }
        fn write_iptc(&mut self, slot: u32, data: &[u8]) -> Result<bool, PtpUsbError> {
            self.log.push(format!("iptc:{slot}"));
            self.err(&format!("iptc:{slot}"), 0x9413)?;
            self.iptc[slot as usize - 1] = data.to_vec();
            Ok(!self.unverified.contains(&format!("iptc:{slot}")))
        }
        fn delete_all_ftp(&mut self) -> Result<u32, PtpUsbError> {
            self.log.push("ftp_delete".into());
            self.err("ftp_delete", 0x90EA)?;
            self.ftp.clear();
            Ok(self.delete_leaves)
        }
        fn add_ftp(&mut self, blob: &[u8], count_before: u32) -> Result<bool, PtpUsbError> {
            self.log.push(format!("ftp_add:{count_before}"));
            self.err(&format!("ftp_add:{count_before}"), 0x90EE)?;
            assert_eq!(count_before as usize, self.ftp.len(), "an add is sent with the stored count");
            self.ftp.insert(0, blob.to_vec());
            Ok(!self.unverified.contains(&format!("ftp_add:{count_before}")))
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

    /// Fails if `text` carries a secret of the fixture's profiles in any shape a leak takes: the plain text, its ASCII
    /// bytes in hex, its UTF-16 bytes in hex, a whole stored blob in hex, or a blob / the secret as the list of byte
    /// values a derived `Debug` of a `Vec<u8>` prints. (A check for the plain text alone cannot fail for a view that
    /// holds only names or hashes.)
    fn assert_no_secret_anywhere(text: &str, blobs: &[Vec<u8>]) {
        let lower = text.to_lowercase();
        let hex_ascii = |s: &str| hex_encode(s.as_bytes());
        let hex_utf16 = |s: &str| hex_encode(&s.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>());
        let mut needles: Vec<(String, String)> = Vec::new();
        for secret in ["password-1", "password-2", "password-9", "key-12345"] {
            needles.push((format!("{secret} as text"), secret.to_string()));
            needles.push((format!("{secret} as ASCII hex"), hex_ascii(secret)));
            needles.push((format!("{secret} as UTF-16 hex"), hex_utf16(secret)));
            needles.push((format!("{secret} as a byte list"), format!("{:?}", secret.as_bytes())));
        }
        for blob in blobs {
            needles.push(("a stored blob as hex".to_string(), hex_encode(blob)));
            needles.push(("a stored blob as a byte list".to_string(), format!("{blob:?}")));
        }
        for (what, needle) in needles {
            assert!(!lower.contains(&needle.to_lowercase()), "{what} is in: {text}");
        }
    }

    #[test]
    fn no_plan_report_or_debug_of_either_carries_a_password_or_key_in_any_shape() {
        let (mut b, f) = drifted();
        let blobs = vec![profile("one", "password-1"), profile("two", "password-2"), profile("new", "password-9")];
        let plan = plan_of(&mut b, &f, None);
        // The planner's own items, with the work apply would send (the FTP blobs), through Debug.
        let live = b.extended();
        let planned = plan_extended_restore(f.snap.extended.as_ref().unwrap(), &live, None);
        assert!(planned.iter().any(|p| matches!(p.work, Some(Work::Ftp { .. }))), "the fixture plans an FTP rebuild, so the blobs are in the Debug under test");
        let report = apply(&mut b, &f, None).unwrap();
        for text in [
            serde_json::to_string(&plan).unwrap(),
            serde_json::to_string(&report).unwrap(),
            plan.to_text(),
            report.to_text(),
            format!("{plan:?}"),
            format!("{report:?}"),
            format!("{planned:?}"),
            format!("{:#?}", planned),
        ] {
            assert_no_secret_anywhere(&text, &blobs);
        }
    }

    #[test]
    fn the_secret_check_catches_each_shape_of_leak() {
        let blobs = vec![profile("one", "password-1")];
        let utf16 = hex_encode(&"password-2".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>());
        for leak in [
            "x password-1 x".to_string(),
            hex_encode(b"password-1"),
            utf16,
            hex_encode(&blobs[0]),
            format!("{:?}", blobs[0]),
            format!("{:?}", b"key-12345"),
        ] {
            let caught = std::panic::catch_unwind(|| assert_no_secret_anywhere(&leak, &profile_blobs_for_check()));
            assert!(caught.is_err(), "a leak went unseen: {leak}");
        }
        assert_no_secret_anywhere("\"one\", \"two\"", &blobs);
    }

    fn profile_blobs_for_check() -> Vec<Vec<u8>> {
        vec![profile("one", "password-1")]
    }

    #[test]
    fn a_planned_ftp_rebuild_prints_no_blob_in_its_debug() {
        // The Debug of the work a plan carries names how many profiles and how many bytes, and nothing else of them.
        let work = Work::Ftp { blobs: vec![profile("one", "password-1"), profile("two", "password-2")] };
        let text = format!("{work:?}");
        assert!(text.contains("profiles: 2") && text.contains("hidden"), "{text}");
        assert_no_secret_anywhere(&text, &[profile("one", "password-1"), profile("two", "password-2")]);
    }

    #[test]
    fn a_string_the_camera_cannot_hold_is_skipped_and_the_boundary_is_u00ff() {
        let mut b = body("Z 9");
        let mut f = fixture(&mut b);
        let at_limit = "x".repeat(254);
        let over_limit = "x".repeat(255);
        for (text, writable) in [
            ("é", true),
            ("\u{ff}", true),
            ("\u{100}", false),
            ("日本", false),
            ("a\tb", false),
            ("", true),
            (at_limit.as_str(), true),
            (over_limit.as_str(), false),
        ] {
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
        let g = fixture(&mut body("Z 9"));
        b.ftp = vec![];
        assert!(matches!(action(&plan_of(&mut b, &g, None), "ftp"), ItemAction::Write { .. }), "2 profiles into a limit of 2");
        b.limit = 1;
        assert!(matches!(action(&plan_of(&mut b, &g, None), "ftp"), ItemAction::Skip { reason: SkipKind::ListTooLong, .. }));
        // Below the limit (1 profile into a limit of 2) is allowed.
        let mut one = body("Z 9");
        one.ftp.truncate(1);
        let h = fixture(&mut one);
        b.limit = 2;
        assert!(matches!(action(&plan_of(&mut b, &h, None), "ftp"), ItemAction::Write { .. }), "1 profile into a limit of 2");
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
    fn a_failed_write_stops_the_run_and_every_later_item_is_not_attempted_and_sends_nothing() {
        let (mut b, f) = drifted();
        b.fail.push("string:0xd072".into());
        let report = apply(&mut b, &f, None).unwrap();
        assert_eq!(report.status, ApplyStatus::Failed);
        let by_id = |id: &str| report.results.iter().find(|r| r.id == id).unwrap().outcome.clone();
        assert!(matches!(by_id("0xd072"), ItemOutcome::Failed { .. }));
        assert!(matches!(by_id("iptc:1"), ItemOutcome::NotAttempted { .. }), "{:?}", by_id("iptc:1"));
        let ItemOutcome::NotAttempted { because } = by_id("ftp") else { panic!("the FTP rebuild must not run on top of a failure") };
        assert!(because.contains("Artist"), "it names the item that stopped the run: {because}");
        assert_eq!(b.log, ["string:0xd072"], "nothing after the failure was sent (the FTP rebuild above all)");
        assert_eq!(b.ftp.len(), 1, "the body's FTP list was not touched");
        assert_eq!(report.undo_set, ["strings"], "an item that was never attempted changed nothing, so its category is not in the undo");
    }

    #[test]
    fn an_indeterminate_write_stops_the_run_too_and_the_status_stays_indeterminate() {
        let (mut b, f) = drifted_on("Z6_3");
        b.props.insert(0xD060, vec![3]); // so the preset switch is a write after the IPTC presets
        b.fail.push("iptc:1".into());
        b.indeterminate.push("iptc:1".into());
        let report = apply(&mut b, &f, None).unwrap();
        assert_eq!(report.status, ApplyStatus::Indeterminate);
        assert_eq!(b.log, ["string:0xd072", "iptc:1"], "no preset select after an IPTC slot that may have changed, and no FTP rebuild");
        let kinds: Vec<&str> = report.results.iter().map(|r| match r.outcome {
            ItemOutcome::Written { .. } => "written",
            ItemOutcome::Indeterminate { .. } => "indeterminate",
            ItemOutcome::NotAttempted { .. } => "not_attempted",
            ItemOutcome::Failed { .. } => "failed",
        }).collect();
        assert_eq!(kinds, ["written", "indeterminate", "not_attempted", "not_attempted"], "{:?}", report.results);
    }

    #[test]
    fn a_write_whose_own_read_back_did_not_match_is_not_a_failure_and_does_not_stop_the_run() {
        let (mut b, f) = drifted();
        b.unverified.push("string:0xd072".into());
        let report = apply(&mut b, &f, None).unwrap();
        assert_eq!(report.status, ApplyStatus::AppliedUnverified, "{report:?}");
        assert!(matches!(report.results[0].outcome, ItemOutcome::Written { verified: false }));
        assert_eq!(b.log, ["string:0xd072", "iptc:1", "ftp_delete", "ftp_add:0", "ftp_add:1"], "the later items still ran");
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
            "nikon-fleet --data-dir '/my data' restore-extended rp.json --only strings"
        );
        assert_eq!(undo_command("rp.json", &[], None), None, "nothing to undo is not a command that restores everything");
        // The command it prints parses back to exactly the categories it names.
        for tag in set {
            assert!(Category::parse(&tag).is_ok());
        }
    }

    // ── a zero-write apply, and the phases ────────────────────────────────

    fn apply_recording(b: &mut Body, f: &Fixture, only: Option<&BTreeSet<Category>>) -> (Result<ExtendedApplyReport, ExtendedApplyError>, Vec<String>) {
        let plan = plan_of(b, f, only);
        let t = target(b);
        let req = ExtendedApplyRequest {
            input: input(f, &t, only),
            snapshots_dir: f.dir.path(),
            expected_digest: &plan.digest,
            expected_summary: &plan.summary,
            captured_at: "2026-11-06T01:00:00Z".into(),
        };
        let mut phases = Vec::new();
        let r = apply_extended_restore(b, &req, &mut |p| phases.push(p.to_string()));
        (r, phases)
    }

    #[test]
    fn the_apply_reports_the_shared_phases_in_order_under_their_wire_names() {
        // The literals are the wire words the supervisor and the worker key on (`ops::worker::PHASE_*`): a rename of a
        // constant, or a typo at a call site, fails here.
        let (mut b, f) = drifted();
        let (r, phases) = apply_recording(&mut b, &f, None);
        r.unwrap();
        assert_eq!(phases, ["plan_verified", "restore_point_written", "writing", "read_back"]);
        // The order is also the order of the effects: no restore point at "plan_verified", one at every phase after it.
        let (mut b, f) = drifted();
        let plan = plan_of(&mut b, &f, None);
        let t = target(&b);
        let req = ExtendedApplyRequest { input: input(&f, &t, None), snapshots_dir: f.dir.path(), expected_digest: &plan.digest, expected_summary: &plan.summary, captured_at: "x".into() };
        let dir = f.dir.path().to_path_buf();
        let mut files_at = Vec::new();
        apply_extended_restore(&mut b, &req, &mut |p| files_at.push((p.to_string(), std::fs::read_dir(&dir).unwrap().count()))).unwrap();
        assert_eq!(files_at.iter().map(|(p, n)| (p.as_str(), *n)).collect::<Vec<_>>(), [("plan_verified", 0), ("restore_point_written", 1), ("writing", 1), ("read_back", 1)]);
    }

    #[test]
    fn a_stale_or_unconfirmed_apply_announces_no_phase_at_all() {
        let (mut b, f) = drifted();
        let plan = plan_of(&mut b, &f, None);
        b.props.insert(0xD073, ptp_string("changed since the plan").unwrap());
        let t = target(&b);
        let mut phases = Vec::new();
        let req = ExtendedApplyRequest { input: input(&f, &t, None), snapshots_dir: f.dir.path(), expected_digest: &plan.digest, expected_summary: &plan.summary, captured_at: "x".into() };
        assert!(apply_extended_restore(&mut b, &req, &mut |p| phases.push(p.to_string())).is_err());
        assert!(phases.is_empty(), "{phases:?}: not even plan_verified before the plan is verified");
    }

    #[test]
    fn a_nothing_to_write_apply_announces_only_that_the_plan_was_verified() {
        let mut b = body("Z 9");
        let f = fixture(&mut b);
        let (r, phases) = apply_recording(&mut b, &f, None);
        assert!(r.unwrap().no_op);
        assert_eq!(phases, ["plan_verified"], "no restore point, no writing, no read-back");
    }

    #[test]
    fn skips_are_routine_or_not_by_one_table() {
        use SkipKind::*;
        let routine = [SnapshotHasNoValue, BodyLacksIt];
        let not_routine = [BodyUnreadable, ValueNotWritable, ModelNotConfirmed, RecordNotWritable, SlotNotOnBody, ListTruncated, ProfileNotDecodable, ListTooLong];
        for k in routine {
            assert!(k.is_routine(), "{k:?}");
        }
        for k in not_routine {
            assert!(!k.is_routine(), "{k:?}");
        }
        assert_eq!(routine.len() + not_routine.len(), 10, "every SkipKind is classified above");
    }

    #[test]
    fn an_apply_whose_every_difference_was_skipped_wrote_nothing_and_is_not_a_verified_no_op() {
        // A Z 9 snapshot against a Z 9 whose EnableCopyright differs: the write is not confirmed on a Z 9, so the one
        // difference is a skip (`model_not_confirmed`), `would_change` is 0, and nothing may be called "matching".
        let mut b = body("Z 9");
        let f = fixture(&mut b);
        b.props.insert(0xD053, vec![0]);
        let plan = plan_of(&mut b, &f, None);
        assert!(matches!(action(&plan, "0xd053"), ItemAction::Skip { reason: SkipKind::ModelNotConfirmed, .. }));
        assert_eq!(plan.would_change, 0, "a skip is not a change");
        let report = apply(&mut b, &f, None).unwrap();
        assert!(report.no_op && report.restore_point.is_none() && report.results.is_empty() && report.undo_set.is_empty());
        assert_eq!(report.status, ApplyStatus::Failed, "the body still differs from the snapshot");
        assert!(b.log.is_empty() && std::fs::read_dir(f.dir.path()).unwrap().count() == 0, "nothing sent, no restore point");
        let text = report.to_text();
        assert!(text.contains("NOT confirmed") && !text.contains("already match"), "{text}");
        // Every difference skipped for a ROUTINE reason (here: the snapshot holds no IPTC or FTP) stays a verified no-op.
        let mut b = body("Z 9");
        let mut f = fixture(&mut b);
        let e = f.snap.extended.as_mut().unwrap();
        e.iptc = IptcState::NotRead;
        e.ftp = FtpState::NotOnModel;
        let report = apply(&mut b, &f, None).unwrap();
        assert!(report.no_op);
        assert_eq!(report.status, ApplyStatus::AppliedVerified);
        assert!(report.to_text().contains("nothing to change"));
        // Mixed: a routine skip beside a non-routine one is still failed.
        b.props.insert(0xD053, vec![0]);
        let report = apply(&mut b, &f, None).unwrap();
        assert!(report.no_op && report.status == ApplyStatus::Failed);
    }

    #[test]
    fn a_skip_beside_a_write_does_not_change_what_a_write_run_reports() {
        // Pinned: a run that wrote something is judged by its writes (the GUI pins "skip does not block" too).
        let (mut b, f) = drifted();
        b.props.insert(0xD053, vec![0]); // a Z 9 switch difference: skipped
        let report = apply(&mut b, &f, None).unwrap();
        assert!(!report.no_op);
        assert_eq!(report.status, ApplyStatus::AppliedVerified, "{report:?}");
    }

    // ── what the digest binds ─────────────────────────────────────────────

    #[test]
    fn the_digest_binds_the_target_serial() {
        // A snapshot with no serial is not checked against the body, so the digest is the only thing tying a plan to the
        // body it was made for.
        let mut b = body("Z 9");
        let mut f = fixture(&mut b);
        f.snap.camera.serial = String::new();
        b.props.insert(0xD072, ptp_string("drift").unwrap());
        let mut digest_for = |serial: &str| {
            let t = Camera { model: "Z 9".into(), serial: serial.into(), firmware: "1".into() };
            build_extended_restore_plan(&mut b, &input(&f, &t, None)).unwrap().digest
        };
        let (one, two) = (digest_for("0000001"), digest_for("0000002"));
        assert_ne!(one, two);
        assert_eq!(one, digest_for("0000001"), "stable for one target");
    }

    #[test]
    fn a_live_ftp_list_that_became_another_list_of_the_same_size_makes_the_plan_stale() {
        let (mut b, f) = drifted(); // the body holds one profile, "new"
        let plan = plan_of(&mut b, &f, None);
        // Same count, same name, a different password: what the plan shows ("1 stored, all deleted first") is unchanged.
        b.ftp = vec![profile("new", "password-8")];
        let again = plan_of(&mut b, &f, None);
        assert_eq!(action(&again, "ftp"), action(&plan, "ftp"), "the display cannot tell the two lists apart");
        assert_ne!(again.digest, plan.digest, "the digest can");
        assert_eq!(apply_with(&mut b, &f, None, &plan.digest, &plan.summary).unwrap_err().tag(), "stale_plan");
        assert!(b.log.is_empty() && std::fs::read_dir(f.dir.path()).unwrap().count() == 0, "nothing sent, no restore point");
    }

    #[test]
    fn a_live_iptc_preset_that_became_another_with_the_same_name_and_field_count_makes_the_plan_stale() {
        let (mut b, f) = drifted(); // slot 1 holds preset "X" with the headline "changed"
        let plan = plan_of(&mut b, &f, None);
        b.iptc[0] = preset("X", "different headline"); // same name, one field filled
        let again = plan_of(&mut b, &f, None);
        assert_eq!(action(&again, "iptc:1"), action(&plan, "iptc:1"), "the display cannot tell the two apart");
        assert_ne!(again.digest, plan.digest);
        assert_eq!(apply_with(&mut b, &f, None, &plan.digest, &plan.summary).unwrap_err().tag(), "stale_plan");
        assert!(b.log.is_empty());
    }

    #[test]
    fn a_live_switch_or_string_that_changed_under_an_item_that_is_only_skipped_still_changes_the_digest() {
        // The fingerprint holds the live value for a skip as well, so a body that moved under a skipped item is noticed.
        let mut b = body("Z 9");
        let f = fixture(&mut b);
        b.props.insert(0xD053, vec![0]);
        let before = plan_of(&mut b, &f, None).digest;
        b.props.insert(0xD053, vec![2]);
        assert_ne!(before, plan_of(&mut b, &f, None).digest);
    }

    // ── the restore point ─────────────────────────────────────────────────

    fn fill_with_mcp_restore_points(dir: &Path, camera: &Camera, n: usize) {
        for i in 0..n {
            let mut rp = Snapshot::new(camera.clone(), Transport::Usb, format!("2026-11-05T00:{:02}:{:02}Z", i / 60, i % 60));
            rp.label = Some(crate::ops::retention::restore_point_label(crate::ops::retention::RestoreOp::PreRestore, &format!("{i:04}")).unwrap());
            rp.save_new(dir.join(rp.suggested_filename())).unwrap();
        }
    }

    #[test]
    fn the_restore_point_obeys_the_data_directorys_restore_point_cap_before_any_write() {
        let cap = crate::ops::retention::MAX_RESTORE_POINTS_PER_CAMERA;
        // One below the cap: another is allowed.
        let (mut b, f) = drifted();
        fill_with_mcp_restore_points(f.dir.path(), &target(&b), cap - 1);
        assert_eq!(apply(&mut b, &f, None).unwrap().status, ApplyStatus::AppliedVerified);
        // At the cap: refused, nothing sent, no file added.
        let (mut b, f) = drifted();
        fill_with_mcp_restore_points(f.dir.path(), &target(&b), cap);
        let err = apply(&mut b, &f, None).unwrap_err();
        assert_eq!(err.tag(), "data_dir_full", "{err}");
        assert!(b.log.is_empty(), "no write: {:?}", b.log);
        assert_eq!(std::fs::read_dir(f.dir.path()).unwrap().count(), cap, "no restore point was added");
        // The restore points this operation wrote itself count toward the same cap (one at the cap, mixed with MCP ones).
        let (mut b, f) = drifted();
        fill_with_mcp_restore_points(f.dir.path(), &target(&b), cap - 2);
        let at = |b: &mut Body, when: &str| {
            let plan = plan_of(b, &f, None);
            let t = target(b);
            let req = ExtendedApplyRequest { input: input(&f, &t, None), snapshots_dir: f.dir.path(), expected_digest: &plan.digest, expected_summary: &plan.summary, captured_at: when.into() };
            apply_extended_restore(b, &req, &mut |_| {})
        };
        let first = at(&mut b, "2026-11-06T01:00:00Z").unwrap().restore_point.unwrap();
        assert_eq!(first.label, RESTORE_POINT_LABEL);
        b.props.insert(0xD072, ptp_string("again").unwrap());
        assert!(at(&mut b, "2026-11-06T02:00:00Z").unwrap().restore_point.is_some(), "cap - 2 MCP + 1 restore-ext = cap - 1: still allowed");
        b.props.insert(0xD072, ptp_string("third").unwrap());
        b.log.clear();
        assert_eq!(at(&mut b, "2026-11-06T03:00:00Z").unwrap_err().tag(), "data_dir_full", "cap - 2 MCP + 2 restore-ext = cap");
        assert!(b.log.is_empty(), "refused before any write");
        // The restore point keeps its one label.
        assert_eq!(RESTORE_POINT_LABEL, "restore-ext");
    }

    // ── the undo command ──────────────────────────────────────────────────

    #[test]
    fn the_undo_command_quotes_what_a_shell_would_split_or_expand_and_the_words_round_trip_through_sh() {
        for text in ["/plain/dir", "/my data", "/a\"b", "/c$HOME", "/d`id`", "/e'f", "", "/g;h", "/h\\i", "/ünï/cødé"] {
            let word = shell_word(text);
            let out = std::process::Command::new("sh").arg("-c").arg(format!("printf %s {word}")).output().unwrap();
            assert_eq!(String::from_utf8(out.stdout).unwrap(), text, "{word}");
        }
        assert_eq!(shell_word("/plain/dir"), "/plain/dir", "a safe word stays bare");
        let cmd = undo_command("rp file.json", &["ftp".to_string()], Some("/d\"ata")).unwrap();
        assert_eq!(cmd, "nikon-fleet --data-dir '/d\"ata' restore-extended 'rp file.json' --only ftp");
    }

    // ── the FTP rebuild's branches ───────────────────────────────────────

    #[test]
    fn a_delete_the_camera_acknowledged_but_that_left_profiles_is_indeterminate_not_nothing_changed() {
        let (mut b, f) = drifted();
        b.delete_leaves = 2;
        let report = apply(&mut b, &f, None).unwrap();
        assert_eq!(report.status, ApplyStatus::Indeterminate);
        let ItemOutcome::Indeterminate { message } = &report.results.iter().find(|r| r.id == "ftp").unwrap().outcome else { panic!("{:?}", report.results) };
        assert!(message.contains("2 profile(s) still stored") && message.contains("restore point"), "{message}");
        assert!(!b.log.iter().any(|l| l.starts_with("ftp_add")), "nothing was added after it: {:?}", b.log);
        assert!(report.undo_set.contains(&"ftp".to_string()));
    }

    #[test]
    fn an_acknowledged_delete_whose_re_read_failed_is_indeterminate() {
        // The library reports it as `FtpWriteIndeterminate` (ptp_usb::delete_all_ftp_profiles).
        let (mut b, f) = drifted();
        b.fail.push("ftp_delete".into());
        b.indeterminate.push("ftp_delete".into());
        let report = apply(&mut b, &f, None).unwrap();
        assert_eq!(report.status, ApplyStatus::Indeterminate);
        assert!(matches!(report.results.iter().find(|r| r.id == "ftp").unwrap().outcome, ItemOutcome::Indeterminate { .. }));
        // A delete the camera refused stays a plain failure.
        let (mut b, f) = drifted();
        b.fail.push("ftp_delete".into());
        let report = apply(&mut b, &f, None).unwrap();
        assert_eq!(report.status, ApplyStatus::Failed);
    }

    #[test]
    fn an_add_whose_own_read_back_did_not_match_leaves_the_rebuild_written_but_unverified() {
        let (mut b, f) = drifted();
        b.unverified.push("ftp_add:1".into());
        let report = apply(&mut b, &f, None).unwrap();
        assert_eq!(report.results.iter().find(|r| r.id == "ftp").unwrap().outcome, ItemOutcome::Written { verified: false });
        assert_eq!(report.status, ApplyStatus::AppliedUnverified);
        assert_eq!(b.ftp.len(), 2, "both adds were sent");
    }

    #[test]
    fn a_snapshot_list_of_zero_profiles_over_a_stored_list_deletes_everything_and_adds_nothing() {
        let mut empty = body("Z 9");
        empty.ftp.clear();
        let f = fixture(&mut empty);
        let mut b = body("Z 9"); // two stored profiles
        let plan = plan_of(&mut b, &f, None);
        let ItemAction::Write { from, to } = action(&plan, "ftp") else { panic!("{:?}", action(&plan, "ftp")) };
        assert_eq!(from.as_deref(), Some("2 stored, all deleted first"));
        assert!(to.starts_with("0 profiles (the list is emptied)"), "{to}");
        let report = apply(&mut b, &f, None).unwrap();
        assert_eq!(b.log, ["ftp_delete"]);
        assert!(b.ftp.is_empty());
        assert_eq!(report.status, ApplyStatus::AppliedVerified, "{report:?}");
    }

    #[test]
    fn a_first_add_that_fails_on_an_already_empty_list_does_not_claim_the_list_was_emptied() {
        let mut b = body("Z 9");
        let f = fixture(&mut b);
        b.ftp.clear();
        b.fail.push("ftp_add:0".into());
        let report = apply(&mut b, &f, None).unwrap();
        let ItemOutcome::Failed { message } = &report.results.iter().find(|r| r.id == "ftp").unwrap().outcome else { panic!() };
        assert!(message.contains("0 of 2") && !message.contains("emptied"), "{message}");
    }

    #[test]
    fn the_plan_says_the_active_profile_cannot_be_set_and_names_the_one_the_snapshot_had() {
        let (mut b, f) = drifted();
        let plan = plan_of(&mut b, &f, None);
        let ItemAction::Write { to, .. } = action(&plan, "ftp") else { panic!() };
        assert!(to.contains("active profile (slot 0 (\"one\")) cannot be set"), "{to}");
        // A snapshot that had none says so.
        let mut g = f;
        if let Some(FtpState::Read(store)) = g.snap.extended.as_mut().map(|e| &mut e.ftp) {
            store.active_slot = None;
        }
        let plan = plan_of(&mut b, &g, None);
        let ItemAction::Write { to, .. } = action(&plan, "ftp") else { panic!() };
        assert!(to.contains("had no active profile"), "{to}");
    }

    // ── truncation and counts ─────────────────────────────────────────────

    #[test]
    fn a_snapshot_list_whose_count_says_more_than_it_holds_is_skipped_even_when_not_flagged_truncated() {
        let mut b = body("Z 9");
        let mut f = fixture(&mut b);
        b.ftp = vec![profile("other", "password-9")];
        let set_count = |f: &mut Fixture, n: u32| {
            if let FtpState::Read(s) = &mut f.snap.extended.as_mut().unwrap().ftp {
                assert!(!s.truncated);
                s.count = n;
            }
        };
        set_count(&mut f, 3); // two profiles held, the body said three
        assert!(matches!(action(&plan_of(&mut b, &f, None), "ftp"), ItemAction::Skip { reason: SkipKind::ListTruncated, .. }), "writing two would delete the third");
        set_count(&mut f, 1); // fewer than held is as inconsistent
        assert!(matches!(action(&plan_of(&mut b, &f, None), "ftp"), ItemAction::Skip { reason: SkipKind::ListTruncated, .. }));
        set_count(&mut f, 2); // the boundary: count == held
        assert!(matches!(action(&plan_of(&mut b, &f, None), "ftp"), ItemAction::Write { .. }));
    }

    #[test]
    fn a_live_ftp_list_read_only_in_part_is_not_replaced() {
        let (mut b, f) = drifted();
        b.ftp_count = Some(5); // the body says five, the read held one
        let a = action(&plan_of(&mut b, &f, None), "ftp").clone();
        assert!(matches!(a, ItemAction::Skip { reason: SkipKind::BodyUnreadable, .. }), "{a:?}");
        b.ftp_count = Some(1);
        assert!(matches!(action(&plan_of(&mut b, &f, None), "ftp"), ItemAction::Write { .. }));
    }

    #[test]
    fn iptc_slots_of_a_snapshot_that_read_only_a_prefix_are_each_still_restored() {
        // Slots are independent; unlike the FTP list there is no whole-list rebuild a prefix would shorten.
        let (mut b, mut f) = drifted();
        if let IptcState::Read(store) = &mut f.snap.extended.as_mut().unwrap().iptc {
            store.truncated = true;
            store.count = 40;
        }
        let plan = plan_of(&mut b, &f, None);
        assert!(matches!(action(&plan, "iptc:1"), ItemAction::Write { .. }), "{:?}", action(&plan, "iptc:1"));
    }

    #[test]
    fn a_slot_listed_twice_is_one_skipped_item_not_two_items_with_one_id() {
        let (mut b, mut f) = drifted();
        let dup = crate::extended::IptcSlotEntry { slot: 1, hex: hex_encode(&preset("dup", "x")) };
        if let IptcState::Read(store) = &mut f.snap.extended.as_mut().unwrap().iptc {
            store.slots.push(dup.clone());
        }
        let plan = plan_of(&mut b, &f, None);
        assert_eq!(plan.items.iter().filter(|i| i.id == "iptc:1").count(), 1, "one item per slot id");
        assert!(matches!(action(&plan, "iptc:1"), ItemAction::Skip { reason: SkipKind::RecordNotWritable, .. }));
        // Duplicates on the body's side: the same.
        let (mut b, f) = drifted();
        b.iptc.push(preset("dup", "x"));
        let mut g = f;
        if let IptcState::Read(store) = &mut g.snap.extended.as_mut().unwrap().iptc {
            store.slots.truncate(3);
        }
        // The body now lists slots 1..=4; make slot 1 appear twice in what it reports.
        struct DupBody(Body);
        impl ExtendedSource for DupBody {
            fn identity(&mut self) -> Result<ReadFrom, PtpUsbError> { self.0.identity() }
            fn prop_bytes(&mut self, c: u16) -> Result<Vec<u8>, PtpUsbError> { self.0.prop_bytes(c) }
            fn ftp_list(&mut self) -> Result<FtpListState, PtpUsbError> { self.0.ftp_list() }
            fn iptc_slots(&mut self, m: u32) -> Result<(u32, Vec<(u32, Vec<u8>)>), PtpUsbError> {
                let (n, mut slots) = self.0.iptc_slots(m)?;
                slots.push((1, preset("again", "y")));
                Ok((n, slots))
            }
        }
        let mut d = DupBody(b);
        let t = target(&d.0);
        let plan = build_extended_restore_plan(&mut d, &input(&g, &t, None)).unwrap();
        assert!(matches!(action(&plan, "iptc:1"), ItemAction::Skip { reason: SkipKind::RecordNotWritable, detail: Some(d) } if d.contains("the body lists slot 1 more than once")), "{:?}", action(&plan, "iptc:1"));
    }

    // ── reasons keep what was recorded ───────────────────────────────────

    #[test]
    fn a_skip_keeps_the_recorded_cause_instead_of_saying_only_that_a_value_is_missing() {
        let mut b = body("Z 9");
        let mut f = fixture(&mut b);
        {
            let e = f.snap.extended.as_mut().unwrap();
            e.props.get_mut("0xd073").unwrap().state = PropState::ReadFailed { tag: "usb_timeout".into(), detail: "d".into() };
            e.iptc = IptcState::ReadFailed { tag: "no_device".into(), detail: "d".into() };
        }
        let plan = plan_of(&mut b, &f, None);
        let detail = |id: &str| match action(&plan, id) {
            ItemAction::Skip { detail, .. } => detail.clone().unwrap(),
            other => panic!("{id}: {other:?}"),
        };
        assert!(detail("0xd073").contains("usb_timeout"), "{}", detail("0xd073"));
        assert!(detail("iptc").contains("no_device"), "{}", detail("iptc"));
        // The body's side too.
        let mut b2 = body("Z 9");
        let f2 = fixture(&mut b2);
        b2.props.remove(&0xD073); // the read fails with a response that is not DevicePropNotSupported
        b2.props.insert(0xD072, ptp_string("drifted").unwrap());
        let plan2 = plan_of(&mut b2, &f2, None);
        let ItemAction::Skip { detail: Some(d), reason: SkipKind::BodyLacksIt } = action(&plan2, "0xd073") else { panic!("{:?}", action(&plan2, "0xd073")) };
        assert!(d.contains("DevicePropNotSupported"), "{d}");
    }

    #[test]
    fn an_iptc_slot_that_is_not_a_preset_says_which_side_and_why() {
        let mut b = body("Z 9");
        let mut f = fixture(&mut b);
        if let IptcState::Read(store) = &mut f.snap.extended.as_mut().unwrap().iptc {
            store.slots[2].hex = hex_encode(&[1, 0, 0, 0, 0xC9, 0, 2, 0, 0, 0, 0, 0, 7, 7, 7, 7]); // another kind
            store.slots[1].hex = "0102".into(); // too short to decode
        }
        let plan = plan_of(&mut b, &f, None);
        let detail = |id: &str| match action(&plan, id) {
            ItemAction::Skip { reason: SkipKind::RecordNotWritable, detail } => detail.clone().unwrap(),
            other => panic!("{id}: {other:?}"),
        };
        let kind2 = detail("iptc:3");
        assert!(kind2.contains("snapshot") && kind2.contains("kind 2") && !kind2.contains("does not decode"), "{kind2}");
        let short = detail("iptc:2");
        assert!(short.contains("snapshot") && short.contains("does not decode"), "{short}");
    }

    // ── the planner and the session's own gates ──────────────────────────

    #[test]
    fn the_planner_files_a_gate_refusal_by_the_errors_variant() {
        assert!(matches!(skip_of_gate_refusal(&PtpUsbError::UnconfirmedModel("Z 8".into())), ItemAction::Skip { reason: SkipKind::ModelNotConfirmed, .. }));
        assert!(matches!(skip_of_gate_refusal(&PtpUsbError::PropWriteUnconfirmed { propcode: 0xD072, model: "Z 9".into() }), ItemAction::Skip { reason: SkipKind::ModelNotConfirmed, .. }));
        assert!(matches!(skip_of_gate_refusal(&PtpUsbError::PropValueBad { propcode: 1, why: "w".into() }), ItemAction::Skip { reason: SkipKind::ValueNotWritable, .. }));
    }

    #[test]
    fn the_planner_agrees_with_the_session_gates_over_the_model_by_value_grid() {
        // Direct (no session): the planner's Write/Skip for a string and a byte is exactly "the gate helper accepts".
        // (`ptp_usb::tests` runs the same grid against a scripted session.)
        let long_ok = "x".repeat(254);
        let long_bad = "x".repeat(255);
        for model in ["Z 9", "Z6_3", "Z 5", "Z 8"] {
            for text in ["ok", "é", "\u{ff}", "\u{100}", "a\tb", long_ok.as_str(), long_bad.as_str()] {
                let mut live = body(model);
                let mut f = fixture(&mut body(model));
                live.props.insert(0xD072, ptp_string("live").unwrap());
                f.snap.extended.as_mut().unwrap().props.get_mut("0xd072").unwrap().state = PropState::Text { text: text.into() };
                let plan = plan_of(&mut live, &f, None);
                let wrote = matches!(action(&plan, "0xd072"), ItemAction::Write { .. });
                assert_eq!(wrote, check_string_property_write(model, 0xD072, text, false).is_ok(), "{model} {text:?}");
            }
            for &(code, _, max) in &BYTE_PROPS {
                for value in [0u8, max, max + 1] {
                    let mut live = body(model);
                    let mut f = fixture(&mut body(model));
                    live.props.insert(code, vec![if value == 0 { 1 } else { 0 }]);
                    f.snap.extended.as_mut().unwrap().props.get_mut(&prop_key(code)).unwrap().state = PropState::Bytes { hex: hex_encode(&[value]) };
                    let plan = plan_of(&mut live, &f, None);
                    let wrote = matches!(action(&plan, &prop_key(code)), ItemAction::Write { .. });
                    assert_eq!(wrote, check_byte_property_write(model, code, value, false).is_ok(), "{model} {code:#x} {value}");
                }
            }
        }
    }

    // ── identity ──────────────────────────────────────────────────────────

    #[test]
    fn a_snapshot_with_an_id_n_serial_is_judged_by_the_serial_its_extended_section_was_read_from() {
        let (mut b, mut f) = drifted();
        f.snap.camera.serial = "id-1".into(); // the handle of one session: identifies no body
        f.snap.extended.as_mut().unwrap().read_from.usb_serial = "0000001".into();
        let t = target(&b);
        assert!(build_extended_restore_plan(&mut b, &input(&f, &t, None)).is_ok(), "the same body");
        b.serial = "0000002".into();
        assert_eq!(build_extended_restore_plan(&mut b, &input(&f, &t, None)).unwrap_err().tag(), "not_this_body", "another body of the model");
        // No evidence either way (the section has no serial): nothing to compare, as for an empty serial.
        f.snap.extended.as_mut().unwrap().read_from.usb_serial = String::new();
        assert!(build_extended_restore_plan(&mut b, &input(&f, &t, None)).is_ok());
        // An EMPTY camera serial is no evidence even when the section has a serial (the existing contract).
        f.snap.camera.serial = String::new();
        f.snap.extended.as_mut().unwrap().read_from.usb_serial = "0000001".into();
        assert!(build_extended_restore_plan(&mut b, &input(&f, &t, None)).is_ok());
        // A real serial on the snapshot still wins over what the section says.
        f.snap.camera.serial = "0000009".into();
        f.snap.extended.as_mut().unwrap().read_from.usb_serial = "0000002".into();
        assert_eq!(build_extended_restore_plan(&mut b, &input(&f, &t, None)).unwrap_err().tag(), "not_this_body");
    }

    // ── the read after the writes ─────────────────────────────────────────

    #[test]
    fn iptc_presets_that_cannot_be_read_after_the_writes_leave_the_apply_unverified() {
        let (mut b, f) = drifted();
        b.iptc_unreadable_after_write = true;
        let report = apply(&mut b, &f, None).unwrap();
        assert_eq!(report.status, ApplyStatus::AppliedUnverified, "{report:?}");
        assert!(report.results.iter().all(|r| matches!(r.outcome, ItemOutcome::Written { verified: true })), "every write's own read-back was fine");
        let why = report.after_read_failed.as_deref().expect("the report says what could not be read");
        assert!(why.contains("IPTC preset 1"), "{why}");
        assert!(report.still_differing.is_empty(), "unreadable is not 'still differing': {:?}", report.still_differing);
    }

    #[test]
    fn an_ftp_list_that_cannot_be_read_after_the_rebuild_leaves_the_apply_unverified_not_differing() {
        let (mut b, f) = drifted();
        b.ftp_unreadable_after_write = true;
        let report = apply(&mut b, &f, None).unwrap();
        assert_eq!(report.status, ApplyStatus::AppliedUnverified, "{report:?}");
        assert!(report.after_read_failed.as_deref().unwrap().contains("FTP profiles"));
        assert!(report.still_differing.is_empty(), "{:?}", report.still_differing);
    }
}

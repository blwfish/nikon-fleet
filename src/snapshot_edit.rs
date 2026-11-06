//! Editing a copy of a stored snapshot.
//!
//! A snapshot read from a camera is a record and is never modified. To change a setting in what will later be
//! restored to a body, make a copy ([`copy_for_edit`]): the copy carries [`DerivedFrom`] (where it came from and
//! everything changed in it), which is what makes it editable and what marks it as edited everywhere it is
//! listed. Every edit function refuses a snapshot without it, so a captured snapshot and a reference cannot be
//! edited by a caller that forgot to check.
//!
//! What is checked here is what the snapshot itself can answer: an enumeration's value must be one the snapshot
//! lists, a stepped range's value must be on its grid, an owner string must be Latin-1 and within the length the
//! cameras were measured to hold, a switch within the range the write takes. What only the camera can answer
//! (whether the property is writable now, whether this firmware accepts the value) is answered when a restore
//! is planned; nothing here claims it.
//!
//! Edits are recorded in `derived_from.edits`, oldest first, without any password or key.
//!
//! Not here yet: changing the fields of an FTP profile, adding one, or duplicating one. Those need a writer
//! that can produce a profile blob from fields, and the existing encoder fixes the server port, folder and
//! protocol; deleting and reordering profiles need no encoder and are here.

use serde_json::{Number, Value};
use thiserror::Error;

use crate::code_policy::never_copy_table;
use crate::extended::{EXTENDED_PROPS, Extended, FtpState, IptcState, PropKind, PropState, hex_decode, hex_encode, prop_key};
use crate::iptc::{IPTC_FIELDS, IptcPreset, IptcRecord, encode_iptc_preset};
use crate::ops::restore::resolve_only;
use crate::ptp_usb::{BYTE_PROPS, decode_ftp_profile, encode_owner_string};
use crate::snapshot::{DerivedFrom, EditRecord, PropertyEntry, Snapshot};
use crate::value_shape::{ValueKind, has_no_span, is_stepped_range, range_scale, value_at, value_kind};

/// The most characters each owner string held on the bodies it was measured on (Z 9, Z6III, Z 5, 2026-11-06: one more
/// was refused by the camera with `0x201B` and left the value unchanged). The write path leaves this to the camera; an
/// edit stops it earlier.
pub const OWNER_STRING_MAX_CHARS: [(u16, usize); 3] = [(0xD072, 36), (0xD073, 54), (0xD090, 36)];

/// A step is accepted when it is this close to a grid point of a stepped range, as a fraction of the step: close
/// enough that "-0.33" means the third of an EV the camera calls -0.3333, far enough from the next point that two
/// grid points are never confused.
const GRID_TOLERANCE: f64 = 0.05;

#[derive(Debug, Error)]
pub enum EditRefusal {
    #[error("this snapshot was read from a camera and is never edited: make an edited copy of it and edit that")]
    NotACopy,
    #[error("the snapshot has no property {name:?}")]
    NoSuchProperty { name: String },
    #[error("{name} is never copied to a camera ({why}), so editing it would change nothing that can be restored")]
    NeverCopied { name: String, why: String },
    #[error("{name} holds a value of kind {kind} (not a choice, a number on a scale, a number or a switch), which cannot be edited here")]
    ShapeNotEditable { name: String, kind: ValueKind },
    #[error("{name}: {why}")]
    ValueNotAllowed { name: String, why: String },
    #[error("the snapshot has no extended section (owner strings, IPTC presets, FTP profiles): it was taken before they were captured, or without them")]
    NoExtendedSection,
    #[error("{item:?} is not one of the owner items; known: {known}")]
    UnknownItem { item: String, known: String },
    #[error("{item}: {why}")]
    ItemUnavailable { item: String, why: String },
    #[error("{item}: {why}")]
    BadText { item: String, why: String },
    #[error("{item}: {why}")]
    BadValue { item: String, why: String },
    #[error("the snapshot has no IPTC slot {slot}")]
    NoSuchSlot { slot: u32 },
    #[error("IPTC slot {slot} is {why}, not a preset this program can edit")]
    NotAPreset { slot: u32, why: String },
    #[error("{field:?} is not an IPTC field; known: name, {known}")]
    NoSuchField { field: String, known: String },
    #[error("FTP profiles: {why}")]
    FtpUnavailable { why: String },
    #[error("FTP profile {index} does not exist: the list has {len}")]
    FtpIndex { index: usize, len: usize },
}

impl EditRefusal {
    /// Closed machine-readable tag, for structured output. Exhaustive match.
    pub fn tag(&self) -> &'static str {
        match self {
            EditRefusal::NotACopy => "not_a_copy",
            EditRefusal::NoSuchProperty { .. } => "no_such_property",
            EditRefusal::NeverCopied { .. } => "never_copied",
            EditRefusal::ShapeNotEditable { .. } => "shape_not_editable",
            EditRefusal::ValueNotAllowed { .. } => "value_not_allowed",
            EditRefusal::NoExtendedSection => "no_extended_section",
            EditRefusal::UnknownItem { .. } => "unknown_item",
            EditRefusal::ItemUnavailable { .. } => "item_unavailable",
            EditRefusal::BadText { .. } => "bad_text",
            EditRefusal::BadValue { .. } => "bad_value",
            EditRefusal::NoSuchSlot { .. } => "no_such_slot",
            EditRefusal::NotAPreset { .. } => "not_a_preset",
            EditRefusal::NoSuchField { .. } => "no_such_field",
            EditRefusal::FtpUnavailable { .. } => "ftp_unavailable",
            EditRefusal::FtpIndex { .. } => "ftp_index",
        }
    }
}

/// What an edit did. `changed: false` is an edit to the value the snapshot already holds: nothing is written or
/// recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub changed: bool,
    /// The stable key recorded in the edit log.
    pub item: String,
    /// For a person: what it was and what it is.
    pub change: String,
}

// ── making a copy ────────────────────────────────────────────────────────

/// A copy of `source` that may be edited. It keeps the source's `captured_at` (the values were read then) and
/// transport, gets a label (`label`, or `edit-` and the source's), and carries the source's own edit history when
/// the source was itself an edited copy. `source_file` is the source's file name, and `now` an RFC 3339 time.
pub fn copy_for_edit(source: &Snapshot, source_file: &str, label: Option<&str>, now: &str) -> Snapshot {
    let mut copy = source.clone();
    let label = match label.map(str::trim).filter(|l| !l.is_empty()) {
        Some(l) => l.to_string(),
        None => {
            let base = source.label.as_deref().unwrap_or("snap");
            format!("edit-{}", base.strip_prefix("edit-").unwrap_or(base))
        }
    };
    copy.label = Some(label);
    copy.derived_from = Some(DerivedFrom {
        source_file: source_file.to_string(),
        source_captured_at: source.captured_at.clone(),
        source_label: source.label.clone(),
        created_at: now.to_string(),
        last_edited_at: now.to_string(),
        edits: source.derived_from.as_ref().map(|d| d.edits.clone()).unwrap_or_default(),
    });
    copy
}

/// The guard every edit starts with.
pub fn ensure_editable(snap: &Snapshot) -> Result<(), EditRefusal> {
    if snap.derived_from.is_some() { Ok(()) } else { Err(EditRefusal::NotACopy) }
}

fn record(snap: &mut Snapshot, now: &str, item: &str, change: &str) {
    if let Some(d) = snap.derived_from.as_mut() {
        d.edits.push(EditRecord { at: now.to_string(), item: item.to_string(), change: change.to_string() });
        d.last_edited_at = now.to_string();
    }
}

/// Record `outcome` if it changed anything, and return it.
fn finish(snap: &mut Snapshot, now: &str, outcome: Applied) -> Applied {
    if outcome.changed {
        record(snap, now, &outcome.item, &outcome.change);
    }
    outcome
}

fn num(f: f64) -> String {
    let s = format!("{f:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

// ── properties ───────────────────────────────────────────────────────────

fn entry<'a>(snap: &'a Snapshot, name: &str) -> Result<&'a PropertyEntry, EditRefusal> {
    let e = snap.properties.get(name).ok_or_else(|| EditRefusal::NoSuchProperty { name: name.to_string() })?;
    if let Some((_, _, why)) = never_copy_table().into_iter().find(|(code, ..)| *code == e.code) {
        return Err(EditRefusal::NeverCopied { name: name.to_string(), why: why.to_string() });
    }
    Ok(e)
}

fn not_allowed(name: &str, why: String) -> EditRefusal {
    EditRefusal::ValueNotAllowed { name: name.to_string(), why }
}

/// Choose, for an enumeration, the value `value` (one of the values the snapshot lists for it; not an index).
pub fn set_enum_value(snap: &mut Snapshot, name: &str, value: i64, now: &str) -> Result<Applied, EditRefusal> {
    ensure_editable(snap)?;
    let e = entry(snap, name)?;
    if value_kind(&e.value) != ValueKind::Enum {
        return Err(EditRefusal::ShapeNotEditable { name: name.to_string(), kind: value_kind(&e.value) });
    }
    let values: Vec<Option<i64>> = e.value.get("values").and_then(Value::as_array).map(|a| a.iter().map(Value::as_i64).collect()).unwrap_or_default();
    let Some(position) = values.iter().position(|v| *v == Some(value)) else {
        let listed = values.iter().flatten().map(i64::to_string).collect::<Vec<_>>().join(", ");
        return Err(not_allowed(name, format!("{value} is not one of the values it offers: {listed}")));
    };
    let old_index = e.value.get("value_index").and_then(Value::as_u64);
    let old_value = old_index.and_then(|i| values.get(i as usize).copied().flatten());
    let item = format!("property:{name}");
    if old_index == Some(position as u64) {
        return Ok(Applied { changed: false, item, change: format!("already {value}") });
    }
    let change = format!("{}: {} -> {value}", short(name), old_value.map_or("(no current value)".to_string(), |v| v.to_string()));
    if let Some(o) = snap.properties.get_mut(name).and_then(|p| p.value.as_object_mut()) {
        o.insert("value_index".into(), Value::from(position as u64));
    }
    Ok(finish(snap, now, Applied { changed: true, item, change }))
}

/// Put a range on `value`. A stepped range takes the grid point nearest `value` if it is within
/// [`GRID_TOLERANCE`] of a step of it, and moves `value_index` (the SDK leaves `value` unused on these); a
/// continuous range takes `value` itself, within `lower..=upper`.
pub fn set_range_value(snap: &mut Snapshot, name: &str, value: f64, now: &str) -> Result<Applied, EditRefusal> {
    ensure_editable(snap)?;
    let e = entry(snap, name)?;
    if value_kind(&e.value) != ValueKind::Range {
        return Err(EditRefusal::ShapeNotEditable { name: name.to_string(), kind: value_kind(&e.value) });
    }
    if !value.is_finite() {
        return Err(not_allowed(name, "that is not a number".to_string()));
    }
    let (lower, upper, steps) = range_scale(&e.value).ok_or_else(|| not_allowed(name, "the range has no usable lower, upper and steps".to_string()))?;
    if has_no_span(&e.value) {
        return Err(not_allowed(name, "the range holds a single number at both ends; there is no choice to make".to_string()));
    }
    let item = format!("property:{name}");
    if is_stepped_range(&e.value) {
        let (lo, hi) = (lower.min(upper), lower.max(upper));
        if value < lo - 1e-9 || value > hi + 1e-9 {
            return Err(not_allowed(name, format!("{} is outside {} to {}", num(value), num(lo), num(hi))));
        }
        let step = (upper - lower) / (steps as f64 - 1.0);
        // Nearest grid point. The bounds check above keeps `value` within a billionth of the scale, far less than half a
        // step, so the rounded index is between 0 and steps - 1 (pinned by a value a hair past each end).
        let index = ((value - lower) / step).round();
        let snapped = value_at(lower, upper, steps, index as u64);
        if (snapped - value).abs() > GRID_TOLERANCE * step.abs() {
            let near = |i: f64| num(value_at(lower, upper, steps, i.clamp(0.0, (steps - 1) as f64) as u64));
            return Err(not_allowed(
                name,
                format!("{} is not on its scale ({} to {} in {steps} steps of {}); nearest are {} and {}", num(value), num(lower), num(upper), num(step.abs()), near(index.floor()), near(index.ceil())),
            ));
        }
        let old_index = e.value.get("value_index").and_then(Value::as_u64);
        let old = old_index.filter(|i| *i < steps).map(|i| num(value_at(lower, upper, steps, i)));
        if old_index == Some(index as u64) {
            return Ok(Applied { changed: false, item, change: format!("already {}", num(snapped)) });
        }
        let change = format!("{}: {} -> {}", short(name), old.unwrap_or_else(|| "(no current value)".to_string()), num(snapped));
        if let Some(o) = snap.properties.get_mut(name).and_then(|p| p.value.as_object_mut()) {
            o.insert("value_index".into(), Value::from(index as u64));
        }
        return Ok(finish(snap, now, Applied { changed: true, item, change }));
    }
    let (lo, hi) = (lower.min(upper), lower.max(upper));
    if value < lo || value > hi {
        return Err(not_allowed(name, format!("{} is outside {} to {}", num(value), num(lo), num(hi))));
    }
    let old = e.value.get("value").and_then(Value::as_f64);
    if old == Some(value) {
        return Ok(Applied { changed: false, item, change: format!("already {}", num(value)) });
    }
    let change = format!("{}: {} -> {}", short(name), old.map_or("(no current value)".to_string(), num), num(value));
    let n = Number::from_f64(value).ok_or_else(|| not_allowed(name, "that is not a number".to_string()))?;
    if let Some(o) = snap.properties.get_mut(name).and_then(|p| p.value.as_object_mut()) {
        o.insert("value".into(), Value::Number(n));
    }
    Ok(finish(snap, now, Applied { changed: true, item, change }))
}

/// Set a plain number. An integer stays an integer: `text` must then parse as one.
pub fn set_number(snap: &mut Snapshot, name: &str, text: &str, now: &str) -> Result<Applied, EditRefusal> {
    ensure_editable(snap)?;
    let e = entry(snap, name)?;
    let Value::Number(old) = &e.value else {
        return Err(EditRefusal::ShapeNotEditable { name: name.to_string(), kind: value_kind(&e.value) });
    };
    let new = if old.is_i64() || old.is_u64() {
        let i: i64 = text.trim().parse().map_err(|_| not_allowed(name, format!("{text:?} is not a whole number, which is what it holds ({old})")))?;
        Number::from(i)
    } else {
        let f: f64 = text.trim().parse().map_err(|_| not_allowed(name, format!("{text:?} is not a number")))?;
        Number::from_f64(f).ok_or_else(|| not_allowed(name, format!("{text:?} is not a finite number")))?
    };
    let item = format!("property:{name}");
    if *old == new {
        return Ok(Applied { changed: false, item, change: format!("already {new}") });
    }
    let change = format!("{}: {old} -> {new}", short(name));
    if let Some(p) = snap.properties.get_mut(name) {
        p.value = Value::Number(new);
    }
    Ok(finish(snap, now, Applied { changed: true, item, change }))
}

/// Set a switch held as a boolean: `true`, `false`, `1` or `0`.
pub fn set_bool(snap: &mut Snapshot, name: &str, text: &str, now: &str) -> Result<Applied, EditRefusal> {
    ensure_editable(snap)?;
    let e = entry(snap, name)?;
    let Value::Bool(old) = &e.value else {
        return Err(EditRefusal::ShapeNotEditable { name: name.to_string(), kind: value_kind(&e.value) });
    };
    let new = match text.trim() {
        "true" | "1" => true,
        "false" | "0" => false,
        other => return Err(not_allowed(name, format!("{other:?} is not true, false, 1 or 0"))),
    };
    let item = format!("property:{name}");
    if *old == new {
        return Ok(Applied { changed: false, item, change: format!("already {new}") });
    }
    let change = format!("{}: {old} -> {new}", short(name));
    if let Some(p) = snap.properties.get_mut(name) {
        p.value = Value::Bool(new);
    }
    Ok(finish(snap, now, Applied { changed: true, item, change }))
}

/// Set a property from text, by what it holds: a choice (its value), a position on a scale, a number or a switch.
/// `requested` is the name as `restore --only` takes it (full, or without the `kNkMAIDCapability_` prefix). Returns
/// the full name with the outcome.
pub fn set_property_text(snap: &mut Snapshot, requested: &str, text: &str, now: &str) -> Result<(String, Applied), EditRefusal> {
    ensure_editable(snap)?;
    let name = resolve_only(&[requested.to_string()], &snap.properties)
        .ok()
        .and_then(|s| s.into_iter().next())
        .ok_or_else(|| EditRefusal::NoSuchProperty { name: requested.to_string() })?;
    let kind = value_kind(&entry(snap, &name)?.value);
    let applied = match kind {
        ValueKind::Enum => {
            let v: i64 = text.trim().parse().map_err(|_| not_allowed(&name, format!("{text:?} is not a whole number; a choice is named by its value")))?;
            set_enum_value(snap, &name, v, now)?
        }
        ValueKind::Range => {
            let v: f64 = text.trim().parse().map_err(|_| not_allowed(&name, format!("{text:?} is not a number")))?;
            set_range_value(snap, &name, v, now)?
        }
        ValueKind::Number => set_number(snap, &name, text, now)?,
        ValueKind::Bool => set_bool(snap, &name, text, now)?,
        other => return Err(EditRefusal::ShapeNotEditable { name, kind: other }),
    };
    Ok((name, applied))
}

fn short(name: &str) -> &str {
    name.strip_prefix(crate::transplant::PROPERTY_PREFIX).unwrap_or(name)
}

// ── owner strings and switches ───────────────────────────────────────────

/// The code of an owner item given by its name ("artist", "Image comment", "enable-copyright", "iptc-preset-select",
/// case, spaces, hyphens and underscores ignored) or by its code ("0xD072").
pub fn resolve_extended_item(text: &str) -> Result<u16, EditRefusal> {
    let fold = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
    let wanted = fold(text);
    if let Some(&(code, ..)) = EXTENDED_PROPS.iter().find(|(_, name, _)| fold(name) == wanted) {
        return Ok(code);
    }
    // A shorter name people use for the longest one.
    let aliases: [(&str, u16); 2] = [("comment", 0xD090), ("iptcpreset", 0xD060)];
    if let Some(&(_, code)) = aliases.iter().find(|(a, _)| *a == wanted) {
        return Ok(code);
    }
    if let Ok(code) = crate::ptp_usb::parse_propcode(text) {
        if EXTENDED_PROPS.iter().any(|(c, ..)| *c == code) {
            return Ok(code);
        }
    }
    Err(EditRefusal::UnknownItem {
        item: text.to_string(),
        known: EXTENDED_PROPS.iter().map(|(c, n, _)| format!("{n} ({c:#06x})")).collect::<Vec<_>>().join(", "),
    })
}

fn extended_mut(snap: &mut Snapshot) -> Result<&mut Extended, EditRefusal> {
    snap.extended.as_mut().ok_or(EditRefusal::NoExtendedSection)
}

fn ext_name(code: u16) -> &'static str {
    EXTENDED_PROPS.iter().find(|(c, ..)| *c == code).map_or("?", |(_, n, _)| *n)
}

/// Set Artist, Copyright or Image comment. The text must be one the camera holds as written (Latin-1, no control
/// characters) and within [`OWNER_STRING_MAX_CHARS`].
pub fn set_owner_string(snap: &mut Snapshot, code: u16, text: &str, now: &str) -> Result<Applied, EditRefusal> {
    ensure_editable(snap)?;
    let name = ext_name(code);
    let kind = EXTENDED_PROPS.iter().find(|(c, ..)| *c == code).map(|(_, _, k)| *k);
    let Some(&(_, max)) = OWNER_STRING_MAX_CHARS.iter().find(|(c, _)| *c == code).filter(|_| kind == Some(PropKind::Text)) else {
        return Err(EditRefusal::UnknownItem {
            item: format!("{code:#06x}"),
            known: OWNER_STRING_MAX_CHARS.iter().map(|(c, _)| format!("{} ({c:#06x})", ext_name(*c))).collect::<Vec<_>>().join(", "),
        });
    };
    if let Err(why) = encode_owner_string(text) {
        return Err(EditRefusal::BadText { item: name.to_string(), why });
    }
    let chars = text.chars().count();
    if chars > max {
        return Err(EditRefusal::BadText {
            item: name.to_string(),
            why: format!("{chars} characters; the cameras this was measured on (Z 9, Z6III, Z 5) hold at most {max}"),
        });
    }
    let ext = extended_mut(snap)?;
    let prop = ext
        .props
        .get_mut(&prop_key(code))
        .ok_or_else(|| EditRefusal::ItemUnavailable { item: name.to_string(), why: "the snapshot does not record it".to_string() })?;
    let before = match &prop.state {
        PropState::Text { text } => format!("{text:?}"),
        PropState::NotSupported => {
            return Err(EditRefusal::ItemUnavailable { item: name.to_string(), why: "the body this was read from answered that it does not have this property".to_string() });
        }
        PropState::Undecodable { .. } | PropState::ReadFailed { .. } => "(not read)".to_string(),
        PropState::Bytes { .. } => {
            return Err(EditRefusal::ItemUnavailable { item: name.to_string(), why: "it is recorded as bytes, not text".to_string() });
        }
    };
    let item = format!("string:{}", prop_key(code));
    if matches!(&prop.state, PropState::Text { text: t } if t == text) {
        return Ok(Applied { changed: false, item, change: format!("already {text:?}") });
    }
    prop.state = PropState::Text { text: text.to_string() };
    let change = format!("{name}: {before} -> {text:?}");
    Ok(finish(snap, now, Applied { changed: true, item, change }))
}

/// Set EnableCopyright (0 or 1) or the IPTC preset select (0 to the highest preset). The limits are the write's own
/// ([`BYTE_PROPS`]).
pub fn set_switch(snap: &mut Snapshot, code: u16, value: u8, now: &str) -> Result<Applied, EditRefusal> {
    ensure_editable(snap)?;
    let name = ext_name(code);
    let Some(&(_, _, max)) = BYTE_PROPS.iter().find(|(c, ..)| *c == code) else {
        return Err(EditRefusal::UnknownItem {
            item: format!("{code:#06x}"),
            known: BYTE_PROPS.iter().map(|(c, n, _)| format!("{n} ({c:#06x})")).collect::<Vec<_>>().join(", "),
        });
    };
    if value > max {
        return Err(EditRefusal::BadValue { item: name.to_string(), why: format!("{value} is above the highest value it takes, {max}") });
    }
    let ext = extended_mut(snap)?;
    let prop = ext
        .props
        .get_mut(&prop_key(code))
        .ok_or_else(|| EditRefusal::ItemUnavailable { item: name.to_string(), why: "the snapshot does not record it".to_string() })?;
    let before = match &prop.state {
        PropState::Bytes { hex } if hex.len() == 2 => hex.clone(),
        PropState::Bytes { hex } => {
            return Err(EditRefusal::ItemUnavailable { item: name.to_string(), why: format!("it holds {hex:?}, not one byte") });
        }
        PropState::NotSupported => {
            return Err(EditRefusal::ItemUnavailable { item: name.to_string(), why: "the body this was read from answered that it does not have this property".to_string() });
        }
        PropState::ReadFailed { .. } | PropState::Undecodable { .. } => "(not read)".to_string(),
        PropState::Text { .. } => {
            return Err(EditRefusal::ItemUnavailable { item: name.to_string(), why: "it is recorded as text, not a byte".to_string() });
        }
    };
    let item = format!("switch:{}", prop_key(code));
    let new_hex = format!("{value:02x}");
    if before == new_hex {
        return Ok(Applied { changed: false, item, change: format!("already {value}") });
    }
    prop.state = PropState::Bytes { hex: new_hex };
    let was = u8::from_str_radix(&before, 16).map_or(before.clone(), |b| b.to_string());
    let change = format!("{name}: {was} -> {value}");
    Ok(finish(snap, now, Applied { changed: true, item, change }))
}

// ── IPTC ─────────────────────────────────────────────────────────────────

fn fold_field(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Set one field of an IPTC preset (`field` is "name" for the preset's own name, or one of [`IPTC_FIELDS`], case and
/// punctuation ignored). The camera's limits on each field's length are not known, so none is applied here: a
/// restore says if the camera refuses one.
pub fn set_iptc_field(snap: &mut Snapshot, slot: u32, field: &str, text: &str, now: &str) -> Result<Applied, EditRefusal> {
    ensure_editable(snap)?;
    let wanted = fold_field(field);
    let index: Option<usize> = if wanted == "name" { None } else { Some(IPTC_FIELDS.iter().position(|f| fold_field(f) == wanted).ok_or_else(|| EditRefusal::NoSuchField { field: field.to_string(), known: IPTC_FIELDS.join(", ") })?) };
    let field_name = index.map_or("name", |i| IPTC_FIELDS[i]);
    let ext = extended_mut(snap)?;
    let store = match &mut ext.iptc {
        IptcState::Read(store) => store,
        IptcState::NotRead => return Err(EditRefusal::ItemUnavailable { item: "IPTC".into(), why: "the snapshot was taken before IPTC presets were captured".into() }),
        IptcState::NotOnModel => return Err(EditRefusal::ItemUnavailable { item: "IPTC".into(), why: "this model has no IPTC presets".into() }),
        IptcState::ReadFailed { tag, .. } => return Err(EditRefusal::ItemUnavailable { item: "IPTC".into(), why: format!("reading them failed ({tag})") }),
    };
    let slot_entry = store.slots.iter_mut().find(|s| s.slot == slot).ok_or(EditRefusal::NoSuchSlot { slot })?;
    let mut preset: IptcPreset = match slot_entry.decode() {
        Ok(IptcRecord::Preset(p)) => p,
        Ok(IptcRecord::Other { kind, .. }) => return Err(EditRefusal::NotAPreset { slot, why: format!("a record of kind {kind}") }),
        Err(why) => return Err(EditRefusal::NotAPreset { slot, why: format!("unreadable ({why})") }),
    };
    let before = match index {
        None => preset.name.clone(),
        Some(i) => preset.fields[i].clone(),
    };
    let item = format!("iptc:{slot}:{}", fold_field(field_name));
    if before == text {
        return Ok(Applied { changed: false, item, change: format!("already {text:?}") });
    }
    match index {
        None => preset.name = text.to_string(),
        Some(i) => preset.fields[i] = text.to_string(),
    }
    let bytes = encode_iptc_preset(&preset).map_err(|why| EditRefusal::BadText { item: format!("IPTC slot {slot} {field_name}"), why })?;
    slot_entry.hex = hex_encode(&bytes);
    let change = format!("IPTC slot {slot} {field_name}: {before:?} -> {text:?}");
    Ok(finish(snap, now, Applied { changed: true, item, change }))
}

// ── FTP profile list ─────────────────────────────────────────────────────

fn ftp_store_mut(snap: &mut Snapshot) -> Result<&mut crate::extended::FtpStore, EditRefusal> {
    let ext = extended_mut(snap)?;
    match &mut ext.ftp {
        FtpState::Read(store) if store.truncated => Err(EditRefusal::FtpUnavailable {
            why: "the snapshot read only part of the list (the camera reported more than were read), so the rest cannot be rearranged".to_string(),
        }),
        FtpState::Read(store) => Ok(store),
        FtpState::NotOnModel => Err(EditRefusal::FtpUnavailable { why: "this model has no FTP profiles".to_string() }),
        FtpState::ReadFailed { tag, .. } => Err(EditRefusal::FtpUnavailable { why: format!("reading them failed ({tag})") }),
    }
}

/// A profile's name for the log, never its password: its stored name, or its position if the blob does not decode.
fn profile_label(hex: &str, index: usize) -> String {
    match hex_decode(hex).ok().and_then(|b| decode_ftp_profile(&b).ok()) {
        Some(p) => format!("{:?}", p.profile_name),
        None => format!("#{index} (undecodable)"),
    }
}

/// Bring the list's own bookkeeping in line with its profiles: slot numbers in order, `count`, and an active slot that
/// exists (none when the list is empty).
fn renumber(store: &mut crate::extended::FtpStore) {
    for (i, p) in store.profiles.iter_mut().enumerate() {
        p.slot = i as u32;
    }
    store.count = store.profiles.len() as u32;
    store.active_slot = match store.active_slot {
        _ if store.profiles.is_empty() => None,
        Some(s) if (s as usize) < store.profiles.len() => Some(s),
        Some(_) => Some(0),
        None => None,
    };
}

/// Remove the profile at `index` (0 is the front of the list, the one the camera uses first).
pub fn ftp_delete(snap: &mut Snapshot, index: usize, now: &str) -> Result<Applied, EditRefusal> {
    ensure_editable(snap)?;
    let store = ftp_store_mut(snap)?;
    if index >= store.profiles.len() {
        return Err(EditRefusal::FtpIndex { index, len: store.profiles.len() });
    }
    let removed = store.profiles.remove(index);
    renumber(store);
    let change = format!("FTP profile {} removed from position {index}; {} left", profile_label(&removed.hex, index), store.profiles.len());
    Ok(finish(snap, now, Applied { changed: true, item: "ftp:delete".to_string(), change }))
}

/// Move the profile at `from` so that it ends up at `to` (the others shift to make room). The camera cannot reorder
/// its list; restoring a reordered list rebuilds it whole.
pub fn ftp_move(snap: &mut Snapshot, from: usize, to: usize, now: &str) -> Result<Applied, EditRefusal> {
    ensure_editable(snap)?;
    let store = ftp_store_mut(snap)?;
    let len = store.profiles.len();
    for index in [from, to] {
        if index >= len {
            return Err(EditRefusal::FtpIndex { index, len });
        }
    }
    if from == to {
        return Ok(Applied { changed: false, item: "ftp:move".to_string(), change: format!("already at position {to}") });
    }
    let moved = store.profiles.remove(from);
    let label = profile_label(&moved.hex, from);
    store.profiles.insert(to, moved);
    renumber(store);
    let change = format!("FTP profile {label} moved from position {from} to {to}");
    Ok(finish(snap, now, Applied { changed: true, item: "ftp:move".to_string(), change }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extended::{ExtendedProp, FtpProfileEntry, FtpStore, IptcSlotEntry, IptcStore, ReadFrom};
    use crate::iptc::encode_iptc_preset;
    use crate::ptp_usb::{FtpProfile, encode_ftp_profile};
    use crate::snapshot::{Camera, Transport};
    use serde_json::json;

    const NOW: &str = "2026-11-06T12:00:00Z";
    const LATER: &str = "2026-11-06T12:05:00Z";

    fn profile_hex(name: &str) -> String {
        hex_encode(
            &encode_ftp_profile(&FtpProfile {
                profile_name: name.into(),
                ssid_24ghz: "net".into(),
                ssid_5ghz: "key-secret-12345".into(),
                host: "10.0.0.2".into(),
                username: "fleet".into(),
                password: "hunter2-secret".into(),
                port: 8080,
            })
            .unwrap(),
        )
    }

    fn preset_hex(name: &str, headline: &str) -> String {
        let mut fields = vec![String::new(); 14];
        fields[2] = headline.into();
        hex_encode(&encode_iptc_preset(&IptcPreset { name: name.into(), fields }).unwrap())
    }

    /// A captured snapshot with one property of each editable shape and a full extended section.
    fn captured() -> Snapshot {
        let mut s = Snapshot::new(Camera { model: "Z 9".into(), serial: "S1".into(), firmware: "5.31".into() }, Transport::Usb, "2026-11-05T10:00:00Z".into());
        s.label = Some("z9a-reference".into());
        s.insert(
            "kNkMAIDCapability_VibrationReduction",
            34053,
            json!({"default_index": 2, "elem_bytes": 4, "elem_count": 3, "elem_type": 2, "value_index": 2, "values": [0, 1, 2]}),
        );
        s.insert("kNkMAIDCapability_ElectronicVR", 33944, json!(1));
        s.insert("kNkMAIDCapability_Flag", 70000, json!(true));
        s.insert(
            "kNkMAIDCapability_ExposureComp",
            33000,
            json!({"default": 0.0, "default_index": 15, "lower": -5.0, "steps": 31, "upper": 5.0, "value": 0.0, "value_index": 15}),
        );
        s.insert("kNkMAIDCapability_Continuous", 33001, json!({"lower": 0.0, "upper": 10.0, "steps": 0, "value": 2.5, "value_index": 0, "default": 0.0, "default_index": 0}));
        s.insert("kNkMAIDCapability_ModelName", 33002, json!("Z 9"));
        s.insert("kNkMAIDCapability_ClockDateTime", 33003, json!({"_type": "datetime"}));
        let mut props = std::collections::BTreeMap::new();
        let mut add = |code: u16, name: &str, state: PropState| {
            props.insert(prop_key(code), ExtendedProp { code: u32::from(code), name: name.into(), state });
        };
        add(0xD072, "Artist", PropState::Text { text: "Brian".into() });
        add(0xD073, "Copyright", PropState::Text { text: "(c)2026".into() });
        add(0xD090, "Image comment", PropState::NotSupported);
        add(0xD053, "EnableCopyright", PropState::Bytes { hex: "01".into() });
        add(0xD060, "IPTCPresetSelect", PropState::Bytes { hex: "00".into() });
        s.extended = Some(Extended {
            read_from: ReadFrom { model: "Z 9".into(), usb_serial: "S1".into() },
            props,
            ftp: FtpState::Read(FtpStore {
                count: 3,
                limit: 20,
                active_slot: Some(0),
                device_name: "Z 9_S1".into(),
                truncated: false,
                profiles: ["alpha", "beta", "gamma"].iter().enumerate().map(|(i, n)| FtpProfileEntry { slot: i as u32, hex: profile_hex(n) }).collect(),
            }),
            iptc: IptcState::Read(IptcStore {
                count: 3,
                truncated: false,
                slots: vec![
                    IptcSlotEntry { slot: 1, hex: preset_hex("Race", "Hello") },
                    IptcSlotEntry { slot: 2, hex: preset_hex("", "") },
                    IptcSlotEntry { slot: 3, hex: "0100000055000200000000000000".into() },
                ],
            }),
        });
        s
    }

    fn edited() -> Snapshot {
        copy_for_edit(&captured(), "z9a-reference.json", None, NOW)
    }

    fn ext(s: &Snapshot) -> &Extended {
        s.extended.as_ref().unwrap()
    }

    fn log(s: &Snapshot) -> &[EditRecord] {
        &s.derived_from.as_ref().unwrap().edits
    }

    // ── the copy, and the refusal to edit anything else ──

    #[test]
    fn a_copy_keeps_the_values_and_the_capture_time_and_says_where_it_came_from() {
        let src = captured();
        let c = copy_for_edit(&src, "z9a-reference.json", None, NOW);
        assert_eq!((&c.properties, &c.extended, &c.captured_at, &c.camera), (&src.properties, &src.extended, &src.captured_at, &src.camera));
        let d = c.derived_from.as_ref().unwrap();
        assert_eq!((d.source_file.as_str(), d.source_captured_at.as_str(), d.created_at.as_str(), d.last_edited_at.as_str()), ("z9a-reference.json", "2026-11-05T10:00:00Z", NOW, NOW));
        assert_eq!(d.source_label.as_deref(), Some("z9a-reference"));
        assert!(d.edits.is_empty() && c.is_edited() && !src.is_edited());
        assert_eq!(c.label.as_deref(), Some("edit-z9a-reference"));
    }

    #[test]
    fn the_copys_label_is_the_one_given_or_edit_and_the_source_label_without_stacking_edit_prefixes() {
        let src = captured();
        assert_eq!(copy_for_edit(&src, "f.json", Some("  no-vr "), NOW).label.as_deref(), Some("no-vr"));
        for blank in ["", "   "] {
            assert_eq!(copy_for_edit(&src, "f.json", Some(blank), NOW).label.as_deref(), Some("edit-z9a-reference"), "{blank:?} is no label");
        }
        let c1 = copy_for_edit(&src, "f.json", None, NOW);
        assert_eq!(copy_for_edit(&c1, "g.json", None, LATER).label.as_deref(), Some("edit-z9a-reference"), "a copy of a copy does not grow edit-edit-");
        let mut unlabelled = captured();
        unlabelled.label = None;
        assert_eq!(copy_for_edit(&unlabelled, "f.json", None, NOW).label.as_deref(), Some("edit-snap"));
    }

    #[test]
    fn a_copy_of_an_edited_copy_carries_the_whole_history() {
        let mut c1 = edited();
        set_enum_value(&mut c1, "kNkMAIDCapability_VibrationReduction", 0, NOW).unwrap();
        let c2 = copy_for_edit(&c1, "c1.json", None, LATER);
        let d = c2.derived_from.as_ref().unwrap();
        assert_eq!(d.source_file, "c1.json", "the immediate source");
        assert_eq!(d.edits, log(&c1), "the edits since the capture, carried");
        assert_eq!(d.created_at, LATER);
    }

    #[test]
    fn a_snapshot_read_from_a_camera_cannot_be_edited_by_any_function() {
        let mut s = captured();
        let before = s.clone();
        let nope = |r: Result<Applied, EditRefusal>| assert!(matches!(r, Err(EditRefusal::NotACopy)), "{r:?}");
        nope(set_enum_value(&mut s, "kNkMAIDCapability_VibrationReduction", 0, NOW));
        nope(set_range_value(&mut s, "kNkMAIDCapability_ExposureComp", 1.0, NOW));
        nope(set_number(&mut s, "kNkMAIDCapability_ElectronicVR", "0", NOW));
        nope(set_bool(&mut s, "kNkMAIDCapability_Flag", "false", NOW));
        nope(set_owner_string(&mut s, 0xD072, "X", NOW));
        nope(set_switch(&mut s, 0xD053, 0, NOW));
        nope(set_iptc_field(&mut s, 1, "headline", "X", NOW));
        nope(ftp_delete(&mut s, 0, NOW));
        nope(ftp_move(&mut s, 0, 1, NOW));
        assert!(matches!(set_property_text(&mut s, "ElectronicVR", "0", NOW), Err(EditRefusal::NotACopy)));
        assert_eq!(s, before, "nothing about it changed");
        assert_eq!(EditRefusal::NotACopy.tag(), "not_a_copy");
    }

    #[test]
    fn the_file_name_of_a_copy_is_built_from_when_it_was_made_so_copies_of_one_snapshot_do_not_collide() {
        let src = captured();
        let a = copy_for_edit(&src, "f.json", Some("x"), "2026-11-06T12:00:00Z");
        let b = copy_for_edit(&src, "f.json", Some("x"), "2026-11-06T12:00:01Z");
        assert_ne!(a.suggested_filename(), b.suggested_filename());
        assert!(a.suggested_filename().contains("20261106T120000Z"), "{}", a.suggested_filename());
        assert!(src.suggested_filename().contains("20261105T100000Z"), "a captured snapshot keeps its capture time: {}", src.suggested_filename());
    }

    // ── choices ──

    const VR: &str = "kNkMAIDCapability_VibrationReduction";

    #[test]
    fn a_choice_is_set_by_one_of_its_values_and_moves_the_index_and_nothing_else() {
        let mut s = edited();
        let a = set_enum_value(&mut s, VR, 0, LATER).unwrap();
        assert!(a.changed && a.change == "VibrationReduction: 2 -> 0", "{a:?}");
        let v = &s.properties[VR].value;
        assert_eq!((v["value_index"].as_u64(), v["default_index"].as_u64(), v["values"].clone()), (Some(0), Some(2), json!([0, 1, 2])));
        assert_eq!(log(&s).len(), 1);
        assert_eq!((log(&s)[0].item.as_str(), log(&s)[0].at.as_str()), ("property:kNkMAIDCapability_VibrationReduction", LATER));
        assert_eq!(s.derived_from.as_ref().unwrap().last_edited_at, LATER);
    }

    #[test]
    fn a_value_the_snapshot_does_not_offer_is_refused_naming_what_it_offers() {
        let mut s = edited();
        for bad in [3, -1, 100] {
            let e = set_enum_value(&mut s, VR, bad, NOW).unwrap_err();
            assert_eq!(e.tag(), "value_not_allowed");
            assert!(e.to_string().contains("0, 1, 2"), "{e}");
        }
        assert!(log(&s).is_empty() && s.properties[VR].value["value_index"] == 2, "a refusal changes nothing");
        // the last listed value is the boundary: allowed
        assert!(set_enum_value(&mut s, VR, 2, NOW).is_ok() && set_enum_value(&mut s, VR, 1, NOW).is_ok());
    }

    #[test]
    fn choosing_what_is_already_chosen_is_not_an_edit() {
        let mut s = edited();
        let a = set_enum_value(&mut s, VR, 2, NOW).unwrap();
        assert!(!a.changed);
        assert!(log(&s).is_empty(), "nothing recorded");
        assert_eq!(s.derived_from.as_ref().unwrap().last_edited_at, NOW, "and the edit time did not move");
    }

    #[test]
    fn a_choice_is_by_value_not_by_position() {
        // values [0, 1, 2, 100] (Movie VR): 100 is at index 3; asking for 3 is not asking for the fourth entry
        let mut s = edited();
        s.insert("kNkMAIDCapability_MovieVibrationReduction", 34056, json!({"default_index": 3, "elem_bytes": 4, "elem_count": 4, "elem_type": 2, "value_index": 3, "values": [0, 1, 2, 100]}));
        assert!(set_enum_value(&mut s, "kNkMAIDCapability_MovieVibrationReduction", 3, NOW).is_err());
        set_enum_value(&mut s, "kNkMAIDCapability_MovieVibrationReduction", 1, NOW).unwrap();
        set_enum_value(&mut s, "kNkMAIDCapability_MovieVibrationReduction", 100, NOW).unwrap();
        assert_eq!(s.properties["kNkMAIDCapability_MovieVibrationReduction"].value["value_index"], 3);
    }

    // ── scales ──

    const EC: &str = "kNkMAIDCapability_ExposureComp";

    #[test]
    fn a_stepped_range_takes_a_value_on_its_grid_and_the_ends_are_allowed() {
        let mut s = edited();
        let idx = |s: &Snapshot| s.properties[EC].value["value_index"].as_u64().unwrap();
        set_range_value(&mut s, EC, -5.0, NOW).unwrap();
        assert_eq!(idx(&s), 0, "the lower end");
        set_range_value(&mut s, EC, 5.0, NOW).unwrap();
        assert_eq!(idx(&s), 30, "the upper end");
        let a = set_range_value(&mut s, EC, 1.0 / 3.0, NOW).unwrap();
        assert_eq!(idx(&s), 16, "31 steps over -5..5 are thirds of an EV; zero is index 15, so +1/3 is 16");
        assert!(a.change.contains("5 -> 0.333333"), "{}", a.change);
        set_range_value(&mut s, EC, 0.33, NOW).unwrap();
        assert_eq!(idx(&s), 16, "0.33 is within a twentieth of a step of 0.3333: it means that grid point (and is the value already held)");
        assert_eq!(s.properties[EC].value["value"], json!(0.0), "the SDK's unused `value` is left alone on a stepped range");
    }

    #[test]
    fn a_value_off_the_grid_or_outside_the_scale_is_refused_at_and_beyond_each_boundary() {
        let mut s = edited();
        for off in [5.0001, 5.1, -5.0001, 100.0, f64::NAN, f64::INFINITY] {
            assert!(set_range_value(&mut s, EC, off, NOW).is_err(), "{off}");
        }
        // half a step off a grid point is not on the grid
        let e = set_range_value(&mut s, EC, 0.1667, NOW).unwrap_err();
        assert!(e.to_string().contains("nearest are"), "{e}");
        // the tolerance edge: just inside a twentieth of a step (1/3 * 0.05 = 0.0167) is taken, just outside is not
        assert!(set_range_value(&mut s, EC, 1.0 / 3.0 + 0.0160, NOW).is_ok());
        assert!(set_range_value(&mut s, EC, 1.0 / 3.0 + 0.0175, NOW).is_err());
        assert!(log(&s).len() == 1, "only the accepted one was recorded");
    }

    #[test]
    fn a_continuous_range_takes_any_value_within_its_ends() {
        let mut s = edited();
        const C: &str = "kNkMAIDCapability_Continuous";
        set_range_value(&mut s, C, 7.25, NOW).unwrap();
        assert_eq!(s.properties[C].value["value"], json!(7.25));
        set_range_value(&mut s, C, 0.0, NOW).unwrap();
        set_range_value(&mut s, C, 10.0, NOW).unwrap();
        assert!(set_range_value(&mut s, C, 10.000001, NOW).is_err() && set_range_value(&mut s, C, -0.000001, NOW).is_err());
        // a range with the same number at both ends holds no choice
        s.insert("kNkMAIDCapability_Readout", 33004, json!({"lower": 0.0, "upper": 0.0, "steps": 0, "value": 0.0, "value_index": 0}));
        assert!(set_range_value(&mut s, "kNkMAIDCapability_Readout", 0.0, NOW).unwrap_err().to_string().contains("no choice"));
    }

    // ── numbers, switches and shapes ──

    #[test]
    fn a_whole_number_stays_whole_and_a_float_stays_a_float() {
        let mut s = edited();
        set_number(&mut s, "kNkMAIDCapability_ElectronicVR", "0", NOW).unwrap();
        assert_eq!(s.properties["kNkMAIDCapability_ElectronicVR"].value, json!(0));
        for bad in ["0.5", "1e3", "x", ""] {
            assert!(set_number(&mut s, "kNkMAIDCapability_ElectronicVR", bad, NOW).is_err(), "{bad:?} is not a whole number");
        }
        s.insert("kNkMAIDCapability_Level", 33005, json!(0.25));
        set_number(&mut s, "kNkMAIDCapability_Level", "1.5", NOW).unwrap();
        assert!(set_number(&mut s, "kNkMAIDCapability_Level", "NaN", NOW).is_err() && set_number(&mut s, "kNkMAIDCapability_Level", "inf", NOW).is_err());
        set_bool(&mut s, "kNkMAIDCapability_Flag", "0", NOW).unwrap();
        assert_eq!(s.properties["kNkMAIDCapability_Flag"].value, json!(false));
        assert!(set_bool(&mut s, "kNkMAIDCapability_Flag", "yes", NOW).is_err());
    }

    #[test]
    fn a_property_by_short_name_and_text_is_set_by_what_it_holds_and_a_string_or_placeholder_is_not_editable() {
        let mut s = edited();
        let (full, a) = set_property_text(&mut s, "VibrationReduction", "1", NOW).unwrap();
        assert_eq!((full.as_str(), a.changed), (VR, true));
        set_property_text(&mut s, "ExposureComp", "-0.3333", NOW).unwrap();
        set_property_text(&mut s, "ElectronicVR", "0", NOW).unwrap();
        set_property_text(&mut s, "Flag", "false", NOW).unwrap();
        for (name, kind) in [("ModelName", ValueKind::String), ("ClockDateTime", ValueKind::Datetime)] {
            let e = set_property_text(&mut s, name, "x", NOW).unwrap_err();
            assert!(matches!(e, EditRefusal::ShapeNotEditable { kind: k, .. } if k == kind), "{e}");
        }
        assert!(matches!(set_property_text(&mut s, "NoSuchThing", "1", NOW), Err(EditRefusal::NoSuchProperty { .. })));
        assert!(matches!(set_property_text(&mut s, "VibrationReduction", "abc", NOW), Err(EditRefusal::ValueNotAllowed { .. })));
    }

    #[test]
    fn a_property_the_restore_never_copies_cannot_be_edited() {
        let (code, name, _) = never_copy_table().into_iter().next().expect("the policy has at least one never-copy record");
        let mut s = edited();
        s.insert(format!("kNkMAIDCapability_{name}"), code, json!(1));
        let e = set_number(&mut s, &format!("kNkMAIDCapability_{name}"), "2", NOW).unwrap_err();
        assert_eq!(e.tag(), "never_copied", "{e}");
        assert_eq!(s.properties[&format!("kNkMAIDCapability_{name}")].value, json!(1));
    }

    // ── owner strings and switches ──

    #[test]
    fn an_owner_string_is_set_within_its_limit_and_one_over_is_refused() {
        for (code, max) in OWNER_STRING_MAX_CHARS {
            let mut s = edited();
            if code == 0xD090 {
                ext_state(&mut s, 0xD090, PropState::Text { text: String::new() });
            }
            assert!(set_owner_string(&mut s, code, &"a".repeat(max), NOW).is_ok(), "{max} characters fit {code:#06x}");
            let e = set_owner_string(&mut s, code, &"a".repeat(max + 1), NOW).unwrap_err();
            assert!(matches!(e, EditRefusal::BadText { .. }) && e.to_string().contains(&format!("at most {max}")), "{e}");
        }
        assert_eq!(OWNER_STRING_MAX_CHARS.map(|(_, m)| m), [36, 54, 36], "the measured limits, pinned");
        let mut s = edited();
        assert!(set_owner_string(&mut s, 0xD072, "", NOW).is_ok(), "empty is allowed (it clears the field)");
    }

    fn ext_state(s: &mut Snapshot, code: u16, state: PropState) {
        s.extended.as_mut().unwrap().props.get_mut(&prop_key(code)).unwrap().state = state;
    }

    #[test]
    fn owner_text_the_camera_would_corrupt_is_refused_with_the_same_rule_the_write_uses() {
        let mut s = edited();
        assert!(set_owner_string(&mut s, 0xD072, "Zoë", NOW).is_ok(), "Latin-1 survives");
        for bad in ["日本", "snow ☃", "a\nb", "a\tb", "nul\0"] {
            assert!(matches!(set_owner_string(&mut s, 0xD072, bad, NOW), Err(EditRefusal::BadText { .. })), "{bad:?}");
        }
        assert_eq!(encode_owner_string("日本").is_err(), true, "it is the write's own check");
        assert!(log(&s).len() == 1);
    }

    #[test]
    fn an_owner_string_that_the_body_does_not_have_cannot_be_given_a_value_but_one_that_failed_to_read_can() {
        let mut s = edited();
        let e = set_owner_string(&mut s, 0xD090, "note", NOW).unwrap_err();
        assert_eq!(e.tag(), "item_unavailable", "{e}");
        ext_state(&mut s, 0xD090, PropState::ReadFailed { tag: "x".into(), detail: "y".into() });
        set_owner_string(&mut s, 0xD090, "note", NOW).unwrap();
        assert!(matches!(&ext(&s).props["0xd090"].state, PropState::Text { text } if text == "note"));
        // not an owner string at all
        assert_eq!(set_owner_string(&mut s, 0xD053, "x", NOW).unwrap_err().tag(), "unknown_item");
        assert_eq!(set_owner_string(&mut s, 0x1234, "x", NOW).unwrap_err().tag(), "unknown_item");
    }

    #[test]
    fn a_snapshot_with_no_extended_section_says_so_for_every_owner_edit() {
        let mut s = edited();
        s.extended = None;
        assert_eq!(set_owner_string(&mut s, 0xD072, "x", NOW).unwrap_err().tag(), "no_extended_section");
        assert_eq!(set_switch(&mut s, 0xD053, 0, NOW).unwrap_err().tag(), "no_extended_section");
        assert_eq!(set_iptc_field(&mut s, 1, "headline", "x", NOW).unwrap_err().tag(), "no_extended_section");
        assert_eq!(ftp_delete(&mut s, 0, NOW).unwrap_err().tag(), "no_extended_section");
    }

    #[test]
    fn a_switch_takes_zero_to_its_highest_value_and_one_more_is_refused() {
        let mut s = edited();
        set_switch(&mut s, 0xD053, 0, NOW).unwrap();
        assert!(matches!(&ext(&s).props["0xd053"].state, PropState::Bytes { hex } if hex == "00"));
        assert_eq!(set_switch(&mut s, 0xD053, 2, NOW).unwrap_err().tag(), "bad_value", "EnableCopyright is 0 or 1");
        let max = BYTE_PROPS.iter().find(|(c, ..)| *c == 0xD060).unwrap().2;
        assert_eq!(max, 13, "the IPTC preset select takes 0 to 13");
        set_switch(&mut s, 0xD060, max, NOW).unwrap();
        assert_eq!(set_switch(&mut s, 0xD060, max + 1, NOW).unwrap_err().tag(), "bad_value");
        assert!(matches!(&ext(&s).props["0xd060"].state, PropState::Bytes { hex } if hex == "0d"), "two lowercase hex digits");
        let a = set_switch(&mut s, 0xD060, max, NOW).unwrap();
        assert!(!a.changed);
        assert_eq!(set_switch(&mut s, 0xD072, 0, NOW).unwrap_err().tag(), "unknown_item", "a string is not a switch");
    }

    #[test]
    fn owner_items_are_found_by_name_or_code_and_nothing_else() {
        for (text, code) in [("artist", 0xD072), ("Artist", 0xD072), ("COPYRIGHT", 0xD073), ("image-comment", 0xD090), ("Image comment", 0xD090), ("comment", 0xD090), ("enable_copyright", 0xD053), ("EnableCopyright", 0xD053), ("iptc-preset", 0xD060), ("IPTCPresetSelect", 0xD060), ("0xD072", 0xD072), ("0xd053", 0xD053)] {
            assert_eq!(resolve_extended_item(text).unwrap(), code, "{text}");
        }
        for bad in ["", "art", "0xD100", "53362x", "owner"] {
            assert_eq!(resolve_extended_item(bad).unwrap_err().tag(), "unknown_item", "{bad:?}");
        }
    }

    // ── IPTC ──

    #[test]
    fn an_iptc_field_and_the_presets_name_are_set_in_place_and_the_rest_is_untouched() {
        let mut s = edited();
        let before = ext(&s).iptc.clone();
        let a = set_iptc_field(&mut s, 1, "Headline", "Race day", LATER).unwrap();
        assert_eq!(a.change, "IPTC slot 1 Headline: \"Hello\" -> \"Race day\"");
        set_iptc_field(&mut s, 2, "name", "Wedding", LATER).unwrap();
        set_iptc_field(&mut s, 2, "Writer/editor", "BLW", LATER).unwrap();
        set_iptc_field(&mut s, 2, "supplemental categories", "a,b", LATER).unwrap();
        let (IptcState::Read(now), IptcState::Read(was)) = (&ext(&s).iptc, &before) else { panic!() };
        let decode = |st: &IptcStore, slot: u32| match st.slots.iter().find(|x| x.slot == slot).unwrap().decode().unwrap() {
            IptcRecord::Preset(p) => p,
            other => panic!("{other:?}"),
        };
        assert_eq!(decode(now, 1).fields[2], "Race day");
        assert_eq!(decode(now, 1).name, "Race", "its name is untouched");
        let p2 = decode(now, 2);
        assert_eq!((p2.name.as_str(), p2.fields[11].as_str(), p2.fields[8].as_str()), ("Wedding", "BLW", "a,b"));
        assert_eq!(now.slots[2], was.slots[2], "slot 3 (another record kind) is byte for byte what it was");
        assert_eq!(now.count, was.count);
    }

    #[test]
    fn iptc_edits_that_cannot_be_made_say_why() {
        let mut s = edited();
        assert_eq!(set_iptc_field(&mut s, 9, "headline", "x", NOW).unwrap_err().tag(), "no_such_slot");
        assert_eq!(set_iptc_field(&mut s, 0, "headline", "x", NOW).unwrap_err().tag(), "no_such_slot", "slot numbers start at 1");
        assert_eq!(set_iptc_field(&mut s, 3, "headline", "x", NOW).unwrap_err().tag(), "not_a_preset");
        let e = set_iptc_field(&mut s, 1, "headlines", "x", NOW).unwrap_err();
        assert!(e.tag() == "no_such_field" && e.to_string().contains("Headline"), "{e}");
        assert_eq!(set_iptc_field(&mut s, 1, "headline", "nul\0", NOW).unwrap_err().tag(), "bad_text");
        assert!(!set_iptc_field(&mut s, 1, "headline", "Hello", NOW).unwrap().changed, "the value it already holds");
        for state in [IptcState::NotRead, IptcState::NotOnModel, IptcState::ReadFailed { tag: "t".into(), detail: "d".into() }] {
            s.extended.as_mut().unwrap().iptc = state;
            assert_eq!(set_iptc_field(&mut s, 1, "headline", "x", NOW).unwrap_err().tag(), "item_unavailable");
        }
        assert!(log(&s).is_empty());
    }

    // ── FTP list ──

    fn names(s: &Snapshot) -> Vec<String> {
        let FtpState::Read(st) = &ext(s).ftp else { panic!() };
        st.profiles.iter().map(|p| decode_ftp_profile(&hex_decode(&p.hex).unwrap()).unwrap().profile_name).collect()
    }

    fn store(s: &Snapshot) -> &FtpStore {
        let FtpState::Read(st) = &ext(s).ftp else { panic!() };
        st
    }

    #[test]
    fn deleting_a_profile_renumbers_the_list_and_keeps_the_others_byte_for_byte() {
        let mut s = edited();
        let before: Vec<String> = store(&s).profiles.iter().map(|p| p.hex.clone()).collect();
        let a = ftp_delete(&mut s, 1, NOW).unwrap();
        assert_eq!(a.change, "FTP profile \"beta\" removed from position 1; 2 left");
        assert_eq!(names(&s), ["alpha", "gamma"]);
        let st = store(&s);
        assert_eq!((st.count, st.profiles.iter().map(|p| p.slot).collect::<Vec<_>>()), (2, vec![0, 1]));
        assert_eq!((&st.profiles[0].hex, &st.profiles[1].hex), (&before[0], &before[2]));
    }

    #[test]
    fn deleting_the_first_the_last_and_the_only_profile_and_one_past_the_end() {
        let mut s = edited();
        ftp_delete(&mut s, 0, NOW).unwrap();
        assert_eq!(names(&s), ["beta", "gamma"]);
        ftp_delete(&mut s, 1, NOW).unwrap();
        assert_eq!(names(&s), ["beta"]);
        assert_eq!(ftp_delete(&mut s, 1, NOW).unwrap_err().tag(), "ftp_index", "one past the end (len 1)");
        ftp_delete(&mut s, 0, NOW).unwrap();
        let st = store(&s);
        assert_eq!((st.count, st.active_slot, st.profiles.len()), (0, None, 0), "an empty list has no active profile");
        assert_eq!(ftp_delete(&mut s, 0, NOW).unwrap_err().tag(), "ftp_index", "nothing to delete");
    }

    #[test]
    fn the_active_slot_always_names_a_profile_that_exists_and_is_none_for_an_empty_list() {
        let set_active = |s: &mut Snapshot, a: Option<u32>| {
            if let FtpState::Read(st) = &mut s.extended.as_mut().unwrap().ftp {
                st.active_slot = a;
            }
        };
        // active slot 2 of 3; deleting the last leaves 2 profiles (slots 0, 1): slot 2 would name nothing
        let mut s = edited();
        set_active(&mut s, Some(2));
        ftp_delete(&mut s, 2, NOW).unwrap();
        assert_eq!(store(&s).active_slot, Some(0), "an active slot equal to the new length is past the end (the boundary)");
        // active slot 1 of 2 still exists after deleting the first
        let mut s = edited();
        set_active(&mut s, Some(1));
        ftp_delete(&mut s, 0, NOW).unwrap();
        assert_eq!(store(&s).active_slot, Some(1));
        // none stays none while there are profiles (stored profiles may exist with none active)
        let mut s = edited();
        set_active(&mut s, None);
        ftp_delete(&mut s, 0, NOW).unwrap();
        assert_eq!(store(&s).active_slot, None);
    }

    #[test]
    fn a_value_a_hair_past_the_end_of_a_scale_is_the_end_and_not_an_index_past_it() {
        let mut s = edited();
        set_range_value(&mut s, EC, 5.0 + 1e-10, NOW).unwrap();
        assert_eq!(s.properties[EC].value["value_index"], 30, "the last step, never step 31");
        set_range_value(&mut s, EC, -5.0 - 1e-10, NOW).unwrap();
        assert_eq!(s.properties[EC].value["value_index"], 0);
    }

    #[test]
    fn moving_a_profile_puts_it_at_the_given_position_and_shifts_the_others() {
        let mut s = edited();
        ftp_move(&mut s, 2, 0, NOW).unwrap();
        assert_eq!(names(&s), ["gamma", "alpha", "beta"]);
        ftp_move(&mut s, 0, 2, NOW).unwrap();
        assert_eq!(names(&s), ["alpha", "beta", "gamma"], "and back");
        ftp_move(&mut s, 0, 1, NOW).unwrap();
        assert_eq!(names(&s), ["beta", "alpha", "gamma"]);
        assert!(!ftp_move(&mut s, 1, 1, NOW).unwrap().changed, "to where it is");
        assert_eq!(ftp_move(&mut s, 0, 3, NOW).unwrap_err().tag(), "ftp_index");
        assert_eq!(ftp_move(&mut s, 3, 0, NOW).unwrap_err().tag(), "ftp_index");
        assert_eq!(store(&s).profiles.iter().map(|p| p.slot).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(log(&s).len(), 3, "the two real moves and the first, the no-op not recorded");
    }

    #[test]
    fn a_partly_read_or_missing_list_cannot_be_rearranged() {
        let mut s = edited();
        if let FtpState::Read(st) = &mut s.extended.as_mut().unwrap().ftp {
            st.truncated = true;
        }
        let e = ftp_delete(&mut s, 0, NOW).unwrap_err();
        assert!(e.tag() == "ftp_unavailable" && e.to_string().contains("only part"), "{e}");
        for state in [FtpState::NotOnModel, FtpState::ReadFailed { tag: "t".into(), detail: "d".into() }] {
            s.extended.as_mut().unwrap().ftp = state;
            assert_eq!(ftp_move(&mut s, 0, 1, NOW).unwrap_err().tag(), "ftp_unavailable");
        }
    }

    #[test]
    fn no_password_or_key_reaches_the_edit_log() {
        let mut s = edited();
        ftp_delete(&mut s, 0, NOW).unwrap();
        ftp_move(&mut s, 0, 1, NOW).unwrap();
        let text = serde_json::to_string(&s.derived_from).unwrap();
        for secret in ["hunter2-secret", "key-secret-12345"] {
            assert!(!text.contains(secret), "{secret} in the log");
        }
        assert!(text.contains("beta"), "a profile name is not a secret and is what makes the log readable");
    }

    #[test]
    fn a_restore_plan_from_an_edited_copy_writes_the_edited_value_and_one_from_the_capture_writes_nothing() {
        use crate::ops::port::FakeCamera;
        use crate::ops::restore_flow::{BodyPolicy, RestorePlanInput, build_restore_plan};
        use crate::sdk::{OP_GET, OP_SET};
        let capture = captured();
        let mut copy = edited();
        set_enum_value(&mut copy, VR, 0, NOW).unwrap();
        // The body holds what was captured.
        let body = FakeCamera::new().with(34053, OP_GET | OP_SET, capture.properties[VR].value.clone());
        let plan_of = |source: &Snapshot| {
            let bytes = source.to_pretty_json().unwrap().into_bytes();
            let input = RestorePlanInput { source, source_name: "s.json", source_bytes: &bytes, target: &capture.camera, only: None, body_policy: BodyPolicy::SameBody };
            build_restore_plan(&body, &input).unwrap()
        };
        assert_eq!(plan_of(&capture).would_change, 0, "the capture is what the body holds");
        let p = plan_of(&copy);
        assert_eq!(p.would_change, 1, "{:?}", p.steps);
        let step = p.steps.iter().find(|s| s.name == VR).expect("the edited property has a step");
        assert_eq!((step.outcome == crate::ops::restore::StepOutcome::WouldWrite, step.changes), (true, Some(true)), "it is the step that would be written, and it changes the body");
        assert_ne!(plan_of(&copy).digest, plan_of(&capture).digest, "so a plan made for one cannot be applied as the other");
    }

    #[test]
    fn an_edited_copy_round_trips_through_json_and_a_captured_snapshot_has_no_provenance_key() {
        let mut s = edited();
        set_enum_value(&mut s, VR, 1, LATER).unwrap();
        let back = Snapshot::from_json(&s.to_pretty_json().unwrap()).unwrap();
        assert_eq!(back, s);
        assert!(!captured().to_pretty_json().unwrap().contains("derived_from"), "a captured snapshot is byte-for-byte what it was before this field existed");
    }
}

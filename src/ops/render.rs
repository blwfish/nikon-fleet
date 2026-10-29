//! What a snapshot holds, as text a person can read: the one formatter the CLI (`show`), the
//! egui GUI's snapshot window and (by parity fixture) the Python GUI's detail window share.
//!
//! [`render_value`] takes the JSON `sdk::decode_value` produced for one property and returns a
//! [`Rendered`]: the text to show, a closed [`Kind`] saying what shape it was, and closed [`Note`]s
//! flagging what a reader should not take at face value. Data that is missing or malformed is shown
//! as such (`(missing)`, `(malformed: why)`), never as a default: a stepped range whose position is
//! unusable has no number, it is not 0.0. Which numbers a range holds is `value_shape`'s question
//! (a stepped range keeps its position in `value_index` and leaves `value` at junk 0.0), not
//! re-derived here.
//!
//! The display strings of well-formed values are those of `gui/fleet_lib.py`'s `fmt_cap_value`.
//! `gui/tests-fixtures/render_values.json` is the table both sides are tested against.
//! [`render_snapshot`] renders a whole snapshot, sorted by name, optionally filtered.
//!
//! The `_with`/`_opts` functions add what only the SDK's own files can say (`ops::labels`): an
//! enumeration's current value is shown with its name (`Label (3)`), a value its list does
//! not have is flagged, rows carry a display name, and a range shows its scale
//! (`612.0 (0..1224, 1225 steps)`). The plain functions show none of that, so their strings are the
//! ones `render_values.json` pins for the Python GUI too. Without labels the viewer entry points
//! still work: they show the numbers, and say the names are unavailable.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Number, Value};
use thiserror::Error;

use crate::ops::labels::{LabelSource, Labels, Lookup};
use crate::ops::paths::{PathError, resolve_existing};
use crate::snapshot::{Snapshot, SnapshotError, Transport};
use crate::transplant::PROPERTY_PREFIX;
use crate::value_shape::{is_enum_shape, is_range_shape, is_stepped_range, range_scale, range_value};

/// What shape a value had. Closed: a consumer may match on it exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Bool,
    Int,
    Float,
    String,
    Enum,
    Range,
    Datetime,
    Array,
    /// A shape the SDK layer does not produce, a placeholder for a type it could not read, or null.
    Unknown,
    /// A recognised shape whose data is missing or unusable; the display says what is wrong.
    Malformed,
}

impl Kind {
    pub const ALL: [Kind; 10] = [
        Kind::Bool,
        Kind::Int,
        Kind::Float,
        Kind::String,
        Kind::Enum,
        Kind::Range,
        Kind::Datetime,
        Kind::Array,
        Kind::Unknown,
        Kind::Malformed,
    ];
}

/// Something a reader should know about a displayed value. Closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Note {
    /// No `value` where one belongs (no key, or a range with no `value`).
    MissingValue,
    /// The value is JSON null: the SDK handed back no data.
    NullValue,
    /// An index at or past the end of the list or the steps it indexes.
    IndexOutOfRange,
    /// An index that is absent or not a non-negative integer.
    IndexInvalid,
    /// The value should be a number and is not (or has none to show).
    NoNumber,
    /// The capture could not decode an enum's option list (`sdk::ENUM_*` sentinel objects).
    DecodeFailed,
    /// A placeholder for a value the capture does not hold (datetime, array, unknown type).
    NotCaptured,
    /// The display is shorter than the value (a long object or list was cut).
    Truncated,
    /// An enum with no `values` key.
    MissingList,
    /// An enum whose `values` is not a list.
    ValuesNotList,
    /// A numeric enum with labels loaded, none of which names this capability's values (or names
    /// this one only ambiguously): the number is all there is to show.
    Unlabelled,
    /// The capability has a list of value names and this value is not in it: either the list is
    /// incomplete or the camera reports something undocumented. Shown, never hidden.
    ValueNotInLabels,
}

impl Note {
    pub const ALL: [Note; 12] = [
        Note::MissingValue,
        Note::NullValue,
        Note::IndexOutOfRange,
        Note::IndexInvalid,
        Note::NoNumber,
        Note::DecodeFailed,
        Note::NotCaptured,
        Note::Truncated,
        Note::MissingList,
        Note::ValuesNotList,
        Note::Unlabelled,
        Note::ValueNotInLabels,
    ];

    /// The notes only a labelled rendering can carry (`render_labels.json` pins them; the
    /// SDK-independent `render_values.json` has none of them).
    pub const LABEL_NOTES: [Note; 2] = [Note::Unlabelled, Note::ValueNotInLabels];

    /// The tag serialised output carries (`serde`'s `snake_case`), for text a UI shows.
    pub fn tag(self) -> &'static str {
        match self {
            Note::MissingValue => "missing_value",
            Note::NullValue => "null_value",
            Note::IndexOutOfRange => "index_out_of_range",
            Note::IndexInvalid => "index_invalid",
            Note::NoNumber => "no_number",
            Note::DecodeFailed => "decode_failed",
            Note::NotCaptured => "not_captured",
            Note::Truncated => "truncated",
            Note::MissingList => "missing_list",
            Note::ValuesNotList => "values_not_list",
            Note::Unlabelled => "unlabelled",
            Note::ValueNotInLabels => "value_not_in_labels",
        }
    }

    /// Whether the note means the data is wrong or absent (a UI shows it as a warning), as opposed
    /// to merely not complete (a placeholder, a cut display, a value our lists do not name).
    pub fn is_problem(self) -> bool {
        match self {
            Note::MissingValue
            | Note::NullValue
            | Note::IndexOutOfRange
            | Note::IndexInvalid
            | Note::NoNumber
            | Note::DecodeFailed
            | Note::MissingList
            | Note::ValuesNotList => true,
            Note::NotCaptured | Note::Truncated | Note::Unlabelled | Note::ValueNotInLabels => false,
        }
    }
}

/// Whether a range's scale is shown after its number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShowScale {
    #[default]
    Off,
    On,
}

/// A range's `lower`, `upper` and `steps` (0: continuous), as the capture holds them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Scale {
    pub lower: f64,
    pub upper: f64,
    pub steps: u64,
}

// The bounds come from JSON numbers, which are never NaN.
impl Eq for Scale {}

/// One value, ready to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rendered {
    pub display: String,
    pub kind: Kind,
    /// Sorted, without duplicates.
    pub notes: Vec<Note>,
    /// The name of an enumeration value, when one was found; `display` then reads `name (value)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_source: Option<LabelSource>,
    /// The scale of a range, when it was asked for and the range has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<Scale>,
}

impl Rendered {
    fn new(display: impl Into<String>, kind: Kind, notes: &[Note]) -> Self {
        let mut notes = notes.to_vec();
        notes.sort();
        notes.dedup();
        Rendered { display: display.into(), kind, notes, label: None, label_source: None, scale: None }
    }

    fn with_note(mut self, note: Note) -> Self {
        self.notes.push(note);
        self.notes.sort();
        self.notes.dedup();
        self
    }

    fn malformed(why: impl std::fmt::Display, notes: &[Note]) -> Self {
        Rendered::new(format!("(malformed: {why})"), Kind::Malformed, notes)
    }

    /// The data is missing or wrong (not merely a placeholder): a UI shows it as a warning.
    pub fn is_problem(&self) -> bool {
        self.kind == Kind::Malformed || self.notes.iter().any(|n| n.is_problem())
    }
}

/// The longest compact-JSON display of an unrecognised object, in characters.
const MAX_SHOWN: usize = 200;
/// The longest list shown in full; a longer one shows its first element and its length.
const MAX_LIST: usize = 6;
/// What `render_optional` shows for a value that is not there at all.
const MISSING: &str = "(missing)";

/// The `sdk::decode_enum_values` sentinel keys that stand in for an option list it could not decode.
const ENUM_SENTINELS: [&str; 3] = [
    crate::sdk::ENUM_INVALID_PHYSICAL_BYTES,
    crate::sdk::ENUM_PACKED_LENGTH_OVERFLOW,
    crate::sdk::ENUM_UNSUPPORTED_TYPE,
];

/// A float as Python's `str(float)` prints it, so the three UIs agree: Rust's `{:?}` has the same
/// digits and the same switch to an exponent, and differs only in writing it (`1e16`, `1e-5`
/// against `1e+16`, `1e-05`).
fn py_float(x: f64) -> String {
    let text = format!("{x:?}");
    let Some((mantissa, exp)) = text.split_once('e') else { return text };
    let (sign, digits) = match exp.strip_prefix('-') {
        Some(d) => ('-', d),
        None => ('+', exp),
    };
    format!("{mantissa}e{sign}{digits:0>2}")
}

/// A JSON number as text, and whether it was an integer or a float (Python keeps the distinction:
/// `3` stays `3`, `3.0` stays `3.0`).
fn number_text(n: &Number) -> (String, Kind) {
    if n.is_i64() || n.is_u64() {
        (n.to_string(), Kind::Int)
    } else {
        (n.as_f64().map_or_else(|| n.to_string(), py_float), Kind::Float)
    }
}

/// Compact JSON the way Python's `json.dumps(sort_keys=True, ensure_ascii=False)` writes it
/// (`{"a": [2], "b": 1}`): sorted keys, `, ` and `: ` separators.
fn py_json(v: &Value) -> String {
    let mut out = String::new();
    write_py_json(v, &mut out);
    out
}

fn write_py_json(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number_text(n).0),
        Value::String(s) => out.push_str(&serde_json::to_string(s).expect("a string always serialises")),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_py_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&serde_json::to_string(key).expect("a string always serialises"));
                out.push_str(": ");
                write_py_json(&map[key], out);
            }
            out.push('}');
        }
    }
}

/// What a rendering may add to the plain strings: names from `ops::labels`, and ranges' scales.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions<'a> {
    pub labels: Option<&'a Labels>,
    pub scale: ShowScale,
}

impl<'a> RenderOptions<'a> {
    /// The plain strings: no names, no scales.
    pub fn raw() -> Self {
        RenderOptions::default()
    }

    /// What the viewers show: names where `labels` has them, and every range's scale.
    pub fn viewer(labels: &'a Labels) -> Self {
        RenderOptions { labels: Some(labels), scale: ShowScale::On }
    }
}

/// The property a value belongs to, for the lookups that need it.
#[derive(Clone, Copy)]
struct Ctx<'a> {
    opts: &'a RenderOptions<'a>,
    code: u32,
    /// The snapshot's property name, when known (`kNkMAIDCapability_ExposureMode`).
    name: Option<&'a str>,
}

/// Render a property's value, which may be absent altogether (a raw entry with no `value` key).
/// Absent is `(missing)`, kind `malformed`, note `missing_value`: not null, not 0, not empty.
pub fn render_optional(v: Option<&Value>) -> Rendered {
    match v {
        Some(v) => render_value(v),
        None => Rendered::new(MISSING, Kind::Malformed, &[Note::MissingValue]),
    }
}

/// Render the JSON `sdk::decode_value` produced for one property.
pub fn render_value(v: &Value) -> Rendered {
    render_in(None, v)
}

/// [`render_value`] for property `code`, with `labels`' names and every range's scale.
pub fn render_value_with(labels: &Labels, code: u32, v: &Value) -> Rendered {
    render_value_opts(&RenderOptions::viewer(labels), code, None, v)
}

/// [`render_value`] for property `code` (named `name` in the snapshot, if known) as `opts` say.
pub fn render_value_opts(opts: &RenderOptions<'_>, code: u32, name: Option<&str>, v: &Value) -> Rendered {
    render_in(Some(Ctx { opts, code, name }), v)
}

fn render_in(ctx: Option<Ctx<'_>>, v: &Value) -> Rendered {
    match v {
        Value::Null => Rendered::new("null", Kind::Unknown, &[Note::NullValue]),
        Value::Bool(b) => Rendered::new(if *b { "Yes" } else { "No" }, Kind::Bool, &[]),
        Value::Number(n) => {
            let (text, kind) = number_text(n);
            label_number(ctx, n, Rendered::new(text, kind, &[]), false)
        }
        Value::String(s) => Rendered::new(s.clone(), Kind::String, &[]),
        Value::Array(items) => render_list(items),
        Value::Object(_) => render_object(ctx, v),
    }
}

/// Name a number the property's lists name, or flag it. `is_enum`: an enumeration's current
/// option, as opposed to a plain integer property (an integer property is flagged only when its
/// list does not have the value; most have no list and say nothing).
fn label_number(ctx: Option<Ctx<'_>>, n: &Number, shown: Rendered, is_enum: bool) -> Rendered {
    let Some(ctx) = ctx else { return shown };
    let Some(labels) = ctx.opts.labels else { return shown };
    let Some(value) = n.as_i64() else { return shown };
    match labels.lookup_value(ctx.code, ctx.name, value) {
        Lookup::Label { text, source } => {
            let display = format!("{text} ({})", shown.display);
            Rendered { display, label: Some(text.to_string()), label_source: Some(source), ..shown }
        }
        Lookup::NotInLabels => shown.with_note(Note::ValueNotInLabels),
        Lookup::Ambiguous | Lookup::NoLabels if is_enum => shown.with_note(Note::Unlabelled),
        Lookup::Ambiguous | Lookup::NoLabels => shown,
    }
}

fn render_list(items: &[Value]) -> Rendered {
    let shown = if items.len() <= MAX_LIST { items } else { &items[..1] };
    let rendered: Vec<Rendered> = shown.iter().map(render_value).collect();
    let mut notes: Vec<Note> = rendered.iter().flat_map(|r| r.notes.iter().copied()).collect();
    let texts: Vec<&str> = rendered.iter().map(|r| r.display.as_str()).collect();
    let display = if items.len() <= MAX_LIST {
        format!("[{}]", texts.join(", "))
    } else {
        notes.push(Note::Truncated);
        format!("[{}, … {} items]", texts[0], items.len())
    };
    Rendered::new(display, Kind::Array, &notes)
}

fn render_object(ctx: Option<Ctx<'_>>, v: &Value) -> Rendered {
    match v.get("_type").and_then(Value::as_str) {
        Some(kind @ "datetime") => return Rendered::new(format!("({kind}: not captured)"), Kind::Datetime, &[Note::NotCaptured]),
        Some(kind @ "array") => return Rendered::new(format!("({kind}: not captured)"), Kind::Array, &[Note::NotCaptured]),
        _ => {}
    }
    if let Some(t) = v.get("_unknown_type") {
        return Rendered::new(format!("(unknown capability type {})", py_json(t)), Kind::Unknown, &[Note::NotCaptured]);
    }
    if is_range_shape(v) {
        return render_range(ctx, v);
    }
    // `value_index` with no `values` is an enum that lost its list; `is_enum_shape` needs the list.
    if is_enum_shape(v) || (v.get("value_index").is_some() && v.get("lower").is_none()) {
        return render_enum(ctx, v);
    }
    let text = py_json(v);
    if text.chars().count() <= MAX_SHOWN {
        Rendered::new(text, Kind::Unknown, &[])
    } else {
        let cut: String = text.chars().take(MAX_SHOWN).collect();
        Rendered::new(format!("{cut}…"), Kind::Unknown, &[Note::Truncated])
    }
}

/// A range bound as text: `0` for `0.0`, `2500` for `2500.0`, `-3.5` for `-3.5`.
fn bound_text(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 { format!("{}", x as i64) } else { py_float(x) }
}

/// `shown` with the range's scale after its number, when the options ask for it:
/// `612.0 (0..1224, 1225 steps)`, or `3.5 (0..10)` for a continuous range (0 steps).
fn with_scale(ctx: Option<Ctx<'_>>, v: &Value, shown: Rendered) -> Rendered {
    if !ctx.is_some_and(|c| c.opts.scale == ShowScale::On) {
        return shown;
    }
    let Some((lower, upper, steps)) = range_scale(v) else { return shown };
    let steps_text = match steps {
        0 => String::new(),
        1 => ", 1 step".to_string(),
        n => format!(", {n} steps"),
    };
    let display = format!("{} ({}..{}{steps_text})", shown.display, bound_text(lower), bound_text(upper));
    Rendered { display, scale: Some(Scale { lower, upper, steps }), ..shown }
}

fn render_range(ctx: Option<Ctx<'_>>, v: &Value) -> Rendered {
    if is_stepped_range(v) {
        if let Some(n) = range_value(v) {
            return with_scale(ctx, v, Rendered::new(py_float(n), Kind::Range, &[]));
        }
        // `range_value` gives a stepped range no number when its position is unusable; say which way.
        let steps = range_scale(v).map_or(0, |(_, _, steps)| steps);
        return match v.get("value_index").and_then(Value::as_u64) {
            Some(i) => Rendered::malformed(
                format!("stepped range position {i} is past its {steps} steps"),
                &[Note::IndexOutOfRange, Note::NoNumber],
            ),
            None => Rendered::malformed(
                format!("stepped range position is missing or invalid ({})", v.get("value_index").map_or("absent".into(), py_json)),
                &[Note::IndexInvalid, Note::NoNumber],
            ),
        };
    }
    match v.get("value") {
        None => Rendered::malformed("range has no value", &[Note::MissingValue, Note::NoNumber]),
        Some(Value::Number(n)) => with_scale(ctx, v, Rendered::new(number_text(n).0, Kind::Range, &[])),
        Some(other) => Rendered::malformed(format!("range value is not a number ({})", py_json(other)), &[Note::NoNumber]),
    }
}

/// The option strings of a packed-string enum the capture stored one byte per element
/// (`elem_type` 7, `elem_bytes` 1: `"J","P","E","G","","R","A","W",""`, `""` a NUL), which
/// `value_index` indexes by string, not by element. `None` for any other layout, whose elements
/// are the options already. `transplant::packed_string_options` reads the same layout (and
/// refuses what it cannot read; this shows what it can and leaves the rest as it was).
fn packed_strings(v: &Value, list: &[Value]) -> Option<Vec<Value>> {
    if v.get("elem_type").and_then(Value::as_u64) != Some(7) || v.get("elem_bytes").and_then(Value::as_u64) != Some(1) {
        return None;
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut open = false;
    for e in list {
        let e = e.as_str()?;
        if e.is_empty() {
            out.push(Value::String(std::mem::take(&mut cur)));
            open = false;
        } else {
            cur.push_str(e);
            open = true;
        }
    }
    if open {
        out.push(Value::String(cur));
    }
    Some(out)
}

fn render_enum(ctx: Option<Ctx<'_>>, v: &Value) -> Rendered {
    let index = v.get("value_index");
    let index_text = index.map_or("absent".to_string(), py_json);
    let Some(values) = v.get("values") else {
        return Rendered::malformed(format!("enum has no list of options (index {index_text})"), &[Note::MissingList]);
    };
    let list = match values {
        Value::Array(list) => list,
        Value::Object(map) if ENUM_SENTINELS.iter().any(|k| map.contains_key(*k)) => {
            return Rendered::malformed(
                format!("the options could not be decoded at capture ({}); index {index_text}", py_json(values)),
                &[Note::DecodeFailed],
            );
        }
        other => {
            return Rendered::malformed(
                format!("enum options are not a list ({}); index {index_text}", py_json(other)),
                &[Note::ValuesNotList],
            );
        }
    };
    let strings;
    let list: &[Value] = match packed_strings(v, list) {
        Some(s) => {
            strings = s;
            &strings
        }
        None => list,
    };
    let Some(position) = index.and_then(Value::as_u64) else {
        return Rendered::malformed(
            format!("enum index is missing or invalid ({index_text}) of {} options", list.len()),
            &[Note::IndexInvalid],
        );
    };
    let Some(option) = usize::try_from(position).ok().and_then(|i| list.get(i)) else {
        return Rendered::malformed(
            format!("enum index {position} is past its {} options", list.len()),
            &[Note::IndexOutOfRange],
        );
    };
    let mut shown = render_value(option);
    if shown.kind != Kind::Malformed {
        shown.kind = Kind::Enum;
        if let Value::Number(n) = option {
            shown = label_number(ctx, n, shown, true);
        }
    }
    shown
}

// ── Whole snapshots ───────────────────────────────────────────────────────

/// Who a rendered snapshot is. `property_count` counts every property in the file, whatever the
/// filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Header {
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub label: Option<String>,
    pub captured_at: String,
    pub transport: Transport,
    pub property_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    /// The property's full name (`kNkMAIDCapability_Aperture`).
    pub name: String,
    pub code: u32,
    /// What the SDK calls it for a person (`Mode A` for `ModeA`; a base capability's own name for a `cap_0x..` key),
    /// when labels are loaded and know the code. `None`: unknown, not empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub value: Rendered,
}

impl Row {
    /// The name without the `kNkMAIDCapability_` prefix every property shares.
    pub fn short_name(&self) -> &str {
        self.name.strip_prefix(PROPERTY_PREFIX).unwrap_or(&self.name)
    }

    /// The code as the GUIs show it: `0xd053`, at least four digits.
    pub fn code_hex(&self) -> String {
        format!("{:#06x}", self.code)
    }

    /// The display name when it says something the name does not (`Mode A` for
    /// `ModeA`; not `Aperture` for `Aperture`, and not the symbol `kNkMAIDCapability_Aperture`
    /// the schema gives many capabilities as their description).
    pub fn extra_display_name(&self) -> Option<&str> {
        self.display_name.as_deref().filter(|d| *d != self.short_name() && *d != self.name)
    }
}

/// The one rule for which rows a filter keeps: a case-insensitive substring of the name, of the
/// display name, of the displayed value, of the code in hex (`0xd053`, so `d053` too) or of the
/// code in decimal.
/// Surrounding whitespace is ignored; an empty filter keeps everything.
pub struct RowFilter {
    needle: String,
}

impl RowFilter {
    pub fn new(text: &str) -> Self {
        RowFilter { needle: text.trim().to_lowercase() }
    }

    pub fn is_empty(&self) -> bool {
        self.needle.is_empty()
    }

    pub fn matches(&self, row: &Row) -> bool {
        self.is_empty()
            || row.name.to_lowercase().contains(&self.needle)
            || row.display_name.as_deref().is_some_and(|d| d.to_lowercase().contains(&self.needle))
            || row.value.display.to_lowercase().contains(&self.needle)
            || row.code_hex().contains(&self.needle)
            || row.code.to_string().contains(&self.needle)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RenderedSnapshot {
    pub header: Header,
    /// Sorted by name; only the rows the filter kept.
    pub rows: Vec<Row>,
    /// Properties the filter dropped: `rows.len() + filtered_out == header.property_count`.
    pub filtered_out: usize,
    /// Some row's display is shorter than its value (a `truncated` note); the file has the rest.
    pub truncated: bool,
    /// Set when labels were asked for and some part is missing: why the names are not all there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels_note: Option<String>,
    /// How much of the snapshot the loaded labels name; present when labels were used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Coverage>,
}

/// The snapshot as the plain strings show it: no names, no scales.
pub fn render_snapshot(s: &Snapshot, filter: Option<&str>) -> RenderedSnapshot {
    render_snapshot_opts(s, filter, &RenderOptions::raw())
}

/// The snapshot as the viewers show it: `labels`' names and every range's scale.
pub fn render_snapshot_with(s: &Snapshot, filter: Option<&str>, labels: &Labels) -> RenderedSnapshot {
    render_snapshot_opts(s, filter, &RenderOptions::viewer(labels))
}

pub fn render_snapshot_opts(s: &Snapshot, filter: Option<&str>, opts: &RenderOptions<'_>) -> RenderedSnapshot {
    let filter = RowFilter::new(filter.unwrap_or(""));
    let all = s.properties.len();
    // BTreeMap order: by name.
    let rows: Vec<Row> = s
        .properties
        .iter()
        .map(|(name, entry)| Row {
            name: name.clone(),
            code: entry.code,
            display_name: opts.labels.and_then(|l| l.capability_display_name(entry.code)).map(str::to_string),
            value: render_value_opts(opts, entry.code, Some(name), &entry.value),
        })
        .filter(|row| filter.matches(row))
        .collect();
    let mut out = RenderedSnapshot {
        header: Header {
            model: s.camera.model.clone(),
            serial: s.camera.serial.clone(),
            firmware: s.camera.firmware.clone(),
            label: s.label.clone(),
            captured_at: s.captured_at.clone(),
            transport: s.transport,
            property_count: all,
        },
        filtered_out: all - rows.len(),
        truncated: rows.iter().any(|r| r.value.notes.contains(&Note::Truncated)),
        rows,
        labels_note: opts.labels.and_then(Labels::note),
        coverage: None,
    };
    if opts.labels.is_some() {
        out.coverage = Some(Coverage::of(&out));
    }
    out
}

/// How much of what a rendered snapshot shows the loaded labels name. Counts are of the rows kept
/// by the filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Coverage {
    pub properties: usize,
    pub with_display_name: usize,
    /// Properties no display name is known for, by name.
    pub no_display_name: Vec<String>,
    /// Enumerations whose current value is a number (the others are named by the camera itself).
    pub numeric_enums: usize,
    pub labelled: usize,
    pub unlabelled: usize,
    /// Properties whose current value is not in the list of names the capability has, as
    /// `Name = value`.
    pub not_in_labels: Vec<String>,
}

impl Coverage {
    pub fn of(r: &RenderedSnapshot) -> Coverage {
        let has = |row: &Row, note: Note| row.value.notes.contains(&note);
        let numeric_enum = |row: &Row| row.value.kind == Kind::Enum && (row.value.label.is_some() || has(row, Note::Unlabelled) || has(row, Note::ValueNotInLabels));
        Coverage {
            properties: r.rows.len(),
            with_display_name: r.rows.iter().filter(|row| row.display_name.is_some()).count(),
            no_display_name: r.rows.iter().filter(|row| row.display_name.is_none()).map(|row| row.name.clone()).collect(),
            numeric_enums: r.rows.iter().filter(|row| numeric_enum(row)).count(),
            labelled: r.rows.iter().filter(|row| numeric_enum(row) && row.value.label.is_some()).count(),
            unlabelled: r.rows.iter().filter(|row| has(row, Note::Unlabelled)).count(),
            not_in_labels: r
                .rows
                .iter()
                .filter(|row| has(row, Note::ValueNotInLabels))
                .map(|row| format!("{} = {}", row.short_name(), row.value.display))
                .collect(),
        }
    }
}

impl Labels {
    /// How much of `s` these labels name.
    pub fn coverage(&self, s: &Snapshot) -> Coverage {
        Coverage::of(&render_snapshot_with(s, None, self))
    }
}

/// Text from a file or a camera, safe to print on a terminal: control characters (an escape
/// sequence in a camera name would otherwise reach the terminal) are written as `\u{..}` escapes.
pub fn printable(text: &str) -> String {
    text.chars().flat_map(|c| if c.is_control() { c.escape_default().collect::<Vec<_>>() } else { vec![c] }).collect()
}

/// What the text of a rendered snapshot includes beyond name and value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextOptions {
    /// The `[0xCODE]` column.
    pub codes: bool,
    /// The display-name column (for the rows that have a name differing from the short one).
    pub names: bool,
}

impl RenderedSnapshot {
    /// The `show` output as it was before names: a header line, then `name [0xCODE] = display` per
    /// row, aligned. `filter` is only echoed in the closing line.
    pub fn to_text(&self, filter: Option<&str>) -> String {
        self.to_text_with(filter, TextOptions { codes: true, names: false })
    }

    /// The `show` output: a header line, then per row the name, the display name (if `names` and it
    /// differs), the `[0xCODE]` (if `codes`) and `= value`, aligned. A value its capability's list
    /// of names does not have is marked `[value_not_in_labels]`, so it is not mistaken for a named one.
    pub fn to_text_with(&self, filter: Option<&str>, opts: TextOptions) -> String {
        let h = &self.header;
        let mut out = format!(
            "{} {}  fw {}  label={}  captured {}  ({} properties)\n",
            printable(&h.model),
            printable(&h.serial),
            printable(&h.firmware),
            h.label.as_deref().map_or("-".to_string(), printable),
            printable(&h.captured_at),
            h.property_count,
        );
        let width = |f: &dyn Fn(&Row) -> String| self.rows.iter().map(|r| f(r).chars().count()).max().unwrap_or(0);
        let name_width = width(&|r| printable(r.short_name()));
        let code_width = width(&Row::code_hex);
        let shown_name = |r: &Row| if opts.names { r.extra_display_name().map(printable).unwrap_or_default() } else { String::new() };
        let display_width = width(&shown_name);
        for row in &self.rows {
            let mut line = format!("{:<name_width$}", printable(row.short_name()));
            if display_width > 0 {
                line.push_str(&format!("  {:<display_width$}", shown_name(row)));
            }
            if opts.codes {
                line.push_str(&format!(" [{:<code_width$}]", row.code_hex()));
            }
            line.push_str(&format!(" = {}", printable(&row.value.display)));
            if row.value.notes.contains(&Note::ValueNotInLabels) {
                line.push_str(&format!("  [{}]", Note::ValueNotInLabels.tag()));
            }
            out.push_str(&line);
            out.push('\n');
        }
        let filter = filter.map(str::trim).filter(|f| !f.is_empty());
        match (filter, self.rows.is_empty()) {
            (Some(f), true) => out.push_str(&format!("(no properties match {:?})\n", printable(f))),
            (Some(f), false) => out.push_str(&format!(
                "{} of {} properties match {:?}\n",
                self.rows.len(),
                h.property_count,
                printable(f)
            )),
            (None, true) => out.push_str("(the snapshot holds no properties)\n"),
            (None, false) => {}
        }
        if self.truncated {
            out.push_str("note: some values are cut for display; the snapshot file has them whole\n");
        }
        out
    }
}

// ── Finding and reading the file ──────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ShowError {
    #[error("{0}")]
    Path(#[from] PathError),
    #[error("{}: {source}", path.display())]
    Snapshot {
        path: PathBuf,
        #[source]
        source: SnapshotError,
    },
}

impl ShowError {
    /// Closed machine-readable tag: a `PathError` tag or a `SnapshotError` tag.
    pub fn tag(&self) -> &'static str {
        match self {
            ShowError::Path(e) => e.tag(),
            ShowError::Snapshot { source, .. } => source.tag(),
        }
    }
}

/// A bare file name: no directory part, so it names a file in a directory rather than a path.
fn is_bare_name(arg: &Path) -> Option<&str> {
    let s = arg.to_str()?;
    (!s.contains(['/', '\\']) && !arg.is_absolute()).then_some(s)
}

/// The first of `dirs` that has a regular file called `name`. A directory that does not exist or
/// has no such file is passed over; a name that escapes its directory or is not a file is an error.
pub fn find_named(dirs: &[&Path], name: &str) -> Result<PathBuf, PathError> {
    for dir in dirs {
        match resolve_existing(dir, name) {
            Ok(path) => return Ok(path),
            Err(PathError::NotFound { .. } | PathError::NoDirectory { .. }) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(PathError::NotFound { name: name.to_string() })
}

/// What `show <arg>` opens: a path (anything with a directory part, or absolute) as it is; a bare
/// file name from `snapshots/`, then `references/`, of `data_dir` (a name `ls` printed), and only
/// failing that a file of that name in the current directory.
pub fn locate(data_dir: &Path, arg: &Path) -> Result<PathBuf, PathError> {
    let Some(name) = is_bare_name(arg) else { return Ok(arg.to_path_buf()) };
    let dirs = [data_dir.join("snapshots"), data_dir.join("references")];
    match find_named(&[&dirs[0], &dirs[1]], name) {
        Err(PathError::NotFound { .. }) if arg.is_file() => Ok(arg.to_path_buf()),
        other => other,
    }
}

/// Read and validate a snapshot file by the library's one validity rule (`Snapshot::from_json`).
pub fn load(path: &Path) -> Result<Snapshot, ShowError> {
    Snapshot::load_from_file(path).map_err(|source| ShowError::Snapshot { path: path.to_path_buf(), source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Camera;
    use serde_json::json;

    fn show(v: Value) -> (String, Kind, Vec<Note>) {
        let r = render_value(&v);
        (r.display, r.kind, r.notes)
    }

    fn wb_tune(index: Value) -> Value {
        json!({"value": 0.0, "default": 0.0, "value_index": index, "default_index": 0, "lower": 0.0, "upper": 1224.0, "steps": 1225})
    }

    fn enum_of(index: Value, values: Value) -> Value {
        json!({"elem_type": 7, "value_index": index, "elem_count": 6, "elem_bytes": 12, "default_index": 0, "values": values})
    }

    // ── the shared fixture ────────────────────────────────────────────

    fn fixture() -> Vec<Value> {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/gui/tests-fixtures/render_values.json"))
            .expect("the shared fixture");
        serde_json::from_str(&text).expect("the fixture is a JSON list")
    }

    #[test]
    fn every_fixture_case_renders_to_the_expected_display_kind_and_notes() {
        let cases = fixture();
        assert!(cases.len() > 60, "{} cases", cases.len());
        for case in &cases {
            let name = case["name"].as_str().expect("every case is named");
            let got = render_optional(case.get("value"));
            assert_eq!(got.display, case["display"].as_str().expect("display"), "{name}: display");
            assert_eq!(serde_json::to_value(got.kind).unwrap(), case["kind"], "{name}: kind");
            assert_eq!(serde_json::to_value(&got.notes).unwrap(), case["notes"], "{name}: notes");
        }
    }

    #[test]
    fn the_fixture_exercises_every_kind_and_every_note() {
        let cases = fixture();
        for kind in Kind::ALL {
            let tag = serde_json::to_value(kind).unwrap();
            assert!(cases.iter().any(|c| c["kind"] == tag), "no fixture case of kind {tag}");
        }
        for note in Note::ALL.into_iter().filter(|n| !Note::LABEL_NOTES.contains(n)) {
            let tag = serde_json::to_value(note).unwrap();
            assert!(cases.iter().any(|c| c["notes"].as_array().is_some_and(|n| n.contains(&tag))), "no fixture case with note {tag}");
        }
        // The notes only labels can raise belong to render_labels.json: this table needs no SDK file.
        for note in Note::LABEL_NOTES {
            let tag = serde_json::to_value(note).unwrap();
            assert!(!cases.iter().any(|c| c["notes"].as_array().is_some_and(|n| n.contains(&tag))), "render_values.json has the label note {tag}");
        }
    }

    #[test]
    fn the_fixture_cases_have_distinct_names_and_the_shared_sdk_shapes_are_in_it() {
        let cases = fixture();
        let mut names: Vec<&str> = cases.iter().map(|c| c["name"].as_str().unwrap()).collect();
        names.sort();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n, "duplicate case names");
        // The shapes tests/fixtures/capability_value_shapes.json pins as what the SDK layer writes.
        let shapes: Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/capability_value_shapes.json")).unwrap(),
        )
        .unwrap();
        for (key, value) in shapes.as_object().unwrap() {
            if key.starts_with('_') {
                continue;
            }
            assert!(cases.iter().any(|c| c.get("value") == Some(value)), "capability_value_shapes.json `{key}` is not a fixture case");
        }
    }

    // ── ranges ────────────────────────────────────────────────────────

    #[test]
    fn a_stepped_range_shows_the_number_at_its_position_not_its_junk_value() {
        assert_eq!(show(wb_tune(json!(1224))).0, "1224.0");
        assert_eq!(show(wb_tune(json!(0))).0, "0.0");
        let mut with_junk = wb_tune(json!(7));
        with_junk["value"] = json!(99.0);
        assert_eq!(show(with_junk).0, "7.0", "value 99.0 is ignored on a stepped range");
        let kelvin = json!({"value": 0.0, "value_index": 750, "lower": 2500.0, "upper": 10000.0, "steps": 751});
        assert_eq!(show(kelvin).0, "10000.0");
    }

    #[test]
    fn a_stepped_ranges_position_at_below_and_above_its_last_step() {
        let (last, kind, notes) = show(wb_tune(json!(1224)));
        assert_eq!((last.as_str(), kind, notes), ("1224.0", Kind::Range, vec![]));
        assert_eq!(show(wb_tune(json!(1223))).0, "1223.0");
        let (past, kind, notes) = show(wb_tune(json!(1225)));
        assert_eq!(kind, Kind::Malformed);
        assert_eq!(notes, vec![Note::IndexOutOfRange, Note::NoNumber]);
        assert!(past.contains("1225") && past.contains("past"), "{past}");
        assert!(!past.contains("0.0"), "the junk value must not stand in for the number: {past}");
    }

    #[test]
    fn a_stepped_range_whose_position_is_not_a_position_has_no_number() {
        for index in [json!(-1), json!("5"), json!(5.0), json!(5.5), json!(null), json!(true), json!([5])] {
            let (shown, kind, notes) = show(wb_tune(index.clone()));
            assert_eq!(kind, Kind::Malformed, "{index}: {shown}");
            assert_eq!(notes, vec![Note::IndexInvalid, Note::NoNumber], "{index}");
        }
        let mut no_index = wb_tune(json!(0));
        no_index.as_object_mut().unwrap().remove("value_index");
        let (shown, kind, _) = show(no_index);
        assert_eq!(kind, Kind::Malformed);
        assert!(shown.contains("absent"), "{shown}");
    }

    #[test]
    fn two_steps_is_the_least_that_is_stepped_and_a_span_is_needed() {
        let range = |steps: u64, upper: f64| json!({"value": 0.25, "value_index": 1, "lower": 0.0, "upper": upper, "steps": steps});
        assert_eq!(show(range(2, 1.0)).0, "1.0", "stepped: from the position");
        assert_eq!(show(range(1, 1.0)).0, "0.25", "one step is not stepped: its value");
        assert_eq!(show(range(0, 1.0)).0, "0.25", "continuous");
        assert_eq!(show(range(10, 0.0)).0, "0.25", "no span is not stepped");
    }

    #[test]
    fn a_range_with_no_usable_value_says_so_and_never_shows_a_default() {
        let none = json!({"lower": 0.0, "upper": 10.0, "steps": 0});
        let (shown, kind, notes) = show(none);
        assert_eq!((kind, notes), (Kind::Malformed, vec![Note::MissingValue, Note::NoNumber]));
        assert!(shown.starts_with("(malformed: ") && !shown.contains('0'), "{shown}");
        let text = json!({"value": "x", "lower": 0.0, "upper": 10.0, "steps": 0});
        let (shown, kind, notes) = show(text);
        assert_eq!((kind, notes), (Kind::Malformed, vec![Note::NoNumber]));
        assert!(shown.contains("\"x\""), "{shown}");
    }

    #[test]
    fn a_continuous_range_keeps_whether_its_value_was_an_integer() {
        let range = |v: Value| json!({"value": v, "lower": 0.0, "upper": 10.0, "steps": 0});
        assert_eq!(show(range(json!(3))).0, "3");
        assert_eq!(show(range(json!(3.0))).0, "3.0");
        assert_eq!(show(range(json!(3.5))).0, "3.5");
    }

    #[test]
    fn lower_without_upper_or_upper_without_lower_is_not_a_range() {
        assert_eq!(show(json!({"lower": 0.0, "value": 3.0})), (r#"{"lower": 0.0, "value": 3.0}"#.into(), Kind::Unknown, vec![]));
        assert_eq!(show(json!({"upper": 9.0, "value": 3.0})).0, r#"{"upper": 9.0, "value": 3.0}"#);
    }

    // ── enums ─────────────────────────────────────────────────────────

    #[test]
    fn an_enum_index_at_below_and_above_the_end() {
        let labels = json!(["JPEG Basic", "JPEG Normal", "JPEG Fine", "RAW", "RAW+Basic", "RAW+Fine"]);
        assert_eq!(show(enum_of(json!(5), labels.clone())), ("RAW+Fine".into(), Kind::Enum, vec![]));
        assert_eq!(show(enum_of(json!(0), labels.clone())).0, "JPEG Basic");
        for past in [6, 7, 99] {
            let (shown, kind, notes) = show(enum_of(json!(past), labels.clone()));
            assert_eq!((kind, notes), (Kind::Malformed, vec![Note::IndexOutOfRange]), "{past}");
            assert!(shown.contains(&past.to_string()) && shown.contains('6'), "{shown}");
        }
    }

    #[test]
    fn an_enum_index_that_is_not_a_position_is_not_python_style_indexing() {
        let labels = json!(["a", "b", "c"]);
        for index in [json!(-1), json!("1"), json!(1.0), json!(null), json!(true), json!([1])] {
            let (shown, kind, notes) = show(enum_of(index.clone(), labels.clone()));
            assert_eq!((kind, notes), (Kind::Malformed, vec![Note::IndexInvalid]), "{index}");
            assert!(!shown.ends_with(" c") && shown != "c", "{index}: {shown}");
        }
        let mut no_index = enum_of(json!(0), labels);
        no_index.as_object_mut().unwrap().remove("value_index");
        assert_eq!(render_value(&no_index).notes, vec![Note::IndexInvalid]);
    }

    #[test]
    fn an_enum_without_a_list_differs_from_an_empty_list() {
        let no_list = json!({"elem_type": 7, "value_index": 2});
        assert_eq!(show(no_list).2, vec![Note::MissingList]);
        let empty = show(enum_of(json!(2), json!([])));
        assert_eq!((empty.1, empty.2), (Kind::Malformed, vec![Note::IndexOutOfRange]));
    }

    #[test]
    fn every_sdk_decode_failure_sentinel_is_a_decode_failure() {
        for key in [crate::sdk::ENUM_INVALID_PHYSICAL_BYTES, crate::sdk::ENUM_PACKED_LENGTH_OVERFLOW, crate::sdk::ENUM_UNSUPPORTED_TYPE] {
            let (shown, kind, notes) = show(enum_of(json!(0), json!({ key: 99 })));
            assert_eq!((kind, notes), (Kind::Malformed, vec![Note::DecodeFailed]), "{key}");
            assert!(shown.contains(key), "{shown}");
        }
        // An object that is not one of them is not a known decode failure.
        let (_, _, notes) = show(enum_of(json!(0), json!({"other": 1})));
        assert_eq!(notes, vec![Note::ValuesNotList]);
        assert_eq!(show(enum_of(json!(0), json!("x"))).2, vec![Note::ValuesNotList]);
    }

    #[test]
    fn an_enum_option_keeps_the_enum_kind_and_a_missing_option_value_is_still_malformed() {
        assert_eq!(show(enum_of(json!(1), json!([0, 1, 2]))), ("1".into(), Kind::Enum, vec![]));
        assert_eq!(show(enum_of(json!(0), json!([null]))), ("null".into(), Kind::Enum, vec![Note::NullValue]));
        // An option that is itself malformed stays malformed rather than passing for a good enum.
        let (_, kind, _) = show(enum_of(json!(0), json!([{"lower": 0, "upper": 1, "steps": 0}])));
        assert_eq!(kind, Kind::Malformed);
        assert_eq!(show(json!({"value_index": 1, "values": ["a", "b"]})).0, "b", "no elem_type needed");
    }

    // ── scalars, placeholders, lists ──────────────────────────────────

    #[test]
    fn scalars_placeholders_and_null() {
        assert_eq!(show(json!(true)), ("Yes".into(), Kind::Bool, vec![]));
        assert_eq!(show(json!(false)).0, "No");
        assert_eq!(show(json!(100)), ("100".into(), Kind::Int, vec![]));
        assert_eq!(show(json!(u64::MAX)).0, "18446744073709551615");
        assert_eq!(show(json!(5.6)), ("5.6".into(), Kind::Float, vec![]));
        assert_eq!(show(json!("USB")), ("USB".into(), Kind::String, vec![]));
        assert_eq!(show(json!("")), (String::new(), Kind::String, vec![]), "an empty string is a value, not a missing one");
        assert_eq!(show(json!(null)), ("null".into(), Kind::Unknown, vec![Note::NullValue]));
        assert_eq!(show(json!({"_type": "datetime"})), ("(datetime: not captured)".into(), Kind::Datetime, vec![Note::NotCaptured]));
        assert_eq!(show(json!({"_type": "array"})), ("(array: not captured)".into(), Kind::Array, vec![Note::NotCaptured]));
        assert_eq!(show(json!({"_unknown_type": 17})), ("(unknown capability type 17)".into(), Kind::Unknown, vec![Note::NotCaptured]));
    }

    #[test]
    fn a_missing_value_is_marked_missing_and_is_not_null_or_empty() {
        let r = render_optional(None);
        assert_eq!((r.display.as_str(), r.kind, r.notes.clone()), ("(missing)", Kind::Malformed, vec![Note::MissingValue]));
        assert!(r.is_problem());
        assert_ne!(r, render_optional(Some(&json!(null))));
        assert_ne!(r.display, render_optional(Some(&json!(""))).display);
    }

    #[test]
    fn a_list_of_six_is_shown_whole_and_a_list_of_seven_is_cut_and_says_so() {
        assert_eq!(show(json!([])), ("[]".into(), Kind::Array, vec![]));
        assert_eq!(show(json!([1, 2, 3, 4, 5, 6])), ("[1, 2, 3, 4, 5, 6]".into(), Kind::Array, vec![]));
        assert_eq!(show(json!([1, 2, 3, 4, 5, 6, 7])), ("[1, … 7 items]".into(), Kind::Array, vec![Note::Truncated]));
    }

    #[test]
    fn an_unrecognised_object_is_compact_json_cut_at_200_characters_with_a_flag() {
        // `{"k": "` is 7 characters and `"}` 2: a value of n characters makes a text of n + 9.
        let object = |n: usize| json!({"k": "x".repeat(n)});
        let (at, _, notes) = show(object(191));
        assert_eq!((at.chars().count(), notes), (200, vec![]), "exactly 200 characters is not cut");
        let (above, kind, notes) = show(object(192));
        assert_eq!((kind, notes), (Kind::Unknown, vec![Note::Truncated]));
        assert_eq!(above.chars().count(), 201, "200 characters and the ellipsis");
        assert!(above.ends_with('…'));
        let (below, _, notes) = show(object(190));
        assert_eq!((below.chars().count(), notes), (199, vec![]));
        // Counted in characters, not bytes: 200 two-byte characters are not cut.
        assert!(show(json!({"k": "é".repeat(191)})).2.is_empty());
    }

    #[test]
    fn unrecognised_objects_are_written_like_pythons_sorted_compact_json() {
        assert_eq!(show(json!({"b": 1, "a": [2, {"d": null, "c": true}]})).0, r#"{"a": [2, {"c": true, "d": null}], "b": 1}"#);
    }

    #[test]
    fn floats_print_like_pythons_str() {
        for (x, want) in [
            (1224.0, "1224.0"),
            (-0.0, "-0.0"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (1e100, "1e+100"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (-2.5e-9, "-2.5e-09"),
            (1e-100, "1e-100"),
        ] {
            assert_eq!(py_float(x), want, "{x:?}");
        }
    }

    #[test]
    fn a_notes_tag_is_the_tag_it_serialises_to() {
        for note in Note::ALL {
            assert_eq!(serde_json::to_value(note).unwrap(), json!(note.tag()), "{note:?}");
        }
    }

    #[test]
    fn notes_are_sorted_and_without_duplicates() {
        let r = Rendered::new("x", Kind::Malformed, &[Note::NoNumber, Note::MissingValue, Note::NoNumber]);
        assert_eq!(r.notes, vec![Note::MissingValue, Note::NoNumber]);
    }

    #[test]
    fn only_the_data_problems_are_problems() {
        for note in Note::ALL {
            let benign = matches!(note, Note::NotCaptured | Note::Truncated | Note::Unlabelled | Note::ValueNotInLabels);
            assert_eq!(note.is_problem(), !benign, "{note:?}");
        }
        assert!(!render_value(&json!({"_type": "datetime"})).is_problem(), "a placeholder is not an error");
        assert!(!render_value(&json!([1, 2, 3, 4, 5, 6, 7])).is_problem(), "a cut list is not an error");
        assert!(render_value(&json!(null)).is_problem());
        assert!(render_value(&wb_tune(json!(5000))).is_problem());
        assert!(!render_value(&json!(5)).is_problem());
    }

    #[test]
    fn rendered_values_serialise_with_closed_snake_case_tags() {
        let v = serde_json::to_value(render_value(&wb_tune(json!(1225)))).unwrap();
        assert_eq!(v["kind"], "malformed");
        assert_eq!(v["notes"], json!(["index_out_of_range", "no_number"]));
        assert!(v["display"].as_str().unwrap().starts_with("(malformed: "));
    }

    // ── snapshots ─────────────────────────────────────────────────────

    fn snap() -> Snapshot {
        let mut s = Snapshot::new(
            Camera { model: "Z 9".into(), serial: "30012345".into(), firmware: "5.31".into() },
            Transport::Usb,
            "2026-10-27T12:00:00Z".into(),
        );
        s.label = Some("base".into());
        s.insert("kNkMAIDCapability_Beta", 0xd053, json!("Auto ISO"));
        s.insert("kNkMAIDCapability_alpha", 7, json!(true));
        s.insert("kNkMAIDCapability_Alpha", 12, enum_of(json!(1), json!(["Off", "On"])));
        s.insert("kNkMAIDCapability_Gamma", 4242, wb_tune(json!(1225)));
        s
    }

    fn names(r: &RenderedSnapshot) -> Vec<&str> {
        r.rows.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn rows_are_sorted_by_name_and_carry_the_header() {
        let r = render_snapshot(&snap(), None);
        assert_eq!(names(&r), ["kNkMAIDCapability_Alpha", "kNkMAIDCapability_Beta", "kNkMAIDCapability_Gamma", "kNkMAIDCapability_alpha"]);
        assert_eq!(r.header.property_count, 4);
        assert_eq!((r.header.model.as_str(), r.header.serial.as_str(), r.header.firmware.as_str()), ("Z 9", "30012345", "5.31"));
        assert_eq!((r.header.label.as_deref(), r.header.captured_at.as_str(), r.header.transport), (Some("base"), "2026-10-27T12:00:00Z", Transport::Usb));
        assert_eq!((r.filtered_out, r.truncated), (0, false));
        assert_eq!(r.rows[0].short_name(), "Alpha");
        assert_eq!(r.rows[0].value.display, "On");
    }

    #[test]
    fn the_filter_matches_name_display_hex_and_decimal_code_case_insensitively() {
        let s = snap();
        let hit = |f: &str| -> Vec<String> { render_snapshot(&s, Some(f)).rows.iter().map(|r| r.short_name().to_string()).collect() };
        assert_eq!(hit("BETA"), ["Beta"], "name, upper-case needle");
        assert_eq!(hit("beta"), ["Beta"]);
        assert_eq!(hit("auto iso"), ["Beta"], "display text, with an inner space");
        assert_eq!(hit("AUTO"), ["Beta"]);
        assert_eq!(hit("0xD053"), ["Beta"], "hex code with its prefix, either case");
        assert_eq!(hit("d053"), ["Beta"], "hex digits alone");
        assert_eq!(hit("53331"), ["Beta"], "decimal code of 0xd053");
        assert_eq!(hit("4242"), ["Gamma"], "decimal code");
        assert_eq!(hit("0x1092"), ["Gamma"], "4242 in hex");
        assert_eq!(hit("malformed"), ["Gamma"], "a problem is found by the word in its display");
        assert_eq!(hit("alpha"), ["Alpha", "alpha"], "both spellings, name order");
    }

    #[test]
    fn an_empty_or_blank_filter_keeps_everything_and_a_miss_keeps_nothing() {
        let s = snap();
        assert_eq!(render_snapshot(&s, None).rows.len(), 4);
        assert_eq!(render_snapshot(&s, Some("")).rows.len(), 4);
        assert_eq!(render_snapshot(&s, Some("   ")).rows.len(), 4, "whitespace alone is no filter");
        assert_eq!(render_snapshot(&s, Some("  beta ")).rows.len(), 1, "surrounding whitespace is ignored");
        let none = render_snapshot(&s, Some("no such thing"));
        assert!(none.rows.is_empty());
        assert_eq!((none.filtered_out, none.header.property_count), (4, 4), "the count is of the file, not of the match");
    }

    #[test]
    fn kept_and_dropped_rows_always_add_up_to_the_file() {
        let s = snap();
        for f in [None, Some(""), Some("a"), Some("beta"), Some("zzz")] {
            let r = render_snapshot(&s, f);
            assert_eq!(r.rows.len() + r.filtered_out, r.header.property_count, "{f:?}");
        }
    }

    #[test]
    fn a_cut_value_sets_the_snapshot_flag_only_when_a_cut_row_is_shown() {
        let mut s = snap();
        s.insert("kNkMAIDCapability_Long", 1, json!({"k": "x".repeat(500)}));
        assert!(render_snapshot(&s, None).truncated);
        assert!(!render_snapshot(&s, Some("beta")).truncated, "the cut row was filtered out");
    }

    #[test]
    fn an_empty_snapshot_renders_no_rows() {
        let s = Snapshot::new(Camera { model: "M".into(), serial: "S".into(), firmware: "F".into() }, Transport::File, "t".into());
        let r = render_snapshot(&s, None);
        assert_eq!((r.rows.len(), r.filtered_out, r.header.property_count), (0, 0, 0));
        assert!(r.to_text(None).contains("(the snapshot holds no properties)"));
    }

    #[test]
    fn the_text_has_a_header_then_aligned_name_code_value_lines() {
        let text = render_snapshot(&snap(), None).to_text(None);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Z 9 30012345  fw 5.31  label=base  captured 2026-10-27T12:00:00Z  (4 properties)");
        assert_eq!(lines[1], "Alpha [0x000c] = On");
        assert_eq!(lines[2], "Beta  [0xd053] = Auto ISO");
        assert!(lines[3].starts_with("Gamma [0x1092] = "), "{}", lines[3]);
        assert!(lines[3].contains("= (malformed: "), "{}", lines[3]);
        assert_eq!(lines[4], "alpha [0x0007] = Yes");
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn the_text_says_how_many_matched_and_when_none_did() {
        let s = snap();
        let some = render_snapshot(&s, Some("beta")).to_text(Some("beta"));
        assert!(some.ends_with("1 of 4 properties match \"beta\"\n"), "{some}");
        let none = render_snapshot(&s, Some("zzz")).to_text(Some("zzz"));
        assert!(none.ends_with("(no properties match \"zzz\")\n"), "{none}");
        // A blank filter is no filter: no closing line.
        assert!(!render_snapshot(&s, Some(" ")).to_text(Some(" ")).contains("match"));
    }

    #[test]
    fn the_text_flags_values_it_cut() {
        let mut s = snap();
        s.insert("kNkMAIDCapability_Long", 1, json!({"k": "x".repeat(500)}));
        assert!(render_snapshot(&s, None).to_text(None).ends_with("the snapshot file has them whole\n"));
        assert!(!render_snapshot(&snap(), None).to_text(None).contains("cut for display"));
    }

    #[test]
    fn control_characters_from_a_file_never_reach_the_terminal() {
        let mut s = snap();
        s.camera.model = "Z\u{1b}[31m 9".into();
        s.label = Some("a\nb".into());
        s.insert("kNkMAIDCapability_Evil\u{7}", 1, json!("\u{1b}]0;title\u{7}"));
        let text = render_snapshot(&s, None).to_text(None);
        assert!(!text.chars().any(|c| c.is_control() && c != '\n'), "{text:?}");
        assert_eq!(text.lines().count(), 1 + 5, "a newline in a value does not add a line: {text:?}");
        assert!(text.contains("\\u{1b}"), "{text}");
        assert_eq!(printable("plain é text"), "plain é text", "printable characters pass");
        assert_eq!(printable("\t"), "\\t");
    }

    // ── finding and reading the file ──────────────────────────────────

    fn data_dir() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("snapshots")).unwrap();
        std::fs::create_dir_all(d.path().join("references")).unwrap();
        d
    }

    #[test]
    fn a_bare_name_is_looked_up_in_snapshots_then_references() {
        let d = data_dir();
        snap().save_to_file(d.path().join("snapshots").join("a.json")).unwrap();
        snap().save_to_file(d.path().join("references").join("r.json")).unwrap();
        let canon = |p: PathBuf| p.canonicalize().unwrap();
        assert_eq!(canon(locate(d.path(), Path::new("a.json")).unwrap()), canon(d.path().join("snapshots/a.json")));
        assert_eq!(canon(locate(d.path(), Path::new("r.json")).unwrap()), canon(d.path().join("references/r.json")));
        // Both directories have it: snapshots/ wins.
        snap().save_to_file(d.path().join("references").join("a.json")).unwrap();
        assert_eq!(canon(locate(d.path(), Path::new("a.json")).unwrap()), canon(d.path().join("snapshots/a.json")));
    }

    #[test]
    fn a_name_that_is_nowhere_is_not_found_and_a_missing_references_dir_is_not_an_error() {
        let d = tempfile::tempdir().unwrap(); // no snapshots/, no references/
        assert_eq!(locate(d.path(), Path::new("no_such_file_for_show.json")).unwrap_err().tag(), "not_found");
        let d = data_dir();
        std::fs::remove_dir(d.path().join("references")).unwrap();
        snap().save_to_file(d.path().join("snapshots").join("a.json")).unwrap();
        assert!(locate(d.path(), Path::new("a.json")).is_ok());
        assert_eq!(locate(d.path(), Path::new("b.json")).unwrap_err().tag(), "not_found");
    }

    #[test]
    fn a_path_with_a_directory_part_is_used_as_it_is_and_unsafe_bare_names_are_refused() {
        let d = data_dir();
        for arg in ["/some/where/x.json", "sub/x.json", "./x.json", "../x.json", "a/"] {
            assert_eq!(locate(d.path(), Path::new(arg)).unwrap(), PathBuf::from(arg), "{arg}");
        }
        for arg in ["", ".", ".."] {
            assert_eq!(locate(d.path(), Path::new(arg)).unwrap_err().tag(), "unsafe_name", "{arg:?}");
        }
    }

    #[test]
    fn a_symlink_out_of_the_data_directory_is_refused() {
        let d = data_dir();
        let outside = tempfile::tempdir().unwrap();
        snap().save_to_file(outside.path().join("o.json")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("o.json"), d.path().join("snapshots/link.json")).unwrap();
        assert_eq!(locate(d.path(), Path::new("link.json")).unwrap_err().tag(), "escapes_directory");
    }

    #[test]
    fn load_reads_a_snapshot_and_refuses_the_rest_with_typed_errors() {
        let d = data_dir();
        let ok = d.path().join("ok.json");
        snap().save_to_file(&ok).unwrap();
        assert_eq!(load(&ok).unwrap().properties.len(), 4);
        let bad = d.path().join("bad.json");
        std::fs::write(&bad, "{ nope").unwrap();
        assert_eq!(load(&bad).unwrap_err().tag(), "json");
        let old = d.path().join("old.json");
        std::fs::write(&old, snap().to_pretty_json().unwrap().replace("\"format_version\": 1", "\"format_version\": 99")).unwrap();
        assert_eq!(load(&old).unwrap_err().tag(), "unsupported_version");
        let err = load(&d.path().join("absent.json")).unwrap_err();
        assert_eq!(err.tag(), "io");
        assert!(err.to_string().contains("absent.json"), "the message names the file: {err}");
        assert_eq!(load(d.path()).unwrap_err().tag(), "io", "a directory is not a snapshot");
    }

    // ── names and scales (render_labels.json) ─────────────────────────

    fn label_fixture() -> Value {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/gui/tests-fixtures/render_labels.json"))
            .expect("the labels fixture");
        serde_json::from_str(&text).expect("the labels fixture is JSON")
    }

    fn fixture_labels(fx: &Value) -> Labels {
        let schema: Vec<&str> = fx["schema_lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
        let schema = schema.join("\n");
        let doc = fx["doc"].to_string();
        Labels::from_sources(
            Some(crate::ops::labels::Source { path: "schema", text: &schema }),
            Some(crate::ops::labels::Source { path: "doc", text: &doc }),
            fx["model"].as_str().unwrap(),
            fx["firmware"].as_str(),
        )
    }

    #[test]
    fn every_label_fixture_case_renders_to_the_expected_display_kind_notes_label_and_scale() {
        let fx = label_fixture();
        let labels = fixture_labels(&fx);
        assert_eq!(labels.unavailable(), [], "the fixture's own files load whole");
        let cases = fx["cases"].as_array().unwrap();
        assert!(cases.len() > 40, "{} cases", cases.len());
        for case in cases {
            let name = case["name"].as_str().expect("every case is named");
            let opts = match case["mode"].as_str().unwrap() {
                "viewer" => RenderOptions::viewer(&labels),
                "labels_only" => RenderOptions { labels: Some(&labels), scale: ShowScale::Off },
                "raw" => RenderOptions::raw(),
                other => panic!("{name}: unknown mode {other}"),
            };
            let got = render_value_opts(&opts, case["code"].as_u64().unwrap() as u32, case["property"].as_str(), &case["value"]);
            assert_eq!(got.display, case["display"].as_str().unwrap(), "{name}: display");
            assert_eq!(serde_json::to_value(got.kind).unwrap(), case["kind"], "{name}: kind");
            assert_eq!(serde_json::to_value(&got.notes).unwrap(), case["notes"], "{name}: notes");
            assert_eq!(got.label.as_deref(), case["label"].as_str(), "{name}: label");
            assert_eq!(got.label_source.map(|s| serde_json::to_value(s).unwrap()), case.get("label_source").cloned(), "{name}: label_source");
            assert_eq!(got.scale.map(|s| serde_json::to_value(s).unwrap()), case.get("scale").cloned(), "{name}: scale");
        }
    }

    #[test]
    fn the_label_fixture_exercises_every_label_note_source_and_mode() {
        let fx = label_fixture();
        let cases = fx["cases"].as_array().unwrap();
        for note in Note::LABEL_NOTES {
            let tag = serde_json::to_value(note).unwrap();
            assert!(cases.iter().any(|c| c["notes"].as_array().unwrap().contains(&tag)), "no case with {tag}");
        }
        for source in ["schema", "sdk_doc"] {
            assert!(cases.iter().any(|c| c.get("label_source") == Some(&json!(source))), "no case labelled from {source}");
        }
        for mode in ["viewer", "labels_only", "raw"] {
            assert!(cases.iter().any(|c| c["mode"] == mode), "no {mode} case");
        }
        let mut names: Vec<&str> = cases.iter().map(|c| c["name"].as_str().unwrap()).collect();
        names.sort();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n, "duplicate case names");
    }

    #[test]
    fn render_value_with_is_the_viewer_rendering_by_code_alone() {
        let fx = label_fixture();
        let labels = fixture_labels(&fx);
        let v = enum_of(json!(1), json!([0, 1, 7]));
        let r = render_value_with(&labels, 40001, &v);
        assert_eq!((r.display.as_str(), r.label.as_deref()), ("Beta (1)", Some("Beta")));
        // No name given: the schema finds the capability by its code; a code it lacks has nothing to find.
        assert_eq!(render_value_with(&labels, 99999, &enum_of(json!(0), json!([1]))).notes, vec![Note::Unlabelled]);
        let wb = render_value_with(&labels, 1, &wb_tune(json!(612)));
        assert_eq!(wb.display, "612.0 (0..1224, 1225 steps)");
    }

    #[test]
    fn the_plain_functions_never_name_or_scale_anything() {
        let r = render_value(&wb_tune(json!(612)));
        assert_eq!((r.display.as_str(), r.scale, r.label.clone()), ("612.0", None, None));
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("scale").is_none() && v.get("label").is_none() && v.get("label_source").is_none(), "{v}");
        let fx = label_fixture();
        let labels = fixture_labels(&fx);
        let snap = labelled_snap();
        let plain = serde_json::to_value(render_snapshot(&snap, None)).unwrap();
        assert!(plain.get("labels_note").is_none() && plain.get("coverage").is_none(), "{plain}");
        assert!(plain["rows"].as_array().unwrap().iter().all(|r| r.get("display_name").is_none()), "{plain}");
        let with = serde_json::to_value(render_snapshot_with(&snap, None, &labels)).unwrap();
        assert!(with.get("coverage").is_some() && with["rows"][0].get("display_name").is_some(), "{with}");
    }

    fn labelled_snap() -> Snapshot {
        let mut s = Snapshot::new(
            Camera { model: "Test 1".into(), serial: "1".into(), firmware: "2.00".into() },
            Transport::Usb,
            "2026-10-27T12:00:00Z".into(),
        );
        s.insert("kNkMAIDCapability_ModeA", 40001, enum_of(json!(1), json!([0, 1, 7])));
        s.insert("kNkMAIDCapability_ModeB", 40002, enum_of(json!(1), json!([0, 1])));
        s.insert("kNkMAIDCapability_Plain", 40004, enum_of(json!(0), json!([3])));
        s.insert("kNkMAIDCapability_Odd", 40005, enum_of(json!(0), json!([8])));
        s.insert("kNkMAIDCapability_Slot", 40010, json!(5));
        s.insert("cap_0x9", 9, json!("Z 9"));
        s.insert("kNkMAIDCapability_Quality", 40020, enum_of(json!(1), json!(["Basic", "Fine"])));
        s
    }

    #[test]
    fn rows_carry_the_display_name_and_none_when_it_is_unknown() {
        let fx = label_fixture();
        let labels = fixture_labels(&fx);
        let r = render_snapshot_with(&labelled_snap(), None, &labels);
        let by_name = |n: &str| r.rows.iter().find(|row| row.name == n).unwrap();
        assert_eq!(by_name("kNkMAIDCapability_ModeA").display_name.as_deref(), Some("Mode A"));
        assert_eq!(by_name("cap_0x9").display_name.as_deref(), Some("Base Nine"), "a base capability by its number");
        assert_eq!(by_name("kNkMAIDCapability_Odd").display_name, None, "the schema does not know code 40005");
        assert_eq!(by_name("kNkMAIDCapability_Slot").display_name, None, "an empty description is not a name");
        // And with no labels at all, none.
        assert!(render_snapshot(&labelled_snap(), None).rows.iter().all(|row| row.display_name.is_none()));
    }

    #[test]
    fn the_filter_finds_a_row_by_its_display_name() {
        let fx = label_fixture();
        let labels = fixture_labels(&fx);
        let hit = |f: &str| -> Vec<String> { render_snapshot_with(&labelled_snap(), Some(f), &labels).rows.iter().map(|r| r.short_name().to_string()).collect() };
        assert_eq!(hit("mode a"), ["ModeA"], "the display name has a space the name does not");
        assert_eq!(hit("beta"), ["ModeA"], "the label is part of the display");
        assert_eq!(hit("base nine"), ["cap_0x9"], "a base capability is found by its SDK name, which its key lacks");
        // Without labels the display name is not there to match.
        assert!(render_snapshot(&labelled_snap(), Some("mode a")).rows.is_empty());
    }

    #[test]
    fn coverage_counts_what_the_labels_name_and_lists_what_they_do_not() {
        let fx = label_fixture();
        let labels = fixture_labels(&fx);
        let c = labels.coverage(&labelled_snap());
        assert_eq!(c.properties, 7);
        assert_eq!(c.with_display_name, 4, "ModeA, ModeB, Plain (its description is its symbol: still a name) and cap_0x9");
        let mut none = c.no_display_name.clone();
        none.sort();
        assert_eq!(none, ["kNkMAIDCapability_Odd", "kNkMAIDCapability_Quality", "kNkMAIDCapability_Slot"]);
        // Numeric enums: ModeA Beta(1), ModeB On(1), Plain unlabelled, Odd unlabelled. Quality is named by the camera.
        assert_eq!((c.numeric_enums, c.labelled, c.unlabelled), (4, 2, 2));
        assert_eq!(c.not_in_labels, ["Slot = 5"]);
    }

    #[test]
    fn the_text_hides_codes_by_default_and_shows_display_names_when_they_say_something() {
        let fx = label_fixture();
        let labels = fixture_labels(&fx);
        let r = render_snapshot_with(&labelled_snap(), None, &labels);
        let text = r.to_text_with(None, TextOptions { codes: false, names: true });
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("Test 1 1  fw 2.00"), "{}", lines[0]);
        assert!(!text.contains("[0x"), "no code column: {text}");
        let mode_a = lines.iter().find(|l| l.starts_with("ModeA")).unwrap();
        assert!(mode_a.contains("Mode A") && mode_a.ends_with("= Beta (1)"), "{mode_a}");
        let plain = lines.iter().find(|l| l.starts_with("Plain")).unwrap();
        assert_eq!(plain.split_whitespace().collect::<Vec<_>>(), ["Plain", "=", "3"], "its display name is its symbol: nothing extra {plain}");
        let slot = lines.iter().find(|l| l.starts_with("Slot")).unwrap();
        assert!(slot.ends_with("= 5  [value_not_in_labels]"), "{slot}");
        let with_codes = r.to_text_with(None, TextOptions { codes: true, names: true });
        assert!(with_codes.contains("[0x9c41]"), "40001 is 0x9c41: {with_codes}");
        let no_names = r.to_text_with(None, TextOptions { codes: false, names: false });
        assert!(!no_names.contains("Mode A"), "{no_names}");
    }

    #[test]
    fn to_text_is_the_text_as_it_was_before_names() {
        let r = render_snapshot(&snap(), None);
        assert_eq!(r.to_text(None), r.to_text_with(None, TextOptions { codes: true, names: false }));
        assert_eq!(r.to_text(None).lines().nth(1), Some("Alpha [0x000c] = On"));
        // The same rows, names asked for but none known: the same text.
        assert_eq!(r.to_text(None), r.to_text_with(None, TextOptions { codes: true, names: true }));
    }

    #[test]
    fn a_rendering_with_labels_that_are_partly_missing_says_so() {
        let fx = label_fixture();
        let schema: Vec<&str> = fx["schema_lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
        let schema = schema.join("\n");
        let labels = Labels::from_sources(Some(crate::ops::labels::Source { path: "schema", text: &schema }), None, "Test 1", None);
        assert!(labels.note().is_none(), "from_sources has no file to miss; load_from reports those");
        let none = Labels::load_from(Path::new("/nonexistent/MaidLayer.config"), Path::new("/nonexistent/maid-labels.json"), "Z 9", None);
        let r = render_snapshot_with(&snap(), None, &none);
        let note = r.labels_note.expect("labels unavailable are said so");
        assert!(note.contains("MaidLayer.config") && note.contains("maid-labels.json"), "{note}");
        assert_eq!(r.rows[0].value.display, "On", "the numbers are shown as they were");
        assert_eq!(r.coverage.as_ref().map(|c| c.with_display_name), Some(0));
        assert!(render_snapshot(&snap(), None).labels_note.is_none(), "raw rendering is a choice, not a gap");
    }

    #[test]
    fn extra_display_name_is_only_a_name_that_differs_from_the_symbol_and_the_short_name() {
        let row = |name: &str, display: Option<&str>| Row {
            name: name.into(),
            code: 1,
            display_name: display.map(Into::into),
            value: render_value(&json!(1)),
        };
        assert_eq!(row("kNkMAIDCapability_Aperture", Some("Aperture")).extra_display_name(), None);
        assert_eq!(row("kNkMAIDCapability_Aperture", Some("kNkMAIDCapability_Aperture")).extra_display_name(), None);
        assert_eq!(row("kNkMAIDCapability_Aperture", Some("Aperture ")).extra_display_name(), Some("Aperture "), "a differing name is shown as it is");
        assert_eq!(row("cap_0x9", Some("Base Nine")).extra_display_name(), Some("Base Nine"));
        assert_eq!(row("cap_0x9", None).extra_display_name(), None);
    }
}

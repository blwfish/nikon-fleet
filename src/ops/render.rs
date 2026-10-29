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

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Number, Value};
use thiserror::Error;

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
}

impl Note {
    pub const ALL: [Note; 10] = [
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
    ];

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
        }
    }

    /// Whether the note means the data is wrong or absent (a UI shows it as a warning), as opposed
    /// to merely not complete (a placeholder, a cut display).
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
            Note::NotCaptured | Note::Truncated => false,
        }
    }
}

/// One value, ready to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rendered {
    pub display: String,
    pub kind: Kind,
    /// Sorted, without duplicates.
    pub notes: Vec<Note>,
}

impl Rendered {
    fn new(display: impl Into<String>, kind: Kind, notes: &[Note]) -> Self {
        let mut notes = notes.to_vec();
        notes.sort();
        notes.dedup();
        Rendered { display: display.into(), kind, notes }
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
    match v {
        Value::Null => Rendered::new("null", Kind::Unknown, &[Note::NullValue]),
        Value::Bool(b) => Rendered::new(if *b { "Yes" } else { "No" }, Kind::Bool, &[]),
        Value::Number(n) => {
            let (text, kind) = number_text(n);
            Rendered::new(text, kind, &[])
        }
        Value::String(s) => Rendered::new(s.clone(), Kind::String, &[]),
        Value::Array(items) => render_list(items),
        Value::Object(_) => render_object(v),
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

fn render_object(v: &Value) -> Rendered {
    match v.get("_type").and_then(Value::as_str) {
        Some(kind @ "datetime") => return Rendered::new(format!("({kind}: not captured)"), Kind::Datetime, &[Note::NotCaptured]),
        Some(kind @ "array") => return Rendered::new(format!("({kind}: not captured)"), Kind::Array, &[Note::NotCaptured]),
        _ => {}
    }
    if let Some(t) = v.get("_unknown_type") {
        return Rendered::new(format!("(unknown capability type {})", py_json(t)), Kind::Unknown, &[Note::NotCaptured]);
    }
    if is_range_shape(v) {
        return render_range(v);
    }
    // `value_index` with no `values` is an enum that lost its list; `is_enum_shape` needs the list.
    if is_enum_shape(v) || (v.get("value_index").is_some() && v.get("lower").is_none()) {
        return render_enum(v);
    }
    let text = py_json(v);
    if text.chars().count() <= MAX_SHOWN {
        Rendered::new(text, Kind::Unknown, &[])
    } else {
        let cut: String = text.chars().take(MAX_SHOWN).collect();
        Rendered::new(format!("{cut}…"), Kind::Unknown, &[Note::Truncated])
    }
}

fn render_range(v: &Value) -> Rendered {
    if is_stepped_range(v) {
        if let Some(n) = range_value(v) {
            return Rendered::new(py_float(n), Kind::Range, &[]);
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
        Some(Value::Number(n)) => Rendered::new(number_text(n).0, Kind::Range, &[]),
        Some(other) => Rendered::malformed(format!("range value is not a number ({})", py_json(other)), &[Note::NoNumber]),
    }
}

fn render_enum(v: &Value) -> Rendered {
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
}

/// The one rule for which rows a filter keeps: a case-insensitive substring of the name, of the
/// displayed value, of the code in hex (`0xd053`, so `d053` too) or of the code in decimal.
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
}

pub fn render_snapshot(s: &Snapshot, filter: Option<&str>) -> RenderedSnapshot {
    let filter = RowFilter::new(filter.unwrap_or(""));
    let all = s.properties.len();
    // BTreeMap order: by name.
    let rows: Vec<Row> = s
        .properties
        .iter()
        .map(|(name, entry)| Row { name: name.clone(), code: entry.code, value: render_value(&entry.value) })
        .filter(|row| filter.matches(row))
        .collect();
    RenderedSnapshot {
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
    }
}

/// Text from a file or a camera, safe to print on a terminal: control characters (an escape
/// sequence in a camera name would otherwise reach the terminal) are written as `\u{..}` escapes.
pub fn printable(text: &str) -> String {
    text.chars().flat_map(|c| if c.is_control() { c.escape_default().collect::<Vec<_>>() } else { vec![c] }).collect()
}

impl RenderedSnapshot {
    /// The `show` output: a header line, then `name [0xCODE] = display` per row, aligned.
    /// `filter` is only echoed in the closing line.
    pub fn to_text(&self, filter: Option<&str>) -> String {
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
        let codes: Vec<String> = self.rows.iter().map(Row::code_hex).collect();
        let name_width = self.rows.iter().map(|r| printable(r.short_name()).chars().count()).max().unwrap_or(0);
        let code_width = codes.iter().map(|c| c.chars().count()).max().unwrap_or(0);
        for (row, code) in self.rows.iter().zip(&codes) {
            out.push_str(&format!(
                "{:<name_width$} [{code:<code_width$}] = {}\n",
                printable(row.short_name()),
                printable(&row.value.display)
            ));
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
        for note in Note::ALL {
            let tag = serde_json::to_value(note).unwrap();
            assert!(cases.iter().any(|c| c["notes"].as_array().is_some_and(|n| n.contains(&tag))), "no fixture case with note {tag}");
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
            assert_eq!(note.is_problem(), !matches!(note, Note::NotCaptured | Note::Truncated), "{note:?}");
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
}

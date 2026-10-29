//! Names for what a snapshot holds: display names and value labels, from files that are generated
//! or staged on the machine that has the Nikon SDK, never from this repository.
//!
//! Three optional sources, each read from `sdk-runtime/`:
//!
//! - `MaidLayer.config`: a capability's display name (`<description>`) and, for some, the names of
//!   its enumeration's values (`<resourcestring>VALUE,RESID,"Label"</resourcestring>`). A model has
//!   a section per firmware generation; the ones that apply are chosen the way
//!   `firmware::capability_map_for_fw` chooses them (`common` and every numeric version up to the
//!   snapshot's firmware major, a later section replacing an earlier one's entry for a code).
//! - `maid-labels.json`, written by `scripts/extract-maid-labels.py`: the base capability names
//!   (what a `cap_0x..` key is called) from `Maid3.h` and the value names the SDK's PDF documents.
//!
//! Every part is optional and a missing one is reported ([`Labels::unavailable`]), never replaced by
//! a guess: a caller shows the raw number and says why there is no name. A label list is only
//! trusted where it is unambiguous; a value two entries name differently, or one whose name depends
//! on another capability's state, is [`Lookup::Ambiguous`] and stays a bare number.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::config_parse::{extract_between, split_name_code, strip_open_tag};
use crate::firmware::schema_major;
use crate::transplant::PROPERTY_PREFIX;

/// The extractor's output file, next to `MaidLayer.config` in `sdk-runtime/`.
pub const LABELS_FILE: &str = "maid-labels.json";
/// The SDK's capability schema, in `sdk-runtime/`.
pub const SCHEMA_FILE: &str = "MaidLayer.config";
/// The `extractor_version` of `maid-labels.json` this reads.
const SUPPORTED_EXTRACTOR_VERSION: u64 = 1;

/// Where a value's label came from. Closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelSource {
    /// `MaidLayer.config`'s `resourcestring` list for the model.
    Schema,
    /// The SDK's `MAID3-COMMON(E).pdf`, through `maid-labels.json`.
    SdkDoc,
}

/// What a value's label lookup found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// The value has a name.
    Label { text: &'a str, source: LabelSource },
    /// The capability has a list of value names and this value is not in it.
    NotInLabels,
    /// The value is named, but not unambiguously (two different names, or a name that depends on
    /// another capability's state): it is not labelled.
    Ambiguous,
    /// No list of value names is known for the capability.
    NoLabels,
}

/// Why a part of the labels is missing. Closed; [`Unavailable::message`] is the text for a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unavailable {
    SchemaMissing { path: String },
    SchemaUnreadable { path: String, why: String },
    /// The schema has no section for the model (at that firmware).
    NoSection { model: String, firmware: Option<String> },
    LabelsFileMissing { path: String },
    LabelsFileInvalid { path: String, why: String },
}

impl Unavailable {
    pub fn message(&self) -> String {
        match self {
            Unavailable::SchemaMissing { path } => format!("display names and value labels: {path} not found (scripts/setup-sdk-runtime.sh stages it)"),
            Unavailable::SchemaUnreadable { path, why } => format!("display names and value labels: {path} cannot be read ({why})"),
            Unavailable::NoSection { model, firmware: Some(fw) } => {
                format!("display names and value labels: the schema has no section for {model} at firmware {fw}")
            }
            Unavailable::NoSection { model, firmware: None } => format!("display names and value labels: the schema has no section for {model}"),
            Unavailable::LabelsFileMissing { path } => {
                format!("base capability names and SDK-documented value names: {path} not found (scripts/extract-maid-labels.py writes it)")
            }
            Unavailable::LabelsFileInvalid { path, why } => {
                format!("base capability names and SDK-documented value names: {path} is not usable ({why}); run scripts/extract-maid-labels.py again")
            }
        }
    }
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

/// One capability's entry in the schema, for the model and firmware asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SchemaCapability {
    /// `kNkMAIDCapability_ModeA`.
    name: String,
    /// The `<description>` text; may be empty.
    display: String,
    /// Value -> label, for values with exactly one unconditional label.
    values: BTreeMap<i64, String>,
    /// Values the list names ambiguously.
    ambiguous: BTreeSet<i64>,
}

impl SchemaCapability {
    fn has_list(&self) -> bool {
        !self.values.is_empty() || !self.ambiguous.is_empty()
    }
}

/// The names of one enumeration the SDK's PDF documents.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DocEnum {
    values: BTreeMap<i64, String>,
}

/// The contents of one of the two files, and where it was read from (for messages).
#[derive(Debug, Clone, Copy)]
pub struct Source<'a> {
    pub path: &'a str,
    pub text: &'a str,
}

/// A display name two or more capabilities of the model share, which names none of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedDisplayName {
    /// As the first of them writes it.
    pub display: String,
    /// The capabilities that carry it, by code.
    pub codes: Vec<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct Labels {
    base: HashMap<u32, String>,
    schema: HashMap<u32, SchemaCapability>,
    /// By the short capability name (`ModeA`).
    doc: HashMap<String, DocEnum>,
    shared_display_names: Vec<SharedDisplayName>,
    unavailable: Vec<Unavailable>,
}

// ── MaidLayer.config ──────────────────────────────────────────────────────

/// One `<resourcestring>` body: `VALUE,RESID,"Label"`, or the same with more fields after the
/// label (`22800,-1,"20000",34105,0`: the name holds only while capability 34105 is 0).
#[derive(Debug, PartialEq, Eq)]
enum ResourceString {
    Plain { value: i64, label: String },
    /// A name that depends on another capability's state; the value is known, the name is not usable.
    Conditional { value: i64 },
    Malformed,
}

fn parse_resource_string(body: &str) -> ResourceString {
    let mut parts = body.splitn(3, ',');
    let (Some(value), Some(_resid), Some(rest)) = (parts.next(), parts.next(), parts.next()) else { return ResourceString::Malformed };
    let (Ok(value), Some(rest)) = (value.trim().parse::<i64>(), rest.trim().strip_prefix('"')) else { return ResourceString::Malformed };
    match rest.strip_suffix('"') {
        Some(label) => ResourceString::Plain { value, label: label.to_string() },
        None if rest.contains("\",") => ResourceString::Conditional { value },
        None => ResourceString::Malformed,
    }
}

/// The text between the first two quotes of `flags,iconId,"label"`, as `maid_layer` reads it (a
/// parity test pins that the two agree on the real file).
fn description_label(body: &str) -> String {
    let Some(open) = body.find('"') else { return String::new() };
    let Some(len) = body[open + 1..].find('"') else { return String::new() };
    body[open + 1..open + 1 + len].to_string()
}

#[derive(Debug, Default)]
struct ScannedCapability {
    code: u32,
    name: String,
    display: String,
    entries: Vec<ResourceString>,
}

#[derive(Debug)]
struct ScannedSection {
    version: String,
    capabilities: Vec<ScannedCapability>,
}

/// The sections of `model` in `config`, in file order. Lines of other models are skipped unread.
fn scan_schema(config: &str, model: &str) -> Result<Vec<ScannedSection>, String> {
    let mut sections: Vec<ScannedSection> = Vec::new();
    let mut in_model = false;
    let mut in_capability = false;
    for (i, raw) in config.lines().enumerate() {
        let line = raw.trim();
        if let Some(name) = strip_open_tag(line, "model") {
            in_model = name == model;
            in_capability = false;
            if in_model {
                sections.push(ScannedSection { version: String::new(), capabilities: Vec::new() });
            }
            continue;
        }
        if line == "</model>" {
            in_model = false;
            in_capability = false;
            continue;
        }
        if !in_model {
            continue;
        }
        let section = sections.last_mut().expect("a section was pushed when the model started");
        if in_capability {
            let cap = section.capabilities.last_mut().expect("a capability was pushed when it started");
            if line == "</capability>" {
                in_capability = false;
            } else if let Some(body) = extract_between(line, "<description>", "</description>") {
                cap.display = description_label(body);
            } else if let Some(body) = extract_between(line, "<resourcestring>", "</resourcestring>") {
                cap.entries.push(parse_resource_string(body));
            }
            continue;
        }
        if let Some(v) = extract_between(line, "<version>", "</version>") {
            section.version = v.to_string();
        } else if let Some(payload) = strip_open_tag(line, "capability") {
            let (name, code) = split_name_code(payload).map_err(|d| format!("line {}: malformed capability tag: {d}", i + 1))?;
            section.capabilities.push(ScannedCapability { code, name, ..ScannedCapability::default() });
            in_capability = true;
        }
    }
    Ok(sections)
}

/// Fold a capability's entries into its usable labels: one label per value, a value with two
/// different labels or a conditional one ambiguous. Entries that did not parse are counted by the caller.
fn schema_capability(cap: &ScannedCapability) -> SchemaCapability {
    let mut values: BTreeMap<i64, String> = BTreeMap::new();
    let mut ambiguous: BTreeSet<i64> = BTreeSet::new();
    for entry in &cap.entries {
        match entry {
            ResourceString::Plain { value, label } => match values.get(value) {
                Some(seen) if seen != label => {
                    ambiguous.insert(*value);
                }
                Some(_) => {}
                None => {
                    values.insert(*value, label.clone());
                }
            },
            ResourceString::Conditional { value } => {
                ambiguous.insert(*value);
            }
            ResourceString::Malformed => {}
        }
    }
    for value in &ambiguous {
        values.remove(value);
    }
    SchemaCapability { name: cap.name.clone(), display: cap.display.clone(), values, ambiguous }
}

/// A display name for comparing: letters and digits, lower case (`Flash Sync Time` is `FlashSyncTime`).
fn display_key(display: &str) -> String {
    display.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

/// Blank the display name of every capability whose name another capability of the model also
/// has, and say which. The config has copy-paste slips (one capability described with another's
/// display name); a name two capabilities share
/// names neither, and showing it would put a wrong name next to a right number.
fn drop_shared_display_names(schema: &mut HashMap<u32, SchemaCapability>) -> Vec<SharedDisplayName> {
    let mut by_key: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for (code, cap) in schema.iter() {
        let key = display_key(&cap.display);
        if !key.is_empty() {
            by_key.entry(key).or_default().push(*code);
        }
    }
    let mut shared = Vec::new();
    for mut codes in by_key.into_values().filter(|codes| codes.len() > 1) {
        codes.sort_unstable();
        shared.push(SharedDisplayName { display: schema[&codes[0]].display.clone(), codes: codes.clone() });
        for code in codes {
            if let Some(cap) = schema.get_mut(&code) {
                cap.display.clear();
            }
        }
    }
    shared.sort_by(|a, b| a.codes.cmp(&b.codes));
    shared
}

/// Whether a section applies to a snapshot of firmware major `fw_major`: the same rule as
/// `firmware::capability_map_for_fw`.
fn section_applies(version: &str, fw_major: Option<u32>) -> bool {
    match (schema_major(version), fw_major) {
        (Some(section), Some(fw)) => section <= fw,
        _ => true,
    }
}

// ── maid-labels.json ──────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct DocLabels {
    base: HashMap<u32, String>,
    capabilities: HashMap<String, DocEnum>,
}

fn parse_doc_labels(text: &str) -> Result<DocLabels, String> {
    let root: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    let version = root.pointer("/source/extractor_version").and_then(Value::as_u64);
    if version != Some(SUPPORTED_EXTRACTOR_VERSION) {
        return Err(format!("extractor_version {} is not the {SUPPORTED_EXTRACTOR_VERSION} this reads", version.map_or("absent".into(), |v| v.to_string())));
    }
    let mut out = DocLabels::default();
    let base = root.get("base_capabilities").and_then(Value::as_object).ok_or("no base_capabilities object")?;
    for (code, name) in base {
        let code: u32 = code.parse().map_err(|_| format!("base capability number {code:?} is not a number"))?;
        let name = name.as_str().ok_or_else(|| format!("base capability {code} has no name"))?;
        out.base.insert(code, name.to_string());
    }
    let capabilities = root.get("capabilities").and_then(Value::as_object).ok_or("no capabilities object")?;
    for (name, entry) in capabilities {
        let values = entry.get("values").and_then(Value::as_object).ok_or_else(|| format!("{name} has no values object"))?;
        let mut map = BTreeMap::new();
        for (value, label) in values {
            let value: i64 = value.parse().map_err(|_| format!("{name}: value {value:?} is not a number"))?;
            let label = label.as_str().ok_or_else(|| format!("{name}: value {value} has no label"))?;
            map.insert(value, label.to_string());
        }
        out.capabilities.insert(name.clone(), DocEnum { values: map });
    }
    Ok(out)
}

// ── Labels ────────────────────────────────────────────────────────────────

impl Labels {
    /// No names at all, and nothing reported missing: the choice not to use any (`show --raw`).
    pub fn none() -> Labels {
        Labels::default()
    }

    /// Everything `sdk_runtime_dir` offers for `model`, the newest firmware generation's entries
    /// where a code is in several.
    pub fn load(sdk_runtime_dir: &Path, model: &str) -> Labels {
        Labels::load_for(sdk_runtime_dir, model, None)
    }

    /// As [`Labels::load`], for a snapshot of `firmware` (`"5.31"`): sections of a later firmware
    /// generation than it are not used. An unparsable or absent firmware uses every section.
    pub fn load_for(sdk_runtime_dir: &Path, model: &str, firmware: Option<&str>) -> Labels {
        Labels::load_from(&sdk_runtime_dir.join(SCHEMA_FILE), &sdk_runtime_dir.join(LABELS_FILE), model, firmware)
    }

    /// As [`Labels::load_for`] with the two files named (`--schema` may point anywhere).
    pub fn load_from(schema_path: &Path, labels_path: &Path, model: &str, firmware: Option<&str>) -> Labels {
        let mut unavailable = Vec::new();
        let schema_text = read_optional(schema_path, Unavailable::SchemaMissing { path: schema_path.display().to_string() }, &mut unavailable, |why| {
            Unavailable::SchemaUnreadable { path: schema_path.display().to_string(), why }
        });
        let labels_text = read_optional(labels_path, Unavailable::LabelsFileMissing { path: labels_path.display().to_string() }, &mut unavailable, |why| {
            Unavailable::LabelsFileInvalid { path: labels_path.display().to_string(), why }
        });
        let schema_path = schema_path.display().to_string();
        let labels_path = labels_path.display().to_string();
        let mut labels = Labels::from_sources(
            schema_text.as_deref().map(|text| Source { path: &schema_path, text }),
            labels_text.as_deref().map(|text| Source { path: &labels_path, text }),
            model,
            firmware,
        );
        unavailable.append(&mut labels.unavailable);
        labels.unavailable = unavailable;
        labels
    }

    /// The labels from the contents of the two files (`None`: the file is not there and the caller
    /// has said so). No I/O.
    pub fn from_sources(schema: Option<Source<'_>>, labels: Option<Source<'_>>, model: &str, firmware: Option<&str>) -> Labels {
        let mut out = Labels::default();
        if let Some(Source { path, text }) = schema {
            match scan_schema(text, model) {
                Err(why) => out.unavailable.push(Unavailable::SchemaUnreadable { path: path.to_string(), why }),
                Ok(sections) => {
                    let fw_major = firmware.and_then(schema_major);
                    let mut any = false;
                    for section in sections.iter().filter(|s| section_applies(&s.version, fw_major)) {
                        any = true;
                        for cap in &section.capabilities {
                            // File order: a later generation's entry replaces an earlier one's.
                            out.schema.insert(cap.code, schema_capability(cap));
                        }
                    }
                    if !any {
                        out.unavailable.push(Unavailable::NoSection { model: model.to_string(), firmware: firmware.map(str::to_string) });
                    }
                    out.shared_display_names = drop_shared_display_names(&mut out.schema);
                }
            }
        }
        if let Some(Source { path, text }) = labels {
            match parse_doc_labels(text) {
                Ok(doc) => {
                    out.base = doc.base;
                    out.doc = doc.capabilities;
                }
                Err(why) => out.unavailable.push(Unavailable::LabelsFileInvalid { path: path.to_string(), why }),
            }
        }
        out
    }

    /// The display names the schema gives several capabilities at once, which are therefore not
    /// shown for any of them.
    pub fn shared_display_names(&self) -> &[SharedDisplayName] {
        &self.shared_display_names
    }

    /// Why a part of the names is missing; empty when everything asked for was found.
    pub fn unavailable(&self) -> &[Unavailable] {
        &self.unavailable
    }

    /// One line saying what is missing, for a person; `None` when nothing is.
    pub fn note(&self) -> Option<String> {
        if self.unavailable.is_empty() {
            return None;
        }
        let parts: Vec<String> = self.unavailable.iter().map(Unavailable::message).collect();
        Some(format!("names unavailable, showing raw values: {}", parts.join("; ")))
    }

    /// The name of a base capability (the code 9 has one), from `Maid3.h`.
    pub fn base_name(&self, code: u32) -> Option<&str> {
        self.base.get(&code).map(String::as_str)
    }

    /// A capability's display name (`Mode A`), from the schema, or the base capability's
    /// name for a base code. `None` when unknown or when the schema's description is empty.
    pub fn capability_display_name(&self, code: u32) -> Option<&str> {
        let from_schema = self.schema.get(&code).map(|c| c.display.as_str()).filter(|d| !d.is_empty());
        from_schema.or_else(|| self.base_name(code))
    }

    /// The short name (`ModeA`) of a code in the schema, if it has one.
    fn schema_short_name(&self, code: u32) -> Option<&str> {
        self.schema.get(&code).map(|c| c.name.strip_prefix(PROPERTY_PREFIX).unwrap_or(&c.name))
    }

    /// What names `value` of the capability with `code`. `name` (the snapshot's property name) is
    /// used for the SDK-document lookup when the schema does not know the code. The schema's list
    /// is asked first; a value it lacks is asked of the document.
    pub fn lookup_value(&self, code: u32, name: Option<&str>, value: i64) -> Lookup<'_> {
        let schema = self.schema.get(&code);
        if let Some(s) = schema {
            if let Some(text) = s.values.get(&value) {
                return Lookup::Label { text, source: LabelSource::Schema };
            }
            if s.ambiguous.contains(&value) {
                return Lookup::Ambiguous;
            }
        }
        let short = self.schema_short_name(code).or_else(|| name.map(|n| n.strip_prefix(PROPERTY_PREFIX).unwrap_or(n)));
        let doc = short.and_then(|n| self.doc.get(n));
        if let Some(text) = doc.and_then(|d| d.values.get(&value)) {
            return Lookup::Label { text, source: LabelSource::SdkDoc };
        }
        if schema.is_some_and(SchemaCapability::has_list) || doc.is_some() { Lookup::NotInLabels } else { Lookup::NoLabels }
    }

    /// The label of `value` of the capability with `code`, when it has one.
    pub fn value_label(&self, code: u32, value: i64) -> Option<&str> {
        match self.lookup_value(code, None, value) {
            Lookup::Label { text, .. } => Some(text),
            _ => None,
        }
    }
}

/// The text of `path`, or `None` with the reason pushed: `missing` when there is no such file,
/// `unreadable(why)` for any other failure.
fn read_optional(path: &Path, missing: Unavailable, unavailable: &mut Vec<Unavailable>, unreadable: impl FnOnce(String) -> Unavailable) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            unavailable.push(missing);
            None
        }
        Err(e) => {
            unavailable.push(unreadable(e.to_string()));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maid_layer::MaidLayerConfig;

    // A schema in the shape of the real one, invented here: three generations of model `M`, one of `N`.
    const CONFIG: &str = r#"<ProfileVersion>1.0</ProfileVersion>
<model:M>
	<version>common</version>
	<caplist>
		<capability:kNkMAIDCapability_Shared-1001>
			<description>0,11100,"Shared common"</description>
			<resourcestringList>
				<resourcestring>0,1,"Common zero"</resourcestring>
			</resourcestringList>
		</capability>
		<capability:kNkMAIDCapability_Moved-1002>
			<description>0,11100,"Moved common"</description>
			<resourcestringList>
				<resourcestring>0,1,"Zero before"</resourcestring>
			</resourcestringList>
		</capability>
	</caplist>
</model>
<model:N>
	<version>common</version>
	<caplist>
		<capability:kNkMAIDCapability_Shared-1001>
			<description>0,11100,"Other model"</description>
		</capability>
	</caplist>
</model>
<model:M>
	<version>2.0</version>
	<caplist>
		<capability:kNkMAIDCapability_Moved-1002>
			<description>0,11100,"Moved 2.0"</description>
			<resourcestringList>
				<resourcestring>0,1,"Zero two"</resourcestring>
				<resourcestring>1,2,"One two"</resourcestring>
			</resourcestringList>
		</capability>
	</caplist>
</model>
<model:M>
	<version>3.0</version>
	<caplist>
		<capability:kNkMAIDCapability_Moved-1002>
			<description>0,11100,"Moved 3.0"</description>
			<resourcestringList>
			</resourcestringList>
		</capability>
		<capability:kNkMAIDCapability_Late-1003>
			<description>0,11100,"Late"</description>
		</capability>
	</caplist>
</model>
"#;

    fn src(text: &str) -> Option<Source<'_>> {
        Some(Source { path: "test", text })
    }

    fn schema_only(model: &str, firmware: Option<&str>) -> Labels {
        Labels::from_sources(src(CONFIG), None, model, firmware)
    }

    fn display(l: &Labels, code: u32) -> Option<&str> {
        l.capability_display_name(code)
    }

    // ── one resourcestring ────────────────────────────────────────────

    #[test]
    fn a_resource_string_is_value_resid_and_a_quoted_label() {
        let plain = |value, label: &str| ResourceString::Plain { value, label: label.to_string() };
        assert_eq!(parse_resource_string("1,18942,\"M(6192x4128)\""), plain(1, "M(6192x4128)"));
        assert_eq!(parse_resource_string("-1,0,\"Unset\""), plain(-1, "Unset"));
        assert_eq!(parse_resource_string("7,4,\"Seven, with a comma\""), plain(7, "Seven, with a comma"));
        assert_eq!(parse_resource_string("0,1,\"\""), plain(0, ""), "an empty label is a label");
        assert_eq!(parse_resource_string("0,1,\"a \"b\" c\""), plain(0, "a \"b\" c"), "quotes inside a label are kept");
        assert_eq!(parse_resource_string(" 3 ,1,\"x\""), plain(3, "x"), "the value may have spaces around it");
    }

    #[test]
    fn a_resource_string_with_fields_after_the_label_is_conditional_not_plain() {
        assert_eq!(parse_resource_string("22800,-1,\"20000\",34105,0"), ResourceString::Conditional { value: 22800 });
        assert_eq!(parse_resource_string("22800,-1,\"22800\",34105,1"), ResourceString::Conditional { value: 22800 });
    }

    #[test]
    fn what_is_not_a_resource_string_is_malformed_and_never_a_label() {
        for body in ["", "1", "1,2", "x,2,\"a\"", "1,2,a", "1,2,\"a", "1,2,\"a\" trailing", "1.5,2,\"a\"", ",,\"a\""] {
            assert_eq!(parse_resource_string(body), ResourceString::Malformed, "{body:?}");
        }
    }

    #[test]
    fn a_description_label_is_between_its_first_two_quotes() {
        assert_eq!(description_label("0,11100,\"Mode A\""), "Mode A");
        assert_eq!(description_label("0,11100,\"a\" \"b\""), "a");
        assert_eq!(description_label("0,11100,\"\""), "");
        assert_eq!(description_label("0,11100,"), "");
        assert_eq!(description_label("0,11100,\"unterminated"), "");
    }

    #[test]
    fn the_schema_scan_reads_the_same_display_names_as_the_maid_layer_parser() {
        let parsed = MaidLayerConfig::parse(CONFIG).unwrap();
        for section in &parsed.sections {
            let ours = scan_schema(CONFIG, &section.model).unwrap();
            let mine: Vec<(&str, u32, &str)> = ours
                .iter()
                .filter(|s| s.version == section.version)
                .flat_map(|s| s.capabilities.iter().map(|c| (c.name.as_str(), c.code, c.display.as_str())))
                .collect();
            let theirs: Vec<(&str, u32, &str)> = section.capabilities.iter().map(|c| (c.name.as_str(), c.code, c.display.as_str())).collect();
            assert_eq!(mine, theirs, "{} {}", section.model, section.version);
        }
    }

    // ── which sections apply ──────────────────────────────────────────

    #[test]
    fn a_later_generations_entry_replaces_an_earlier_ones_and_the_model_is_matched_exactly() {
        let l = schema_only("M", None);
        assert_eq!(display(&l, 1001), Some("Shared common"), "only M's common entry, not N's");
        assert_eq!(display(&l, 1002), Some("Moved 3.0"), "no firmware: the newest generation");
        assert_eq!(display(&l, 1003), Some("Late"));
        assert_eq!(schema_only("N", None).capability_display_name(1001), Some("Other model"));
        for near in ["m", "M ", "MM", ""] {
            assert!(schema_only(near, None).unavailable().iter().any(|u| matches!(u, Unavailable::NoSection { .. })), "{near:?} is not M");
        }
    }

    #[test]
    fn firmware_picks_the_generations_up_to_its_major_at_below_and_above_the_boundary() {
        // Section 2.0: in for firmware major 2, out for major 1.
        assert_eq!(display(&schema_only("M", Some("2.00")), 1002), Some("Moved 2.0"), "at");
        assert_eq!(display(&schema_only("M", Some("2.99")), 1002), Some("Moved 2.0"), "major 2 up to 2.99");
        assert_eq!(display(&schema_only("M", Some("1.99")), 1002), Some("Moved common"), "below");
        assert_eq!(display(&schema_only("M", Some("3.00")), 1002), Some("Moved 3.0"), "above: 3.0 applies too");
        assert_eq!(display(&schema_only("M", Some("2.00")), 1003), None, "3.0's own capability is not in firmware 2");
        assert_eq!(display(&schema_only("M", Some("0.9")), 1002), Some("Moved common"), "common always applies");
    }

    #[test]
    fn a_firmware_that_is_not_a_version_uses_every_section() {
        for fw in ["", "abc", "common", ".5"] {
            assert_eq!(display(&schema_only("M", Some(fw)), 1002), Some("Moved 3.0"), "{fw:?}");
        }
    }

    #[test]
    fn a_later_generation_replaces_the_whole_entry_including_an_emptied_list() {
        assert_eq!(schema_only("M", Some("1.0")).value_label(1002, 0), Some("Zero before"));
        assert_eq!(schema_only("M", Some("2.0")).value_label(1002, 0), Some("Zero two"));
        assert_eq!(schema_only("M", Some("2.0")).value_label(1002, 1), Some("One two"));
        // 3.0 lists the capability with no names: its names are gone, not inherited from 2.0.
        let l = schema_only("M", Some("3.0"));
        assert_eq!(l.value_label(1002, 0), None);
        assert_eq!(l.lookup_value(1002, None, 0), Lookup::NoLabels);
    }

    #[test]
    fn a_model_with_no_applicable_section_is_reported_not_silently_empty() {
        let none = Labels::from_sources(src("<model:X>\n<version>5.0</version>\n</model>\n"), None, "X", Some("4.1"));
        assert_eq!(none.unavailable(), [Unavailable::NoSection { model: "X".into(), firmware: Some("4.1".into()) }]);
        assert!(none.note().unwrap().contains("firmware 4.1"));
        let none = schema_only("Nope", None);
        assert_eq!(none.unavailable(), [Unavailable::NoSection { model: "Nope".into(), firmware: None }]);
        assert!(none.note().unwrap().contains("no section for Nope"));
    }

    #[test]
    fn a_malformed_capability_tag_makes_the_schema_unreadable_and_says_the_line() {
        let bad = "<model:M>\n<version>1.0</version>\n<capability:NoCodeHere>\n</capability>\n</model>\n";
        let l = Labels::from_sources(src(bad), None, "M", None);
        match l.unavailable() {
            [Unavailable::SchemaUnreadable { path, why }] => {
                assert_eq!(path, "test");
                assert!(why.contains("line 3"), "{why}");
            }
            other => panic!("{other:?}"),
        }
        // Another model's malformed tag is not read, so not an error for this one.
        assert!(Labels::from_sources(src(bad), None, "Other", None).unavailable().iter().all(|u| !matches!(u, Unavailable::SchemaUnreadable { .. })));
    }

    #[test]
    fn a_display_name_two_capabilities_share_names_neither_and_their_value_names_stay() {
        let cfg = "<model:M>\n<version>common</version>\n\
            <capability:kNkMAIDCapability_First-1>\n<description>0,1,\"Shared Words\"</description>\n\
            <resourcestringList>\n<resourcestring>0,0,\"Off\"</resourcestring>\n</resourcestringList>\n</capability>\n\
            <capability:kNkMAIDCapability_Second-2>\n<description>0,1,\"shared words\"</description>\n</capability>\n\
            <capability:kNkMAIDCapability_Other-3>\n<description>0,1,\"Other thing\"</description>\n</capability>\n\
            <capability:kNkMAIDCapability_Punct-4>\n<description>0,1,\"Other-Thing!\"</description>\n</capability>\n\
            <capability:kNkMAIDCapability_Alone-5>\n<description>0,1,\"Alone\"</description>\n</capability>\n</model>\n";
        let l = Labels::from_sources(src(cfg), None, "M", None);
        assert_eq!([1, 2, 3, 4, 5].map(|c| l.capability_display_name(c)), [None, None, None, None, Some("Alone")]);
        assert_eq!(l.value_label(1, 0), Some("Off"), "only the display name is dropped");
        let codes: Vec<&[u32]> = l.shared_display_names().iter().map(|s| s.codes.as_slice()).collect();
        assert_eq!(codes, [&[1, 2][..], &[3, 4][..]]);
        assert_eq!(l.shared_display_names()[0].display, "Shared Words");
    }

    #[test]
    fn one_capability_named_the_same_in_two_generations_is_not_a_shared_name() {
        let cfg = "<model:M>\n<version>1.0</version>\n<capability:kNkMAIDCapability_A-1>\n<description>0,1,\"Same\"</description>\n</capability>\n</model>\n\
            <model:M>\n<version>2.0</version>\n<capability:kNkMAIDCapability_A-1>\n<description>0,1,\"Same\"</description>\n</capability>\n</model>\n";
        let l = Labels::from_sources(src(cfg), None, "M", None);
        assert_eq!(l.capability_display_name(1), Some("Same"));
        assert!(l.shared_display_names().is_empty());
    }

    // ── value names ───────────────────────────────────────────────────

    fn caps_config(strings: &[&str]) -> String {
        let mut text = String::from("<model:M>\n<version>common</version>\n<capability:kNkMAIDCapability_C-5>\n<description>0,1,\"C\"</description>\n<resourcestringList>\n");
        for s in strings {
            text.push_str(&format!("<resourcestring>{s}</resourcestring>\n"));
        }
        text.push_str("</resourcestringList>\n</capability>\n</model>\n");
        text
    }

    fn lookup_in(strings: &[&str], value: i64) -> String {
        let cfg = caps_config(strings);
        let l = Labels::from_sources(src(&cfg), None, "M", None);
        match l.lookup_value(5, None, value) {
            Lookup::Label { text, source } => format!("{text}/{source:?}"),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn a_value_is_named_not_in_the_list_ambiguous_or_has_no_list() {
        assert_eq!(lookup_in(&["1,0,\"One\""], 1), "One/Schema");
        assert_eq!(lookup_in(&["1,0,\"One\""], 2), "NotInLabels");
        assert_eq!(lookup_in(&[], 1), "NoLabels");
        assert_eq!(lookup_in(&["1,0,\"One\"", "1,0,\"Uno\""], 1), "Ambiguous", "two names");
        assert_eq!(lookup_in(&["1,0,\"One\"", "1,0,\"One\""], 1), "One/Schema", "one name twice");
        assert_eq!(lookup_in(&["1,0,\"One\"", "2,-1,\"Two\",34105,1"], 2), "Ambiguous", "depends on another capability");
        assert_eq!(lookup_in(&["1,0,\"One\"", "2,-1,\"Two\",34105,1"], 1), "One/Schema", "its neighbour is still named");
        assert_eq!(lookup_in(&["1,0,\"One\"", "2,0,\"Two\"", "garbage"], 2), "Two/Schema", "a bad line costs only itself");
        assert_eq!(lookup_in(&["garbage"], 1), "NoLabels", "a list of nothing readable is no list");
        // A conditional entry first, the same value plain after it: still ambiguous, whatever the order.
        assert_eq!(lookup_in(&["2,-1,\"Two\",34105,1", "2,0,\"Two\""], 2), "Ambiguous");
    }

    #[test]
    fn the_schema_is_asked_first_and_the_document_fills_what_it_lacks() {
        let cfg = caps_config(&["0,0,\"Schema zero\""]);
        let doc = r#"{"source":{"extractor_version":1},"base_capabilities":{"9":"Base Nine"},
            "capabilities":{"C":{"values":{"0":"Doc zero","1":"Doc one"}}}}"#;
        let l = Labels::from_sources(src(&cfg), Some(Source { path: "doc", text: doc }), "M", None);
        assert_eq!(l.lookup_value(5, None, 0), Lookup::Label { text: "Schema zero", source: LabelSource::Schema });
        assert_eq!(l.lookup_value(5, None, 1), Lookup::Label { text: "Doc one", source: LabelSource::SdkDoc });
        assert_eq!(l.lookup_value(5, None, 2), Lookup::NotInLabels);
        // The document alone, found by the property name; the prefix is optional.
        let doc_only = Labels::from_sources(None, Some(Source { path: "doc", text: doc }), "M", None);
        assert_eq!(doc_only.lookup_value(5, Some("kNkMAIDCapability_C"), 1), Lookup::Label { text: "Doc one", source: LabelSource::SdkDoc });
        assert_eq!(doc_only.lookup_value(5, Some("C"), 1), Lookup::Label { text: "Doc one", source: LabelSource::SdkDoc });
        assert_eq!(doc_only.lookup_value(5, None, 1), Lookup::NoLabels, "no schema and no name: nothing to look it up by");
        assert_eq!(doc_only.lookup_value(5, Some("kNkMAIDCapability_D"), 1), Lookup::NoLabels);
        assert_eq!(doc_only.value_label(5, 1), None, "value_label has only the code");
    }

    #[test]
    fn display_names_fall_back_to_the_base_capability_name_and_an_empty_description_is_not_a_name() {
        let cfg = "<model:M>\n<version>common</version>\n<capability:kNkMAIDCapability_Blank-9>\n<description>0,1,\"\"</description>\n</capability>\n</model>\n";
        let doc = r#"{"source":{"extractor_version":1},"base_capabilities":{"9":"Base Nine","12":"Base Twelve"},"capabilities":{}}"#;
        let l = Labels::from_sources(src(cfg), Some(Source { path: "doc", text: doc }), "M", None);
        assert_eq!(l.base_name(9), Some("Base Nine"));
        assert_eq!(l.base_name(10), None);
        assert_eq!(l.capability_display_name(9), Some("Base Nine"), "the schema's empty description does not hide the base name");
        assert_eq!(l.capability_display_name(12), Some("Base Twelve"));
        assert_eq!(l.capability_display_name(13), None);
        assert_eq!(Labels::none().capability_display_name(9), None);
    }

    // ── maid-labels.json ──────────────────────────────────────────────

    fn doc_error(text: &str) -> String {
        match Labels::from_sources(None, Some(Source { path: "doc", text }), "M", None).unavailable() {
            [Unavailable::LabelsFileInvalid { path, why }] => {
                assert_eq!(path, "doc");
                why.clone()
            }
            other => panic!("{other:?} for {text}"),
        }
    }

    #[test]
    fn a_labels_file_that_is_not_the_extractors_is_refused_whole_with_the_reason() {
        assert!(doc_error("{ nope").contains("not JSON"));
        let ok = r#"{"source":{"extractor_version":1},"base_capabilities":{},"capabilities":{}}"#;
        assert!(Labels::from_sources(None, Some(Source { path: "doc", text: ok }), "M", None).unavailable().is_empty());
        assert!(doc_error(r#"{"base_capabilities":{},"capabilities":{}}"#).contains("extractor_version absent"));
        assert!(doc_error(r#"{"source":{"extractor_version":2},"base_capabilities":{},"capabilities":{}}"#).contains("extractor_version 2"));
        assert!(doc_error(r#"{"source":{"extractor_version":1},"capabilities":{}}"#).contains("base_capabilities"));
        assert!(doc_error(r#"{"source":{"extractor_version":1},"base_capabilities":{}}"#).contains("capabilities"));
        assert!(doc_error(r#"{"source":{"extractor_version":1},"base_capabilities":{"x":"A"},"capabilities":{}}"#).contains("\"x\""));
        assert!(doc_error(r#"{"source":{"extractor_version":1},"base_capabilities":{"1":5},"capabilities":{}}"#).contains("no name"));
        assert!(doc_error(r#"{"source":{"extractor_version":1},"base_capabilities":{},"capabilities":{"C":{}}}"#).contains("no values"));
        assert!(doc_error(r#"{"source":{"extractor_version":1},"base_capabilities":{},"capabilities":{"C":{"values":{"a":"x"}}}}"#).contains("\"a\""));
        assert!(doc_error(r#"{"source":{"extractor_version":1},"base_capabilities":{},"capabilities":{"C":{"values":{"1":7}}}}"#).contains("no label"));
        // One bad entry costs the whole file: a half-trusted list is worse than none.
        let half = r#"{"source":{"extractor_version":1},"base_capabilities":{"9":"Base Nine"},"capabilities":{"C":{"values":{"1":7}}}}"#;
        let l = Labels::from_sources(None, Some(Source { path: "doc", text: half }), "M", None);
        assert_eq!(l.base_name(9), None);
    }

    // ── files ─────────────────────────────────────────────────────────

    #[test]
    fn load_reads_both_files_from_the_directory_and_load_for_picks_the_firmware() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(SCHEMA_FILE), CONFIG).unwrap();
        std::fs::write(d.path().join(LABELS_FILE), r#"{"source":{"extractor_version":1},"base_capabilities":{"9":"Base Nine"},"capabilities":{}}"#).unwrap();
        let l = Labels::load(d.path(), "M");
        assert!(l.unavailable().is_empty() && l.note().is_none());
        assert_eq!((l.capability_display_name(1002), l.base_name(9)), (Some("Moved 3.0"), Some("Base Nine")));
        assert_eq!(Labels::load_for(d.path(), "M", Some("2.00")).capability_display_name(1002), Some("Moved 2.0"));
    }

    #[test]
    fn a_missing_file_is_reported_once_with_its_path_and_the_other_part_still_loads() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(SCHEMA_FILE), CONFIG).unwrap();
        let l = Labels::load(d.path(), "M");
        assert_eq!(l.capability_display_name(1001), Some("Shared common"));
        assert_eq!(l.unavailable().len(), 1);
        let note = l.note().unwrap();
        assert!(note.contains(LABELS_FILE) && note.contains("extract-maid-labels.py") && !note.contains("not found (scripts/setup"), "{note}");
        let both = Labels::load(&d.path().join("nowhere"), "M");
        assert_eq!(both.unavailable().len(), 2, "{:?}", both.unavailable());
        assert!(both.note().unwrap().starts_with("names unavailable, showing raw values: "));
        assert!(Labels::none().note().is_none(), "choosing no labels is not a gap");
    }

    #[test]
    fn a_file_that_cannot_be_read_is_unreadable_not_missing_and_a_bad_labels_file_is_named() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join(SCHEMA_FILE)).unwrap(); // a directory where the file belongs
        std::fs::write(d.path().join(LABELS_FILE), "{ nope").unwrap();
        let l = Labels::load(d.path(), "M");
        assert!(matches!(l.unavailable(), [Unavailable::SchemaUnreadable { .. }, Unavailable::LabelsFileInvalid { .. }]), "{:?}", l.unavailable());
        let note = l.note().unwrap();
        assert!(note.contains("cannot be read") && note.contains("not usable") && note.contains(LABELS_FILE), "{note}");
    }

    #[test]
    fn the_schema_is_found_wherever_schema_points_and_the_labels_beside_it() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("custom.config"), CONFIG).unwrap();
        let l = Labels::load_from(&d.path().join("custom.config"), &d.path().join(LABELS_FILE), "M", None);
        assert_eq!(l.capability_display_name(1001), Some("Shared common"));
        assert!(matches!(l.unavailable(), [Unavailable::LabelsFileMissing { .. }]));
    }

    // ── the real files (optional) ─────────────────────────────────────

    /// Whether this run must have the gitignored `sdk-runtime/maid-labels.json`: the repo's own
    /// switch (`REQUIRE_SDK_RUNTIME`) or a CI run, as `ops::baseline` decides it. Anything but
    /// empty, `0` or `false` counts as set.
    fn sdk_runtime_required(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
        ["REQUIRE_SDK_RUNTIME", "CI"].iter().any(|name| var(name).is_some_and(|v| !v.is_empty() && v != "0" && v != "false"))
    }

    #[test]
    fn the_sdk_runtime_requirement_is_decided_by_both_switches() {
        fn env<'a>(set: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
            move |name| set.iter().find(|(k, _)| *k == name).map(|(_, v)| std::ffi::OsString::from(*v))
        }
        assert!(!sdk_runtime_required(env(&[])));
        assert!(sdk_runtime_required(env(&[("REQUIRE_SDK_RUNTIME", "1")])));
        assert!(sdk_runtime_required(env(&[("CI", "true")])), "CI alone requires it");
        for off in ["", "0", "false"] {
            assert!(!sdk_runtime_required(env(&[("CI", off)])), "CI={off:?}");
            assert!(!sdk_runtime_required(env(&[("REQUIRE_SDK_RUNTIME", off)])), "REQUIRE_SDK_RUNTIME={off:?}");
        }
        assert!(sdk_runtime_required(env(&[("CI", "0"), ("REQUIRE_SDK_RUNTIME", "1")])));
    }

    #[test]
    fn the_real_generated_labels_load_and_agree_with_the_schema_parser() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk-runtime");
        let labels_path = dir.join(LABELS_FILE);
        if !labels_path.exists() {
            assert!(
                !sdk_runtime_required(|n| std::env::var_os(n)),
                "REQUIRE_SDK_RUNTIME or CI is set but {labels_path:?} does not exist (run scripts/extract-maid-labels.py)"
            );
            eprintln!("SKIPPED real-labels check: {labels_path:?} not present (run scripts/extract-maid-labels.py <sdk folder>)");
            return;
        }
        let l = Labels::load_for(&dir, "Z 9", Some("5.31"));
        assert!(l.unavailable().is_empty(), "{:?}", l.unavailable());
        // The base capabilities a real snapshot holds as cap_0x1 .. cap_0xc are all named.
        for code in 1..=12 {
            assert!(l.base_name(code).is_some_and(|n| !n.is_empty()), "cap_{code:#x} has no base name");
        }
        // The header's enumeration jumps over one number (`X = Feed + 2`); nothing is invented for it.
        let named: Vec<u32> = (1..=40).filter(|c| l.base_name(*c).is_some()).collect();
        assert!(named.windows(2).any(|w| w[1] - w[0] == 2), "no gap in the base capability numbers: {named:?}");
        // A display name from the real schema agrees with the schema parser the CLI uses.
        let parsed = MaidLayerConfig::parse_file(dir.join(SCHEMA_FILE)).expect("the real MaidLayer.config parses");
        let shared: BTreeSet<u32> = l.shared_display_names().iter().flat_map(|s| s.codes.iter().copied()).collect();
        let mut checked = 0;
        for section in parsed.sections_for_model("Z 9").filter(|s| s.version == "5.0") {
            for cap in section.capabilities.iter().filter(|c| !c.display.is_empty()) {
                // A name the config gives several capabilities is shown for none of them.
                let want = if shared.contains(&cap.code) { l.base_name(cap.code) } else { Some(cap.display.as_str()) };
                assert_eq!(l.capability_display_name(cap.code), want, "{}", cap.name);
                checked += 1;
            }
        }
        assert!(checked > 100, "{checked} display names compared");
        assert!(!l.shared_display_names().is_empty(), "the real config shares at least one display name between capabilities");
        assert!(l.shared_display_names().iter().all(|s| s.codes.len() >= 2));
        // Some enumeration the PDF documents is named by the code the schema gives it.
        let reachable = l.schema.iter().any(|(code, cap)| {
            let short = cap.name.strip_prefix(PROPERTY_PREFIX).unwrap_or(&cap.name);
            l.doc.get(short).is_some_and(|d| d.values.keys().any(|v| l.value_label(*code, *v).is_some()))
        });
        assert!(reachable, "no documented enumeration is reachable by its schema code");
    }
}

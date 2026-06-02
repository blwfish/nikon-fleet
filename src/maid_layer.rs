//! Parser for Nikon SDK's `MaidLayer.config`.
//!
//! This file ships with the Nikon Remote SDK and lists every "capability"
//! (settings property) the SDK knows about, grouped by camera model and
//! firmware version. We use it as the source-of-truth schema so the fleet
//! tool can name properties, know their PTP opcodes, and (later) understand
//! cross-model availability — all without parsing the per-body PDFs.
//!
//! ## Format
//!
//! The file looks like XML at first glance but isn't well-formed. Data is
//! embedded directly in tag names using a colon delimiter:
//!
//! ```text
//! <model:Z 9>
//!   <version>2.0</version>
//!   <caplist>
//!     <capability:kNkMAIDCapability_Aperture-33285>
//!       <description>0,11100,"Aperture"</description>
//!       <allowedoperation:14></allowedoperation>
//!       <DeviceCommand>215,0,0,0,0</DeviceCommand>
//!       ... (observer cascade fields we currently ignore) ...
//!     </capability>
//!   </caplist>
//! </model>
//! ```
//!
//! Note `</model>` and `</capability>` close — they don't repeat the embedded
//! data. A standard XML parser can't handle `<capability:Name-Code>` so we
//! roll a small line-oriented state machine instead.
//!
//! ## What we extract (v1)
//!
//! For each `(model, version)` section, every capability's:
//!   - symbolic name (e.g. `kNkMAIDCapability_Aperture`)
//!   - numeric code (e.g. 33285)
//!   - human-readable label (from `<description>`)
//!   - allowed-operations bitmask (from `<allowedoperation:N>`)
//!   - PTP DeviceCommand (5-byte tuple)
//!
//! We currently ignore the observer-cascade fields. Those are interesting
//! for a future "what's locking me out" diagnostic but not needed for
//! snapshot-and-diff.

use std::fs;
use std::path::Path;

use thiserror::Error;

/// One settings capability within a `(model, version)` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub name: String,
    pub code: u32,
    /// Human-readable label, e.g. "Aperture". Pulled from the quoted string
    /// in `<description>flags,iconId,"label"</description>`. May be empty.
    pub display: String,
    /// Bitmask of allowed MAID operations on this capability.
    /// (Exact bit meanings are documented in `MAID3.pdf`; we just preserve
    /// the value for now and surface it via the schema.)
    pub allowed_ops: u32,
    /// 5-byte PTP DeviceCommand tuple, if the section specified one.
    pub device_command: Option<[u32; 5]>,
}

/// A `<model:NAME>` block at a specific firmware `<version>`.
///
/// One body (e.g. "Z 9") typically appears in multiple sections, one per
/// firmware version ("common", "2.0", "3.0", ...). Capabilities can be
/// added or refined per version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSection {
    pub model: String,
    pub version: String,
    pub capabilities: Vec<Capability>,
}

/// Top-level parsed view of `MaidLayer.config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaidLayerConfig {
    pub profile_version: Option<String>,
    pub sections: Vec<ModelSection>,
}

/// Errors that can come out of parsing.
///
/// `thiserror` lets us define a typed error enum with a `Display` impl
/// derived from `#[error("...")]` annotations. Callers can match on the
/// variant; `?` conversion from `std::io::Error` comes free via `#[from]`.
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("I/O error reading config: {0}")]
    Io(#[from] std::io::Error),
    #[error("line {line}: malformed {what}: {detail}")]
    Malformed {
        line: usize,
        what: &'static str,
        detail: String,
    },
    #[error("line {line}: unexpected `{tag}` while in state {state:?}")]
    UnexpectedTag {
        line: usize,
        tag: String,
        state: ParserState,
    },
}

/// Where the parser is in its walk. Exposed in errors so they're useful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserState {
    TopLevel,
    InModel,
    InCapability,
}

impl MaidLayerConfig {
    /// Parse from an in-memory string. Useful for tests.
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let mut config = MaidLayerConfig {
            profile_version: None,
            sections: Vec::new(),
        };
        let mut state = ParserState::TopLevel;
        // Index of the section/capability currently being built. We use
        // indices into the Vec rather than &mut references because Rust's
        // borrow checker would otherwise tie our hands when also reading
        // from `config` for error context.
        let mut cur_section: Option<usize> = None;
        let mut cur_cap: Option<usize> = None;

        for (i, raw_line) in input.lines().enumerate() {
            let line_num = i + 1;
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }

            // ---- Top-level: ProfileVersion or <model:NAME> ----
            if let Some(v) = extract_between(line, "<ProfileVersion>", "</ProfileVersion>") {
                config.profile_version = Some(v.to_string());
                continue;
            }
            if let Some(model_name) = strip_open_tag(line, "model") {
                if state != ParserState::TopLevel {
                    return Err(ParseError::UnexpectedTag {
                        line: line_num,
                        tag: line.to_string(),
                        state,
                    });
                }
                config.sections.push(ModelSection {
                    model: model_name.to_string(),
                    version: String::new(),
                    capabilities: Vec::new(),
                });
                cur_section = Some(config.sections.len() - 1);
                state = ParserState::InModel;
                continue;
            }
            if line == "</model>" {
                state = ParserState::TopLevel;
                cur_section = None;
                continue;
            }

            // ---- Inside a model: <version>, <caplist>, or <capability:...> ----
            if state == ParserState::InModel {
                if let Some(v) = extract_between(line, "<version>", "</version>") {
                    if let Some(idx) = cur_section {
                        config.sections[idx].version = v.to_string();
                    }
                    continue;
                }
                // We don't care about <caplist>, <support>, <lvheader>, etc.
                if line == "<caplist>" || line == "</caplist>" {
                    continue;
                }
                if let Some(payload) = strip_open_tag(line, "capability") {
                    let (name, code) = split_name_code(payload).map_err(|d| {
                        ParseError::Malformed {
                            line: line_num,
                            what: "capability tag",
                            detail: d,
                        }
                    })?;
                    if let Some(idx) = cur_section {
                        config.sections[idx].capabilities.push(Capability {
                            name,
                            code,
                            display: String::new(),
                            allowed_ops: 0,
                            device_command: None,
                        });
                        cur_cap = Some(config.sections[idx].capabilities.len() - 1);
                    }
                    state = ParserState::InCapability;
                    continue;
                }
                // Anything else at the model level we silently skip — there
                // are several diagnostic fields we don't need.
                continue;
            }

            // ---- Inside a capability ----
            if state == ParserState::InCapability {
                if line == "</capability>" {
                    state = ParserState::InModel;
                    cur_cap = None;
                    continue;
                }
                if let Some(body) = extract_between(line, "<description>", "</description>") {
                    if let (Some(si), Some(ci)) = (cur_section, cur_cap) {
                        config.sections[si].capabilities[ci].display =
                            parse_description_label(body);
                    }
                    continue;
                }
                if let Some(n) = strip_self_tag(line, "allowedoperation") {
                    let bits: u32 = n.parse().map_err(|_| ParseError::Malformed {
                        line: line_num,
                        what: "allowedoperation",
                        detail: n.to_string(),
                    })?;
                    if let (Some(si), Some(ci)) = (cur_section, cur_cap) {
                        config.sections[si].capabilities[ci].allowed_ops = bits;
                    }
                    continue;
                }
                if let Some(body) = extract_between(line, "<DeviceCommand>", "</DeviceCommand>") {
                    let parts: Result<Vec<u32>, _> =
                        body.split(',').map(|s| s.trim().parse()).collect();
                    let parts = parts.map_err(|_| ParseError::Malformed {
                        line: line_num,
                        what: "DeviceCommand",
                        detail: body.to_string(),
                    })?;
                    if parts.len() != 5 {
                        return Err(ParseError::Malformed {
                            line: line_num,
                            what: "DeviceCommand",
                            detail: format!("expected 5 ints, got {}", parts.len()),
                        });
                    }
                    let mut arr = [0u32; 5];
                    arr.copy_from_slice(&parts);
                    if let (Some(si), Some(ci)) = (cur_section, cur_cap) {
                        config.sections[si].capabilities[ci].device_command = Some(arr);
                    }
                    continue;
                }
                // Skip everything else (observer fields, resourcestringList, etc.)
                continue;
            }

            // TopLevel and we didn't match anything we care about — silently skip.
        }

        Ok(config)
    }

    /// Read and parse a file from disk.
    pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Self, ParseError> {
        let text = fs::read_to_string(path)?;
        Self::parse(&text)
    }

    /// Iterate every section whose model matches `model_name`.
    /// (A model has multiple sections — one per firmware version.)
    pub fn sections_for_model<'a>(
        &'a self,
        model_name: &'a str,
    ) -> impl Iterator<Item = &'a ModelSection> + 'a {
        self.sections
            .iter()
            .filter(move |s| s.model == model_name)
    }

    /// Distinct list of model names actually present in the file.
    pub fn known_models(&self) -> Vec<String> {
        let mut v: Vec<String> = self.sections.iter().map(|s| s.model.clone()).collect();
        v.sort();
        v.dedup();
        v
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────

/// Extract the substring between `open` and `close` on the same line.
/// Returns `None` if either marker is absent or they're out of order.
fn extract_between<'a>(line: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = line.find(open)? + open.len();
    let end = line[start..].find(close)? + start;
    Some(&line[start..end])
}

/// Match an opening tag with embedded data: `<NAME:PAYLOAD>`. On match,
/// return the payload string. Returns `None` if the line isn't this shape.
///
/// The match is strict — we anchor on `<NAME:` to avoid eating
/// `<observercapability:...>` when we asked for `capability`.
fn strip_open_tag<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("<{name}:");
    if !line.starts_with(&prefix) {
        return None;
    }
    let rest = &line[prefix.len()..];
    // The payload runs up to the next '>'. Some lines have trailing whitespace
    // or self-close on the same line, but the opener itself ends at '>'.
    let end = rest.find('>')?;
    Some(&rest[..end])
}

/// Match `<NAME:VALUE></NAME>` on a single line. Returns VALUE.
fn strip_self_tag<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let payload = strip_open_tag(line, name)?;
    let closer = format!("</{name}>");
    if line.contains(&closer) {
        Some(payload)
    } else {
        None
    }
}

/// Split "kNkMAIDCapability_Aperture-33285" into ("kNkMAIDCapability_Aperture", 33285).
fn split_name_code(payload: &str) -> Result<(String, u32), String> {
    let dash = payload
        .rfind('-')
        .ok_or_else(|| format!("no '-' in {payload:?}"))?;
    let name = &payload[..dash];
    let code: u32 = payload[dash + 1..]
        .parse()
        .map_err(|_| format!("non-numeric code in {payload:?}"))?;
    Ok((name.to_string(), code))
}

/// Pull the quoted label out of `flags,iconId,"label"`. Returns "" if no
/// quoted string is present.
fn parse_description_label(body: &str) -> String {
    if let Some(first_q) = body.find('"') {
        if let Some(rel_close) = body[first_q + 1..].find('"') {
            return body[first_q + 1..first_q + 1 + rel_close].to_string();
        }
    }
    String::new()
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────
//
// Rust convention: unit tests live in a `#[cfg(test)] mod tests` block at
// the bottom of the same file. `cargo test` runs them. The `#[cfg(test)]`
// gate means the test code (and its imports) is only compiled during
// `cargo test`, not for `cargo build`.

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const SAMPLE: &str = r#"<ProfileVersion>9.0.3000</ProfileVersion>
<AppCompatibilityVersion>5</AppCompatibilityVersion>
<model:Z 9>
    <version>common</version>
    <support>1</support>
    <caplist>
        <capability:kNkMAIDCapability_Aperture-33285>
            <description>0,11100,"Aperture"</description>
            <resourcestringList></resourcestringList>
            <defoperation:14></defoperation>
            <allowedoperation:14></allowedoperation>
            <DeviceCommand>215,0,0,0,0</DeviceCommand>
        </capability>
        <capability:kNkMAIDCapability_Viewmode-34147>
            <description>0,11100,"View mode"</description>
            <allowedoperation:6></allowedoperation>
            <observercapList>
                <observercapability:kNkMAIDCapability_Aperture-33285>
                    <applicable:1></applicable>
                </observercapability>
            </observercapList>
            <DeviceCommand>219,0,0,0,0</DeviceCommand>
        </capability>
    </caplist>
</model>
<model:Z 8>
    <version>1.0</version>
    <caplist>
        <capability:kNkMAIDCapability_Aperture-33285>
            <description>0,11100,"Aperture"</description>
            <allowedoperation:14></allowedoperation>
            <DeviceCommand>215,0,0,0,0</DeviceCommand>
        </capability>
    </caplist>
</model>
"#;

    #[test]
    fn parses_profile_version() {
        let cfg = MaidLayerConfig::parse(SAMPLE).unwrap();
        assert_eq!(cfg.profile_version.as_deref(), Some("9.0.3000"));
    }

    #[test]
    fn finds_both_models() {
        let cfg = MaidLayerConfig::parse(SAMPLE).unwrap();
        assert_eq!(cfg.known_models(), vec!["Z 8".to_string(), "Z 9".to_string()]);
    }

    #[test]
    fn captures_capabilities_for_z9() {
        let cfg = MaidLayerConfig::parse(SAMPLE).unwrap();
        let z9: Vec<_> = cfg.sections_for_model("Z 9").collect();
        assert_eq!(z9.len(), 1);
        let caps = &z9[0].capabilities;
        assert_eq!(caps.len(), 2);

        assert_eq!(caps[0].name, "kNkMAIDCapability_Aperture");
        assert_eq!(caps[0].code, 33285);
        assert_eq!(caps[0].display, "Aperture");
        assert_eq!(caps[0].allowed_ops, 14);
        assert_eq!(caps[0].device_command, Some([215, 0, 0, 0, 0]));

        assert_eq!(caps[1].name, "kNkMAIDCapability_Viewmode");
        assert_eq!(caps[1].code, 34147);
        assert_eq!(caps[1].display, "View mode");
        assert_eq!(caps[1].allowed_ops, 6);
    }

    #[test]
    fn does_not_mistake_observercapability_for_capability() {
        // The Viewmode entry contains a nested <observercapability:...>
        // block. If the parser confused those for a capability opener, we'd
        // see 3 caps instead of 2.
        let cfg = MaidLayerConfig::parse(SAMPLE).unwrap();
        let z9: Vec<_> = cfg.sections_for_model("Z 9").collect();
        assert_eq!(z9[0].capabilities.len(), 2);
    }

    #[test]
    fn version_string_captured() {
        let cfg = MaidLayerConfig::parse(SAMPLE).unwrap();
        let z8: Vec<_> = cfg.sections_for_model("Z 8").collect();
        assert_eq!(z8[0].version, "1.0");
    }

    #[test]
    fn description_label_parsing() {
        assert_eq!(
            parse_description_label(r#"0,11100,"Aperture""#),
            "Aperture"
        );
        assert_eq!(
            parse_description_label(r#"0,11100,"View mode""#),
            "View mode"
        );
        // Empty string is acceptable.
        assert_eq!(parse_description_label("0,11100,"), "");
    }

    #[test]
    fn split_name_code_works() {
        assert_eq!(
            split_name_code("kNkMAIDCapability_Aperture-33285").unwrap(),
            ("kNkMAIDCapability_Aperture".to_string(), 33285u32)
        );
    }

    #[test]
    fn split_name_code_hyphen_in_name() {
        // rfind ensures the LAST '-' is the code separator, so hyphens
        // within the capability name are preserved.
        assert_eq!(
            split_name_code("kNkMAIDCapability_Some-Name-33285").unwrap(),
            ("kNkMAIDCapability_Some-Name".to_string(), 33285u32)
        );
    }

    #[test]
    fn split_name_code_no_dash_is_err() {
        assert!(split_name_code("kNkMAIDCapability_NoDash").is_err());
    }

    #[test]
    fn split_name_code_non_numeric_code_is_err() {
        assert!(split_name_code("kNkMAIDCapability_Foo-BAR").is_err());
    }
}

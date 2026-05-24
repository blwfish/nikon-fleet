//! Parser for Nikon SDK's `RangeValue.config`.
//!
//! Sibling to [`crate::maid_layer`]. Same XML-ish format; different payload.
//! Where MaidLayer says "this capability exists on this body," RangeValue
//! says "and these are its allowed values plus the default."
//!
//! For MVP we capture raw value strings as opaque text — interpretation of
//! the six value subtypes (enum, enumEV, enumSpotWB, int, range, string)
//! happens at display time, not parse time. That keeps this parser tiny
//! and lets us move on to snapshot/diff sooner.
//!
//! ## Format
//!
//! ```text
//! <model:Z 9>
//!   <version:common>
//!     <capability:kNkMAIDCapability_Active_D_Lighting-33330>
//!       <value:enum>6,5,0,1,2,3</value>
//!       <default:enum>0</default>
//!     </capability>
//!   </version>
//! </model>
//! ```
//!
//! Notes vs MaidLayer:
//!   - `<version:VAL>` is tag-embedded (vs MaidLayer's `<version>VAL</version>`).
//!   - `<value:TYPE>...</value>` carries the subtype in the opener; the
//!      closer is the generic `</value>`.
//!   - Model names sometimes match camera bodies ("Z 9"), sometimes name
//!      shared profiles ("Type23"). We preserve both as-is.

use std::fs;
use std::path::Path;

use thiserror::Error;

/// One value-spec entry within a `(model, version)` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueSpec {
    pub name: String,
    pub code: u32,
    /// Subtype tag from `<value:TYPE>`, e.g. "enum", "range", "int",
    /// "enumEV", "enumSpotWB", "string".
    pub value_type: String,
    /// Raw value list, exactly as it appears between `<value:...>` and `</value>`.
    pub value_raw: String,
    /// Raw default, from `<default:...>`. May be empty if absent.
    pub default_raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSection {
    pub model: String,
    pub version: String,
    pub values: Vec<ValueSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeValueConfig {
    pub profile_version: Option<String>,
    pub sections: Vec<ModelSection>,
}

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    TopLevel,
    InModel,        // saw <model:NAME>, awaiting <version:VAL>
    InVersion,      // inside <version:VAL>...</version>
    InCapability,   // inside <capability:NAME-CODE>...</capability>
}

impl RangeValueConfig {
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let mut config = RangeValueConfig {
            profile_version: None,
            sections: Vec::new(),
        };
        let mut state = State::TopLevel;
        let mut cur_model: Option<String> = None;
        let mut cur_section: Option<usize> = None;
        let mut cur_value: Option<usize> = None;

        for (i, raw_line) in input.lines().enumerate() {
            let line_num = i + 1;
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }

            // Top-level: ProfileVersion
            if let Some(v) = extract_between(line, "<ProfileVersion>", "</ProfileVersion>") {
                config.profile_version = Some(v.to_string());
                continue;
            }

            // <model:NAME>
            if let Some(name) = strip_open_tag(line, "model") {
                cur_model = Some(name.to_string());
                state = State::InModel;
                continue;
            }
            if line == "</model>" {
                cur_model = None;
                state = State::TopLevel;
                continue;
            }

            // <version:VAL>  — opens a section
            if let Some(version) = strip_open_tag(line, "version") {
                if let Some(model) = &cur_model {
                    config.sections.push(ModelSection {
                        model: model.clone(),
                        version: version.to_string(),
                        values: Vec::new(),
                    });
                    cur_section = Some(config.sections.len() - 1);
                    state = State::InVersion;
                }
                continue;
            }
            if line == "</version>" {
                cur_section = None;
                state = State::InModel;
                continue;
            }

            // <capability:NAME-CODE>
            if let Some(payload) = strip_open_tag(line, "capability") {
                let (name, code) = split_name_code(payload).map_err(|d| {
                    ParseError::Malformed {
                        line: line_num,
                        what: "capability tag",
                        detail: d,
                    }
                })?;
                if let Some(idx) = cur_section {
                    config.sections[idx].values.push(ValueSpec {
                        name,
                        code,
                        value_type: String::new(),
                        value_raw: String::new(),
                        default_raw: String::new(),
                    });
                    cur_value = Some(config.sections[idx].values.len() - 1);
                }
                state = State::InCapability;
                continue;
            }
            if line == "</capability>" {
                cur_value = None;
                state = State::InVersion;
                continue;
            }

            // Inside capability: <value:TYPE>...</value> and <default:TYPE>...</default>
            if state == State::InCapability {
                // Find any <value:...> opener, then extract up to </value>.
                if let Some((vtype, body)) = extract_typed(line, "value") {
                    if let (Some(si), Some(vi)) = (cur_section, cur_value) {
                        config.sections[si].values[vi].value_type = vtype;
                        config.sections[si].values[vi].value_raw = body;
                    }
                    continue;
                }
                if let Some((_dtype, body)) = extract_typed(line, "default") {
                    if let (Some(si), Some(vi)) = (cur_section, cur_value) {
                        config.sections[si].values[vi].default_raw = body;
                    }
                    continue;
                }
                continue;
            }
        }

        Ok(config)
    }

    pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Self, ParseError> {
        let text = fs::read_to_string(path)?;
        Self::parse(&text)
    }

    pub fn sections_for_model<'a>(
        &'a self,
        model_name: &'a str,
    ) -> impl Iterator<Item = &'a ModelSection> + 'a {
        self.sections
            .iter()
            .filter(move |s| s.model == model_name)
    }

    pub fn known_models(&self) -> Vec<String> {
        let mut v: Vec<String> = self.sections.iter().map(|s| s.model.clone()).collect();
        v.sort();
        v.dedup();
        v
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Helpers
// (Largely the same shape as in maid_layer.rs. When a third parser shows
// up, we'll factor these into a shared module.)
// ─────────────────────────────────────────────────────────────────────────

fn extract_between<'a>(line: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = line.find(open)? + open.len();
    let end = line[start..].find(close)? + start;
    Some(&line[start..end])
}

fn strip_open_tag<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("<{name}:");
    if !line.starts_with(&prefix) {
        return None;
    }
    let rest = &line[prefix.len()..];
    let end = rest.find('>')?;
    Some(&rest[..end])
}

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

/// Match `<name:TYPE>BODY</name>` on one line. Returns (TYPE, BODY).
/// Returns None unless the closer is also on this line.
fn extract_typed(line: &str, name: &str) -> Option<(String, String)> {
    let open_prefix = format!("<{name}:");
    let close = format!("</{name}>");
    let open_start = line.find(&open_prefix)?;
    let after_prefix = &line[open_start + open_prefix.len()..];
    let gt = after_prefix.find('>')?;
    let vtype = &after_prefix[..gt];
    let body_start = open_start + open_prefix.len() + gt + 1;
    let body_end_rel = line[body_start..].find(&close)?;
    let body = &line[body_start..body_start + body_end_rel];
    Some((vtype.to_string(), body.to_string()))
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const SAMPLE: &str = r#"<ProfileVersion>9.0.3000</ProfileVersion>
<model:Z 9>
  <version:common>
    <capability:kNkMAIDCapability_Active_D_Lighting-33330>
      <value:enum>6,5,0,1,2,3</value>
      <default:enum>0</default>
    </capability>
    <capability:kNkMAIDCapability_MFDriveStep-33352>
      <value:range>1,32767,32767</value>
      <default:range>1</default>
    </capability>
  </version>
  <version:2.0>
    <capability:kNkMAIDCapability_ColorSpace-33243>
      <value:enum>0,1</value>
      <default:enum>0</default>
    </capability>
  </version>
</model>
<model:Type23>
  <version:common>
    <capability:kNkMAIDCapability_WhiteBalance-33000>
      <value:enumSpotWB>{0,0},{1,0},{2,0},{6,1},{7,1}</value>
      <default:enumSpotWB>0</default>
    </capability>
  </version>
</model>
"#;

    #[test]
    fn parses_profile_version() {
        let cfg = RangeValueConfig::parse(SAMPLE).unwrap();
        assert_eq!(cfg.profile_version.as_deref(), Some("9.0.3000"));
    }

    #[test]
    fn captures_two_z9_sections() {
        let cfg = RangeValueConfig::parse(SAMPLE).unwrap();
        let z9: Vec<_> = cfg.sections_for_model("Z 9").collect();
        assert_eq!(z9.len(), 2);
        assert_eq!(z9[0].version, "common");
        assert_eq!(z9[1].version, "2.0");
    }

    #[test]
    fn captures_value_and_default_strings_raw() {
        let cfg = RangeValueConfig::parse(SAMPLE).unwrap();
        let z9_common = cfg
            .sections_for_model("Z 9")
            .find(|s| s.version == "common")
            .unwrap();
        let adl = &z9_common.values[0];
        assert_eq!(adl.name, "kNkMAIDCapability_Active_D_Lighting");
        assert_eq!(adl.code, 33330);
        assert_eq!(adl.value_type, "enum");
        assert_eq!(adl.value_raw, "6,5,0,1,2,3");
        assert_eq!(adl.default_raw, "0");

        let mf = &z9_common.values[1];
        assert_eq!(mf.value_type, "range");
        assert_eq!(mf.value_raw, "1,32767,32767");
    }

    #[test]
    fn handles_enum_spot_wb_with_braces() {
        let cfg = RangeValueConfig::parse(SAMPLE).unwrap();
        let t23 = cfg.sections_for_model("Type23").next().unwrap();
        let wb = &t23.values[0];
        assert_eq!(wb.value_type, "enumSpotWB");
        assert_eq!(wb.value_raw, "{0,0},{1,0},{2,0},{6,1},{7,1}");
    }

    #[test]
    fn extract_typed_works() {
        let (t, b) = extract_typed("<value:range>1,32767,32767</value>", "value").unwrap();
        assert_eq!(t, "range");
        assert_eq!(b, "1,32767,32767");
    }
}

//! Shared test fixtures (compiled only for tests).

use crate::maid_layer::MaidLayerConfig;

/// Build a MaidLayer.config text: one section per `(model, caps)`, each
/// cap `(symbolic name without prefix, code)`.
pub(crate) fn config_text(models: &[(&str, &[(&str, u32)])]) -> String {
    let mut out = String::from("<ProfileVersion>9.0.3000</ProfileVersion>\n");
    for (model, caps) in models {
        out.push_str(&format!("<model:{model}>\n<version>common</version>\n<caplist>\n"));
        for (name, code) in *caps {
            out.push_str(&format!(
                "<capability:kNkMAIDCapability_{name}-{code}>\n<description>0,0,\"{name}\"</description>\n\
                 <allowedoperation:14></allowedoperation>\n<DeviceCommand>1,0,0,0,0</DeviceCommand>\n</capability>\n"
            ));
        }
        out.push_str("</caplist>\n</model>\n");
    }
    out
}

pub(crate) fn parse_config(models: &[(&str, &[(&str, u32)])]) -> MaidLayerConfig {
    MaidLayerConfig::parse(&config_text(models)).expect("fixture config parses")
}

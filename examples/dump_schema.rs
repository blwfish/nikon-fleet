//! Parse the real MaidLayer.config and print summary stats.
//!
//! Run with:
//!     cargo run --example dump_schema -- /path/to/MaidLayer.config
//!
//! If no path is given, falls back to the SDK location on Blair's machine.

use std::collections::BTreeSet;
use std::env;
use std::process;

use nikon_fleet::maid_layer::MaidLayerConfig;

const DEFAULT_PATH: &str = "/Volumes/Files/claude/Nikon-SDK/S-SDKZ-200BF-ALLIN/Module/Win/BinaryFile/MaidLayer.config";

fn main() {
    let path = env::args().nth(1).unwrap_or_else(|| DEFAULT_PATH.to_string());

    let cfg = match MaidLayerConfig::parse_file(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("parse failed: {e}");
            process::exit(1);
        }
    };

    println!("Profile version: {:?}", cfg.profile_version);
    println!("Total sections: {}", cfg.sections.len());

    println!("\nModels found:");
    for model in cfg.known_models() {
        let sections: Vec<_> = cfg.sections_for_model(&model).collect();
        let total_caps: usize = sections.iter().map(|s| s.capabilities.len()).sum();
        let versions: Vec<&str> = sections.iter().map(|s| s.version.as_str()).collect();
        println!(
            "  {model:12}  {n} section(s) [{versions}]  {total_caps} capability rows",
            n = sections.len(),
            versions = versions.join(", "),
        );
    }

    // Cross-model overlap: how many unique capability names exist?
    let mut all_names: BTreeSet<&str> = BTreeSet::new();
    for s in &cfg.sections {
        for c in &s.capabilities {
            all_names.insert(&c.name);
        }
    }
    println!("\nUnique capability names across all models: {}", all_names.len());

    // Sanity check on one model the user cares about.
    let z9_unique: BTreeSet<&str> = cfg
        .sections_for_model("Z 9")
        .flat_map(|s| s.capabilities.iter().map(|c| c.name.as_str()))
        .collect();
    println!("Z 9 unique capabilities (union across all Z 9 versions): {}", z9_unique.len());

    let z6iii_unique: BTreeSet<&str> = cfg
        .sections_for_model("Z6_3")
        .flat_map(|s| s.capabilities.iter().map(|c| c.name.as_str()))
        .collect();
    println!("Z6_3 unique capabilities: {}", z6iii_unique.len());

    // Show 5 sample caps from Z 9 with full detail.
    println!("\nSample Z 9 capabilities:");
    if let Some(z9) = cfg.sections_for_model("Z 9").next() {
        for cap in z9.capabilities.iter().take(5) {
            println!(
                "  {name:60}  code={code:5}  ops={ops:3}  cmd={cmd:?}  display={display:?}",
                name = cap.name,
                code = cap.code,
                ops = cap.allowed_ops,
                cmd = cap.device_command,
                display = cap.display,
            );
        }
    }
}

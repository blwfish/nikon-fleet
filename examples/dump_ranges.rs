//! Parse the real RangeValue.config and print summary stats.
//!
//!     cargo run --example dump_ranges -- /path/to/RangeValue.config

use std::collections::BTreeSet;
use std::env;
use std::process;

use nikon_fleet::range_value::RangeValueConfig;

const DEFAULT_PATH: &str = "/Volumes/Files/claude/Nikon-SDK/S-SDKZ-200BF-ALLIN/Module/Win/BinaryFile/RangeValue.config";

fn main() {
    let path = env::args().nth(1).unwrap_or_else(|| DEFAULT_PATH.to_string());
    let cfg = match RangeValueConfig::parse_file(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("parse failed: {e}");
            process::exit(1);
        }
    };

    println!("Profile version: {:?}", cfg.profile_version);
    println!("Total sections: {}", cfg.sections.len());

    println!("\nModels:");
    for m in cfg.known_models() {
        let secs: Vec<_> = cfg.sections_for_model(&m).collect();
        let total: usize = secs.iter().map(|s| s.values.len()).sum();
        println!("  {m:14}  {n} section(s)  {total} value rows", n = secs.len());
    }

    // Value-type histogram
    let mut by_type: std::collections::BTreeMap<&str, usize> = Default::default();
    for s in &cfg.sections {
        for v in &s.values {
            *by_type.entry(v.value_type.as_str()).or_default() += 1;
        }
    }
    println!("\nValue-type histogram:");
    for (t, n) in &by_type {
        println!("  {t:14}  {n}");
    }

    // Sample Z 9
    println!("\nSample Z 9 (common) values:");
    if let Some(s) = cfg.sections_for_model("Z 9").find(|s| s.version == "common") {
        for v in s.values.iter().take(8) {
            println!(
                "  {name:60}  [{vt}] value={val:?} default={def:?}",
                name = v.name,
                vt = v.value_type,
                val = v.value_raw,
                def = v.default_raw,
            );
        }
    } else {
        println!("  (no 'common' version for Z 9 — versions are {:?})",
            cfg.sections_for_model("Z 9").map(|s| s.version.clone()).collect::<Vec<_>>());
    }

    // Unique value-spec names across everything
    let names: BTreeSet<&str> = cfg.sections.iter()
        .flat_map(|s| s.values.iter().map(|v| v.name.as_str()))
        .collect();
    println!("\nUnique capability names with value specs: {}", names.len());
}

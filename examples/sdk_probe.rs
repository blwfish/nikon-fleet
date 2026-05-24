//! Probe a connected camera via the SDK. Shorter than a full snapshot —
//! just initializes, enumerates devices, connects to the first one,
//! prints capability count, reads ~5 sample values, disconnects.
//!
//! Run:
//!     cargo build --example sdk_probe
//!     codesign --force --sign - --entitlements sdk-entitlements.plist \
//!         target/debug/examples/sdk_probe
//!     cargo run --example sdk_probe

use std::path::PathBuf;
use std::process;

use nikon_fleet::sdk::Sdk;

const BUNDLE_EXE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/sdk-runtime/TypeCommon Module.bundle/Contents/MacOS/TypeCommon Module"
);

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = PathBuf::from(BUNDLE_EXE);
    println!("Loading SDK from {}", path.display());
    let mut sdk = Sdk::open(&path)?;
    println!("Initializing…");
    sdk.initialize()?;

    println!("Enumerating devices…");
    let devices = sdk.devices()?;
    if devices.is_empty() {
        println!("(no devices found)");
        return Ok(());
    }
    for d in &devices {
        println!(
            "  id={} name={:?} available={} pid={} version={:?}",
            d.id, d.name, d.available, d.connected_pid, d.version
        );
    }

    let target = devices.iter().find(|d| d.available).cloned().unwrap_or(devices[0].clone());
    println!("\nConnecting to id={} ({:?})…", target.id, target.name);
    let device = sdk.connect(target.id)?;
    println!("Connected. {} capabilities exposed.", device.capabilities.len());

    println!("\nFirst 10 capability descriptions:");
    for c in device.capabilities.iter().take(10) {
        println!(
            "  id={:#x} ops={:#x} {}",
            c.id, c.operations, c.description
        );
    }

    println!("\nSampling values for the first 5 readable capabilities:");
    let mut tried = 0;
    for c in &device.capabilities {
        if tried >= 5 {
            break;
        }
        // ulOperations bit 1 (0x02) = "Get" — only try those (rough heuristic).
        if c.operations & 0x02 == 0 {
            continue;
        }
        match device.read_capability(c.id) {
            Ok(v) => println!("  id={:#x} {:30}  value={}", c.id, c.description, v),
            Err(e) => println!("  id={:#x} {:30}  ERR {}", c.id, c.description, e),
        }
        tried += 1;
    }

    println!("\nDisconnecting and freeing SDK.");
    Ok(())
}

//! Minimal experiment: can we load the Nikon SDK bundle and find an entry
//! point symbol? No camera I/O — just dyld linkage validation.
//!
//! Run with:
//!     cargo run --example load_sdk

use std::ffi::CString;
use std::path::PathBuf;

use libloading::{Library, Symbol};

const BUNDLE_EXE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/sdk-runtime/TypeCommon Module.bundle/Contents/MacOS/TypeCommon Module"
);

fn main() {
    let path = PathBuf::from(BUNDLE_EXE);
    println!("Attempting to load: {}", path.display());
    if !path.exists() {
        eprintln!("ERROR: bundle executable not found at the path above.");
        eprintln!("Did you run the sdk-runtime setup steps?");
        std::process::exit(1);
    }

    // SAFETY: dlopen has process-wide effects. We trust the path is what we
    // expect; loading the SDK's own bundle is the intended use.
    let lib = match unsafe { Library::new(&path) } {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Library::new failed: {e}");
            std::process::exit(1);
        }
    };
    println!("Loaded bundle.");

    // Try resolving each symbol we'll need later. We don't call them yet.
    for sym_name in [
        "MAIDEntryPoint",
        "InitializeSDK",
        "FreeSDK",
        "EnumDevices",
        "ConnectDevice",
        "DisconnectDevice",
        "EnumCapabilities",
        "GetCapability",
        "SetCapability",
    ] {
        let c_name = CString::new(sym_name).unwrap();
        // The type parameter is just "function pointer of unknown shape"
        // for this experiment — we're only checking that the symbol resolves.
        let result: Result<Symbol<unsafe extern "C" fn()>, _> =
            unsafe { lib.get(c_name.as_bytes_with_nul()) };
        match result {
            Ok(_) => println!("  ok   {sym_name}"),
            Err(e) => println!("  FAIL {sym_name}: {e}"),
        }
    }

    println!("\nIf all symbols resolved, the dyld linkage is working and we can proceed to the full FFI.");
}

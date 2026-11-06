//! Shared by the integration tests that start the `nikon-fleet` binary.
//!
//! Every process the CLI starts keeps diagnostics files (`docs/diagnostics.md`), in the real diagnostics
//! directory unless `NIKON_FLEET_DIAG_DIR` says otherwise. `.cargo/config.toml` sets that variable for
//! `cargo test`, but only when cargo finds the file from the working directory: a test binary run directly,
//! or cargo run from outside the tree, would write test processes' files into the real directory and push a
//! real crash's evidence out of its 20-process window. So no test relies on that: every spawn goes through
//! [`cli`], which sets the variable itself.
#![allow(dead_code)] // each test crate uses some of this

use std::path::PathBuf;
use std::process::Command;

/// A scratch diagnostics directory under cargo's per-run scratch space (`target/tmp`), never the real one.
/// Shared by the spawns of one test binary: the CLI keeps the 20 newest finished processes' files in it, so it
/// does not grow, and no test reads it (a test that reads diagnostics back passes its own directory).
pub fn diag_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("diag-tests")
}

/// `Command::new(bin)` with `NIKON_FLEET_DIAG_DIR` set to [`diag_dir`]. A test that needs another directory sets
/// the variable after this (the later `env` call wins).
pub fn cli(bin: &str) -> Command {
    let mut cmd = Command::new(bin);
    cmd.env(nikon_fleet::diag::DIR_ENV, diag_dir());
    cmd
}

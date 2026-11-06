//! A CLI process a test spawns keeps its diagnostics in the test directory ([`common::diag_dir`]), set by the
//! spawn itself, and never in the directory the program would use by default (a real user's, under their data dir).
//! Nothing here relies on `.cargo/config.toml` having been found.

use std::path::Path;
use std::process::{Command, Stdio};

use nikon_fleet::diag::{DIR_ENV, dir_for, ring_path};
use nikon_fleet::settings::Os;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

/// Run `settings data-dir` (no camera, no network) with a scratch HOME and the given base command; the child's pid
/// and whether it succeeded.
fn run(mut cmd: Command, home: &Path) -> (u32, bool) {
    let child = cmd
        .env("HOME", home)
        .env("APPDATA", home.join("AppData").join("Roaming"))
        .env("NIKON_FLEET_SETTINGS", home.join("settings.json"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .current_dir(home)
        .args(["settings", "data-dir"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn CLI");
    let pid = child.id();
    let out = child.wait_with_output().expect("wait for CLI");
    (pid, out.status.success())
}

#[test]
fn a_cli_spawned_through_the_helper_records_in_the_test_diag_dir_and_not_in_the_default_one() {
    let home = tempfile::tempdir().unwrap();
    let default_dir = dir_for(Os::CURRENT, &|k| match k {
        "HOME" => Some(home.path().into()),
        "APPDATA" => Some(home.path().join("AppData").join("Roaming").into()),
        _ => None,
    })
    .expect("a default diagnostics directory exists for the scratch home");

    let (pid, ok) = run(common::cli(BIN), home.path());
    assert!(ok);
    assert!(ring_path(&common::diag_dir(), pid).exists(), "the process's ring file is in the test directory");
    assert!(!ring_path(&default_dir, pid).exists(), "and not in the default one");

    // The control: a spawn that does not go through the helper and has no variable in its environment writes to the
    // default directory (here a scratch one), which is what the helper exists to prevent. If this stopped being true
    // the assertions above would prove nothing.
    let mut bare = Command::new(BIN);
    bare.env_remove(DIR_ENV);
    let (pid, ok) = run(bare, home.path());
    assert!(ok);
    assert!(ring_path(&default_dir, pid).exists(), "the control spawn used the default directory");
    assert!(!ring_path(&common::diag_dir(), pid).exists());
}

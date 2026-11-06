//! The boundary between this GUI and the CLI it drives (full-review nikon-fleet-20261105-b7d3 #03,
//! #84; before that nikon-fleet-20260909-7b2e#16 and -20261028-e5a3 H14, which the Tk GUI's
//! `gui_cli_argv_parity` test answered).
//!
//! The GUI runs the `nikon-fleet` binary as a subprocess with argv it builds itself. Each builder's
//! unit test pins the argv it builds as a string literal, which stays green when the CLI renames or
//! drops the flag. This module feeds every argv shape the GUI builds to the REAL binary's parser
//! (`NIKON_FLEET_PARSE_ONLY=1`: parse, then exit 0, nothing runs, no camera), with awkward values (a
//! leading `-` in every free-text field, a space in a path). A flag renamed on either side, or a
//! value form clap rejects, fails here instead of at a user's click (clap usage error, exit 2).
//!
//! The binary is built by this test (`cargo build -p nikon-fleet --bin nikon-fleet`, a no-op when it
//! is current), so the parser it checks is always the one in the tree, never a stale one left in
//! `target/`, and a build that cannot be made fails the test: it never skips.
//!
//! What it does not do: run any command (the parse stops before that), so what a flag MEANS is the
//! CLI's own tests' business (`tests/gui_argv_cli.rs` does that for `restore` and `transplant`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use nikon_fleet::ops::cameras::describe_device;
use nikon_fleet::ptp_usb::FtpProfile;
use nikon_fleet::sdk::{DeviceInfo, UsbCameraInfo};
use nikon_fleet::vendor_argv::{vendor_read_argv, vendor_write_ftp_argv};

use crate::check::{CheckRequest, check_argv};
use crate::firmware_win::FwOp;
use crate::restore::{RestoreRequest, restore_argv};
use crate::{CameraRow, TransplantRequest, discover_argv, parse_discover, snapshot_argv, transplant_argv};

/// The CLI binary built from this tree, next to the test executable's profile directory.
fn real_cli() -> &'static Path {
    static CLI: OnceLock<PathBuf> = OnceLock::new();
    CLI.get_or_init(|| {
        let exe = std::env::current_exe().expect("the test executable's path");
        // target/<profile>/deps/fleet_gui-<hash>  ->  target/<profile>
        let profile_dir = exe.parent().and_then(Path::parent).expect("the test executable sits in <profile>/deps").to_path_buf();
        let mut build = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        build.args(["build", "-p", "nikon-fleet", "--bin", "nikon-fleet"]).current_dir(env!("CARGO_MANIFEST_DIR"));
        if profile_dir.file_name().is_some_and(|n| n == "release") {
            build.arg("--release");
        }
        let out = build.output().expect("could not run cargo to build the nikon-fleet CLI");
        assert!(
            out.status.success(),
            "building the nikon-fleet CLI failed, so its parser cannot be checked:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let bin = profile_dir.join(format!("nikon-fleet{}", std::env::consts::EXE_SUFFIX));
        assert!(bin.is_file(), "cargo built the CLI but {} does not exist (a --target layout?)", bin.display());
        bin
    })
}

fn parse_only(argv: &[String]) -> Output {
    // The parse stops before the CLI starts its diagnostics recorder, so nothing is written; the
    // directory is set anyway so that this never depends on `.cargo/config.toml` having been found
    // (a test binary run directly, cargo run from outside the tree) to keep it out of the real one.
    static DIAG: OnceLock<tempfile::TempDir> = OnceLock::new();
    let diag = DIAG.get_or_init(|| tempfile::tempdir().expect("a scratch diagnostics directory"));
    Command::new(real_cli())
        .env("NIKON_FLEET_PARSE_ONLY", "1")
        .env("NIKON_FLEET_DIAG_DIR", diag.path())
        .args(argv)
        .output()
        .expect("failed to run the CLI binary")
}

fn camera() -> CameraRow {
    CameraRow { model: "Z 9".into(), serial: "-S1".into(), firmware: "5.31".into(), known_model: true, baseline: true, model_count: 1 }
}

fn profile(dashed: bool) -> FtpProfile {
    let v = |s: &str| if dashed { format!("-{s}") } else { s.to_string() };
    FtpProfile {
        profile_name: v("CLAUDE"),
        ssid_24ghz: v("net24"),
        ssid_5ghz: v("net5"),
        host: v("ftp.example.com"),
        port: 2121,
        username: v("user"),
        password: "-s3cret".into(),
    }
}

/// Which `FwOp` variant, by an exhaustive match: a new variant stops compiling here until it is
/// numbered, and `fw_ops` must then hold one (`every_firmware_operation_is_covered`).
const FW_VARIANTS: usize = 7;
fn fw_variant(op: &FwOp) -> usize {
    match op {
        FwOp::Refresh => 0,
        FwOp::Latest { .. } => 1,
        FwOp::Browse { .. } => 2,
        FwOp::Add { .. } => 3,
        FwOp::FetchFile { .. } => 4,
        FwOp::Pin { .. } => 5,
        FwOp::Rollback { .. } => 6,
    }
}

fn fw_ops() -> Vec<(&'static str, FwOp)> {
    vec![
        ("firmware list (no process)", FwOp::Refresh),
        ("firmware latest, default models", FwOp::Latest { models: vec![] }),
        ("firmware latest, models", FwOp::Latest { models: vec!["Z 9".into(), "Z6_3".into()] }),
        ("firmware latest, dashed model", FwOp::Latest { models: vec!["-Z".into()] }),
        ("firmware browse", FwOp::Browse { model: "Z 9".into() }),
        ("firmware browse, dashed model", FwOp::Browse { model: "-Z".into() }),
        ("firmware add", FwOp::Add { file: PathBuf::from("/f/fw.bin"), model: "Z 9".into(), version: "5.31".into() }),
        ("firmware add, dashed", FwOp::Add { file: PathBuf::from("-fw.bin"), model: "-Z".into(), version: "-5.31".into() }),
        ("firmware fetch dry run", FwOp::FetchFile { file: PathBuf::from("/f/fw.bin"), model: "Z 9".into(), dry_run: true }),
        ("firmware fetch", FwOp::FetchFile { file: PathBuf::from("/f/my fw.bin"), model: "Z 9".into(), dry_run: false }),
        ("firmware fetch, dashed", FwOp::FetchFile { file: PathBuf::from("-fw.bin"), model: "-Z".into(), dry_run: false }),
        ("firmware pin", FwOp::Pin { snapshot: "snap.json".into(), model: "Z 9".into(), version: "5.31".into() }),
        ("firmware pin, dashed", FwOp::Pin { snapshot: "-snap.json".into(), model: "-Z".into(), version: "-5.31".into() }),
        ("firmware rollback", FwOp::Rollback { model: "Z 9".into(), version: "5.31".into(), output_dir: PathBuf::from("/data/rollback-bundles/x y") }),
        ("firmware rollback, dashed", FwOp::Rollback { model: "-Z".into(), version: "-5.31".into(), output_dir: PathBuf::from("-out") }),
    ]
}

/// Every argv the GUI builds, by name. `dir` has a space in it.
fn all_shapes(dir: &Path) -> Vec<(String, Vec<String>)> {
    let mut v: Vec<(String, Vec<String>)> = Vec::new();
    let mut add = |name: &str, argv: Vec<String>| v.push((name.to_string(), argv));

    add("discover", discover_argv());
    add("snapshot", snapshot_argv(dir, "30012345", "").unwrap());
    add("snapshot, label", snapshot_argv(dir, "id-3", "wedding").unwrap());
    add("snapshot, dashed", snapshot_argv(dir, "-S1", "-guest").unwrap());

    for allow_partial in [false, true] {
        let name = if allow_partial { "check, allow partial" } else { "check" };
        add(name, check_argv(dir, &CheckRequest { target: camera(), allow_partial }).unwrap());
    }

    for (name, op) in fw_ops() {
        if let Some(argv) = op.argv(dir).unwrap() {
            add(name, argv);
        }
    }

    let restore = |only: Option<&[&str]>| RestoreRequest {
        source_file: "snap.json".into(),
        target: camera(),
        only: only.map(|o| o.iter().map(|s| s.to_string()).collect()),
    };
    add("restore plan", restore_argv(dir, &restore(None), None).unwrap());
    add("restore plan, limited", restore_argv(dir, &restore(Some(&["kNkMAIDCapability_A", "kNkMAIDCapability_B"])), None).unwrap());
    add("restore apply", restore_argv(dir, &restore(None), Some(("abc123", "Z 9 1: restore from a.json: 3 would change; only: all"))).unwrap());
    add("restore apply, dashed summary", restore_argv(dir, &restore(Some(&["kNkMAIDCapability_A"])), Some(("-d", "-s: --x"))).unwrap());

    let transplant = |optins: &[&str]| TransplantRequest {
        source_body: ("Z6_3".into(), "SRC".into()),
        source_file: "snap.json".into(),
        target: camera(),
        allow_raw_code: optins.iter().map(|s| s.to_string()).collect(),
    };
    add("transplant plan", transplant_argv(dir, &transplant(&[]), None).unwrap());
    add("transplant plan, opted in", transplant_argv(dir, &transplant(&["kNkMAIDCapability_MeteringMode", "kNkMAIDCapability_WhiteBalance"]), None).unwrap());
    add("transplant apply", transplant_argv(dir, &transplant(&[]), Some(("abc123", "summary"))).unwrap());
    add("transplant apply, dashed summary", transplant_argv(dir, &transplant(&["kNkMAIDCapability_MeteringMode"]), Some(("-d", "-s: --x"))).unwrap());

    add("vendor-read", vendor_read_argv(0xD053, None));
    add("vendor-read, serial", vendor_read_argv(0x0053, Some("-S1")));
    add("vendor-write-ftp", vendor_write_ftp_argv(&profile(false), None));
    add("vendor-write-ftp, serial", vendor_write_ftp_argv(&profile(false), Some("30012345")));
    add("vendor-write-ftp, dashed", vendor_write_ftp_argv(&profile(true), Some("-S1")));
    v
}

fn shapes() -> (tempfile::TempDir, Vec<(String, Vec<String>)>) {
    let d = tempfile::Builder::new().prefix("fleet data ").tempdir().unwrap();
    let s = all_shapes(d.path());
    (d, s)
}

/// The command words of an argv: the global `--data-dir=` is skipped, then the subcommand (and for
/// `firmware` its subcommand).
fn command_of(argv: &[String]) -> String {
    let words: Vec<&str> = argv.iter().map(String::as_str).filter(|a| !a.starts_with("--")).collect();
    match words.as_slice() {
        ["firmware", sub, ..] => format!("firmware {sub}"),
        [first, ..] => (*first).to_string(),
        [] => String::new(),
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn every_argv_the_gui_builds_is_accepted_by_the_real_parser() {
    let (_d, cases) = shapes();
    for (name, argv) in &cases {
        let out = parse_only(argv);
        assert!(out.status.success(), "the CLI rejects the argv the GUI builds for {name:?}: {argv:?}\n{}", stderr(&out));
    }
}

/// A shape dropped from `all_shapes` would silently stop being checked: pin which CLI commands the
/// GUI is checked against. (`ls`, `ref set`, `settings` were the Tk GUI's; this GUI does not run them.)
#[test]
fn the_shapes_cover_every_cli_command_the_gui_runs() {
    let (_d, cases) = shapes();
    let covered: BTreeSet<String> = cases.iter().map(|(_, a)| command_of(a)).collect();
    let expected: BTreeSet<String> = [
        "discover", "snapshot", "check", "restore", "transplant", "vendor-read", "vendor-write-ftp", "firmware latest",
        "firmware browse", "firmware add", "firmware fetch", "firmware pin", "firmware rollback",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(covered, expected);
}

#[test]
fn every_firmware_operation_is_covered() {
    let seen: BTreeSet<usize> = fw_ops().iter().map(|(_, op)| fw_variant(op)).collect();
    assert_eq!(seen, (0..FW_VARIANTS).collect::<BTreeSet<_>>(), "fw_ops() must hold every FwOp variant");
}

/// The controls that make the test above mean something: the parse-only mode does reject, flag by
/// flag. Every flag of every shape, spelled wrongly, must fail the parse; a value after a flag that
/// the CLI does not take must too. (Only flags before a `--` are flags.)
#[test]
fn a_misspelled_flag_fails_the_parse_for_every_flag_of_every_shape() {
    let (_d, cases) = shapes();
    let mut checked = 0;
    for (name, argv) in &cases {
        let end = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
        for i in (0..end).filter(|i| argv[*i].starts_with("--")) {
            let mut bad = argv.clone();
            bad[i] = match argv[i].split_once('=') {
                Some((flag, value)) => format!("{flag}-no-such={value}"),
                None => format!("{}-no-such", argv[i]),
            };
            let out = parse_only(&bad);
            assert_eq!(out.status.code(), Some(2), "{name:?}: {} renamed went through: {bad:?}\n{}", argv[i], stderr(&out));
            checked += 1;
        }
    }
    assert!(checked > 80, "only {checked} flags were exercised: shapes() lost cases");
}

/// The reason the GUI uses `--flag=value`: a dashed value in the space-separated form is read as
/// another flag.
#[test]
fn a_dashed_value_in_the_space_separated_form_is_refused() {
    let (_d, cases) = shapes();
    let (_, base) = cases.iter().find(|(n, _)| n == "vendor-write-ftp, dashed").unwrap();
    assert!(parse_only(base).status.success(), "the unmodified argv must parse");
    let spaced: Vec<String> = base
        .iter()
        .flat_map(|a| match a.split_once('=') {
            Some(("--profile-name", v)) => vec!["--profile-name".to_string(), v.to_string()],
            _ => vec![a.clone()],
        })
        .collect();
    assert_ne!(&spaced, base, "the control must actually change something");
    assert_eq!(parse_only(&spaced).status.code(), Some(2), "`--profile-name -CLAUDE` must be refused; the GUI uses `=`");
}

#[test]
fn a_command_the_cli_does_not_have_fails_the_parse() {
    let (_d, cases) = shapes();
    let (_, base) = cases.iter().find(|(n, _)| n == "check").unwrap();
    let mut bad = base.clone();
    let at = bad.iter().position(|a| a == "check").unwrap();
    bad[at] = "chek".into();
    assert_eq!(parse_only(&bad).status.code(), Some(2));
}

/// `parse_discover` against what `nikon-fleet discover --json` really serialises (the library's
/// `DiscoveredCamera`, in the `{"cameras": [...]}` document `discover_report` prints), not a hand-
/// written copy of it: a field the library renames or drops fails here.
#[test]
fn discover_json_as_the_cli_serialises_it_parses() {
    let named = |id: u32, name: &str| DeviceInfo { id, name: name.into(), available: true, connected_pid: 0, version: String::new() };
    let usb = |serial: &str, model: &str, fw: &str| UsbCameraInfo { product_id: 1, serial: serial.into(), firmware: fw.into(), model: model.into() };
    let devices = [(named(3, "Z 9"), Some(usb("S9", "Z 9", "5.31"))), (named(4, "Z 5"), None), (named(5, "Z 9"), Some(usb("S9B", "Z 9", "5.31")))];
    let cameras: Vec<_> = devices.iter().map(|(dev, usb)| describe_device(dev, usb)).collect();
    let stdout = serde_json::json!({ "cameras": cameras }).to_string();
    let rows = parse_discover(&stdout).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!((rows[0].model.as_str(), rows[0].serial.as_str(), rows[0].firmware.as_str()), ("Z 9", "S9", "5.31"));
    assert!(rows[0].known_model && rows[0].baseline);
    assert_eq!(rows[1].serial, "id-4", "no USB serial: the storable id fallback");
    assert_eq!((rows[0].model_count, rows[1].model_count, rows[2].model_count), (2, 1, 2));
    assert_eq!(parse_discover(&serde_json::json!({ "cameras": Vec::<serde_json::Value>::new() }).to_string()).unwrap(), vec![]);
}

//! The boundary between `gui/fleet_gui.py` and the CLI it drives (full-review
//! nikon-fleet-20260909-7b2e#16, nikon-fleet-20261028-e5a3 H14).
//!
//! The Tk GUI builds argv by hand in `gui/fleet_lib.py`. This test runs
//! `gui/dump_argv.py`, which calls every one of those builders with awkward
//! values (a leading `-` in every free-text field, a path with a space), and feeds each argv to
//! the real binary's parser (`NIKON_FLEET_PARSE_ONLY=1`: parse, then exit 0). A flag renamed on either side,
//! or a value form clap rejects, fails here instead of at a user's click.
//!
//! Needs `python3` on PATH. Without it the test skips locally; it fails when `CI` or
//! `REQUIRE_PYTHON` is set, so a CI runner cannot pass by not running it.

use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");
const ROOT: &str = env!("CARGO_MANIFEST_DIR");

fn python_required() -> bool {
    ["CI", "REQUIRE_PYTHON"].iter().any(|k| std::env::var(k).is_ok_and(|v| !v.is_empty() && v != "0" && v != "false"))
}

/// The argv lists the Python GUI builds, or `None` when python3 cannot be run and that is allowed.
fn gui_argvs() -> Option<Vec<(String, Vec<String>)>> {
    let out = match Command::new("python3").arg(format!("{ROOT}/gui/dump_argv.py")).stderr(Stdio::inherit()).output() {
        Ok(o) => o,
        Err(e) if !python_required() => {
            eprintln!("skipping: python3 is not runnable ({e}); set REQUIRE_PYTHON=1 to make this a failure");
            return None;
        }
        Err(e) => panic!("python3 is required (CI or REQUIRE_PYTHON is set) but cannot be run: {e}"),
    };
    assert!(out.status.success(), "gui/dump_argv.py failed: {:?}", out.status);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("dump_argv.py prints JSON");
    Some(
        v.as_array()
            .expect("a list")
            .iter()
            .map(|c| {
                let name = c["name"].as_str().expect("name").to_string();
                let argv = c["argv"].as_array().expect("argv").iter().map(|a| a.as_str().expect("string").to_string()).collect();
                (name, argv)
            })
            .collect(),
    )
}

fn parse_only(argv: &[String]) -> std::process::Output {
    Command::new(BIN).env("NIKON_FLEET_PARSE_ONLY", "1").args(argv).output().expect("failed to run the CLI binary")
}

#[test]
fn every_argv_the_python_gui_builds_is_accepted_by_the_real_parser() {
    let Some(cases) = gui_argvs() else { return };
    // A builder dropped from dump_argv.py would silently stop being checked: pin the set.
    let names: Vec<&str> = cases.iter().map(|(n, _)| n.as_str()).collect();
    for expected in [
        "discover", "ls", "firmware ls", "settings show", "settings set-data-dir", "snapshot", "ref set",
        "firmware add", "firmware browse", "vendor-read", "vendor-write-ftp", "vendor-write-ftp dry-run",
    ] {
        assert!(names.contains(&expected), "gui/dump_argv.py no longer covers {expected:?}: {names:?}");
    }
    for (name, argv) in &cases {
        let out = parse_only(argv);
        assert!(
            out.status.success(),
            "the CLI rejects the argv gui/fleet_lib.py builds for {name:?}: {argv:?}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn the_parser_check_can_fail_a_renamed_flag_and_the_space_separated_dashed_value() {
    // The controls that make the test above mean something: the parse-only mode does reject. They start
    // from the full argv the GUI really builds (all required flags present) and change one thing.
    let Some(cases) = gui_argvs() else { return };
    let (_, base) = cases.iter().find(|(n, _)| n == "vendor-write-ftp dry-run").expect("the dry-run case");
    let with = |edit: &dyn Fn(&str) -> Vec<String>| -> Vec<String> { base.iter().flat_map(|a| edit(a)).collect() };
    assert!(parse_only(base).status.success(), "the unmodified argv must parse");

    let renamed = with(&|a| vec![if a == "--host=-guest" { "--hostname=-guest".to_string() } else { a.to_string() }]);
    assert_ne!(renamed, *base, "the control must actually change something");
    assert_eq!(parse_only(&renamed).status.code(), Some(2), "a renamed flag must fail the parse");

    // The reason the GUI uses `--flag=value`: a dashed value in the space-separated form is read as
    // another flag.
    let spaced = with(&|a| match a.split_once('=') {
        Some(("--profile-name", v)) => vec!["--profile-name".to_string(), v.to_string()],
        _ => vec![a.to_string()],
    });
    assert_ne!(spaced, *base, "the control must actually change something");
    assert_eq!(parse_only(&spaced).status.code(), Some(2), "`--profile-name -guest` must be refused; the GUI uses `=`");
}

#[test]
fn vendor_read_still_names_a_propcode_argument() {
    let out = Command::new(BIN).args(["vendor-read", "--help"]).output().expect("failed to run the CLI binary");
    let help = String::from_utf8(out.stdout).expect("--help is UTF-8");
    assert!(help.to_lowercase().contains("propcode"), "vendor-read --help no longer mentions a propcode argument:\n{help}");
}

#[test]
fn parse_only_mode_is_only_the_environment_variable() {
    // `settings show --json` is camera-free and prints a document. Without the variable it runs;
    // with it, nothing runs; a `--parse-only` argument is not an option at all.
    let tmp = std::env::temp_dir().join(format!("nikon-fleet-parse-only-{}.json", std::process::id()));
    let run = |env: Option<&str>, extra: &[&str]| {
        let mut c = Command::new(BIN);
        c.env("NIKON_FLEET_SETTINGS", &tmp).args(["settings", "show", "--json"]).args(extra);
        match env {
            Some(v) => c.env("NIKON_FLEET_PARSE_ONLY", v),
            None => c.env_remove("NIKON_FLEET_PARSE_ONLY"),
        };
        c.output().expect("failed to run the CLI binary")
    };
    let normal = run(None, &[]);
    assert!(normal.status.success() && !normal.stdout.is_empty(), "runs normally without the variable");
    let parsed_only = run(Some("1"), &[]);
    assert!(parsed_only.status.success() && parsed_only.stdout.is_empty(), "with the variable nothing runs");
    let not_one = run(Some("0"), &[]);
    assert!(!not_one.stdout.is_empty(), "only the value 1 turns it on");
    let as_flag = run(None, &["--parse-only"]);
    assert_eq!(as_flag.status.code(), Some(2), "there is no --parse-only option");
}

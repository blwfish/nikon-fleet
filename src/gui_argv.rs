//! Argv for the CLI's guarded writes (`restore`, `transplant`): the plan (`--dry-run`) and the
//! apply held to the plan a person confirmed (`--expect-digest`, `--expect-summary`), for a GUI that
//! runs them as a subprocess.
//!
//! They live in the library, as `vendor_argv` does, so the CLI's own integration tests feed their
//! exact output to the real parser (`tests/gui_argv_cli.rs`): a renamed flag or a value mangled on
//! the way fails there instead of at runtime in the GUI.
//!
//! Every value flag uses the `--flag=value` form, so a value that starts with `-` stays attached to
//! its flag, and `--` comes before the snapshot so a file name cannot be taken for a flag.

use std::collections::BTreeSet;

/// Why an argv cannot be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgvError {
    /// A selection of nothing. `restore` reads no `--only` as "every property", so an empty
    /// selection rendered as no flags would restore everything.
    EmptySelection,
}

impl std::fmt::Display for ArgvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArgvError::EmptySelection => f.write_str(
                "a restore limited to no properties is not a restore: an empty selection would be read as \"everything\"",
            ),
        }
    }
}

impl std::error::Error for ArgvError {}

fn guarded(command: &str, data_dir: &str, serial: &str, confirmed: Option<(&str, &str)>) -> Vec<String> {
    let mut argv = vec![format!("--data-dir={data_dir}"), command.to_string(), "--json".into(), format!("--serial={serial}")];
    match confirmed {
        None => argv.push("--dry-run".into()),
        Some((digest, summary)) => {
            argv.push(format!("--expect-digest={digest}"));
            argv.push(format!("--expect-summary={summary}"));
        }
    }
    argv
}

/// `--data-dir=D restore --json --serial=S (--dry-run | --expect-digest=… --expect-summary=…)
/// [--only=NAME]… -- SNAPSHOT`. `only` is `None` for every property; `Some` of nothing is an error.
pub fn restore_argv(
    data_dir: &str,
    serial: &str,
    only: Option<&BTreeSet<String>>,
    confirmed: Option<(&str, &str)>,
    snapshot: &str,
) -> Result<Vec<String>, ArgvError> {
    if only.is_some_and(BTreeSet::is_empty) {
        return Err(ArgvError::EmptySelection);
    }
    let mut argv = guarded("restore", data_dir, serial, confirmed);
    argv.extend(only.into_iter().flatten().map(|n| format!("--only={n}")));
    argv.extend(["--".to_string(), snapshot.to_string()]);
    Ok(argv)
}

/// `--data-dir=D transplant --json --serial=S (--dry-run | --expect-digest=… --expect-summary=…)
/// [--allow-raw-code=NAME]… -- SNAPSHOT`.
pub fn transplant_argv(
    data_dir: &str,
    serial: &str,
    allow_raw_code: &BTreeSet<String>,
    confirmed: Option<(&str, &str)>,
    snapshot: &str,
) -> Vec<String> {
    let mut argv = guarded("transplant", data_dir, serial, confirmed);
    argv.extend(allow_raw_code.iter().map(|n| format!("--allow-raw-code={n}")));
    argv.extend(["--".to_string(), snapshot.to_string()]);
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_plan_is_a_dry_run_and_an_apply_presents_the_confirmed_plan() {
        let plan = restore_argv("/d", "S1", None, None, "/d/snapshots/a.json").unwrap();
        assert_eq!(plan, ["--data-dir=/d", "restore", "--json", "--serial=S1", "--dry-run", "--", "/d/snapshots/a.json"]);
        let apply = restore_argv("/d", "S1", None, Some(("dig", "-a: 1")), "/d/snapshots/a.json").unwrap();
        assert_eq!(
            apply,
            ["--data-dir=/d", "restore", "--json", "--serial=S1", "--expect-digest=dig", "--expect-summary=-a: 1", "--", "/d/snapshots/a.json"]
        );
        let t = transplant_argv("/d", "S1", &set(&["kNkMAIDCapability_B", "kNkMAIDCapability_A"]), None, "/d/snapshots/a.json");
        assert_eq!(
            t,
            [
                "--data-dir=/d", "transplant", "--json", "--serial=S1", "--dry-run",
                "--allow-raw-code=kNkMAIDCapability_A", "--allow-raw-code=kNkMAIDCapability_B", "--", "/d/snapshots/a.json"
            ]
        );
    }

    #[test]
    fn a_selection_is_one_flag_per_property_and_a_selection_of_nothing_is_refused() {
        let argv = restore_argv("/d", "S", Some(&set(&["kNkMAIDCapability_A", "kNkMAIDCapability_B"])), None, "f").unwrap();
        assert_eq!(argv.iter().filter(|a| a.starts_with("--only=")).count(), 2);
        let one = restore_argv("/d", "S", Some(&set(&["kNkMAIDCapability_A"])), None, "f").unwrap();
        assert_eq!(one.iter().filter(|a| a.starts_with("--only=")).count(), 1);
        assert_eq!(restore_argv("/d", "S", Some(&BTreeSet::new()), None, "f"), Err(ArgvError::EmptySelection));
        assert!(!restore_argv("/d", "S", None, None, "f").unwrap().iter().any(|a| a.starts_with("--only")));
    }
}

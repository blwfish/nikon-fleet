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

use crate::snapshot::Snapshot;

/// Why an argv cannot be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgvError {
    /// A selection of nothing. `restore` reads no `--only` as "every property", so an empty
    /// selection rendered as no flags would restore everything.
    EmptySelection,
    /// The command line would not fit on every platform (Windows allows 32,767 characters): a
    /// spawn failure would come before any write, but it is told here, in words.
    TooLong { chars: usize },
}

/// The longest command line, in characters, these builders will produce. Under the smallest limit
/// the supported platforms have (32,767 on Windows), with room for the program path.
pub const ARGV_LIMIT: usize = 30_000;

impl std::fmt::Display for ArgvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArgvError::TooLong { chars } => write!(
                f,
                "the command line would be {chars} characters long (the limit here is {ARGV_LIMIT}): too many properties are selected for one restore; narrow the selection"
            ),
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
    restore_like_argv("restore", data_dir, serial, only, confirmed, snapshot)
}

/// `restore-extended`: the same line with `--only=CATEGORY` (strings, iptc, switches, ftp) in place of a property
/// name. `only` is `None` for every category; `Some` of nothing is an error, for the same reason.
pub fn restore_extended_argv(
    data_dir: &str,
    serial: &str,
    only: Option<&BTreeSet<String>>,
    confirmed: Option<(&str, &str)>,
    snapshot: &str,
) -> Result<Vec<String>, ArgvError> {
    restore_like_argv("restore-extended", data_dir, serial, only, confirmed, snapshot)
}

fn restore_like_argv(
    command: &str,
    data_dir: &str,
    serial: &str,
    only: Option<&BTreeSet<String>>,
    confirmed: Option<(&str, &str)>,
    snapshot: &str,
) -> Result<Vec<String>, ArgvError> {
    if only.is_some_and(BTreeSet::is_empty) {
        return Err(ArgvError::EmptySelection);
    }
    let mut argv = guarded(command, data_dir, serial, confirmed);
    argv.extend(only.into_iter().flatten().map(|n| format!("--only={n}")));
    argv.extend(["--".to_string(), snapshot.to_string()]);
    let chars: usize = argv.iter().map(|a| a.chars().count() + 1).sum();
    if chars > ARGV_LIMIT {
        return Err(ArgvError::TooLong { chars });
    }
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

/// Why a snapshot cannot be promoted to a reference: it is an edited copy.
///
/// The ONE statement of "an edited copy never becomes a reference": a reference is what `check`
/// compares a body against, and an edited copy is not a reading from a camera. The CLI's
/// `ref set` and the GUI's "Set ref" both call [`refuse_edited_reference`] and put their own file
/// name in front of this text, so the rule and its wording cannot drift apart between the two.
/// (It lives in this module only because this is the library file both front ends share and the one
/// this change could own; `Snapshot` or `ops::paths` would be the natural home.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditedCopyRefused;

impl std::fmt::Display for EditedCopyRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "is an edited copy, not a reading from a camera, and a reference is what `check` compares a body against: \
             choose a snapshot that was captured",
        )
    }
}

/// `Err` when `snap` is an edited copy (`Snapshot::is_edited`); a captured snapshot passes.
pub fn refuse_edited_reference(snap: &Snapshot) -> Result<(), EditedCopyRefused> {
    if snap.is_edited() { Err(EditedCopyRefused) } else { Ok(()) }
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
    fn the_extended_restore_is_the_same_line_with_its_own_command_and_categories() {
        let only = set(&["ftp", "strings"]);
        assert_eq!(
            restore_extended_argv("/d", "S1", Some(&only), None, "/d/snapshots/a.json").unwrap(),
            ["--data-dir=/d", "restore-extended", "--json", "--serial=S1", "--dry-run", "--only=ftp", "--only=strings", "--", "/d/snapshots/a.json"]
        );
        let apply = restore_extended_argv("/d", "S1", None, Some(("dig", "-a: 1")), "f").unwrap();
        assert_eq!(apply, ["--data-dir=/d", "restore-extended", "--json", "--serial=S1", "--expect-digest=dig", "--expect-summary=-a: 1", "--", "f"]);
        assert_eq!(restore_extended_argv("/d", "S", Some(&BTreeSet::new()), None, "f"), Err(ArgvError::EmptySelection));
    }

    #[test]
    fn a_command_line_too_long_for_every_platform_is_refused_in_words_before_it_is_run() {
        assert!(ARGV_LIMIT < 32_767, "under the smallest limit a supported platform has (Windows)");
        // a full undo is about 200 names, and the summary the apply echoes repeats them
        let names = |n: usize| (0..n).map(|i| format!("kNkMAIDCapability_Property{i:04}")).collect::<BTreeSet<_>>();
        let summary = |n: usize| format!("Z 9 T1: restore from rp.json: {n} would change; only: {}", names(n).into_iter().collect::<Vec<_>>().join(", "));
        let ok = names(200);
        let argv = restore_argv("/d", "S", Some(&ok), Some(("d", &summary(200))), "/d/snapshots/rp.json").unwrap();
        let used: usize = argv.iter().map(|a| a.chars().count() + 1).sum();
        assert!(used < ARGV_LIMIT, "{used}");
        // the limit is the boundary: one more name tips it over
        let mut n = 200;
        let tipped = loop {
            n += 1;
            let r = restore_argv("/d", "S", Some(&names(n)), Some(("d", &summary(n))), "/d/snapshots/rp.json");
            if let Err(e) = r {
                break (n, e);
            }
            assert!(n < 2000, "never refused");
        };
        let ArgvError::TooLong { chars } = tipped.1 else { panic!("{:?}", tipped.1) };
        assert!(chars > ARGV_LIMIT && tipped.1.to_string().contains("narrow the selection"));
        let before = restore_argv("/d", "S", Some(&names(tipped.0 - 1)), Some(("d", &summary(tipped.0 - 1))), "/d/snapshots/rp.json").unwrap();
        assert!(before.iter().map(|a| a.chars().count() + 1).sum::<usize>() <= ARGV_LIMIT);
    }

    #[test]
    fn an_edited_copy_is_refused_as_a_reference_and_a_captured_snapshot_is_not() {
        use crate::snapshot::{Camera, Transport};
        let captured = Snapshot::new(Camera { model: "Z 9".into(), serial: "S1".into(), firmware: "1".into() }, Transport::Usb, "2026-01-01T00:00:00Z".into());
        assert_eq!(refuse_edited_reference(&captured), Ok(()));
        let copy = crate::snapshot_edit::copy_for_edit(&captured, "a.json", None, "2026-02-02T00:00:00Z");
        assert!(copy.is_edited());
        assert_eq!(refuse_edited_reference(&copy), Err(EditedCopyRefused));
        assert!(EditedCopyRefused.to_string().contains("edited copy"));
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

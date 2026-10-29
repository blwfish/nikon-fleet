//! `firmware add` / `firmware ls` through the real CLI binary: the exit status for "already
//! archived" (so a script or GUI tests for it rather than reading the message) and the
//! machine-readable listing. No camera and no network: `add` copies a local file.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

fn run(data_dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN).arg("--data-dir").arg(data_dir).args(args).output().expect("spawn CLI")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn bin(dir: &Path, bytes: &[u8]) -> PathBuf {
    let p = dir.join("fw.bin");
    std::fs::write(&p, bytes).unwrap();
    p
}

fn add(data: &Path, bin: &Path, version: &str, extra: &[&str]) -> Output {
    let mut args = vec!["firmware", "add", bin.to_str().unwrap(), "--model", "Z 9", "--version", version];
    args.extend_from_slice(extra);
    run(data, &args)
}

fn archived_bin(data: &Path, version: &str) -> Vec<u8> {
    std::fs::read(data.join("firmware/Z_9").join(version).join("firmware.bin")).unwrap()
}

#[test]
fn adding_an_archived_version_exits_three_and_changes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let first = add(d.path(), &bin(d.path(), b"v1"), "5.31", &[]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));

    let again = add(d.path(), &bin(d.path(), b"v2-different"), "5.31", &[]);
    assert_eq!(again.status.code(), Some(3), "{}", stderr(&again));
    assert!(stderr(&again).contains("already exists for Z 9 5.31"), "{}", stderr(&again));
    assert_eq!(archived_bin(d.path(), "5.31"), b"v1", "the refused add must not touch the archive");
}

#[test]
fn only_already_archived_is_exit_three() {
    let d = tempfile::tempdir().unwrap();
    // A missing file, an unsafe version and a usage error are the ordinary failures.
    let missing = add(d.path(), &d.path().join("nope.bin"), "5.31", &[]);
    assert_eq!(missing.status.code(), Some(1), "{}", stderr(&missing));
    let unsafe_version = add(d.path(), &bin(d.path(), b"v1"), "../x", &[]);
    assert_eq!(unsafe_version.status.code(), Some(1), "{}", stderr(&unsafe_version));
    let usage = run(d.path(), &["firmware", "add"]);
    assert_eq!(usage.status.code(), Some(2));
    // --force is the deliberate overwrite; another version is not a conflict.
    let first = add(d.path(), &bin(d.path(), b"v1"), "5.31", &[]);
    assert_eq!(first.status.code(), Some(0));
    let forced = add(d.path(), &bin(d.path(), b"v2-longer"), "5.31", &["--force"]);
    assert_eq!(forced.status.code(), Some(0), "{}", stderr(&forced));
    assert_eq!(archived_bin(d.path(), "5.31"), b"v2-longer");
    assert_eq!(add(d.path(), &bin(d.path(), b"v3"), "5.32", &[]).status.code(), Some(0));
}

#[test]
fn the_help_of_add_fetch_and_browse_documents_the_exit_status() {
    for cmd in ["add", "fetch", "browse"] {
        let o = run(Path::new("."), &["firmware", cmd, "--help"]);
        assert_eq!(o.status.code(), Some(0));
        let help = stdout(&o);
        assert!(help.contains("Exit status:"), "{cmd}: {help}");
        assert!(help.contains("3  refused: that model and version is already in the archive"), "{cmd}: {help}");
    }
}

fn archive_with_one_good_and_two_unlistable(d: &Path) {
    assert_eq!(add(d, &bin(d, b"hello"), "5.31", &["--notes", "-first"]).status.code(), Some(0));
    let corrupt = d.join("firmware/Z_9/4.00");
    std::fs::create_dir_all(&corrupt).unwrap();
    std::fs::write(corrupt.join("metadata.json"), "{ nope").unwrap();
    let orphan = d.join("firmware/Z6_3/2.00");
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join("firmware.bin"), b"orphan").unwrap();
}

fn json_of(o: &Output) -> Value {
    assert_eq!(o.status.code(), Some(0), "{}", stderr(o));
    // The whole of stdout must parse: no banner, no trailing text.
    serde_json::from_str(&stdout(o)).unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}): {}", stdout(o)))
}

#[test]
fn ls_json_lists_valid_entries_and_accounts_for_the_unlistable_ones() {
    let d = tempfile::tempdir().unwrap();
    archive_with_one_good_and_two_unlistable(d.path());
    let v = json_of(&run(d.path(), &["firmware", "ls", "--json"]));
    assert_eq!(v["dir_exists"], json!(true));
    assert_eq!(v["skipped_dir_entries"], json!(0));
    assert!(v["dir"].as_str().unwrap().ends_with("firmware"));
    let rows = v["entries"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{v}");

    let ok = &rows[0];
    assert_eq!(ok["status"], "ok");
    assert_eq!((ok["model"].as_str(), ok["version"].as_str()), (Some("Z 9"), Some("5.31")));
    assert_eq!(ok["bin_present"], json!(true));
    assert_eq!(ok["bin_size_bytes"], json!(5));
    assert_eq!(ok["bin_sha256"], "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    assert_eq!(ok["notes"], "-first", "a note starting with `-` is accepted and kept");
    assert_eq!(ok["archived_by"], "fleet firmware add");
    assert!(Path::new(ok["directory"].as_str().unwrap()).ends_with(Path::new("Z_9").join("5.31")));
    assert_eq!(ok["canonical_snapshot_path"], Value::Null);

    let kinds: Vec<(&str, &str)> = rows[1..]
        .iter()
        .map(|r| {
            assert_eq!(r["status"], "unreadable");
            assert!(r["error"].as_str().is_some_and(|e| !e.is_empty()));
            (last_component(r["directory"].as_str().unwrap()), r["error_kind"].as_str().unwrap())
        })
        .collect();
    assert_eq!(kinds, [("2.00", "incomplete_archive"), ("4.00", "metadata_corrupt")]);
}

/// The last path component, whichever separator the platform prints.
fn last_component(dir: &str) -> &str {
    Path::new(dir).file_name().and_then(|n| n.to_str()).unwrap()
}

#[test]
fn ls_json_for_a_model_and_for_no_archive_at_all() {
    let d = tempfile::tempdir().unwrap();
    let none = json_of(&run(d.path(), &["firmware", "ls", "--json"]));
    assert_eq!((none["dir_exists"].clone(), none["entries"].clone()), (json!(false), json!([])));
    archive_with_one_good_and_two_unlistable(d.path());
    let z9 = json_of(&run(d.path(), &["firmware", "ls", "--json", "--model", "Z 9"]));
    let shown: Vec<&str> = z9["entries"].as_array().unwrap().iter().map(|e| last_component(e["directory"].as_str().unwrap())).collect();
    assert_eq!(shown, ["5.31", "4.00"], "the orphan under Z6_3 belongs to another model");
}

#[test]
fn the_text_listing_and_the_json_agree_on_what_is_valid_and_what_is_skipped() {
    let d = tempfile::tempdir().unwrap();
    archive_with_one_good_and_two_unlistable(d.path());
    let text = run(d.path(), &["firmware", "ls"]);
    assert_eq!(text.status.code(), Some(0));
    assert!(stdout(&text).contains("5.31") && !stdout(&text).contains("4.00"), "{}", stdout(&text));
    let err = stderr(&text);
    assert!(err.contains("4.00") && err.contains("2.00"), "skipped entries are said on stderr: {err}");
}

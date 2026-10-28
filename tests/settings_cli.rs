//! `nikon-fleet settings ...` through the real binary, against a settings file the test owns
//! (`NIKON_FLEET_SETTINGS`) and a scratch HOME, so nothing here reads or writes the real one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nikon_fleet::settings::{Os, default_data_dir_for};
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

struct Sandbox {
    _dir: tempfile::TempDir,
    home: PathBuf,
    file: PathBuf,
}

fn sandbox() -> Sandbox {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let file = dir.path().join("cfg/settings.json");
    Sandbox { _dir: dir, home, file }
}

impl Sandbox {
    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .env("HOME", &self.home)
            .env("NIKON_FLEET_SETTINGS", &self.file)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .current_dir(&self.home)
            .args(args)
            .output()
            .expect("spawn CLI")
    }

    fn default_dir(&self) -> String {
        let home = self.home.clone();
        let env = move |k: &str| (k == "HOME").then(|| OsString::from(&home));
        default_data_dir_for(Os::CURRENT, &env).display().to_string()
    }

    fn json(&self) -> Value {
        let o = self.run(&["settings", "show", "--json"]);
        assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
        serde_json::from_str(&stdout(&o)).unwrap_or_else(|e| panic!("{e}: {}", stdout(&o)))
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn file_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn with_no_settings_file_the_default_directory_is_in_use() {
    let s = sandbox();
    let v = s.json();
    assert_eq!(v["settings_exist"], json!(false));
    assert_eq!(v["data_dir"], Value::Null);
    assert_eq!(v["effective_data_dir"], json!(s.default_dir()));
    assert_eq!(v["settings_path"], json!(s.file.display().to_string()));
    assert_eq!(v["keys"], json!({}));
    let o = s.run(&["settings", "data-dir"]);
    assert_eq!(stdout(&o), format!("{}\n", s.default_dir()), "data-dir prints the directory and nothing else");
}

#[test]
fn set_data_dir_keeps_every_other_key_and_unset_clears_only_its_own() {
    let s = sandbox();
    std::fs::create_dir_all(s.file.parent().unwrap()).unwrap();
    let other = json!({"theme": "dark", "window": {"w": 800}, "recent": ["a"], "n": 3});
    std::fs::write(&s.file, other.to_string()).unwrap();

    let set = s.run(&["settings", "set-data-dir", "/Volumes/Files/snaps"]);
    assert_eq!(set.status.code(), Some(0), "{}", stderr(&set));
    let mut want = other.as_object().unwrap().clone();
    want.insert("data_dir".into(), json!("/Volumes/Files/snaps"));
    assert_eq!(file_json(&s.file), Value::Object(want));
    assert_eq!(stdout(&s.run(&["settings", "data-dir"])), "/Volumes/Files/snaps\n");
    assert_eq!(s.json()["data_dir"], json!("/Volumes/Files/snaps"));

    let unset = s.run(&["settings", "set-data-dir", "--unset"]);
    assert_eq!(unset.status.code(), Some(0), "{}", stderr(&unset));
    assert_eq!(file_json(&s.file), other, "only data_dir was removed");
    assert_eq!(stdout(&s.run(&["settings", "data-dir"])), format!("{}\n", s.default_dir()));
}

#[test]
fn a_blank_data_dir_in_the_file_is_unset_everywhere() {
    let s = sandbox();
    std::fs::create_dir_all(s.file.parent().unwrap()).unwrap();
    for blank in ["", " ", "\t "] {
        std::fs::write(&s.file, json!({ "data_dir": blank }).to_string()).unwrap();
        let v = s.json();
        assert_eq!(v["data_dir"], Value::Null, "{blank:?}");
        assert_eq!(v["effective_data_dir"], json!(s.default_dir()), "{blank:?}");
        assert_eq!(v["keys"]["data_dir"], json!(blank), "the file is reported as it is");
    }
}

#[test]
fn a_settings_file_that_cannot_be_parsed_is_reported_and_never_overwritten() {
    let s = sandbox();
    std::fs::create_dir_all(s.file.parent().unwrap()).unwrap();
    for corrupt in ["{ \"data_dir\": \"/x\"", "[]", "{\"data_dir\": 5}", ""] {
        std::fs::write(&s.file, corrupt).unwrap();
        for args in [&["settings", "show"][..], &["settings", "data-dir"], &["settings", "set-data-dir", "/new"], &["settings", "set-data-dir", "--unset"]] {
            let o = s.run(args);
            assert_eq!(o.status.code(), Some(1), "{args:?} on {corrupt:?}: {}", stdout(&o));
            assert!(stderr(&o).contains("left it as it is") || stderr(&o).contains("left as it is"), "{}", stderr(&o));
            assert_eq!(std::fs::read_to_string(&s.file).unwrap(), corrupt, "bytes untouched by {args:?}");
        }
    }
}

#[test]
fn set_data_dir_refuses_a_relative_path_and_a_missing_argument() {
    let s = sandbox();
    let relative = s.run(&["settings", "set-data-dir", "snaps"]);
    assert_eq!(relative.status.code(), Some(1));
    assert!(stderr(&relative).contains("not an absolute path"), "{}", stderr(&relative));
    let nothing = s.run(&["settings", "set-data-dir"]);
    assert_eq!(nothing.status.code(), Some(1));
    assert!(stderr(&nothing).contains("--unset"), "{}", stderr(&nothing));
    let both = s.run(&["settings", "set-data-dir", "/x", "--unset"]);
    assert_eq!(both.status.code(), Some(2), "clap refuses the combination");
    assert!(!s.file.exists(), "no refusal created the file");
}

#[test]
fn the_cli_keeps_its_default_of_the_current_directory_and_says_so() {
    // Decision (see the report): `--data-dir` still defaults to `.`; the GUIs' directory is
    // opt-in through `settings data-dir`. A plain `ls` must not read settings.json.
    let s = sandbox();
    std::fs::create_dir_all(s.file.parent().unwrap()).unwrap();
    std::fs::write(&s.file, json!({"data_dir": "/definitely/not/used"}).to_string()).unwrap();
    let o = s.run(&["ls"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(stdout(&o).starts_with("(no snapshots — directory ./snapshots does not exist)"), "{}", stdout(&o));
    // And the shell form the help text gives reaches the GUIs' directory.
    let help = Command::new(BIN).arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&help.stdout).into_owned();
    assert!(help.contains("CURRENT DIRECTORY") && help.contains("settings data-dir"), "{help}");
}

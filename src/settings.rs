//! The one `settings.json` loader and saver, shared by the CLI and both GUIs.
//!
//! The file lives at `{config dir}/net.blw.fleet/settings.json` (on macOS
//! `~/Library/Application Support/net.blw.fleet/settings.json`) and today holds
//! one key the programs act on, `data_dir`: the base directory for `snapshots/`,
//! `references/` and `firmware/`. The rules, which the GUIs used to carry as
//! comments:
//!
//! - **Unknown keys round-trip.** [`Settings`] keeps every key it read, so a save
//!   by one program never drops what another wrote.
//! - **A blank `data_dir` is unset.** Empty or whitespace-only means "use the
//!   default", in every program; `null` is unset too. Any other non-string value
//!   is an error, not a guess.
//! - **A file that cannot be read is an error, never a default.** A missing file
//!   is a fresh start (`Ok`); a file that exists but is not a JSON object, or
//!   cannot be read, is [`SettingsError`] and is **never overwritten**: [`update_in`]
//!   (the way to change a setting) reads the current file first and refuses to
//!   write over one it could not parse.
//! - **One location.** [`settings_path`] and [`default_data_dir`] are the only
//!   place the per-OS directories are decided. `NIKON_FLEET_SETTINGS` overrides
//!   the file's location (tests, portable setups); it moves the file, not the
//!   default data directory.
//!
//! Writes go through `ops::paths::write_atomic` (a temporary file in the same
//! directory, renamed into place), so a crash mid-write leaves the old file, not
//! half of a new one.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use thiserror::Error;

use crate::ops::paths::write_atomic;

/// Directory name under the OS config / data directory.
pub const APP_DIR: &str = "net.blw.fleet";
pub const SETTINGS_FILE: &str = "settings.json";
/// Environment variable that replaces the settings file's full path.
pub const PATH_ENV: &str = "NIKON_FLEET_SETTINGS";
const DATA_DIR_KEY: &str = "data_dir";

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid JSON ({source}); it was left as it is. Fix or delete it")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path} is not a JSON object; it was left as it is. Fix or delete it")]
    NotAnObject { path: String },
    #[error("{path}: {key:?} must be a string (or null), not {found}; it was left as it is")]
    WrongType { path: String, key: &'static str, found: &'static str },
    #[error("writing {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl SettingsError {
    /// Closed vocabulary for structured output.
    pub fn tag(&self) -> &'static str {
        match self {
            SettingsError::Io { .. } => "io",
            SettingsError::Parse { .. } => "parse",
            SettingsError::NotAnObject { .. } => "not_an_object",
            SettingsError::WrongType { .. } => "wrong_type",
            SettingsError::Write { .. } => "write",
        }
    }
}

/// What was read from `settings.json`: every key, with `data_dir` interpreted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    keys: Map<String, Value>,
}

impl Settings {
    /// The configured data directory, or `None` when unset (missing, `null`, empty or
    /// whitespace-only). The value is returned as written: it is not trimmed.
    pub fn data_dir(&self) -> Option<&str> {
        self.keys.get(DATA_DIR_KEY).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
    }

    /// Set (or, with `None`, clear) the data directory. A blank value clears it.
    pub fn set_data_dir(&mut self, dir: Option<&str>) {
        match dir.filter(|s| !s.trim().is_empty()) {
            Some(d) => {
                self.keys.insert(DATA_DIR_KEY.into(), Value::String(d.to_string()));
            }
            None => {
                self.keys.remove(DATA_DIR_KEY);
            }
        }
    }

    /// The configured data directory, else [`default_data_dir`].
    pub fn effective_data_dir(&self) -> PathBuf {
        self.data_dir().map(PathBuf::from).unwrap_or_else(default_data_dir)
    }

    /// Every key held, including the ones this crate does not interpret.
    pub fn keys(&self) -> &Map<String, Value> {
        &self.keys
    }

    /// Parse the text of a settings file. `path` is only for the error messages.
    pub fn from_json(text: &str, path: &Path) -> Result<Settings, SettingsError> {
        let shown = path.display().to_string();
        let value: Value =
            serde_json::from_str(text).map_err(|source| SettingsError::Parse { path: shown.clone(), source })?;
        let Value::Object(keys) = value else {
            return Err(SettingsError::NotAnObject { path: shown });
        };
        if let Some(v) = keys.get(DATA_DIR_KEY) {
            if !matches!(v, Value::String(_) | Value::Null) {
                return Err(SettingsError::WrongType { path: shown, key: DATA_DIR_KEY, found: json_type(v) });
            }
        }
        Ok(Settings { keys })
    }

    /// Read `path`. A missing file is empty settings; any other failure is an error.
    pub fn load_from(path: &Path) -> Result<Settings, SettingsError> {
        match fs::read_to_string(path) {
            Ok(text) => Settings::from_json(&text, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(source) => Err(SettingsError::Io { path: path.display().to_string(), source }),
        }
    }

    /// Write every key to `path`, atomically, creating its directory. Prefer [`update_in`] for a
    /// change: it re-reads the file first, so it cannot drop what another program wrote since
    /// this value was loaded, or overwrite a file that could not be parsed.
    pub fn save_to(&self, path: &Path) -> Result<(), SettingsError> {
        let write_err = |source| SettingsError::Write { path: path.display().to_string(), source };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).map_err(write_err)?;
        }
        let json = serde_json::to_string_pretty(&self.keys)
            .map_err(|e| write_err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
        write_atomic(path, (json + "\n").as_bytes()).map_err(write_err)
    }
}

/// Read the current file at `path` (a parse error stops here, with the file untouched), let `f`
/// change it, and write it back. The way every program changes a setting.
pub fn update_in(path: &Path, f: impl FnOnce(&mut Settings)) -> Result<Settings, SettingsError> {
    let mut settings = Settings::load_from(path)?;
    f(&mut settings);
    settings.save_to(path)?;
    Ok(settings)
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Locations
// ─────────────────────────────────────────────────────────────────────────

/// The operating systems whose conventions differ here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Windows,
    /// Linux and the rest: the XDG directories.
    Other,
}

impl Os {
    pub const CURRENT: Os = if cfg!(target_os = "macos") {
        Os::MacOs
    } else if cfg!(target_os = "windows") {
        Os::Windows
    } else {
        Os::Other
    };
}

/// `{config dir}` for `os`: macOS `~/Library/Application Support`, Windows `%APPDATA%`,
/// elsewhere `$XDG_CONFIG_HOME` or `~/.config`. `None` when the variables needed are unset.
pub fn config_dir_for(os: Os, env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    base_dir(os, env, "XDG_CONFIG_HOME", ".config")
}

/// `{data dir}` for `os`: the same as the config dir on macOS and Windows, elsewhere
/// `$XDG_DATA_HOME` or `~/.local/share`.
pub fn data_dir_for(os: Os, env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    base_dir(os, env, "XDG_DATA_HOME", ".local/share")
}

fn base_dir(os: Os, env: &dyn Fn(&str) -> Option<OsString>, xdg: &str, xdg_default: &str) -> Option<PathBuf> {
    let set = |name: &str| env(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    match os {
        Os::MacOs => set("HOME").map(|h| h.join("Library").join("Application Support")),
        Os::Windows => set("APPDATA"),
        Os::Other => set(xdg).filter(|p| p.is_absolute()).or_else(|| set("HOME").map(|h| h.join(xdg_default))),
    }
}

/// Where `settings.json` is, given an environment: `NIKON_FLEET_SETTINGS` if set, else
/// `{config dir}/net.blw.fleet/settings.json`, else (no home directory at all) `./settings.json`.
pub fn settings_path_for(os: Os, env: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
    if let Some(p) = env(PATH_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    config_dir_for(os, env).unwrap_or_else(|| PathBuf::from(".")).join(APP_DIR).join(SETTINGS_FILE)
}

/// The default data directory for `os`: `{data dir}/net.blw.fleet`, else `./net.blw.fleet`.
pub fn default_data_dir_for(os: Os, env: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
    data_dir_for(os, env).unwrap_or_else(|| PathBuf::from(".")).join(APP_DIR)
}

/// [`settings_path_for`] for this process.
pub fn settings_path() -> PathBuf {
    settings_path_for(Os::CURRENT, &|k| std::env::var_os(k))
}

/// [`default_data_dir_for`] for this process.
pub fn default_data_dir() -> PathBuf {
    default_data_dir_for(Os::CURRENT, &|k| std::env::var_os(k))
}

/// Read this process's `settings.json` (a missing file is empty settings).
pub fn load() -> Result<Settings, SettingsError> {
    Settings::load_from(&settings_path())
}

/// What `nikon-fleet settings show --json` prints: where the file is, what it says, and the
/// directory every program resolves from it. One value for a GUI to read instead of re-deriving.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Resolved {
    /// The settings file's path (whether or not it exists).
    pub settings_path: String,
    pub settings_exist: bool,
    /// `data_dir` as written in the file, or `null` when unset/blank.
    pub data_dir: Option<String>,
    /// The directory in use: `data_dir` when set, else the default.
    pub effective_data_dir: String,
    /// Every key in the file, including those this crate does not interpret.
    pub keys: BTreeMap<String, Value>,
}

/// Resolve the settings at `path`.
pub fn resolve(path: &Path) -> Result<Resolved, SettingsError> {
    let settings = Settings::load_from(path)?;
    Ok(Resolved {
        settings_path: path.display().to_string(),
        settings_exist: path.exists(),
        data_dir: settings.data_dir().map(str::to_string),
        effective_data_dir: settings.effective_data_dir().display().to_string(),
        keys: settings.keys().iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn env<'a>(set: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| set.iter().find(|(k, _)| *k == name).map(|(_, v)| OsString::from(*v))
    }

    fn parse(text: &str) -> Result<Settings, SettingsError> {
        Settings::from_json(text, Path::new("settings.json"))
    }

    // ── data_dir ─────────────────────────────────────────────────────────

    #[test]
    fn a_blank_data_dir_is_unset_in_every_spelling() {
        for blank in [json!(""), json!(" "), json!("\t\n "), json!(null)] {
            let s = parse(&json!({ "data_dir": blank }).to_string()).unwrap();
            assert_eq!(s.data_dir(), None, "{blank}");
            assert_eq!(s.effective_data_dir(), default_data_dir());
        }
        assert_eq!(parse("{}").unwrap().data_dir(), None);
        // One real character is a path; it is returned exactly as written, not trimmed.
        assert_eq!(parse(r#"{"data_dir": " /x "}"#).unwrap().data_dir(), Some(" /x "));
        assert_eq!(parse(r#"{"data_dir": "/x"}"#).unwrap().effective_data_dir(), PathBuf::from("/x"));
    }

    #[test]
    fn a_wrongly_typed_data_dir_is_an_error_not_a_default() {
        for bad in [json!(5), json!(true), json!(["/x"]), json!({"a": 1})] {
            let e = parse(&json!({ "data_dir": bad }).to_string()).unwrap_err();
            assert_eq!(e.tag(), "wrong_type", "{bad}");
            assert!(e.to_string().contains("data_dir"), "{e}");
        }
    }

    #[test]
    fn setting_a_blank_data_dir_clears_it() {
        let mut s = parse(r#"{"data_dir": "/x"}"#).unwrap();
        s.set_data_dir(Some("  "));
        assert_eq!(s.data_dir(), None);
        assert!(!s.keys().contains_key("data_dir"), "cleared, not stored as a blank");
        s.set_data_dir(Some("/y"));
        assert_eq!(s.data_dir(), Some("/y"));
        s.set_data_dir(None);
        assert_eq!(s.data_dir(), None);
    }

    // ── parse failures are surfaced and never overwritten ────────────────

    #[test]
    fn a_file_that_is_not_a_json_object_is_an_error() {
        for (text, tag) in [("{ nope", "parse"), ("", "parse"), ("[1]", "not_an_object"), ("\"x\"", "not_an_object"), ("5", "not_an_object")] {
            assert_eq!(parse(text).unwrap_err().tag(), tag, "{text:?}");
        }
    }

    #[test]
    fn a_missing_file_is_a_fresh_start_but_an_unreadable_one_is_an_error() {
        let d = TempDir::new().unwrap();
        assert_eq!(Settings::load_from(&d.path().join("nope.json")).unwrap(), Settings::default());
        // A directory where the file should be: exists, cannot be read.
        let dir_as_file = d.path().join("settings.json");
        fs::create_dir(&dir_as_file).unwrap();
        assert_eq!(Settings::load_from(&dir_as_file).unwrap_err().tag(), "io");
    }

    #[test]
    fn a_corrupt_file_is_never_overwritten_by_an_update() {
        let d = TempDir::new().unwrap();
        let path = d.path().join("settings.json");
        for corrupt in ["{ \"data_dir\": \"/x\"", "[]", "{\"data_dir\": 5}"] {
            fs::write(&path, corrupt).unwrap();
            let e = update_in(&path, |s| s.set_data_dir(Some("/new"))).unwrap_err();
            assert!(matches!(e.tag(), "parse" | "not_an_object" | "wrong_type"), "{e}");
            assert_eq!(fs::read_to_string(&path).unwrap(), corrupt, "bytes untouched");
        }
    }

    // ── round trip ───────────────────────────────────────────────────────

    #[test]
    fn unknown_keys_survive_a_save_of_another_key() {
        let d = TempDir::new().unwrap();
        let path = d.path().join("nested/dir/settings.json");
        // Written by "another program": a key this crate has never heard of, of every JSON type.
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let other = json!({"theme": "dark", "window": {"w": 800, "h": 600}, "recent": ["a", "b"], "n": 3, "flag": true});
        fs::write(&path, other.to_string()).unwrap();
        update_in(&path, |s| s.set_data_dir(Some("/snaps"))).unwrap();
        let back: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let mut want = other.as_object().unwrap().clone();
        want.insert("data_dir".into(), json!("/snaps"));
        assert_eq!(back, Value::Object(want));
    }

    #[test]
    fn an_update_rereads_the_file_so_a_stale_copy_cannot_drop_a_newer_key() {
        let d = TempDir::new().unwrap();
        let path = d.path().join("settings.json");
        fs::write(&path, r#"{"data_dir": "/old"}"#).unwrap();
        let stale = Settings::load_from(&path).unwrap(); // a GUI loaded this once, at start
        fs::write(&path, r#"{"data_dir": "/old", "added_later": 1}"#).unwrap(); // the other GUI saved
        update_in(&path, |s| s.set_data_dir(Some("/new"))).unwrap();
        let back = Settings::load_from(&path).unwrap();
        assert_eq!(back.data_dir(), Some("/new"));
        assert_eq!(back.keys().get("added_later"), Some(&json!(1)));
        // Saving the stale copy directly is the lossy path update_in exists to avoid.
        stale.save_to(&path).unwrap();
        assert!(Settings::load_from(&path).unwrap().keys().get("added_later").is_none());
    }

    #[test]
    fn saving_creates_the_directory_and_leaves_no_temp_file() {
        let d = TempDir::new().unwrap();
        let path = d.path().join("a/b/settings.json");
        update_in(&path, |s| s.set_data_dir(Some("/x"))).unwrap();
        let names: Vec<String> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["settings.json"]);
        assert_eq!(Settings::load_from(&path).unwrap().data_dir(), Some("/x"));
    }

    #[test]
    fn a_failed_write_is_reported_and_the_old_file_stays() {
        let d = TempDir::new().unwrap();
        // The target is a directory: rename over it fails.
        let path = d.path().join("settings.json");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), "x").unwrap();
        let e = Settings::default().save_to(&path).unwrap_err();
        assert_eq!(e.tag(), "write");
        assert!(path.join("keep").exists());
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1, "no temp file left behind");
    }

    // ── one location ─────────────────────────────────────────────────────

    #[test]
    fn the_macos_paths_are_the_ones_both_guis_have_always_used() {
        let e = env(&[("HOME", "/Users/me")]);
        assert_eq!(
            settings_path_for(Os::MacOs, &e),
            PathBuf::from("/Users/me/Library/Application Support/net.blw.fleet/settings.json")
        );
        assert_eq!(
            default_data_dir_for(Os::MacOs, &e),
            PathBuf::from("/Users/me/Library/Application Support/net.blw.fleet")
        );
    }

    #[test]
    fn windows_and_xdg_locations() {
        let w = env(&[("APPDATA", r"C:\Users\me\AppData\Roaming")]);
        assert_eq!(
            settings_path_for(Os::Windows, &w),
            PathBuf::from(r"C:\Users\me\AppData\Roaming").join(APP_DIR).join(SETTINGS_FILE)
        );
        // Expected paths are joined, not written with `/`: on a Windows host `join` puts a backslash in.
        let under = |base: &str, rest: &[&str]| rest.iter().fold(PathBuf::from(base), |p, c| p.join(c));
        let home = env(&[("HOME", "/home/me")]);
        assert_eq!(settings_path_for(Os::Other, &home), under("/home/me", &[".config", APP_DIR, SETTINGS_FILE]));
        assert_eq!(default_data_dir_for(Os::Other, &home), under("/home/me", &[".local", "share", APP_DIR]));
        // An XDG value is honoured only when it is absolute, which "/cfg" is not on a Windows host.
        let abs = |p: &str| if cfg!(windows) { format!("C:{p}") } else { p.to_string() };
        let (cfg, dat) = (abs("/cfg"), abs("/dat"));
        let pairs = [("HOME", "/home/me"), ("XDG_CONFIG_HOME", cfg.as_str()), ("XDG_DATA_HOME", dat.as_str())];
        let xdg = env(&pairs);
        assert_eq!(settings_path_for(Os::Other, &xdg), under(&cfg, &[APP_DIR, SETTINGS_FILE]));
        assert_eq!(default_data_dir_for(Os::Other, &xdg), under(&dat, &[APP_DIR]));
        // A relative or empty XDG value is ignored, as the spec says.
        let rel = env(&[("HOME", "/home/me"), ("XDG_CONFIG_HOME", "cfg"), ("XDG_DATA_HOME", "")]);
        assert_eq!(settings_path_for(Os::Other, &rel), under("/home/me", &[".config", APP_DIR, SETTINGS_FILE]));
        assert_eq!(default_data_dir_for(Os::Other, &rel), under("/home/me", &[".local", "share", APP_DIR]));
    }

    #[test]
    fn with_no_home_the_paths_fall_back_to_the_working_directory_not_an_error() {
        let none = env(&[]);
        for os in [Os::MacOs, Os::Windows, Os::Other] {
            assert_eq!(settings_path_for(os, &none), Path::new(".").join(APP_DIR).join(SETTINGS_FILE), "{os:?}");
            assert_eq!(default_data_dir_for(os, &none), Path::new(".").join(APP_DIR), "{os:?}");
        }
        // An empty HOME is no home.
        assert_eq!(default_data_dir_for(Os::MacOs, &env(&[("HOME", "")])), Path::new(".").join(APP_DIR));
    }

    #[test]
    fn the_path_override_moves_the_file_and_not_the_default_data_dir() {
        let e = env(&[("HOME", "/Users/me"), (PATH_ENV, "/tmp/s.json")]);
        assert_eq!(settings_path_for(Os::MacOs, &e), PathBuf::from("/tmp/s.json"));
        assert_eq!(
            default_data_dir_for(Os::MacOs, &e),
            PathBuf::from("/Users/me/Library/Application Support/net.blw.fleet")
        );
        // An empty override is no override.
        let e = env(&[("HOME", "/Users/me"), (PATH_ENV, "")]);
        assert!(settings_path_for(Os::MacOs, &e).ends_with("net.blw.fleet/settings.json"));
    }

    #[test]
    fn resolve_reports_the_file_the_key_and_the_directory_in_use() {
        let d = TempDir::new().unwrap();
        let path = d.path().join("settings.json");
        let missing = resolve(&path).unwrap();
        assert!(!missing.settings_exist && missing.data_dir.is_none() && missing.keys.is_empty());
        assert_eq!(missing.effective_data_dir, default_data_dir().display().to_string());
        fs::write(&path, r#"{"data_dir": "/snaps", "theme": "dark"}"#).unwrap();
        let r = resolve(&path).unwrap();
        assert!(r.settings_exist);
        assert_eq!((r.data_dir.as_deref(), r.effective_data_dir.as_str()), (Some("/snaps"), "/snaps"));
        assert_eq!(r.keys["theme"], json!("dark"));
        fs::write(&path, r#"{"data_dir": "  "}"#).unwrap();
        assert_eq!(resolve(&path).unwrap().data_dir, None, "blank is unset");
        fs::write(&path, "{").unwrap();
        assert_eq!(resolve(&path).unwrap_err().tag(), "parse");
    }
}

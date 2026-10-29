//! Names, not paths.
//!
//! Tools that act for an agent take snapshot **names** and resolve them inside
//! one directory. An agent-supplied value must not read arbitrary JSON or write
//! anywhere (docs/mcp-plan.md §3), so this does not reuse the CLI's
//! `resolve_snapshot_path`, which passes through absolute paths and any
//! relative path that exists under the current directory.
//!
//! A name is one plain file-name component. For a file that must exist, the
//! name is joined to the canonical directory, the result is canonicalized
//! (following symlinks) and must still be a regular file **directly in** that
//! directory: a symlink out of the directory is refused, a symlink to another
//! file in it is allowed. For a file to be created, the directory must
//! canonicalize and the name must be plain; creating it is the caller's job
//! (create-new).

use std::path::{Path, PathBuf};

/// A single plain path component: non-empty, not `.` or `..`, no separators,
/// no NUL.
///
/// The one definition of "plain" for the library: `ops::list` (the reference
/// lookup), `firmware` (archive names) and this module's resolvers all call it,
/// and the GUIs reach it through [`checked_reference_filename`] and
/// `ops::list::is_plain_name_component` (an alias). `PLAIN_COMPONENT_TABLE` is
/// the table every caller's test runs.
pub fn is_safe_path_component(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', '\0']) && !windows_unsafe_name(s)
}

/// Names Windows would not keep as written: the characters it forbids in a file name, control
/// characters, a trailing dot or space (it strips them, so `...` would name the directory itself and
/// `a ` would silently become `a`), and the device names (`CON`, `NUL`, `COM1`, also with an
/// extension). Always false elsewhere, where all of these are ordinary names.
fn windows_unsafe_name(s: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }
    if s.chars().any(|c| c.is_control() || "<>:\"|?*".contains(c)) || s.ends_with(['.', ' ']) {
        return true;
    }
    let stem = s.split('.').next().unwrap_or(s).trim_end_matches(' ').to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|p| {
            stem.strip_prefix(p).is_some_and(|n| n.len() == 1 && matches!(n.as_bytes()[0], b'1'..=b'9'))
        })
}

/// The file name of the reference for a camera, for a model and serial that
/// came from a data file (a snapshot's own `camera` fields) rather than from a
/// body this process just read: `{model_slug}_{serial}.json`, or
/// [`PathError::Unsafe`] when either is not a plain component. Every writer of
/// `references/` (`ref set`, both GUIs) builds the name through this, so a `/`
/// or `..` in a snapshot's serial cannot steer the write out of the directory.
pub fn checked_reference_filename(model: &str, serial: &str) -> Result<String, PathError> {
    for value in [model, serial] {
        if !is_safe_path_component(value) {
            return Err(PathError::Unsafe { name: value.to_string() });
        }
    }
    Ok(crate::ops::list::reference_filename(model, serial))
}

/// Write `bytes` to `path` so a reader sees the old file or the whole new one, never half: a
/// temporary file in the same directory (a name no other writer in this process or another can
/// pick), synced to disk, then a rename over `path`. The temporary file is removed when any step
/// fails. The directory must exist. When `path` is a symlink the file it points to is replaced and
/// the link is kept (a dotfile-managed settings.json stays managed).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let target = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    let dir = target.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let written = std::fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, &target));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp); // best effort; `written` is the error to report
    }
    written
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// Not a single plain file name (separators, `..`, empty, NUL).
    Unsafe { name: String },
    /// The directory itself cannot be resolved.
    NoDirectory { dir: String },
    /// No such file in the directory.
    NotFound { name: String },
    /// Resolves (through a symlink) to somewhere other than directly inside the directory.
    EscapesDirectory { name: String },
    /// Exists but is not a regular file.
    NotAFile { name: String },
}

impl PathError {
    pub fn tag(&self) -> &'static str {
        match self {
            PathError::Unsafe { .. } => "unsafe_name",
            PathError::NoDirectory { .. } => "no_directory",
            PathError::NotFound { .. } => "not_found",
            PathError::EscapesDirectory { .. } => "escapes_directory",
            PathError::NotAFile { .. } => "not_a_file",
        }
    }
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::Unsafe { name } => write!(f, "{name:?} is not a plain file name (no separators, no \"..\")"),
            PathError::NoDirectory { dir } => write!(f, "directory {dir} does not exist or cannot be resolved"),
            PathError::NotFound { name } => write!(f, "no file named {name:?} in the data directory"),
            PathError::EscapesDirectory { name } => write!(f, "{name:?} resolves to a file outside the data directory"),
            PathError::NotAFile { name } => write!(f, "{name:?} is not a regular file"),
        }
    }
}

impl std::error::Error for PathError {}

fn canonical_dir(dir: &Path) -> Result<PathBuf, PathError> {
    dir.canonicalize().map_err(|_| PathError::NoDirectory { dir: dir.display().to_string() })
}

/// Resolve `name` to an existing regular file directly inside `dir`.
pub fn resolve_existing(dir: &Path, name: &str) -> Result<PathBuf, PathError> {
    if !is_safe_path_component(name) {
        return Err(PathError::Unsafe { name: name.to_string() });
    }
    let dir = canonical_dir(dir)?;
    let resolved = dir
        .join(name)
        .canonicalize()
        .map_err(|_| PathError::NotFound { name: name.to_string() })?;
    if resolved.parent() != Some(dir.as_path()) {
        return Err(PathError::EscapesDirectory { name: name.to_string() });
    }
    if !resolved.is_file() {
        return Err(PathError::NotAFile { name: name.to_string() });
    }
    Ok(resolved)
}

/// The path a new file called `name` would have inside `dir`.
pub fn resolve_new(dir: &Path, name: &str) -> Result<PathBuf, PathError> {
    if !is_safe_path_component(name) {
        return Err(PathError::Unsafe { name: name.to_string() });
    }
    Ok(canonical_dir(dir)?.join(name))
}

/// Every value any caller of the plain-component predicate must agree on, and
/// whether it is plain. `ops::list`, `firmware` and this module's tests each run
/// their own entry point over this one table, so a copy that drifts fails there.
#[cfg(test)]
pub(crate) const PLAIN_COMPONENT_TABLE: &[(&str, bool)] = &[
    ("5.31", true),
    ("Z_9", true),
    ("Z 9", true),
    ("a.json", true),
    ("...", true),
    (".hidden", true),
    ("id-1", true),
    ("é", true),
    ("0000003023668", true),
    // Pinned: control characters other than NUL are plain today.
    ("a\nb", true),
    ("a ", true),
    ("", false),
    (".", false),
    ("..", false),
    ("../../../../tmp/pwned", false),
    ("../x", false),
    ("./a", false),
    ("a/b", false),
    ("a/", false),
    ("/", false),
    ("/etc/passwd", false),
    ("a\\b", false),
    ("\\", false),
    ("a\0b", false),
    ("\0", false),
];

/// Table entries that are plain on unix but not on Windows (see `windows_unsafe_name`), and names
/// that only Windows refuses; a caller's expectation for `value` on this platform comes from
/// [`plain_here`], not from the table's flag alone.
#[cfg(test)]
pub(crate) const WINDOWS_ONLY_UNSAFE: &[&str] = &["...", "a\nb", "a ", "a:b", "CON", "nul.txt", "com1", "LPT9", "a?", "a*", "a\"b", "a<b", "a|b"];

/// Whether `value` is plain on THIS platform: the table's flag, unless Windows additionally refuses it.
#[cfg(test)]
pub(crate) fn plain_here(value: &str, plain: bool) -> bool {
    plain && !(cfg!(windows) && WINDOWS_ONLY_UNSAFE.contains(&value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn plain_components_and_boundaries() {
        for (value, plain) in PLAIN_COMPONENT_TABLE {
            assert_eq!(is_safe_path_component(value), plain_here(value, *plain), "{value:?}");
        }
    }

    #[test]
    fn what_only_windows_refuses_is_plain_everywhere_else_and_refused_there() {
        for value in WINDOWS_ONLY_UNSAFE {
            // Plain by the portable rule; Windows alone refuses it.
            assert_eq!(is_safe_path_component(value), !cfg!(windows), "{value:?}");
        }
        // Not device names: a longer stem, or a digit outside 1..=9, is an ordinary name everywhere.
        for ok in ["CONSOLE", "COM0", "COM10", "LPT", "com1x", "nul_", "Z 9", "5.31"] {
            assert!(is_safe_path_component(ok), "{ok:?}");
        }
    }

    #[test]
    fn checked_reference_filename_accepts_plain_and_refuses_everything_else() {
        assert_eq!(checked_reference_filename("Z 9", "30012345").unwrap(), "Z_9_30012345.json");
        assert_eq!(checked_reference_filename("Z6_3", "id-1").unwrap(), "Z6_3_id-1.json");
        // Each field is checked on its own: a plain model never excuses a hostile serial, or the reverse.
        for (value, plain) in PLAIN_COMPONENT_TABLE {
            assert_eq!(checked_reference_filename("Z 9", value).is_ok(), plain_here(value, *plain), "serial {value:?}");
            assert_eq!(checked_reference_filename(value, "1").is_ok(), plain_here(value, *plain), "model {value:?}");
        }
        assert_eq!(
            checked_reference_filename("Z 9", "x/y").unwrap_err(),
            PathError::Unsafe { name: "x/y".into() }
        );
        assert_eq!(checked_reference_filename("../evil", "1").unwrap_err().tag(), "unsafe_name");
    }

    #[test]
    fn the_checked_name_is_the_unchecked_name_for_plain_input() {
        // One naming rule: checking never changes the name, it only refuses.
        for (m, s) in [("Z 9", "1"), ("Z6_3", "id-12"), ("Z 5", "0000003023668")] {
            assert_eq!(checked_reference_filename(m, s).unwrap(), crate::ops::list::reference_filename(m, s));
        }
    }

    #[test]
    fn write_atomic_replaces_whole_and_leaves_no_temp() {
        let d = TempDir::new().unwrap();
        let p = d.path().join("f.json");
        write_atomic(&p, b"one").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"one");
        write_atomic(&p, b"two-longer").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two-longer");
        write_atomic(&p, b"").unwrap(); // zero bytes is a write, not a no-op
        assert_eq!(fs::read(&p).unwrap(), b"");
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1, "no temp file left");
    }

    #[cfg(unix)]
    #[test]
    fn writing_through_a_symlink_replaces_its_target_and_keeps_the_link() {
        let d = TempDir::new().unwrap();
        let real = d.path().join("managed");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("settings.json"), "old").unwrap();
        let link = d.path().join("settings.json");
        std::os::unix::fs::symlink(real.join("settings.json"), &link).unwrap();
        write_atomic(&link, b"new").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link is kept");
        assert_eq!(fs::read_to_string(real.join("settings.json")).unwrap(), "new");
        assert_eq!(fs::read_dir(&real).unwrap().count(), 1, "no temp file left beside the target");
    }

    #[test]
    fn concurrent_atomic_writes_to_one_path_never_share_a_temp_name() {
        let d = TempDir::new().unwrap();
        let p = d.path().join("f.json");
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        write_atomic(&p, format!("{{\"writer\":{i}}}").as_bytes()).expect("a concurrent write must not collide");
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("{\"writer\":") && text.ends_with('}'), "a whole file from one writer: {text}");
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1, "no temp files left");
    }

    #[test]
    fn a_failed_atomic_write_keeps_the_old_content_and_cleans_up() {
        let d = TempDir::new().unwrap();
        // The target is a non-empty directory: the rename cannot replace it.
        let p = d.path().join("f.json");
        fs::create_dir(&p).unwrap();
        fs::write(p.join("keep"), "x").unwrap();
        assert!(write_atomic(&p, b"new").is_err());
        assert!(p.join("keep").exists());
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1, "temp removed");
        // A missing directory fails and creates nothing.
        assert!(write_atomic(&d.path().join("no/such/f.json"), b"x").is_err());
        assert!(!d.path().join("no").exists());
    }

    #[test]
    fn a_plain_existing_file_resolves_inside_the_directory() {
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("a.json"), "{}").unwrap();
        let p = resolve_existing(d.path(), "a.json").unwrap();
        assert_eq!(p, d.path().canonicalize().unwrap().join("a.json"));
    }

    #[test]
    fn separators_dotdot_absolute_and_empty_are_unsafe() {
        let d = TempDir::new().unwrap();
        for bad in ["", "..", "a/b.json", "../x.json", "/etc/passwd", "sub\\x.json"] {
            let e = resolve_existing(d.path(), bad).unwrap_err();
            assert_eq!(e.tag(), "unsafe_name", "{bad:?}");
            assert_eq!(resolve_new(d.path(), bad).unwrap_err().tag(), "unsafe_name", "{bad:?}");
        }
    }

    #[test]
    fn a_bare_name_that_exists_only_under_the_cwd_is_not_found() {
        // The CLI's resolve_snapshot_path would pass this through; this must not.
        let d = TempDir::new().unwrap();
        let name = "Cargo.toml"; // exists under the test's cwd (the package root), not in `d`
        assert!(Path::new(name).exists());
        assert_eq!(resolve_existing(d.path(), name).unwrap_err(), PathError::NotFound { name: name.into() });
    }

    #[test]
    fn a_missing_file_and_a_missing_directory_are_distinct() {
        let d = TempDir::new().unwrap();
        assert_eq!(resolve_existing(d.path(), "no.json").unwrap_err().tag(), "not_found");
        assert_eq!(resolve_existing(&d.path().join("nope"), "a.json").unwrap_err().tag(), "no_directory");
        assert_eq!(resolve_new(&d.path().join("nope"), "a.json").unwrap_err().tag(), "no_directory");
    }

    #[test]
    fn a_directory_is_not_a_file() {
        let d = TempDir::new().unwrap();
        fs::create_dir(d.path().join("sub.json")).unwrap();
        assert_eq!(resolve_existing(d.path(), "sub.json").unwrap_err().tag(), "not_a_file");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_directory_is_refused() {
        let d = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret.json"), "{}").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.json"), d.path().join("link.json")).unwrap();
        assert_eq!(resolve_existing(d.path(), "link.json").unwrap_err().tag(), "escapes_directory");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_another_file_in_the_directory_is_allowed_and_resolves_to_its_target() {
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("real.json"), "{}").unwrap();
        std::os::unix::fs::symlink(d.path().join("real.json"), d.path().join("alias.json")).unwrap();
        let p = resolve_existing(d.path(), "alias.json").unwrap();
        assert_eq!(p, d.path().canonicalize().unwrap().join("real.json"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_file_in_a_subdirectory_is_refused() {
        let d = TempDir::new().unwrap();
        fs::create_dir(d.path().join("sub")).unwrap();
        fs::write(d.path().join("sub/x.json"), "{}").unwrap();
        std::os::unix::fs::symlink(d.path().join("sub/x.json"), d.path().join("deep.json")).unwrap();
        assert_eq!(resolve_existing(d.path(), "deep.json").unwrap_err().tag(), "escapes_directory");
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_not_found() {
        let d = TempDir::new().unwrap();
        std::os::unix::fs::symlink(d.path().join("gone.json"), d.path().join("dangling.json")).unwrap();
        assert_eq!(resolve_existing(d.path(), "dangling.json").unwrap_err().tag(), "not_found");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_is_resolved_not_trusted_by_its_name() {
        // The data directory itself may be a symlink; files in it still resolve.
        let real = TempDir::new().unwrap();
        fs::write(real.path().join("a.json"), "{}").unwrap();
        let holder = TempDir::new().unwrap();
        let link = holder.path().join("data");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        assert_eq!(resolve_existing(&link, "a.json").unwrap(), real.path().canonicalize().unwrap().join("a.json"));
    }

    #[test]
    fn resolve_new_joins_to_the_canonical_directory_without_requiring_the_file() {
        let d = TempDir::new().unwrap();
        let p = resolve_new(d.path(), "fresh.json").unwrap();
        assert_eq!(p, d.path().canonicalize().unwrap().join("fresh.json"));
        assert!(!p.exists());
    }

    #[test]
    fn errors_have_distinct_tags_and_readable_messages() {
        let all = [
            PathError::Unsafe { name: "a".into() },
            PathError::NoDirectory { dir: "d".into() },
            PathError::NotFound { name: "a".into() },
            PathError::EscapesDirectory { name: "a".into() },
            PathError::NotAFile { name: "a".into() },
        ];
        let tags: std::collections::BTreeSet<_> = all.iter().map(PathError::tag).collect();
        assert_eq!(tags.len(), all.len());
        assert!(all.iter().all(|e| !e.to_string().is_empty()));
    }
}

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
pub fn is_safe_path_component(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', '\0'])
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn plain_components_and_boundaries() {
        for ok in ["5.31", "Z_9", "a.json", "...", ".hidden", "id-1", "é"] {
            assert!(is_safe_path_component(ok), "{ok:?}");
        }
        for bad in ["", ".", "..", "../../../../tmp/pwned", "a/b", "a\\b", "/etc/passwd", "a\0b", "./a", "a/"] {
            assert!(!is_safe_path_component(bad), "{bad:?}");
        }
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

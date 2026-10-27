//! Cross-process lock serialising Nikon SDK sessions.
//!
//! Two SDK sessions at once can hang the one that holds the camera in
//! `DisconnectDevice` (docs/mcp-hardware-findings.md F3), and a USB reset
//! during another process's capture yields empty or partial snapshots (F2).
//! The CLI, the GUI and the MCP worker therefore take this lock around every
//! SDK session (docs/mcp-plan.md §3).
//!
//! - It is an `flock` **held on an open file descriptor**, never a lock file
//!   or PID file: the kernel releases it when the holder dies, including when
//!   the MCP watchdog kills a hung worker, whereas a stale file would wedge
//!   every later call. The file at the path is only an anchor; its presence
//!   or contents mean nothing.
//! - It lives at a **fixed per-user path**, not in `--data-dir`: the CLI, GUI
//!   and MCP can each be given a different data dir, and the lock protects
//!   the USB bus, not a data dir. `flock` on a network share is also
//!   unreliable.
//! - Contention is a typed [`LockError::Busy`], never a wait or a retry loop.
//! - The descriptor is close-on-exec, so a child process (the GUI spawns CLI
//!   subprocesses) does not inherit the lock.
//!
//! The lock cannot exclude USB clients outside this project (NX Studio,
//! Image Capture, other PTP software).

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Environment override for the lock path (tests, unusual layouts). An empty
/// value is ignored, like an unset one.
pub const LOCK_PATH_ENV: &str = "NIKON_FLEET_CAMERA_LOCK";

#[derive(Debug, Error)]
pub enum LockError {
    #[error(
        "camera busy: another nikon-fleet session (CLI, GUI or MCP) holds the camera lock at {}",
        path.display()
    )]
    Busy { path: PathBuf },
    #[error("cannot use the camera lock at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot determine the camera lock path: {LOCK_PATH_ENV} and HOME are both unset or empty")]
    NoPath,
    #[error("the camera lock is not supported on this platform")]
    Unsupported,
}

impl LockError {
    /// Closed vocabulary for structured output.
    pub fn tag(&self) -> &'static str {
        match self {
            LockError::Busy { .. } => "camera_busy",
            LockError::Io { .. } => "lock_io",
            LockError::NoPath => "lock_no_path",
            LockError::Unsupported => "lock_unsupported",
        }
    }
}

/// Held for as long as this value lives; released on drop or process death.
#[derive(Debug)]
pub struct CameraLock {
    _file: File,
    path: PathBuf,
}

impl CameraLock {
    /// Take the lock at the default path (see [`lock_path_from`]).
    pub fn acquire() -> Result<Self, LockError> {
        let path = lock_path_from(std::env::var_os(LOCK_PATH_ENV), std::env::var_os("HOME"))?;
        Self::acquire_at(&path)
    }

    /// Take the lock at `path`, creating its parent directories. Fails with
    /// [`LockError::Busy`] at once if another open descriptor holds it, in
    /// this process or another.
    pub fn acquire_at(path: &Path) -> Result<Self, LockError> {
        let io_err = |source| LockError::Io { path: path.to_path_buf(), source };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(io_err)?;
        }
        // Not truncate: the contents are irrelevant, and truncating would
        // disturb nothing but also gains nothing.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(io_err)?;
        try_flock_exclusive(&file, path)?;
        Ok(CameraLock { _file: file, path: path.to_path_buf() })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
fn try_flock_exclusive(file: &File, path: &Path) -> Result<(), LockError> {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: the fd is open for the duration of the call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) => return Err(LockError::Busy { path: path.to_path_buf() }),
            Some(libc::EINTR) => continue,
            _ => return Err(LockError::Io { path: path.to_path_buf(), source: err }),
        }
    }
}

#[cfg(not(unix))]
fn try_flock_exclusive(_file: &File, _path: &Path) -> Result<(), LockError> {
    Err(LockError::Unsupported)
}

/// The lock path: the override if set and non-empty, else a fixed per-user
/// location under `home`. Pure so the choice is testable without touching the
/// environment.
pub fn lock_path_from(
    env_override: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, LockError> {
    if let Some(p) = env_override.filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let home = home.filter(|h| !h.is_empty()).ok_or(LockError::NoPath)?;
    let base = PathBuf::from(home);
    Ok(if cfg!(target_os = "macos") {
        base.join("Library/Application Support/nikon-fleet/camera.lock")
    } else {
        base.join(".local/state/nikon-fleet/camera.lock")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn lock_in(dir: &TempDir) -> PathBuf {
        dir.path().join("camera.lock")
    }

    #[test]
    fn creates_parent_directories_and_the_anchor_file() {
        let d = TempDir::new().unwrap();
        let p = d.path().join("a/b/camera.lock");
        let l = CameraLock::acquire_at(&p).unwrap();
        assert!(p.is_file());
        assert_eq!(l.path(), p);
    }

    #[test]
    fn second_holder_is_busy_while_first_lives() {
        let d = TempDir::new().unwrap();
        let _first = CameraLock::acquire_at(&lock_in(&d)).unwrap();
        let second = CameraLock::acquire_at(&lock_in(&d)).unwrap_err();
        assert!(matches!(second, LockError::Busy { .. }), "{second:?}");
        assert_eq!(second.tag(), "camera_busy");
        assert!(second.to_string().contains("camera.lock"), "message names the path");
    }

    #[test]
    fn released_on_drop() {
        let d = TempDir::new().unwrap();
        let first = CameraLock::acquire_at(&lock_in(&d)).unwrap();
        drop(first);
        CameraLock::acquire_at(&lock_in(&d)).unwrap();
    }

    #[test]
    fn stale_anchor_file_does_not_block() {
        // The reason this is an fd lock and not a lock file.
        let d = TempDir::new().unwrap();
        fs::write(lock_in(&d), "pid 99999 left over\n").unwrap();
        CameraLock::acquire_at(&lock_in(&d)).unwrap();
    }

    #[test]
    fn different_paths_do_not_conflict() {
        let d = TempDir::new().unwrap();
        let _a = CameraLock::acquire_at(&d.path().join("a.lock")).unwrap();
        let _b = CameraLock::acquire_at(&d.path().join("b.lock")).unwrap();
    }

    #[test]
    fn path_that_is_a_directory_is_io_not_busy() {
        let d = TempDir::new().unwrap();
        let e = CameraLock::acquire_at(d.path()).unwrap_err();
        assert!(matches!(e, LockError::Io { .. }), "{e:?}");
        assert_eq!(e.tag(), "lock_io");
    }

    #[test]
    fn parent_that_is_a_file_is_io() {
        let d = TempDir::new().unwrap();
        let f = d.path().join("file");
        fs::write(&f, "x").unwrap();
        let e = CameraLock::acquire_at(&f.join("camera.lock")).unwrap_err();
        assert!(matches!(e, LockError::Io { .. }), "{e:?}");
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_is_close_on_exec() {
        // A child process (the GUI spawns CLI subprocesses) must not inherit it.
        use std::os::fd::AsRawFd;
        let d = TempDir::new().unwrap();
        let l = CameraLock::acquire_at(&lock_in(&d)).unwrap();
        let flags = unsafe { libc::fcntl(l._file.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0);
        assert!(flags & libc::FD_CLOEXEC != 0);
    }

    #[cfg(unix)]
    #[test]
    fn lock_dies_with_a_killed_holder() {
        // The watchdog kills hung workers with SIGKILL; the next call must not
        // be camera_busy. A separate process holds the lock via perl's flock.
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let d = TempDir::new().unwrap();
        let p = lock_in(&d);
        let mut child = Command::new("perl")
            .args([
                "-MFcntl=:flock",
                "-e",
                "open(F, '>>', $ARGV[0]) or exit 2; flock(F, LOCK_EX|LOCK_NB) or exit 3; \
                 $|=1; print \"locked\\n\"; sleep 60;",
            ])
            .arg(&p)
            .stdout(Stdio::piped())
            .spawn()
            .expect("perl is needed for this test");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "locked");

        let busy = CameraLock::acquire_at(&p).unwrap_err();
        assert!(matches!(busy, LockError::Busy { .. }), "{busy:?}");

        child.kill().unwrap(); // SIGKILL
        child.wait().unwrap();
        CameraLock::acquire_at(&p).expect("kernel released the killed holder's lock");
    }

    // ── lock_path_from ───────────────────────────────────────────────────

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn override_wins_over_home() {
        assert_eq!(lock_path_from(os("/x/y.lock"), os("/home/u")).unwrap(), PathBuf::from("/x/y.lock"));
    }

    #[test]
    fn override_alone_is_enough() {
        assert_eq!(lock_path_from(os("/x/y.lock"), None).unwrap(), PathBuf::from("/x/y.lock"));
    }

    #[test]
    fn empty_override_is_ignored_not_an_empty_path() {
        let p = lock_path_from(os(""), os("/home/u")).unwrap();
        assert!(p.starts_with("/home/u"), "{p:?}");
    }

    #[test]
    fn default_path_is_under_home_and_never_the_data_dir() {
        let p = lock_path_from(None, os("/home/u")).unwrap();
        assert!(p.starts_with("/home/u") && p.ends_with("nikon-fleet/camera.lock"), "{p:?}");
    }

    #[test]
    fn no_override_and_no_usable_home_is_an_error() {
        for home in [None, os("")] {
            let e = lock_path_from(None, home.clone()).unwrap_err();
            assert!(matches!(e, LockError::NoPath), "{home:?} -> {e:?}");
            assert_eq!(e.tag(), "lock_no_path");
        }
        // An empty override does not rescue a missing HOME.
        assert!(matches!(lock_path_from(os(""), None), Err(LockError::NoPath)));
    }

    #[test]
    fn error_tags_are_distinct() {
        let tags = [
            LockError::Busy { path: PathBuf::new() }.tag(),
            LockError::Io { path: PathBuf::new(), source: io::Error::other("x") }.tag(),
            LockError::NoPath.tag(),
            LockError::Unsupported.tag(),
        ];
        let mut sorted = tags.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), tags.len());
    }
}

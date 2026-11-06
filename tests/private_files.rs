//! A snapshot, a restore point or a reference can hold a camera's FTP password in clear text, so every file the
//! library writes for one is owner-only (`0600`) on unix. Run here through the public API with a umask of 0, so a
//! file made with the default mode would be `0666` and the assertion could not pass by accident. Unix only: other
//! platforms have no modes.
#![cfg(unix)]

use nikon_fleet::ops::paths::{create_new_private, write_atomic};
use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;

/// The umask is process-wide and these tests run on several threads: one at a time.
static UMASK: Mutex<()> = Mutex::new(());

fn with_umask_zero(body: impl FnOnce()) {
    let _guard = UMASK.lock().unwrap_or_else(|p| p.into_inner());
    // SAFETY: umask only swaps the process's file-creation mask; it is restored before the guard drops.
    let old = unsafe { libc::umask(0) };
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
    unsafe { libc::umask(old) };
    if let Err(p) = out {
        std::panic::resume_unwind(p);
    }
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn snapshot() -> Snapshot {
    let mut s = Snapshot::new(
        Camera { model: "Z 9".into(), serial: "0000003023668".into(), firmware: "5.31".into() },
        Transport::File,
        "2026-11-06T00:00:00Z".into(),
    );
    s.insert("Artist", 0xD072, serde_json::json!("somebody"));
    s
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

#[test]
fn save_to_file_publishes_an_owner_only_file_over_a_world_readable_one() {
    with_umask_zero(|| {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("snap.json");
        snapshot().save_to_file(&p).unwrap();
        assert_eq!(mode_of(&p), 0o600, "a new snapshot");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        snapshot().save_to_file(&p).unwrap();
        assert_eq!(mode_of(&p), 0o600, "a snapshot saved over an older world-readable one");
        assert_eq!(names_in(d.path()), ["snap.json"], "no temporary left behind");
    });
}

/// Captures, restore points and edited copies are all published with `save_new`.
#[test]
fn save_new_publishes_an_owner_only_file_and_leaves_no_temporary() {
    with_umask_zero(|| {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("capture.json");
        snapshot().save_new(&p).unwrap();
        assert_eq!(mode_of(&p), 0o600, "snapshot.rs::publish_new must create its temporary with paths::create_new_private");
        assert_eq!(names_in(d.path()), ["capture.json"]);
        assert!(snapshot().save_new(&p).is_err(), "never replaces");
        assert_eq!(mode_of(&p), 0o600);
    });
}

#[test]
fn the_public_helpers_make_owner_only_files() {
    with_umask_zero(|| {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        drop(create_new_private(&a).unwrap());
        assert_eq!(mode_of(&a), 0o600);
        let b = d.path().join("b");
        write_atomic(&b, b"x").unwrap();
        assert_eq!(mode_of(&b), 0o600);
    });
}

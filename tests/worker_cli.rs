//! `nikon-fleet worker` through the real binary: the protocol on an inherited
//! file descriptor, with nothing on stdout, a result that is readable before
//! the process exits, and typed errors for what can be checked without a camera.
#![cfg(unix)]

use std::fs::File;
use std::io::BufReader;
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use nikon_fleet::camera_lock::CameraLock;
use nikon_fleet::ops::worker::{Outcome, Read, ReadEnd, read_until_result};
use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");
const CHANNEL_FD: i32 = 3;

struct Worker {
    child: Child,
    reader: BufReader<File>,
}

/// Spawn `nikon-fleet [global...] worker --result-fd 3 <op...>` with a pipe's
/// write end installed as descriptor 3 in the child.
fn spawn(global: &[&str], op: &[&str], env: &[(&str, &str)]) -> Worker {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (r, w) = (fds[0], fds[1]);
    let mut cmd = Command::new(BIN);
    cmd.args(global).arg("worker").arg("--result-fd").arg(CHANNEL_FD.to_string()).args(op);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(w, CHANNEL_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // dup2 already replaced whatever was at CHANNEL_FD (possibly `r` itself).
            if r != CHANNEL_FD {
                libc::close(r);
            }
            if w != CHANNEL_FD {
                libc::close(w);
            }
            Ok(())
        });
    }
    let child = cmd.spawn().expect("spawn worker");
    unsafe { libc::close(w) }; // only the child holds the write end now
    let reader = BufReader::new(unsafe { File::from_raw_fd(r) });
    Worker { child, reader }
}

impl Worker {
    fn read(&mut self) -> Read {
        read_until_result(&mut self.reader, |_| {}).expect("read protocol")
    }

    /// Wait for exit and return (status code, stdout, stderr).
    fn finish(mut self) -> (Option<i32>, String, String) {
        use std::io::Read as _;
        let mut out = String::new();
        let mut err = String::new();
        self.child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
        self.child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
        let status = self.child.wait().unwrap();
        (status.code(), out, err)
    }
}

fn err_tag(r: &Read) -> String {
    match &r.end {
        ReadEnd::Result(Outcome::Err { error, .. }) => error.tag.clone(),
        other => panic!("expected an error result, got {other:?}"),
    }
}

#[test]
fn selftest_reports_progress_then_an_ok_result_and_keeps_stdout_empty() {
    let mut w = spawn(&[], &["selftest"], &[]);
    let r = w.read();
    assert_eq!(r.progress, ["starting", "working"]);
    assert_eq!(r.end, ReadEnd::Result(Outcome::ok(json!({"selftest": true}))));
    assert_eq!(r.garbage_lines, 0);
    let (code, stdout, _) = w.finish();
    assert_eq!(code, Some(0));
    assert_eq!(stdout, "", "nothing may reach stdout");
}

#[test]
fn a_failing_selftest_is_an_error_result_with_exit_status_1() {
    let mut w = spawn(&[], &["selftest", "--fail"], &[]);
    let r = w.read();
    assert_eq!(err_tag(&r), "selftest_failed");
    match r.end {
        ReadEnd::Result(Outcome::Err { error, .. }) => assert_eq!(error.detail, Some(json!({"why": "test"}))),
        _ => unreachable!(),
    }
    let (code, stdout, _) = w.finish();
    assert_eq!((code, stdout.as_str()), (Some(1), ""));
}

#[test]
fn the_result_is_readable_while_the_process_is_still_hanging_after_it() {
    // A teardown hang (F3): the work is done and reported, the process never exits.
    let mut w = spawn(&[], &["selftest", "--hang-after-result"], &[]);
    let r = w.read();
    assert_eq!(r.end, ReadEnd::Result(Outcome::ok(json!({"selftest": true}))));
    std::thread::sleep(Duration::from_millis(300));
    assert!(w.child.try_wait().unwrap().is_none(), "still alive: the parent got the result without waiting for exit");
    w.child.kill().unwrap(); // SIGKILL, as the watchdog would
    let status = w.child.wait().unwrap();
    assert_eq!(status.code(), None, "killed by a signal, not an exit");
}

#[test]
fn the_exit_status_is_not_the_answer() {
    let mut w = spawn(&[], &["selftest", "--exit-code", "7"], &[]);
    let r = w.read();
    assert_eq!(r.end, ReadEnd::Result(Outcome::ok(json!({"selftest": true}))), "ok result despite the odd exit");
    assert_eq!(w.finish().0, Some(7));
}

#[test]
fn a_result_channel_that_is_not_open_or_is_a_standard_stream_is_refused() {
    for (fd, needle) in [("77", "not open"), ("1", "stdout"), ("2", "standard stream"), ("0", "standard stream")] {
        let out = Command::new(BIN)
            .args(["worker", "--result-fd", fd, "selftest"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "fd {fd}");
        assert!(String::from_utf8_lossy(&out.stderr).contains(needle), "fd {fd}: {}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stdout.is_empty(), "fd {fd}: the protocol must never fall back to stdout");
    }
}

#[test]
fn capture_refuses_a_serial_that_cannot_name_a_body_before_touching_the_sdk() {
    for bad in ["id-5", "id-0", ""] {
        let mut w = spawn(&["--sdk-bundle", "/nonexistent/sdk"], &["capture", "--serial", bad], &[]);
        let r = w.read();
        assert_eq!(err_tag(&r), "no_usb_serial", "{bad:?}");
        assert_eq!(r.progress, ["starting"], "stopped at validation");
        assert_eq!(w.finish().0, Some(1));
    }
}

#[test]
fn capture_reports_a_missing_sdk_bundle_as_a_typed_error() {
    let mut w = spawn(&["--sdk-bundle", "/nonexistent/sdk"], &["capture", "--serial", "0000003023668"], &[]);
    let r = w.read();
    assert_eq!(err_tag(&r), "sdk_bundle_missing");
    let (code, stdout, _) = w.finish();
    assert_eq!((code, stdout.as_str()), (Some(1), ""));
}

#[test]
fn capture_reports_camera_busy_when_another_session_holds_the_lock_without_loading_the_sdk() {
    let schema = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk-runtime/MaidLayer.config");
    if !schema.exists() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join("camera.lock");
    let _held = CameraLock::acquire_at(&lock_path).unwrap();
    // The bundle only has to exist: the lock is taken before it is loaded.
    let bundle = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let mut w = spawn(
        &["--sdk-bundle", bundle.to_str().unwrap(), "--schema", schema.to_str().unwrap()],
        &["capture", "--serial", "0000003023668"],
        &[("NIKON_FLEET_CAMERA_LOCK", lock_path.to_str().unwrap())],
    );
    let r = w.read();
    assert_eq!(err_tag(&r), "camera_busy");
    match &r.end {
        ReadEnd::Result(Outcome::Err { error, .. }) => assert!(error.message.contains("camera busy"), "{}", error.message),
        _ => unreachable!(),
    }
    assert_eq!(r.progress, ["starting"], "never got as far as the SDK");
    assert_eq!(w.finish().0, Some(1));
}

#[test]
fn a_result_that_cannot_be_delivered_is_exit_status_2_not_a_success() {
    // The parent has gone: nobody holds the read end of the pipe.
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (r, w) = (fds[0], fds[1]);
    unsafe { libc::close(r) };
    let mut cmd = Command::new(BIN);
    cmd.args(["worker", "--result-fd", &CHANNEL_FD.to_string(), "selftest"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(w, CHANNEL_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if w != CHANNEL_FD {
                libc::close(w);
            }
            Ok(())
        });
    }
    let out = cmd.spawn().unwrap().wait_with_output().unwrap();
    unsafe { libc::close(w) };
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty());
}

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

// ── check / transplant-plan / transplant-apply: what is refused before any camera ──

fn data_dir_with_source() -> (tempfile::TempDir, String) {
    use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
    let d = tempfile::tempdir().unwrap();
    let snaps = d.path().join("snapshots");
    std::fs::create_dir_all(&snaps).unwrap();
    let mut s = Snapshot::new(
        Camera { model: "Z6_3".into(), serial: "SRC".into(), firmware: "2.00".into() },
        Transport::Usb,
        "2026-10-27T00:00:00Z".into(),
    );
    s.insert("kNkMAIDCapability_Copyright", 1, json!("me"));
    s.insert("kNkMAIDCapability_WBAutoType", 2, json!(3));
    s.save_to_file(snaps.join("source.json")).unwrap();
    (d, "source.json".to_string())
}

fn g<'a>(data_dir: &'a std::path::Path) -> Vec<&'a str> {
    vec!["--data-dir", data_dir.to_str().unwrap(), "--sdk-bundle", "/nonexistent/sdk"]
}

#[test]
fn check_refuses_a_bad_serial_and_a_missing_bundle_with_typed_errors() {
    let d = tempfile::tempdir().unwrap();
    let mut w = spawn(&g(d.path()), &["check", "--serial", "id-3"], &[]);
    assert_eq!(err_tag(&w.read()), "no_usb_serial");
    let mut w = spawn(&g(d.path()), &["check", "--serial", "0000003023668"], &[]);
    assert_eq!(err_tag(&w.read()), "sdk_bundle_missing");
}

#[test]
fn transplant_plan_refuses_unsafe_and_missing_sources_before_the_sdk() {
    let (d, _) = data_dir_with_source();
    for (name, tag) in [("../source.json", "unsafe_name"), ("/etc/passwd", "unsafe_name"), ("a/b.json", "unsafe_name"), ("", "unsafe_name"), ("nope.json", "not_found")] {
        let mut w = spawn(&g(d.path()), &["transplant-plan", "--serial", "0000003023668", "--source", name], &[]);
        let r = w.read();
        assert_eq!(err_tag(&r), tag, "{name:?}");
        assert_eq!(r.progress, ["starting"], "{name:?}: stopped before the SDK");
        let (code, stdout, _) = w.finish();
        assert_eq!((code, stdout.as_str()), (Some(1), ""), "{name:?}");
    }
}

#[test]
fn transplant_plan_refuses_a_bad_opt_in_name_before_the_sdk() {
    let (d, src) = data_dir_with_source();
    let mut w = spawn(&g(d.path()), &["transplant-plan", "--serial", "0000003023668", "--source", &src, "--allow-raw-code", "Nope"], &[]);
    let r = w.read();
    assert_eq!(err_tag(&r), "bad_opt_in");
    assert_eq!(r.progress, ["starting"]);
    // A string property is not something the opt-in applies to.
    let mut w = spawn(&g(d.path()), &["transplant-plan", "--serial", "0000003023668", "--source", &src, "--allow-raw-code", "Copyright"], &[]);
    assert_eq!(err_tag(&w.read()), "bad_opt_in");
}

#[test]
fn a_valid_plan_request_gets_as_far_as_the_missing_bundle() {
    let (d, src) = data_dir_with_source();
    let mut w = spawn(&g(d.path()), &["transplant-plan", "--serial", "0000003023668", "--source", &src, "--allow-raw-code", "WBAutoType"], &[]);
    assert_eq!(err_tag(&w.read()), "sdk_bundle_missing", "every earlier check passed");
}

#[test]
fn transplant_apply_demands_the_digest_and_the_summary_before_anything_else() {
    let (d, src) = data_dir_with_source();
    for (digest, summary) in [("", "s"), ("  ", "s"), ("d", ""), ("d", "   ")] {
        let mut w = spawn(
            &g(d.path()),
            &["transplant-apply", "--serial", "0000003023668", "--source", &src, "--digest", digest, "--summary", summary],
            &[],
        );
        let r = w.read();
        assert_eq!(err_tag(&r), "missing_confirmation", "{digest:?}/{summary:?}");
        assert_eq!(r.progress, ["starting"]);
    }
    // With both present the request goes on (here to the missing bundle).
    let mut w = spawn(
        &g(d.path()),
        &["transplant-apply", "--serial", "0000003023668", "--source", &src, "--digest", "d", "--summary", "s"],
        &[],
    );
    assert_eq!(err_tag(&w.read()), "sdk_bundle_missing");
}

#[test]
fn transplant_apply_refuses_an_unsafe_source_even_with_a_valid_confirmation() {
    let (d, _) = data_dir_with_source();
    let mut w = spawn(
        &g(d.path()),
        &["transplant-apply", "--serial", "0000003023668", "--source", "../x", "--digest", "d", "--summary", "s"],
        &[],
    );
    assert_eq!(err_tag(&w.read()), "unsafe_name");
}

#[test]
fn plan_and_apply_report_camera_busy_without_loading_the_sdk() {
    let schema = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk-runtime/MaidLayer.config");
    if !schema.exists() {
        return;
    }
    let (d, src) = data_dir_with_source();
    let lock_dir = tempfile::tempdir().unwrap();
    let lock_path = lock_dir.path().join("camera.lock");
    let _held = CameraLock::acquire_at(&lock_path).unwrap();
    let bundle = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let global = ["--data-dir", d.path().to_str().unwrap(), "--sdk-bundle", bundle.to_str().unwrap(), "--schema", schema.to_str().unwrap()];
    let env = [("NIKON_FLEET_CAMERA_LOCK", lock_path.to_str().unwrap())];
    for op in [
        vec!["transplant-plan", "--serial", "0000003023668", "--source", &src],
        vec!["transplant-apply", "--serial", "0000003023668", "--source", &src, "--digest", "d", "--summary", "s"],
        vec!["check", "--serial", "0000003023668"],
        vec!["restore-plan", "--serial", "0000003023668", "--source", &src],
        vec!["restore-apply", "--serial", "0000003023668", "--source", &src, "--digest", "d", "--summary", "s"],
    ] {
        let mut w = spawn(&global, &op, &env);
        let r = w.read();
        assert_eq!(err_tag(&r), "camera_busy", "{op:?}");
        assert_eq!(r.progress, ["starting"], "{op:?}");
        assert_eq!(w.finish().0, Some(1));
    }
}

// ── restore-plan / restore-apply: what is refused before any camera ──

#[test]
fn restore_plan_refuses_unsafe_and_missing_sources_before_the_sdk() {
    let (d, _) = data_dir_with_source();
    for (name, tag) in [("../source.json", "unsafe_name"), ("/etc/passwd", "unsafe_name"), ("a/b.json", "unsafe_name"), ("", "unsafe_name"), ("nope.json", "not_found")] {
        let mut w = spawn(&g(d.path()), &["restore-plan", "--serial", "0000003023668", "--source", name], &[]);
        let r = w.read();
        assert_eq!(err_tag(&r), tag, "{name:?}");
        assert_eq!(r.progress, ["starting"], "{name:?}: stopped before the SDK");
        let (code, stdout, _) = w.finish();
        assert_eq!((code, stdout.as_str()), (Some(1), ""), "{name:?}");
    }
}

#[test]
fn restore_plan_refuses_a_bad_only_name_before_the_sdk() {
    let (d, src) = data_dir_with_source();
    for bad in ["Nope", "copyright", " Copyright", ""] {
        let mut w = spawn(&g(d.path()), &["restore-plan", "--serial", "0000003023668", "--source", &src, "--only", bad], &[]);
        let r = w.read();
        assert_eq!(err_tag(&r), "no_such_property", "{bad:?}");
        assert_eq!(r.progress, ["starting"], "{bad:?}");
    }
    // One bad name among good ones fails the whole selection.
    let mut w = spawn(
        &g(d.path()),
        &["restore-plan", "--serial", "0000003023668", "--source", &src, "--only", "Copyright", "--only", "Nope"],
        &[],
    );
    assert_eq!(err_tag(&w.read()), "no_such_property");
}

#[test]
fn a_valid_restore_request_gets_as_far_as_the_missing_bundle() {
    let (d, src) = data_dir_with_source();
    for only in [vec![], vec!["--only", "Copyright"], vec!["--only", "kNkMAIDCapability_WBAutoType", "--only", "Copyright"]] {
        let mut args = vec!["restore-plan", "--serial", "0000003023668", "--source", src.as_str()];
        args.extend(only.iter().copied());
        let mut w = spawn(&g(d.path()), &args, &[]);
        assert_eq!(err_tag(&w.read()), "sdk_bundle_missing", "{only:?}: every earlier check passed");
    }
}

#[test]
fn restore_apply_demands_the_digest_and_the_summary_before_anything_else() {
    let (d, src) = data_dir_with_source();
    for (digest, summary) in [("", "s"), ("  ", "s"), ("d", ""), ("d", "   ")] {
        let mut w = spawn(
            &g(d.path()),
            &["restore-apply", "--serial", "0000003023668", "--source", &src, "--digest", digest, "--summary", summary],
            &[],
        );
        let r = w.read();
        assert_eq!(err_tag(&r), "missing_confirmation", "{digest:?}/{summary:?}");
        assert_eq!(r.progress, ["starting"]);
    }
    let mut w = spawn(
        &g(d.path()),
        &["restore-apply", "--serial", "0000003023668", "--source", &src, "--digest", "d", "--summary", "s"],
        &[],
    );
    assert_eq!(err_tag(&w.read()), "sdk_bundle_missing");
}

#[test]
fn restore_apply_refuses_an_unsafe_source_or_bad_only_even_with_a_valid_confirmation() {
    let (d, src) = data_dir_with_source();
    let mut w = spawn(
        &g(d.path()),
        &["restore-apply", "--serial", "0000003023668", "--source", "../x", "--digest", "d", "--summary", "s"],
        &[],
    );
    assert_eq!(err_tag(&w.read()), "unsafe_name");
    let mut w = spawn(
        &g(d.path()),
        &["restore-apply", "--serial", "0000003023668", "--source", &src, "--only", "Nope", "--digest", "d", "--summary", "s"],
        &[],
    );
    assert_eq!(err_tag(&w.read()), "no_such_property");
}

#[test]
fn restore_ops_refuse_a_serial_that_cannot_name_a_body_before_the_sdk() {
    let (d, src) = data_dir_with_source();
    for op in [
        vec!["restore-plan", "--serial", "id-3", "--source", src.as_str()],
        vec!["restore-apply", "--serial", "id-3", "--source", src.as_str(), "--digest", "d", "--summary", "s"],
    ] {
        let mut w = spawn(&g(d.path()), &op, &[]);
        assert_eq!(err_tag(&w.read()), "no_usb_serial", "{op:?}");
    }
}

// ── list-cameras ──

#[test]
fn list_cameras_needs_no_sdk_bundle_and_no_lock_and_keeps_stdout_empty() {
    let d = tempfile::tempdir().unwrap();
    let lock_dir = tempfile::tempdir().unwrap();
    let lock_path = lock_dir.path().join("camera.lock");
    let _held = CameraLock::acquire_at(&lock_path).unwrap();
    let env = [("NIKON_FLEET_CAMERA_LOCK", lock_path.to_str().unwrap())];
    let mut w = spawn(&g(d.path()), &["list-cameras"], &env);
    let r = w.read();
    assert_eq!(r.progress, ["starting"], "no SDK phases: the SDK is never loaded");
    let ReadEnd::Result(Outcome::Ok { data, .. }) = &r.end else { panic!("expected ok, got {:?}", r.end) };
    assert!(data["cameras"].is_array() && data["unreadable"].is_array(), "{data}");
    assert!(data["complete"].is_boolean() && data.get("enumeration_error").is_some(), "{data}");
    for c in data["cameras"].as_array().unwrap() {
        for k in ["model", "serial", "firmware", "known_model", "serial_readable", "pairing_ambiguous"] {
            assert!(c.get(k).is_some(), "{k} missing from {c}");
        }
    }
    let (code, stdout, _) = w.finish();
    assert_eq!((code, stdout.as_str()), (Some(0), ""));
}

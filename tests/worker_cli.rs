//! `nikon-fleet worker` through the real binary: the protocol on an inherited
//! file descriptor, with nothing on stdout, a result that is readable before
//! the process exits, and typed errors for what can be checked without a camera.
#![cfg(unix)]

use std::fs::File;
use std::io::BufReader;
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};
use std::time::Duration;

use nikon_fleet::camera_lock::CameraLock;
use nikon_fleet::ops::worker::{Outcome, Read, ReadEnd, read_until_result};
use serde_json::json;

mod common;

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
    let mut cmd = common::cli(BIN);
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
fn exit_now_leaves_with_the_result_delivered_and_beats_a_hang_that_follows_it() {
    // The read-only teardown (F24): result out, then `_exit` — the hang flag after it never gets a chance.
    let mut w = spawn(&[], &["selftest", "--exit-now", "--hang-after-result"], &[]);
    let r = w.read();
    assert_eq!(r.end, ReadEnd::Result(Outcome::ok(json!({"selftest": true}))));
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = w.child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(0));
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "exit_now did not end the process");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn exit_now_keeps_the_exit_status_rules_and_an_error_result_is_status_one() {
    for (args, want) in [
        (vec!["selftest", "--exit-now"], Some(0)),
        (vec!["selftest", "--exit-now", "--fail"], Some(1)),
        (vec!["selftest", "--exit-now", "--exit-code", "7"], Some(7)),
    ] {
        let mut w = spawn(&[], &args, &[]);
        let _ = w.read();
        assert_eq!(w.finish().0, want, "{args:?}");
    }
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
        let out = common::cli(BIN)
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

/// The real MaidLayer.config when the SDK runtime is present, else a minimal valid one written into `dir`. These
/// tests only need a schema that parses (the lock is refused before anything reads a model section), so a missing
/// runtime must not turn them into tests that return early and pass having checked nothing.
fn schema_or_stand_in(dir: &std::path::Path) -> std::path::PathBuf {
    let real = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk-runtime/MaidLayer.config");
    if real.exists() {
        return real;
    }
    let stand_in = dir.join("MaidLayer.config");
    std::fs::write(&stand_in, "<ProfileVersion>9.0.3000</ProfileVersion>\n").unwrap();
    stand_in
}

#[test]
fn capture_reports_camera_busy_when_another_session_holds_the_lock_without_loading_the_sdk() {
    let dir = tempfile::tempdir().unwrap();
    let schema = schema_or_stand_in(dir.path());
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
    let mut cmd = common::cli(BIN);
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
    let (d, src) = data_dir_with_source();
    let lock_dir = tempfile::tempdir().unwrap();
    let schema = schema_or_stand_in(lock_dir.path());
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

// ── diagnostics ──────────────────────────────────────────────────────────

/// What the worker process left in `diag_dir`, read back with the real reader.
fn worker_trace(diag_dir: &std::path::Path, pid: u32) -> nikon_fleet::diag::ProcessTrace {
    let traces = nikon_fleet::diag::load(diag_dir, &nikon_fleet::diag::pid_alive).unwrap();
    traces.into_iter().find(|t| t.pid == pid).unwrap_or_else(|| panic!("no diagnostics for pid {pid}"))
}

fn field<'a>(e: &'a nikon_fleet::diag::Entry, key: &str) -> Option<&'a nikon_fleet::diag::Val> {
    e.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

#[test]
fn a_worker_records_its_phases_its_parent_and_the_request_id_and_a_clean_exit() {
    use nikon_fleet::diag::{Outcome as O, Val, Verdict};
    let dir = tempfile::tempdir().unwrap();
    let diag = dir.path().join("diag");
    let env = [("NIKON_FLEET_DIAG_DIR", diag.to_str().unwrap()), ("NIKON_FLEET_REQUEST_ID", "req-test-1")];
    let mut w = spawn(&[], &["selftest"], &env);
    let _ = w.read();
    let pid = w.child.id();
    assert_eq!(w.finish().0, Some(0));

    let t = worker_trace(&diag, pid);
    assert_eq!(t.verdict(), Verdict::ExitedCleanly { code: 0 });
    let start = t.entries.iter().find(|e| e.op == "start").expect("a start event");
    assert_eq!(field(start, "request_id"), Some(&Val::text("req-test-1")), "the id the parent gave it");
    assert_eq!(field(start, "ppid"), Some(&Val::U64(u64::from(std::process::id()))), "and who the parent was");
    assert_eq!(field(start, "role"), Some(&Val::text("cli")));
    let phases: Vec<&Val> = t.entries.iter().filter(|e| e.op == "phase").filter_map(|e| field(e, "phase")).collect();
    assert_eq!(phases, vec![&Val::text("starting"), &Val::text("working")], "each progress line, in order");
    let command = t.entries.iter().find(|e| e.op == "command").unwrap();
    assert_eq!((command.outcome, field(command, "name")), (Some(O::Ok), Some(&Val::text("worker selftest"))));
}

#[test]
fn a_failing_worker_records_a_failed_command_and_its_exit_code() {
    use nikon_fleet::diag::{Outcome as O, Val, Verdict};
    let dir = tempfile::tempdir().unwrap();
    let diag = dir.path().join("diag");
    let mut w = spawn(&[], &["selftest", "--fail"], &[("NIKON_FLEET_DIAG_DIR", diag.to_str().unwrap())]);
    let _ = w.read();
    let pid = w.child.id();
    assert_eq!(w.finish().0, Some(1));
    let t = worker_trace(&diag, pid);
    assert_eq!(t.verdict(), Verdict::ExitedCleanly { code: 1 });
    let command = t.entries.iter().find(|e| e.op == "command").unwrap();
    assert_eq!(command.outcome, Some(O::Failed));
    let start = t.entries.iter().find(|e| e.op == "start").unwrap();
    assert!(field(start, "request_id").is_none(), "no parent gave one");
    let _ = Val::U64(0);
}

#[test]
fn a_worker_that_hangs_after_its_result_is_a_running_process_with_its_command_open() {
    use nikon_fleet::diag::Verdict;
    let dir = tempfile::tempdir().unwrap();
    let diag = dir.path().join("diag");
    // The result is out, then the worker sleeps forever: the teardown hang the supervisor has to kill.
    let mut w = spawn(&[], &["selftest", "--hang-after-result"], &[("NIKON_FLEET_DIAG_DIR", diag.to_str().unwrap())]);
    let _ = w.read();
    let pid = w.child.id();
    let t = worker_trace(&diag, pid);
    assert_eq!(t.verdict(), Verdict::RunningWithSpansOpen, "alive, with its command span still open");
    // Killed from outside, as the supervisor does: it never reached its exit.
    w.child.kill().unwrap();
    let _ = w.child.wait();
    let t = worker_trace(&diag, pid);
    assert_eq!(t.verdict(), Verdict::GoneWithSpansOpen);
    assert!(t.entries.iter().filter(|e| e.op == "phase").count() == 2, "the phases before the hang are in the ring");
}

#[test]
fn a_refused_camera_lock_is_recorded_as_an_event_in_the_refused_process() {
    use nikon_fleet::diag::Val;
    let dir = tempfile::tempdir().unwrap();
    let schema = schema_or_stand_in(dir.path());
    let lock_path = dir.path().join("camera.lock");
    let diag = dir.path().join("diag");
    let _held = CameraLock::acquire_at(&lock_path).unwrap();
    let bundle = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let mut w = spawn(
        &["--sdk-bundle", bundle.to_str().unwrap(), "--schema", schema.to_str().unwrap()],
        &["capture", "--serial", "0000003023668"],
        &[("NIKON_FLEET_CAMERA_LOCK", lock_path.to_str().unwrap()), ("NIKON_FLEET_DIAG_DIR", diag.to_str().unwrap())],
    );
    let _ = w.read();
    let pid = w.child.id();
    assert_eq!(w.finish().0, Some(1));
    let t = worker_trace(&diag, pid);
    let refused = t.entries.iter().find(|e| e.op == "lock_refused").expect("a lock_refused event");
    assert_eq!(field(refused, "reason"), Some(&Val::text("camera_busy")));
    assert!(t.entries.iter().all(|e| e.op != "camera_lock_held"), "it never held the lock");
}

// ── the teardown watchdog ────────────────────────────────────────────────

/// Wait for the child to exit on its own, up to `within`; the time it took, and its status code.
fn wait_for_exit(w: &mut Worker, within: Duration) -> (Duration, Option<i32>) {
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = w.child.try_wait().unwrap() {
            return (start.elapsed(), status.code());
        }
        if start.elapsed() > within {
            w.child.kill().unwrap();
            let _ = w.child.wait();
            panic!("the worker did not exit by itself within {within:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stderr_of(w: &mut Worker) -> String {
    use std::io::Read as _;
    let mut e = String::new();
    w.child.stderr.take().unwrap().read_to_string(&mut e).unwrap();
    e
}

#[test]
fn a_worker_that_hangs_after_its_result_is_ended_by_the_watchdog_with_the_status_it_was_going_to_have() {
    use nikon_fleet::diag::{Val, Verdict};
    let dir = tempfile::tempdir().unwrap();
    let diag = dir.path().join("diag");
    let env = [("NIKON_FLEET_DIAG_DIR", diag.to_str().unwrap())];
    // Result out, watchdog armed at 500 ms, then a hang that would otherwise last for ever.
    let mut w = spawn(&[], &["selftest", "--hang-after-result", "--teardown-limit-ms", "500"], &env);
    let r = w.read();
    assert_eq!(r.end, ReadEnd::Result(Outcome::ok(json!({"selftest": true}))), "the result is intact and arrived first");
    let pid = w.child.id();
    let (took, code) = wait_for_exit(&mut w, Duration::from_secs(20));
    assert_eq!(code, Some(0), "the status the result had: ok");
    assert!(took >= Duration::from_millis(400), "it must not cut the teardown short before its limit: {took:?}");
    assert!(took < Duration::from_secs(10), "{took:?}");
    let err = stderr_of(&mut w);
    assert!(err.contains("did not close within") && err.contains("exit status 0"), "{err}");

    let t = worker_trace(&diag, pid);
    // The hung main thread still had the command span open (the selftest stands in for a hang inside
    // DisconnectDevice, whose own span is open in the same way). The watchdog ends the process from its own thread:
    // that span must read `abandoned`, and the exit must not read as a clean one (finding #02).
    assert_eq!(t.verdict(), Verdict::ExitedWithWorkCutOff { code: 0, cut_off: 1 }, "an ended hang is an exit that cut work short");
    let command = t.entries.iter().find(|e| e.op == "command").expect("the command span");
    assert_eq!(command.outcome, Some(nikon_fleet::diag::Outcome::Abandoned), "a span still open when the watchdog ended the process is not ok");
    assert!(t.inflight.is_none(), "nothing is left in flight");
    let fired = t.entries.iter().find(|e| e.op == "teardown_watchdog").expect("a teardown_watchdog event");
    assert_eq!(field(fired, "limit_ms"), Some(&Val::U64(500)));
    assert_eq!(field(fired, "code"), Some(&Val::I64(0)));
}

#[test]
fn the_watchdog_still_ends_the_process_when_stderr_has_no_reader_left() {
    // eprintln! panics on a closed pipe (SIGPIPE is ignored in Rust): the watchdog thread died before `_exit` and the
    // process hung exactly as it did without a watchdog (finding #10).
    let dir = tempfile::tempdir().unwrap();
    let diag = dir.path().join("diag");
    let mut w = spawn(&[], &["selftest", "--hang-after-result", "--teardown-limit-ms", "500"], &[("NIKON_FLEET_DIAG_DIR", diag.to_str().unwrap())]);
    let _ = w.read();
    drop(w.child.stderr.take()); // the reader goes away before the watchdog speaks
    let pid = w.child.id();
    let (took, code) = wait_for_exit(&mut w, Duration::from_secs(20));
    assert_eq!(code, Some(0));
    assert!(took < Duration::from_secs(10), "{took:?}");
    assert_eq!(worker_trace(&diag, pid).verdict(), nikon_fleet::diag::Verdict::ExitedWithWorkCutOff { code: 0, cut_off: 1 });
}

#[test]
fn the_watchdog_keeps_a_failed_results_status_and_an_explicit_exit_code() {
    // A failed result (status 1) stays 1; an explicit exit code wins over the result's, as for exit_now.
    for (args, want) in [(vec!["selftest", "--fail", "--hang-after-result", "--teardown-limit-ms", "300"], 1), (vec!["selftest", "--exit-code", "7", "--hang-after-result", "--teardown-limit-ms", "300"], 7)] {
        let mut w = spawn(&[], &args, &[]);
        let _ = w.read();
        let (_, code) = wait_for_exit(&mut w, Duration::from_secs(20));
        assert_eq!(code, Some(want), "{args:?}");
    }
}

#[test]
fn an_armed_watchdog_does_not_delay_a_worker_that_exits_normally() {
    use nikon_fleet::diag::Verdict;
    let dir = tempfile::tempdir().unwrap();
    let diag = dir.path().join("diag");
    // A 60 s limit and a worker that simply finishes: it must be gone almost at once and record no watchdog.
    let mut w = spawn(&[], &["selftest", "--teardown-limit-ms", "60000"], &[("NIKON_FLEET_DIAG_DIR", diag.to_str().unwrap())]);
    let _ = w.read();
    let pid = w.child.id();
    let (took, code) = wait_for_exit(&mut w, Duration::from_secs(20));
    assert_eq!(code, Some(0));
    assert!(took < Duration::from_secs(10), "an armed watchdog must not hold a healthy process: {took:?}");
    let t = worker_trace(&diag, pid);
    assert_eq!(t.verdict(), Verdict::ExitedCleanly { code: 0 });
    assert!(t.entries.iter().all(|e| e.op != "teardown_watchdog"), "nothing fired");
}


//! End-to-end: the real `fleet-mcp` binary on stdio, with a shell script standing in for
//! the camera worker. No SDK, no camera. The client here is a scripted JSON-RPC peer;
//! every line the server writes to stdout must parse as JSON-RPC (stdout is the protocol channel).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_fleet-mcp");

/// Behaviour is chosen by the operation name (`$6`) and, for the applies, by whether the
/// arguments mention `early` (hang before `writing`) or `slow` (finish after 8 s: longer than the
/// few seconds rmcp lets in-flight handlers run after stdin closes).
const FAKE_WORKER: &str = r#"#!/bin/sh
# args: --data-dir D [...] worker --result-fd 3 OP ...
LOG="$2/args.log"
echo "$@" >> "$LOG"
for a in "$@"; do op=$a; done
# the op is the first word after "3"
op=""; seen=0
for a in "$@"; do
  if [ "$seen" = 1 ]; then op="$a"; break; fi
  if [ "$a" = "3" ]; then seen=1; fi
done
echo "start $op" >> "$2/order.log"
p() { echo "{\"type\":\"progress\",\"phase\":\"$1\"}" >&3; }
case "$op" in
  capture)
    p starting; sleep 0.3; p connected
    echo '{"type":"result","ok":true,"data":{"file":"fake.json","gate":{"outcome":"passed"}}}' >&3 ;;
  check) p starting; sleep 60 ;;
  restore-plan)
    echo '{"type":"result","ok":false,"error":{"tag":"camera_busy","message":"held by another process"}}' >&3 ;;
  transplant-plan)
    echo '{"type":"result","ok":true,"data":{"digest":"abc","summary":"s"}}' >&3 ;;
  transplant-apply)
    p plan_verified; p restore_point_written; p writing
    case "$*" in *slow*) echo yes > "$2/writing-emitted"; sleep 8; echo done > "$2/apply-finished" ;; esac
    echo '{"type":"result","ok":true,"data":{"status":"applied_verified"}}' >&3 ;;
  restore-apply)
    case "$*" in
      *early*) p restore_point_written; sleep 0.5; echo yes > "$2/early-emitted"; sleep 60 ;;
      *) p writing; sleep 0.5; echo yes > "$2/writing-emitted"; sleep 60 ;;
    esac ;;
esac
echo "end $op" >> "$2/order.log"
"#;

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Result<Value, String>>,
    next_id: u64,
    pub dir: Arc<TempDir>,
    pub notifications: Vec<Value>,
    /// Responses that arrived while waiting for a different request.
    pending: Vec<Value>,
    /// The `initialize` result.
    init: Value,
}

fn write_worker(dir: &Path) -> PathBuf {
    let p = dir.join("worker.sh");
    std::fs::write(&p, FAKE_WORKER).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

impl Server {
    fn start(extra: &[&str]) -> Server {
        Server::start_in(Arc::new(TempDir::new().unwrap()), extra)
    }

    /// A server whose data directory is `dir`: a second session over the first one's leftovers.
    /// Its stderr is `stderr.log` in `dir` (unless MCP_TEST_STDERR is set).
    fn start_in(dir: Arc<TempDir>, extra: &[&str]) -> Server {
        let worker = write_worker(dir.path());
        let stderr = if std::env::var_os("MCP_TEST_STDERR").is_some() {
            Stdio::inherit()
        } else {
            let n = (0..).find(|n| !dir.path().join(format!("stderr-{n}.log")).exists()).unwrap();
            Stdio::from(std::fs::File::create(dir.path().join(format!("stderr-{n}.log"))).unwrap())
        };
        let mut child = Command::new(BIN)
            .arg("--data-dir")
            .arg(dir.path())
            .arg("--worker")
            .arg(&worker)
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let item = match line {
                    Ok(l) => serde_json::from_str::<Value>(&l).map_err(|e| format!("stdout line is not JSON ({e}): {l:?}")),
                    Err(e) => Err(e.to_string()),
                };
                if tx.send(item).is_err() {
                    break;
                }
            }
        });
        let mut s = Server { child, stdin, rx, next_id: 1, dir, notifications: Vec::new(), pending: Vec::new(), init: Value::Null };
        let init = s.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );
        assert!(init["result"]["serverInfo"].is_object(), "{init}");
        s.init = init["result"].clone();
        s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        s
    }

    fn send(&mut self, v: &Value) {
        let w = self.stdin.as_mut().unwrap();
        writeln!(w, "{v}").unwrap();
        w.flush().unwrap();
    }

    fn send_request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn wait_for(&mut self, id: u64, timeout: Duration) -> Value {
        if let Some(i) = self.pending.iter().position(|v| v["id"] == json!(id)) {
            return self.pending.remove(i);
        }
        let end = Instant::now() + timeout;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(Ok(v)) => {
                    let is_response = v.get("result").is_some() || v.get("error").is_some();
                    if is_response && v["id"] == json!(id) {
                        return v;
                    }
                    if is_response {
                        self.pending.push(v);
                    } else {
                        self.notifications.push(v);
                    }
                }
                Ok(Err(e)) => panic!("{e}"),
                Err(RecvTimeoutError::Timeout) => panic!("no response to request {id} in {timeout:?}"),
                Err(RecvTimeoutError::Disconnected) => panic!("server closed stdout waiting for request {id}"),
            }
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send_request(method, params);
        self.wait_for(id, Duration::from_secs(30))
    }

    fn call(&mut self, tool: &str, args: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": args}))
    }

    fn tool_names(&mut self) -> Vec<String> {
        let r = self.request("tools/list", json!({}));
        let mut v: Vec<String> =
            r["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        v.sort();
        v
    }

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
    }

    /// Everything this data directory's servers have written to stderr, oldest session first.
    fn stderr(&self) -> String {
        (0..)
            .map_while(|n| std::fs::read_to_string(self.dir.path().join(format!("stderr-{n}.log"))).ok())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Wait for the process to exit on its own.
    fn wait_exit(&mut self, within: Duration) -> std::process::ExitStatus {
        let end = Instant::now() + within;
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s;
            }
            assert!(Instant::now() < end, "the server did not exit within {within:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_file(&self, name: &str) {
        let path = self.dir.path().join(name);
        let end = Instant::now() + Duration::from_secs(20);
        while !path.exists() {
            assert!(Instant::now() < end, "the worker never wrote {name}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn shutdown(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s;
            }
            assert!(Instant::now() < end, "the server did not exit after stdin closed");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn structured(r: &Value) -> &Value {
    &r["result"]["structuredContent"]
}

fn is_error(r: &Value) -> bool {
    r["result"]["isError"] == json!(true)
}

fn seed_snapshot(dir: &Path, file: &str, model: &str, serial: &str, label: &str, props: &[(&str, u32, Value)]) {
    use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
    let snaps = dir.join("snapshots");
    std::fs::create_dir_all(&snaps).unwrap();
    let mut s = Snapshot::new(
        Camera { model: model.into(), serial: serial.into(), firmware: "5.31".into() },
        Transport::Usb,
        "2026-10-27T10:00:00Z".into(),
    );
    s.label = Some(label.into());
    for (n, c, v) in props {
        s.insert(*n, *c, v.clone());
    }
    s.save_to_file(snaps.join(file)).unwrap();
}

const READ_ONLY_TOOLS: [&str; 11] = [
    "capture_snapshot",
    "check_against_reference",
    "diff_snapshots",
    "firmware_status",
    "get_snapshot",
    "get_usage_guidance",
    "list_cameras",
    "list_references",
    "list_snapshots",
    "restore_plan",
    "transplant_plan",
];

// ── tool registration ────────────────────────────────────────────────────

#[test]
fn without_enable_writes_the_apply_tools_are_absent_and_a_direct_call_is_refused() {
    let mut s = Server::start(&[]);
    assert_eq!(s.tool_names(), READ_ONLY_TOOLS);
    for tool in ["transplant_apply", "restore_apply"] {
        let r = s.call(tool, json!({"source": "a.json", "serial": "S", "plan_digest": "d", "summary": "s"}));
        assert_eq!(r["error"]["code"], json!(-32602), "{tool}: {r}");
        assert!(r["result"].is_null());
    }
    assert!(s.log("args.log").is_empty(), "no worker was started for a refused call");
}

#[test]
fn with_enable_writes_both_apply_tools_are_registered_and_marked_destructive() {
    let mut s = Server::start(&["--enable-writes"]);
    let names = s.tool_names();
    assert_eq!(names.len(), 13);
    assert!(names.contains(&"transplant_apply".to_string()) && names.contains(&"restore_apply".to_string()));
    let list = s.request("tools/list", json!({}));
    for t in list["result"]["tools"].as_array().unwrap() {
        let name = t["name"].as_str().unwrap();
        let destructive = t["annotations"]["destructiveHint"] == json!(true);
        assert_eq!(destructive, name.ends_with("_apply"), "{name}: {}", t["annotations"]);
        if READ_ONLY_TOOLS.contains(&name) && name != "capture_snapshot" {
            assert_eq!(t["annotations"]["readOnlyHint"], json!(true), "{name}");
        }
    }
}

#[test]
fn an_empty_only_list_is_refused_by_both_restore_tools_before_any_worker_runs() {
    // `only: []` would reach the worker as no `--only` at all, which restores every property.
    let mut s = Server::start(&["--enable-writes"]);
    let plan = s.call("restore_plan", json!({"serial": "0000003023668", "source": "snap.json", "only": []}));
    assert!(is_error(&plan), "{plan}");
    assert_eq!(structured(&plan)["error"]["tag"], json!("empty_selection"));
    let apply = s.call(
        "restore_apply",
        json!({"serial": "0000003023668", "source": "snap.json", "only": [], "plan_digest": "d", "summary": "s"}),
    );
    assert!(is_error(&apply), "{apply}");
    assert_eq!(structured(&apply)["error"]["tag"], json!("empty_selection"));
    assert!(s.log("args.log").is_empty(), "no worker was started: {}", s.log("args.log"));
}

// ── offline tools ────────────────────────────────────────────────────────

#[test]
fn list_cameras_answers_without_a_worker_or_an_sdk() {
    let mut s = Server::start(&[]);
    let r = s.call("list_cameras", json!({}));
    assert!(!is_error(&r), "{r}");
    let d = structured(&r);
    assert!(d["cameras"].is_array() && d["unreadable"].is_array() && d["complete"].is_boolean(), "{d}");
    assert!(s.log("args.log").is_empty(), "list_cameras must not start a worker");
}

#[test]
fn snapshots_are_listed_read_and_diffed_by_name() {
    let mut s = Server::start(&[]);
    let d = s.dir.path().to_path_buf();
    seed_snapshot(&d, "a.json", "Z 9", "S1", "base", &[("kNkMAIDCapability_Artist", 1, json!("x")), ("kNkMAIDCapability_ISO", 2, json!(100))]);
    seed_snapshot(&d, "b.json", "Z 9", "S1", "mcp-restore-pre-restore-1", &[("kNkMAIDCapability_Artist", 1, json!("y")), ("kNkMAIDCapability_ISO", 2, json!(100))]);

    let r = s.call("list_snapshots", json!({}));
    assert_eq!(structured(&r)["entries"].as_array().unwrap().len(), 2);
    let r = s.call("list_snapshots", json!({"label_prefix": "mcp-restore-"}));
    let e = structured(&r)["entries"].as_array().unwrap().clone();
    assert_eq!((e.len(), e[0]["file"].as_str()), (1, Some("b.json")));

    let r = s.call("get_snapshot", json!({"name": "a.json"}));
    let v = structured(&r);
    assert_eq!((v["property_count"].clone(), v["page"]["returned"].clone(), v["page"]["truncated"].clone()), (json!(2), json!(0), json!(true)));
    let r = s.call("get_snapshot", json!({"name": "a.json", "limit": 1, "name_contains": "ISO"}));
    assert_eq!(structured(&r)["properties"][0]["value"], json!(100));
    assert_eq!(structured(&r)["page"]["truncated"], json!(false));

    // `diff_snapshots` names properties from the SDK's `sdk-runtime/MaidLayer.config`, which is
    // gitignored (not redistributable): without it the server answers `schema_unreadable`, which
    // is the server working, not a diff to assert on. Where the file must exist (a machine set up
    // with scripts/setup-sdk-runtime.sh, or REQUIRE_SDK_RUNTIME / CI set) its absence fails; on a
    // bare runner the diff half is skipped, loudly, and the list/get half above still ran.
    let schema = Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk-runtime/MaidLayer.config");
    if !schema.exists() {
        assert!(
            !sdk_runtime_required(|n| std::env::var_os(n)),
            "REQUIRE_SDK_RUNTIME or CI is set but {schema:?} does not exist (run scripts/setup-sdk-runtime.sh)"
        );
        eprintln!("skipping the diff_snapshots half: {schema:?} does not exist");
        return;
    }
    let r = s.call("diff_snapshots", json!({"a": "a.json", "b": "b.json"}));
    assert!(!is_error(&r), "{r}");
    let diff = &structured(&r)["diff"];
    assert_eq!(diff["counts"]["changed"], json!(1));
    assert_eq!(diff["entries"][0]["name"], json!("kNkMAIDCapability_Artist"));
}

/// Whether this run must have the gitignored `sdk-runtime/`: the repo's own switch
/// (`REQUIRE_SDK_RUNTIME`, as in `src/ops/baseline.rs` and `transplant.rs`) or a CI run. Set to
/// anything but empty, `0` or `false`.
fn sdk_runtime_required(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    ["REQUIRE_SDK_RUNTIME", "CI"].iter().any(|name| var(name).is_some_and(|v| !v.is_empty() && v != "0" && v != "false"))
}

#[test]
fn the_sdk_runtime_requirement_is_decided_by_both_switches() {
    fn env<'a>(set: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
        move |name| set.iter().find(|(k, _)| *k == name).map(|(_, v)| std::ffi::OsString::from(*v))
    }
    assert!(!sdk_runtime_required(env(&[])));
    assert!(sdk_runtime_required(env(&[("REQUIRE_SDK_RUNTIME", "1")])));
    assert!(sdk_runtime_required(env(&[("CI", "true")])), "CI alone requires it");
    for off in ["", "0", "false"] {
        assert!(!sdk_runtime_required(env(&[("CI", off), ("REQUIRE_SDK_RUNTIME", off)])), "{off:?}");
    }
    assert!(sdk_runtime_required(env(&[("CI", "0"), ("REQUIRE_SDK_RUNTIME", "1")])));
}

#[test]
fn names_are_names_not_paths() {
    let mut s = Server::start(&[]);
    std::fs::create_dir_all(s.dir.path().join("snapshots")).unwrap();
    for name in ["../a.json", "/etc/passwd", "sub/a.json", "", ".."] {
        let r = s.call("get_snapshot", json!({"name": name}));
        assert!(is_error(&r), "{name:?}: {r}");
        assert_eq!(structured(&r)["error"]["tag"], json!("unsafe_name"), "{name:?}");
    }
    let r = s.call("get_snapshot", json!({"name": "missing.json"}));
    assert_eq!(structured(&r)["error"]["tag"], json!("not_found"));
    std::fs::remove_dir(s.dir.path().join("snapshots")).unwrap();
    let r = s.call("get_snapshot", json!({"name": "missing.json"}));
    assert_eq!(structured(&r)["error"]["tag"], json!("no_directory"), "no data directory is not the same as no such file");
    let r = s.call("diff_snapshots", json!({"a": "missing.json", "b": "../x"}));
    assert!(is_error(&r));
}

// ── camera tools through the worker ──────────────────────────────────────

#[test]
fn a_worker_result_becomes_the_structured_content_and_arguments_reach_the_worker() {
    let mut s = Server::start(&[]);
    let r = s.call("capture_snapshot", json!({"serial": "0000003023668", "label": "before"}));
    assert!(!is_error(&r), "{r}");
    assert_eq!(structured(&r)["file"], json!("fake.json"));
    assert_eq!(structured(&r)["gate"]["outcome"], json!("passed"));
    let log = s.log("args.log");
    assert!(log.contains("worker --result-fd 3 capture --serial 0000003023668 --label before"), "{log}");
    assert!(log.contains(&format!("--data-dir {}", s.dir.path().display())), "{log}");
}

#[test]
fn a_worker_error_is_a_typed_error_result() {
    let mut s = Server::start(&[]);
    let r = s.call("restore_plan", json!({"source": "a.json", "serial": "S", "only": ["WBTuneAuto", "ISO"]}));
    assert!(is_error(&r));
    assert_eq!(structured(&r)["error"]["tag"], json!("camera_busy"));
    assert!(s.log("args.log").contains("restore-plan --serial S --source a.json --only WBTuneAuto --only ISO"));
}

#[test]
fn progress_notifications_arrive_when_the_client_supplied_a_token() {
    let mut s = Server::start(&[]);
    let r = s.request(
        "tools/call",
        json!({"name": "capture_snapshot", "arguments": {"serial": "S"}, "_meta": {"progressToken": "tok-1"}}),
    );
    assert!(!is_error(&r), "{r}");
    let phases: Vec<String> = s
        .notifications
        .iter()
        .filter(|n| n["method"] == json!("notifications/progress") && n["params"]["progressToken"] == json!("tok-1"))
        .map(|n| n["params"]["message"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(phases, ["starting", "connected"], "{:?}", s.notifications);
}

#[test]
fn a_read_that_misses_its_deadline_is_a_typed_no_result_and_the_worker_is_gone() {
    let mut s = Server::start(&["--deadline-secs", "2"]);
    let t = Instant::now();
    let r = s.call("check_against_reference", json!({"serial": "S"}));
    assert!(t.elapsed() < Duration::from_secs(20));
    assert!(is_error(&r));
    let e = &structured(&r)["error"];
    assert_eq!(e["tag"], json!("worker_no_result"));
    assert_eq!(e["detail"]["cause"], json!("deadline"));
    assert_eq!(e["detail"]["writes_possible"], json!(false));
    assert!(structured(&r).get("status").is_none(), "a read has no apply status");
}

#[test]
fn an_apply_that_goes_quiet_after_writing_began_is_indeterminate() {
    let mut s = Server::start(&["--enable-writes", "--deadline-secs", "3"]);
    let r = s.call("restore_apply", json!({"source": "a.json", "serial": "S", "plan_digest": "d", "summary": "s"}));
    assert!(is_error(&r));
    assert_eq!(structured(&r)["status"], json!("indeterminate"));
    assert_eq!(structured(&r)["error"]["detail"]["last_phase"], json!("writing"));
    assert_eq!(structured(&r)["error"]["detail"]["writes_possible"], json!(true));
}

#[test]
fn an_apply_that_goes_quiet_before_writing_is_not_applied_not_indeterminate() {
    let mut s = Server::start(&["--enable-writes", "--deadline-secs", "3"]);
    let r = s.call("restore_apply", json!({"source": "early.json", "serial": "S", "plan_digest": "d", "summary": "s"}));
    assert_eq!(structured(&r)["status"], json!("not_applied"), "{r}");
    assert_eq!(structured(&r)["error"]["detail"]["last_phase"], json!("restore_point_written"));
}

#[test]
fn apply_arguments_reach_the_worker_with_the_digest_and_summary_unchanged() {
    let mut s = Server::start(&["--enable-writes"]);
    let r = s.call(
        "transplant_apply",
        json!({"source": "a.json", "serial": "S", "allow_raw_code": ["WBAutoType"], "plan_digest": "D1", "summary": "Z 9 S: 3 properties"}),
    );
    assert_eq!(structured(&r)["status"], json!("applied_verified"), "{r}");
    assert!(s.log("args.log").contains("transplant-apply --serial S --source a.json --allow-raw-code WBAutoType --digest D1 --summary Z 9 S: 3 properties"));
}

#[test]
fn camera_calls_are_served_one_at_a_time() {
    let mut s = Server::start(&[]);
    let ids: Vec<u64> = (0..3).map(|_| s.send_request("tools/call", json!({"name": "capture_snapshot", "arguments": {"serial": "S"}}))).collect();
    for id in ids {
        let r = s.wait_for(id, Duration::from_secs(60));
        assert!(!is_error(&r), "{r}");
    }
    let order: Vec<String> = s.log("order.log").lines().map(String::from).collect();
    assert_eq!(order, ["start capture", "end capture", "start capture", "end capture", "start capture", "end capture"]);
}

// ── the client going away ────────────────────────────────────────────────

#[test]
fn a_client_that_disconnects_mid_apply_does_not_abandon_the_worker() {
    let mut s = Server::start(&["--enable-writes"]);
    s.send_request(
        "tools/call",
        json!({"name": "transplant_apply", "arguments": {"source": "slow.json", "serial": "S", "plan_digest": "d", "summary": "s"}}),
    );
    // Hang up only once the worker has reported `writing`: before that, cancellation is
    // (correctly) allowed to kill it.
    let marker = s.dir.path().join("writing-emitted");
    let end = Instant::now() + Duration::from_secs(20);
    while !marker.exists() {
        assert!(Instant::now() < end, "the worker never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut s = s;
    drop(s.stdin.take());
    let end = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(st) = s.child.try_wait().unwrap() {
            break st;
        }
        assert!(Instant::now() < end, "the server did not exit");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status:?}");
    assert!(s.dir.path().join("apply-finished").exists(), "the server exited before the apply worker finished");
}

// ── an exit with an apply in flight ──────────────────────────────────────

const APPLY: fn() -> Value = || json!({"source": "a.json", "serial": "S1", "plan_digest": "d", "summary": "s"});

fn marker_path(s: &Server) -> PathBuf {
    s.dir.path().join("in-flight-apply.json")
}

#[test]
fn an_apply_still_writing_when_the_server_leaves_is_recorded_and_the_next_session_reports_it_once() {
    let mut s = Server::start(&["--enable-writes", "--exit-wait-secs", "1"]);
    s.send_request("tools/call", json!({"name": "restore_apply", "arguments": APPLY()}));
    s.wait_for_file("writing-emitted");
    drop(s.stdin.take());
    assert!(s.wait_exit(Duration::from_secs(30)).success());

    let marker: Value = serde_json::from_str(&std::fs::read_to_string(marker_path(&s)).expect("a marker")).unwrap();
    assert_eq!(marker["reason"], json!("client_gone_wait_expired"));
    let f = &marker["in_flight"];
    assert_eq!((f["op"].clone(), f["serial"].clone()), (json!("restore-apply"), json!("S1")));
    assert_eq!((f["last_phase"].clone(), f["writing_seen"].clone(), f["may_write"].clone()), (json!("writing"), json!(true), json!(true)));
    let log = s.stderr();
    assert!(log.contains("a camera call was still running after 1 s"), "{log}");
    assert!(log.contains("restore-apply") && log.contains("S1") && log.contains("writing seen: true"), "{log}");
    assert!(log.contains("may have been left half-done"), "{log}");

    // The next session over the same data directory: the first result carries the note, once.
    let mut next = Server::start_in(s.dir.clone(), &[]);
    let r = next.call("list_snapshots", json!({}));
    let note = &structured(&r)["previous_session_incomplete"];
    assert_eq!(note["reason"], json!("client_gone_wait_expired"), "{r}");
    assert_eq!(note["marker_unreadable"], json!(false));
    assert_eq!((note["in_flight"]["op"].clone(), note["in_flight"]["serial"].clone()), (json!("restore-apply"), json!("S1")));
    assert_eq!(note["in_flight"]["last_phase"], json!("writing"));
    assert!(structured(&r)["entries"].is_array(), "the tool's own result is still there");
    assert!(text_of(&r).contains("previous_session_incomplete"), "the text a client shows carries it too: {r}");
    assert!(!marker_path(&s).exists() && s.dir.path().join("in-flight-apply.seen.json").exists());
    let again = next.call("list_snapshots", json!({}));
    assert!(structured(&again).get("previous_session_incomplete").is_none(), "reported once: {again}");
    // A third session finds nothing to report.
    drop(next);
    let mut third = Server::start_in(s.dir.clone(), &[]);
    assert!(structured(&third.call("list_snapshots", json!({}))).get("previous_session_incomplete").is_none());
}

#[test]
fn an_apply_that_had_not_begun_writing_is_logged_but_leaves_no_marker() {
    let mut s = Server::start(&["--enable-writes", "--exit-wait-secs", "1"]);
    let mut args = APPLY();
    args["source"] = json!("early.json");
    s.send_request("tools/call", json!({"name": "restore_apply", "arguments": args}));
    s.wait_for_file("early-emitted");
    drop(s.stdin.take());
    assert!(s.wait_exit(Duration::from_secs(30)).success());
    let log = s.stderr();
    assert!(log.contains("restore-apply") && log.contains("writing seen: false") && log.contains("restore_point_written"), "{log}");
    assert!(!marker_path(&s).exists(), "nothing was written to the camera: no marker");
}

#[test]
fn a_termination_signal_mid_write_records_the_call_and_exits_without_waiting() {
    let mut s = Server::start(&["--enable-writes"]);
    s.send_request("tools/call", json!({"name": "restore_apply", "arguments": APPLY()}));
    s.wait_for_file("writing-emitted");
    let t = Instant::now();
    assert_eq!(unsafe { libc::kill(s.child.id() as i32, libc::SIGTERM) }, 0);
    let status = s.wait_exit(Duration::from_secs(30));
    assert!(t.elapsed() < Duration::from_secs(20), "a signal does not wait for the apply: {:?}", t.elapsed());
    assert_eq!(status.code(), Some(128 + libc::SIGTERM), "{status:?}");
    let marker: Value = serde_json::from_str(&std::fs::read_to_string(marker_path(&s)).expect("a marker")).unwrap();
    assert_eq!(marker["reason"], json!("sigterm"));
    assert_eq!(marker["in_flight"]["writing_seen"], json!(true));
    assert!(s.stderr().contains("exiting"), "{}", s.stderr());
}

#[test]
fn a_termination_signal_with_nothing_in_flight_exits_quietly_with_no_marker() {
    for (sig, reason) in [(libc::SIGTERM, "SIGTERM"), (libc::SIGINT, "SIGINT"), (libc::SIGHUP, "SIGHUP")] {
        let mut s = Server::start(&[]);
        assert_eq!(unsafe { libc::kill(s.child.id() as i32, sig) }, 0);
        let status = s.wait_exit(Duration::from_secs(30));
        assert_eq!(status.code(), Some(128 + sig), "{reason}: {status:?}");
        assert!(!marker_path(&s).exists(), "{reason}");
    }
}

#[test]
fn a_client_that_leaves_after_the_apply_finished_leaves_no_marker() {
    // The existing disconnect case: the apply completes within the wait, so nothing is lost.
    let mut s = Server::start(&["--enable-writes"]);
    s.send_request(
        "tools/call",
        json!({"name": "transplant_apply", "arguments": {"source": "slow.json", "serial": "S", "plan_digest": "d", "summary": "s"}}),
    );
    s.wait_for_file("writing-emitted");
    drop(s.stdin.take());
    assert!(s.wait_exit(Duration::from_secs(60)).success());
    assert!(s.dir.path().join("apply-finished").exists());
    assert!(!marker_path(&s).exists());
}

// ── what the server tells every client about itself ─────────────────────

#[test]
fn the_handshake_names_the_server_and_carries_the_instructions() {
    let s = Server::start(&[]);
    assert_eq!(s.init["serverInfo"]["name"], json!("fleet-mcp"));
    let ins = s.init["instructions"].as_str().expect("instructions in the initialize result");
    assert!(ins.contains("Camera writes are DISABLED"), "{ins}");
    assert!(ins.contains("Key things to know:") && ins.contains("get_usage_guidance(operation=\"strategy\")"), "{ins}");
    assert!(ins.len() < 4500, "{} bytes", ins.len());
    let w = Server::start(&["--enable-writes"]);
    let ins = w.init["instructions"].as_str().unwrap();
    assert!(ins.contains("Camera writes are ENABLED") && !ins.contains("DISABLED"), "{ins}");
}

#[test]
fn input_schemas_are_plain_json_schema() {
    // Some tool-calling layers reject `format` keywords, `["x","null"]` type unions and combinators.
    fn walk(v: &Value, path: &str, bad: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    if ["format", "anyOf", "oneOf", "allOf", "$ref", "$defs"].contains(&k.as_str()) {
                        bad.push(format!("{path}/{k}"));
                    }
                    if k == "type" && x.is_array() {
                        bad.push(format!("{path}/type is a union"));
                    }
                    walk(x, &format!("{path}/{k}"), bad);
                }
            }
            Value::Array(a) => a.iter().enumerate().for_each(|(i, x)| walk(x, &format!("{path}/{i}"), bad)),
            _ => {}
        }
    }
    let mut s = Server::start(&["--enable-writes"]);
    let list = s.request("tools/list", json!({}));
    let mut bad = Vec::new();
    for t in list["result"]["tools"].as_array().unwrap() {
        walk(&t["inputSchema"], t["name"].as_str().unwrap(), &mut bad);
    }
    assert!(bad.is_empty(), "{bad:?}");
    let req = |name: &str| -> Vec<String> {
        let t = list["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == json!(name)).unwrap();
        let mut r: Vec<String> = t["inputSchema"]["required"].as_array().map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect()).unwrap_or_default();
        r.sort();
        r
    };
    assert_eq!(req("get_snapshot"), ["name"]);
    assert_eq!(req("capture_snapshot"), ["serial"]);
    assert_eq!(req("diff_snapshots"), ["a", "b"]);
    assert_eq!(req("restore_apply"), ["plan_digest", "serial", "source", "summary"]);
    assert_eq!(req("transplant_apply"), ["plan_digest", "serial", "source", "summary"]);
    assert_eq!(req("get_usage_guidance"), ["operation"]);
    assert_eq!(req("list_snapshots"), Vec::<String>::new(), "every argument of list_snapshots is optional");
    let gs = list["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == json!("get_snapshot")).unwrap();
    assert_eq!(gs["inputSchema"]["properties"]["limit"]["type"], json!("integer"));
    assert_eq!(gs["inputSchema"]["properties"]["limit"]["minimum"], json!(0));
}

fn text_of(r: &Value) -> String {
    r["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string()
}

#[test]
fn the_guidance_tool_serves_the_notes_and_refuses_what_it_does_not_know() {
    let mut s = Server::start(&[]);
    let r = s.call("get_usage_guidance", json!({"operation": "strategy"}));
    assert!(!is_error(&r), "{r}");
    let overview = text_of(&r);
    assert!(overview.contains("Also available:"), "with writes off the apply strategy is a one-liner here: {overview}");
    let r = s.call("get_usage_guidance", json!({"operation": "strategy", "topic": "apply"}));
    assert!(text_of(&r).contains("Never compose either one yourself"), "{r}");
    let r = s.call("get_usage_guidance", json!({"operation": "tactics"}));
    assert!(text_of(&r).starts_with("Available tactical topics:"), "{r}");
    let r = s.call("get_usage_guidance", json!({"operation": "find", "problem": "capture had 11 failed reads"}));
    assert!(text_of(&r).contains("Eleven failed reads") || text_of(&r).contains("11 read") || text_of(&r).contains("196"), "{r}");
    let r = s.call("get_usage_guidance", json!({"operation": "find"}));
    assert!(is_error(&r) && structured(&r)["error"]["tag"] == json!("missing_problem"), "{r}");
    let r = s.call("get_usage_guidance", json!({"operation": "find", "problem": "   "}));
    assert_eq!(structured(&r)["error"]["tag"], json!("missing_problem"));
    let r = s.call("get_usage_guidance", json!({"operation": "Strategy"}));
    assert_eq!(structured(&r)["error"]["tag"], json!("unknown_operation"), "no case folding");
    assert!(s.log("args.log").is_empty(), "guidance never starts a worker");
}

#[test]
fn with_writes_the_apply_strategy_is_in_the_overview() {
    let mut s = Server::start(&["--enable-writes"]);
    let r = s.call("get_usage_guidance", json!({"operation": "strategy"}));
    assert!(text_of(&r).contains("Never compose either one yourself"), "{r}");
}

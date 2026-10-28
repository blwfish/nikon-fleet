//! End-to-end: the real `fleet-mcp` binary on stdio, with a shell script standing in for
//! the camera worker. No SDK, no camera. The client here is a scripted JSON-RPC peer;
//! every line the server writes to stdout must parse as JSON-RPC (stdout is the protocol channel).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
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
      *early*) p restore_point_written; sleep 60 ;;
      *) p writing; sleep 60 ;;
    esac ;;
esac
echo "end $op" >> "$2/order.log"
"#;

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Result<Value, String>>,
    next_id: u64,
    pub dir: TempDir,
    pub notifications: Vec<Value>,
    /// Responses that arrived while waiting for a different request.
    pending: Vec<Value>,
}

fn write_worker(dir: &Path) -> PathBuf {
    let p = dir.join("worker.sh");
    std::fs::write(&p, FAKE_WORKER).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

impl Server {
    fn start(extra: &[&str]) -> Server {
        let dir = TempDir::new().unwrap();
        let worker = write_worker(dir.path());
        let mut child = Command::new(BIN)
            .arg("--data-dir")
            .arg(dir.path())
            .arg("--worker")
            .arg(&worker)
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if std::env::var_os("MCP_TEST_STDERR").is_some() { Stdio::inherit() } else { Stdio::null() })
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
        let mut s = Server { child, stdin, rx, next_id: 1, dir, notifications: Vec::new(), pending: Vec::new() };
        let init = s.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );
        assert!(init["result"]["serverInfo"].is_object(), "{init}");
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

const READ_ONLY_TOOLS: [&str; 10] = [
    "capture_snapshot",
    "check_against_reference",
    "diff_snapshots",
    "firmware_status",
    "get_snapshot",
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
    assert_eq!(names.len(), 12);
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

    let r = s.call("diff_snapshots", json!({"a": "a.json", "b": "b.json"}));
    assert!(!is_error(&r), "{r}");
    let diff = &structured(&r)["diff"];
    assert_eq!(diff["counts"]["changed"], json!(1));
    assert_eq!(diff["entries"][0]["name"], json!("kNkMAIDCapability_Artist"));
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

//! Runs `nikon-fleet worker` children for the MCP server: one per call, under a
//! deadline, killable, with calls queued behind each other (docs/mcp-plan.md §3).
//!
//! What the supervisor guarantees, and what it leaves to the caller:
//! - the worker's result channel is a dedicated pipe installed as fd 3 in the
//!   child only (both ends are close-on-exec in this process, so no other child
//!   inherits a copy and delays end-of-input); the child's stdout is discarded
//!   and its stderr is this process's stderr, which is the log;
//! - the answer is the `result` line, never the exit status; once it is read it
//!   is final, the worker gets [`Supervisor::grace`] to exit and is then killed
//!   and reaped, so a teardown hang costs nothing;
//! - on a deadline, or a cancellation the policy allows, the worker is sent
//!   SIGKILL and reaped, and the pipe is drained afterwards: a `result` or a
//!   progress line already written is honoured, because killing does not unwrite it;
//! - dropping the future kills the child (`kill_on_drop`); a caller that must
//!   not abandon a half-applied camera runs the future detached instead;
//! - calls are served one at a time in arrival order, so `camera_busy` from a
//!   worker means a holder outside this server.

use std::ffi::OsString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nikon_fleet::ops::worker::{Line, Outcome};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::unix::pipe;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// The descriptor number the worker is told to write to.
const CHANNEL_FD: i32 = 3;

/// The phase after which camera writes may have happened. The worker emits it
/// (flushed) before its first write, so its absence proves none did.
pub const PHASE_WRITING: &str = "writing";
/// Emitted after the writes, before the read-back.
pub const PHASE_READ_BACK: &str = "read_back";

/// Whether any camera write could have happened by the time the worker last
/// reported `last_phase`.
pub fn writes_possible(last_phase: Option<&str>) -> bool {
    matches!(last_phase, Some(PHASE_WRITING) | Some(PHASE_READ_BACK))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelPolicy {
    /// A cancellation kills the worker (read-only and plan calls).
    Always,
    /// A cancellation kills the worker only before it reports `writing`; after
    /// that it is ignored, because killing mid-write leaves a half-applied camera.
    UntilWriting,
}

#[derive(Debug, Clone)]
pub struct Spec {
    /// Arguments after `worker --result-fd 3`: the op and its flags.
    pub op_args: Vec<String>,
    pub deadline: Duration,
    pub cancel: CancelPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoResultCause {
    /// Killed at the deadline.
    Deadline,
    /// Killed on the caller's cancellation.
    Cancelled,
    /// End of input with no result: the worker died.
    WorkerDied,
    /// Reading the channel failed.
    ChannelError,
}

impl NoResultCause {
    pub fn tag(self) -> &'static str {
        match self {
            NoResultCause::Deadline => "deadline",
            NoResultCause::Cancelled => "cancelled",
            NoResultCause::WorkerDied => "worker_died",
            NoResultCause::ChannelError => "channel_error",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Finished {
    /// The worker answered.
    Result { outcome: Outcome, last_phase: Option<String>, garbage_lines: usize },
    /// The worker was started and gave no answer.
    NoResult { cause: NoResultCause, last_phase: Option<String>, garbage_lines: usize },
    /// Cancelled while waiting for earlier calls: no worker was started.
    CancelledBeforeStart,
}

#[derive(Debug)]
pub enum SpawnError {
    Pipe(std::io::Error),
    Spawn { program: PathBuf, source: std::io::Error },
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::Pipe(e) => write!(f, "could not create the result channel: {e}"),
            SpawnError::Spawn { program, source } => write!(f, "could not start {}: {source}", program.display()),
        }
    }
}

impl std::error::Error for SpawnError {}

pub struct Supervisor {
    program: PathBuf,
    /// Global flags placed before `worker` (`--data-dir`, `--sdk-bundle`, ...).
    global_args: Vec<OsString>,
    queue: Mutex<()>,
    /// How long a worker that has delivered its result may take to exit.
    pub grace: Duration,
    /// How long to keep reading after a kill (the pipe ends as soon as the child is gone).
    drain: Duration,
}

fn pipe_pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are owned by nobody else.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&r, &w] {
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok((r, w))
}

impl Supervisor {
    pub fn new(program: PathBuf, global_args: Vec<OsString>) -> Self {
        Supervisor {
            program,
            global_args,
            queue: Mutex::new(()),
            grace: Duration::from_secs(3),
            drain: Duration::from_secs(2),
        }
    }

    fn spawn(&self, op_args: &[String]) -> Result<(Child, pipe::Receiver), SpawnError> {
        let (r, w) = pipe_pair().map_err(SpawnError::Pipe)?;
        let wfd = w.as_raw_fd();
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.global_args)
            .arg("worker")
            .arg("--result-fd")
            .arg(CHANNEL_FD.to_string())
            .args(op_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        // SAFETY: only async-signal-safe calls (dup2, fcntl) run between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if wfd == CHANNEL_FD {
                    // Already at the right number: just let it survive exec.
                    if libc::fcntl(wfd, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if libc::dup2(wfd, CHANNEL_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().map_err(|source| SpawnError::Spawn { program: self.program.clone(), source })?;
        drop(w); // only the child holds the write end now
        let rx = pipe::Receiver::from_owned_fd(r).map_err(SpawnError::Pipe)?;
        Ok((child, rx))
    }

    /// Run one worker call. `on_progress` is told each phase as it arrives.
    pub async fn run(
        &self,
        spec: &Spec,
        ct: &CancellationToken,
        on_progress: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Finished, SpawnError> {
        let _turn = tokio::select! {
            g = self.queue.lock() => g,
            _ = ct.cancelled() => return Ok(Finished::CancelledBeforeStart),
        };
        let (mut child, rx) = self.spawn(&spec.op_args)?;
        let mut lines = BufReader::new(rx).lines();
        let deadline = tokio::time::sleep(spec.deadline);
        tokio::pin!(deadline);

        let mut last_phase: Option<String> = None;
        let mut garbage = 0usize;
        let mut writing_seen = false;
        let mut cancel_ignored = false;
        let mut stop: Option<NoResultCause> = None;
        let mut answer: Option<Outcome> = None;

        while answer.is_none() && stop.is_none() {
            // Biased: a line already in the pipe is read before a deadline or a cancellation is
            // acted on, so a `writing` or `result` line the worker has flushed is never missed.
            tokio::select! {
                biased;
                line = lines.next_line() => match line {
                    Ok(Some(l)) => absorb(&l, &mut last_phase, &mut garbage, &mut writing_seen, &mut answer, on_progress),
                    Ok(None) => stop = Some(NoResultCause::WorkerDied),
                    Err(e) => {
                        eprintln!("fleet-mcp: reading the worker's result channel: {e}");
                        stop = Some(NoResultCause::ChannelError);
                    }
                },
                _ = &mut deadline => stop = Some(NoResultCause::Deadline),
                _ = ct.cancelled(), if !cancel_ignored => {
                    // Whatever the worker has already flushed counts before the decision: `writing`
                    // is emitted before its first write, so if it is not in the pipe, no write began.
                    drain_ready(&mut lines, &mut last_phase, &mut garbage, &mut writing_seen, &mut answer, on_progress).await;
                    if answer.is_some() {
                        // The worker finished first: that is the outcome.
                    } else if spec.cancel == CancelPolicy::UntilWriting && writing_seen {
                        cancel_ignored = true;
                    } else {
                        stop = Some(NoResultCause::Cancelled);
                    }
                }
            }
        }

        if let Some(outcome) = answer {
            // Final. Give the worker a moment to leave, then end it whatever state it is in.
            if tokio::time::timeout(self.grace, child.wait()).await.is_err() {
                kill_and_reap(&mut child).await;
            }
            return Ok(Finished::Result { outcome, last_phase, garbage_lines: garbage });
        }

        let cause = stop.unwrap_or(NoResultCause::WorkerDied);
        if cause != NoResultCause::WorkerDied {
            kill_and_reap(&mut child).await;
        } else {
            let _ = tokio::time::timeout(self.grace, child.wait()).await;
            kill_and_reap(&mut child).await;
        }
        // Whatever the worker managed to write before it died is still in the pipe.
        let drained = tokio::time::timeout(self.drain, async {
            while let Ok(Some(l)) = lines.next_line().await {
                absorb(&l, &mut last_phase, &mut garbage, &mut writing_seen, &mut answer, on_progress);
                if answer.is_some() {
                    break;
                }
            }
        })
        .await;
        if drained.is_err() {
            eprintln!("fleet-mcp: the worker's channel did not end after it was killed");
        }
        Ok(match answer {
            Some(outcome) => Finished::Result { outcome, last_phase, garbage_lines: garbage },
            None => Finished::NoResult { cause, last_phase, garbage_lines: garbage },
        })
    }
}

/// Absorb the lines that are available right now, without waiting for more.
async fn drain_ready<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut tokio::io::Lines<R>,
    last_phase: &mut Option<String>,
    garbage: &mut usize,
    writing_seen: &mut bool,
    answer: &mut Option<Outcome>,
    on_progress: &mut (dyn FnMut(&str) + Send),
) {
    while answer.is_none() {
        match tokio::time::timeout(Duration::from_millis(20), lines.next_line()).await {
            Ok(Ok(Some(l))) => absorb(&l, last_phase, garbage, writing_seen, answer, on_progress),
            _ => break,
        }
    }
}

fn absorb(
    line: &str,
    last_phase: &mut Option<String>,
    garbage: &mut usize,
    writing_seen: &mut bool,
    answer: &mut Option<Outcome>,
    on_progress: &mut (dyn FnMut(&str) + Send),
) {
    if line.trim().is_empty() {
        return;
    }
    match serde_json::from_str::<Line>(line) {
        Ok(Line::Progress { phase }) => {
            if phase == PHASE_WRITING || phase == PHASE_READ_BACK {
                *writing_seen = true;
            }
            on_progress(&phase);
            *last_phase = Some(phase);
        }
        Ok(Line::Result(o)) => *answer = Some(o),
        Err(_) => *garbage += 1,
    }
}

async fn kill_and_reap(child: &mut Child) {
    if let Err(e) = child.kill().await {
        // Already gone is fine; anything else is worth knowing.
        if e.kind() != std::io::ErrorKind::InvalidInput {
            eprintln!("fleet-mcp: killing the worker: {e}");
        }
    }
}

/// The data of a worker `ok` result, for callers that only care about that.
pub fn ok_data(o: &Outcome) -> Option<&Value> {
    match o {
        Outcome::Ok { data, .. } => Some(data),
        Outcome::Err { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;
    use tempfile::TempDir;

    /// A fake worker: a shell script that gets the real argument vector and writes
    /// protocol lines to fd 3.
    fn fake(dir: &TempDir, name: &str, body: &str) -> Supervisor {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut s = Supervisor::new(path, vec![]);
        s.grace = Duration::from_millis(300);
        s.drain = Duration::from_millis(500);
        s
    }

    fn spec(deadline_ms: u64, cancel: CancelPolicy) -> Spec {
        Spec { op_args: vec!["selftest".into()], deadline: Duration::from_millis(deadline_ms), cancel }
    }

    async fn run(s: &Supervisor, spec: &Spec, ct: &CancellationToken) -> (Finished, Vec<String>) {
        let mut phases = Vec::new();
        let f = s.run(spec, ct, &mut |p| phases.push(p.to_string())).await.unwrap();
        (f, phases)
    }

    /// Run with the cancellation fired from inside the progress callback the moment
    /// `phase` is seen, so the test does not depend on how fast the worker starts.
    async fn run_cancelling_at(s: &Supervisor, spec: &Spec, phase: &str) -> Finished {
        let ct = CancellationToken::new();
        let c2 = ct.clone();
        let phase = phase.to_string();
        s.run(spec, &ct, &mut move |p| {
            if p == phase {
                c2.cancel();
            }
        })
        .await
        .unwrap()
    }

    const OK_LINE: &str = r#"{"type":"result","ok":true,"data":{"n":1}}"#;

    #[tokio::test]
    async fn an_ok_result_with_its_progress_is_returned() {
        let d = TempDir::new().unwrap();
        let s = fake(&d, "w", &format!("echo '{{\"type\":\"progress\",\"phase\":\"a\"}}' >&3\necho '{OK_LINE}' >&3"));
        let (f, phases) = run(&s, &spec(5000, CancelPolicy::Always), &CancellationToken::new()).await;
        assert_eq!(phases, ["a"]);
        match f {
            Finished::Result { outcome, last_phase, garbage_lines } => {
                assert_eq!(ok_data(&outcome), Some(&serde_json::json!({"n": 1})));
                assert_eq!((last_phase.as_deref(), garbage_lines), (Some("a"), 0));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn the_worker_gets_the_documented_argument_vector() {
        let d = TempDir::new().unwrap();
        let out = d.path().join("args.txt");
        let body = format!("echo \"$@\" > {}\necho '{OK_LINE}' >&3", out.display());
        let mut s = fake(&d, "w", &body);
        s.global_args = vec!["--data-dir".into(), "/x".into()];
        let mut sp = spec(5000, CancelPolicy::Always);
        sp.op_args = vec!["capture".into(), "--serial".into(), "S".into()];
        run(&s, &sp, &CancellationToken::new()).await;
        assert_eq!(std::fs::read_to_string(out).unwrap().trim(), "--data-dir /x worker --result-fd 3 capture --serial S");
    }

    #[tokio::test]
    async fn an_error_result_is_returned_as_is() {
        let d = TempDir::new().unwrap();
        let s = fake(&d, "w", r#"echo '{"type":"result","ok":false,"error":{"tag":"camera_busy","message":"m"}}' >&3"#);
        let (f, _) = run(&s, &spec(5000, CancelPolicy::Always), &CancellationToken::new()).await;
        let Finished::Result { outcome: Outcome::Err { error, .. }, .. } = f else { panic!("{f:?}") };
        assert_eq!(error.tag, "camera_busy");
    }

    #[tokio::test]
    async fn exit_status_is_not_the_answer() {
        let d = TempDir::new().unwrap();
        let s = fake(&d, "w", &format!("echo '{OK_LINE}' >&3\nexit 7"));
        let (f, _) = run(&s, &spec(5000, CancelPolicy::Always), &CancellationToken::new()).await;
        assert!(matches!(f, Finished::Result { outcome: Outcome::Ok { .. }, .. }), "{f:?}");
    }

    #[tokio::test]
    async fn a_worker_that_hangs_after_its_result_costs_only_the_grace() {
        let d = TempDir::new().unwrap();
        let s = fake(&d, "w", &format!("echo '{OK_LINE}' >&3\nsleep 60"));
        let t = Instant::now();
        let (f, _) = run(&s, &spec(30_000, CancelPolicy::Always), &CancellationToken::new()).await;
        assert!(matches!(f, Finished::Result { .. }));
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    }

    /// Whether `pid` still exists (a killed but unreaped child still does).
    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    async fn pid_from(path: &std::path::Path) -> i32 {
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(path) {
                if let Ok(n) = s.trim().parse() {
                    return n;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the worker never wrote its pid");
    }

    #[tokio::test]
    async fn the_worker_is_gone_and_reaped_when_run_returns_even_after_a_hang_past_its_result() {
        // The worker holds the camera lock; the next call must not find it still there.
        let d = TempDir::new().unwrap();
        let pidfile = d.path().join("pid");
        let s = fake(&d, "w", &format!("echo $$ > {}\necho '{OK_LINE}' >&3\nsleep 60", pidfile.display()));
        run(&s, &spec(30_000, CancelPolicy::Always), &CancellationToken::new()).await;
        let pid = pid_from(&pidfile).await;
        assert!(!alive(pid), "pid {pid} still exists after run returned");
    }

    #[tokio::test]
    async fn the_worker_is_gone_and_reaped_after_a_deadline_kill() {
        let d = TempDir::new().unwrap();
        let pidfile = d.path().join("pid");
        let s = fake(&d, "w", &format!("echo $$ > {}\nsleep 60", pidfile.display()));
        run(&s, &spec(1500, CancelPolicy::Always), &CancellationToken::new()).await;
        let pid = pid_from(&pidfile).await;
        assert!(!alive(pid), "pid {pid} still exists after run returned");
    }

    #[tokio::test]
    async fn the_deadline_kills_a_silent_worker_and_names_the_last_phase() {
        let d = TempDir::new().unwrap();
        let s = fake(&d, "w", "echo '{\"type\":\"progress\",\"phase\":\"connected\"}' >&3\nsleep 60");
        let t = Instant::now();
        // Long enough for a loaded machine to start the shell and let it write its first line.
        let (f, phases) = run(&s, &spec(3000, CancelPolicy::Always), &CancellationToken::new()).await;
        assert_eq!(phases, ["connected"]);
        assert_eq!(
            f,
            Finished::NoResult { cause: NoResultCause::Deadline, last_phase: Some("connected".into()), garbage_lines: 0 }
        );
        assert!(t.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_worker_that_dies_without_a_result_is_worker_died() {
        let d = TempDir::new().unwrap();
        let s = fake(&d, "w", "echo '{\"type\":\"progress\",\"phase\":\"writing\"}' >&3\nexit 9");
        let (f, _) = run(&s, &spec(5000, CancelPolicy::Always), &CancellationToken::new()).await;
        assert_eq!(
            f,
            Finished::NoResult { cause: NoResultCause::WorkerDied, last_phase: Some("writing".into()), garbage_lines: 0 }
        );
    }

    #[tokio::test]
    async fn garbage_lines_are_counted_and_do_not_end_the_read() {
        let d = TempDir::new().unwrap();
        let s = fake(&d, "w", &format!("echo 'not json' >&3\necho '{{\"type\":\"nope\"}}' >&3\necho '{OK_LINE}' >&3"));
        let (f, _) = run(&s, &spec(5000, CancelPolicy::Always), &CancellationToken::new()).await;
        let Finished::Result { garbage_lines, .. } = f else { panic!("{f:?}") };
        assert_eq!(garbage_lines, 2);
    }

    #[tokio::test]
    async fn stdout_noise_from_the_worker_never_reaches_the_channel() {
        let d = TempDir::new().unwrap();
        let s = fake(&d, "w", &format!("echo 'InitializeSDK Execution duration: 180'\necho '{OK_LINE}' >&3"));
        let (f, _) = run(&s, &spec(5000, CancelPolicy::Always), &CancellationToken::new()).await;
        let Finished::Result { garbage_lines, .. } = f else { panic!("{f:?}") };
        assert_eq!(garbage_lines, 0);
    }

    #[tokio::test]
    async fn cancelling_a_read_kills_the_worker_promptly() {
        let d = TempDir::new().unwrap();
        let s = fake(&d, "w", "sleep 60");
        let ct = CancellationToken::new();
        let c2 = ct.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c2.cancel();
        });
        let t = Instant::now();
        let (f, _) = run(&s, &spec(30_000, CancelPolicy::Always), &ct).await;
        assert!(matches!(f, Finished::NoResult { cause: NoResultCause::Cancelled, .. }), "{f:?}");
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn an_apply_cancelled_before_writing_is_killed() {
        let d = TempDir::new().unwrap();
        let s = fake(&d, "w", "echo '{\"type\":\"progress\",\"phase\":\"restore_point_written\"}' >&3\nsleep 60");
        let f = run_cancelling_at(&s, &spec(30_000, CancelPolicy::UntilWriting), "restore_point_written").await;
        assert_eq!(
            f,
            Finished::NoResult {
                cause: NoResultCause::Cancelled,
                last_phase: Some("restore_point_written".into()),
                garbage_lines: 0
            }
        );
    }

    #[tokio::test]
    async fn an_apply_cancelled_after_writing_is_not_killed_and_delivers_its_result() {
        let d = TempDir::new().unwrap();
        let s = fake(
            &d,
            "w",
            &format!("echo '{{\"type\":\"progress\",\"phase\":\"writing\"}}' >&3\nsleep 1\necho '{OK_LINE}' >&3"),
        );
        let f = run_cancelling_at(&s, &spec(30_000, CancelPolicy::UntilWriting), "writing").await;
        assert!(matches!(f, Finished::Result { outcome: Outcome::Ok { .. }, .. }), "cancellation after `writing` is ignored: {f:?}");
    }

    #[tokio::test]
    async fn the_same_cancellation_at_read_back_is_ignored_too() {
        let d = TempDir::new().unwrap();
        let s = fake(
            &d,
            "w",
            &format!("echo '{{\"type\":\"progress\",\"phase\":\"read_back\"}}' >&3\nsleep 1\necho '{OK_LINE}' >&3"),
        );
        let f = run_cancelling_at(&s, &spec(30_000, CancelPolicy::UntilWriting), "read_back").await;
        assert!(matches!(f, Finished::Result { .. }), "{f:?}");
    }

    #[tokio::test]
    async fn a_read_cancelled_at_any_phase_is_killed() {
        let d = TempDir::new().unwrap();
        let s = fake(&d, "w", "echo '{\"type\":\"progress\",\"phase\":\"writing\"}' >&3\nsleep 60");
        let f = run_cancelling_at(&s, &spec(30_000, CancelPolicy::Always), "writing").await;
        assert!(matches!(f, Finished::NoResult { cause: NoResultCause::Cancelled, .. }), "{f:?}");
    }

    #[tokio::test]
    async fn the_deadline_still_ends_an_apply_that_is_writing() {
        let d = TempDir::new().unwrap();
        let s = fake(&d, "w", "echo '{\"type\":\"progress\",\"phase\":\"writing\"}' >&3\nsleep 60");
        let (f, _) = run(&s, &spec(3000, CancelPolicy::UntilWriting), &CancellationToken::new()).await;
        let Finished::NoResult { cause, last_phase, .. } = f else { panic!("{f:?}") };
        assert_eq!(cause, NoResultCause::Deadline);
        assert!(writes_possible(last_phase.as_deref()));
    }

    #[tokio::test]
    async fn a_result_written_just_before_the_kill_is_honoured() {
        // The worker answers, then hangs; a deadline shorter than the hang must not turn that into no_result.
        let d = TempDir::new().unwrap();
        let s = fake(&d, "w", &format!("echo '{OK_LINE}' >&3\nsleep 60"));
        let (f, _) = run(&s, &spec(10_000, CancelPolicy::Always), &CancellationToken::new()).await;
        assert!(matches!(f, Finished::Result { .. }));
    }

    #[tokio::test]
    async fn calls_are_served_one_at_a_time_in_arrival_order() {
        let d = TempDir::new().unwrap();
        let log = d.path().join("log");
        let body = format!("echo start >> {l}\nsleep 0.4\necho end >> {l}\necho '{OK_LINE}' >&3", l = log.display());
        let s = std::sync::Arc::new(fake(&d, "w", &body));
        let mut handles = Vec::new();
        for _ in 0..3 {
            let s = s.clone();
            handles.push(tokio::spawn(async move {
                run(&s, &spec(10_000, CancelPolicy::Always), &CancellationToken::new()).await.0
            }));
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        for h in handles {
            assert!(matches!(h.await.unwrap(), Finished::Result { .. }));
        }
        let lines: Vec<String> = std::fs::read_to_string(log).unwrap().lines().map(String::from).collect();
        assert_eq!(lines, ["start", "end", "start", "end", "start", "end"], "no two workers overlapped");
    }

    #[tokio::test]
    async fn a_call_cancelled_while_queued_never_starts_a_worker() {
        let d = TempDir::new().unwrap();
        let marker = d.path().join("ran");
        let body = format!("echo x >> {}\nsleep 1\necho '{OK_LINE}' >&3", marker.display());
        let s = std::sync::Arc::new(fake(&d, "w", &body));
        let first = {
            let s = s.clone();
            tokio::spawn(async move { run(&s, &spec(10_000, CancelPolicy::Always), &CancellationToken::new()).await })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        let ct = CancellationToken::new();
        let c2 = ct.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            c2.cancel();
        });
        let (f, _) = run(&s, &spec(10_000, CancelPolicy::Always), &ct).await;
        assert_eq!(f, Finished::CancelledBeforeStart);
        first.await.unwrap();
        assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 1, "only the first call ran a worker");
    }

    #[tokio::test]
    async fn a_missing_worker_binary_is_a_spawn_error_not_a_hang() {
        let s = Supervisor::new(PathBuf::from("/nonexistent/nikon-fleet"), vec![]);
        let err = s.run(&spec(1000, CancelPolicy::Always), &CancellationToken::new(), &mut |_| {}).await.unwrap_err();
        assert!(matches!(err, SpawnError::Spawn { .. }), "{err}");
    }

    #[test]
    fn writes_are_possible_only_from_the_writing_phase_on() {
        for (p, want) in [
            (None, false),
            (Some("starting"), false),
            (Some("sdk_ready"), false),
            (Some("connected"), false),
            (Some("plan_verified"), false),
            (Some("restore_point_written"), false),
            (Some("writing"), true),
            (Some("read_back"), true),
            (Some("Writing"), false),
        ] {
            assert_eq!(writes_possible(p), want, "{p:?}");
        }
    }
}

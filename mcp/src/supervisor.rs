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
//! - the kill is bounded too ([`Supervisor::reap`]): a worker stuck in uninterruptible I/O is
//!   reported and abandoned, so it cannot hold the queue (and the next call) forever;
//! - a line that is not UTF-8 is a garbage line, like any other line that is not protocol: it is
//!   counted and does not end the read (and so never kills a worker that is mid-apply);
//! - dropping the future kills the child (`kill_on_drop`); a caller that must
//!   not abandon a half-applied camera runs the future detached instead;
//! - calls are served one at a time in arrival order, so `camera_busy` from a
//!   worker means a holder outside this server;
//! - the call a worker is running is visible ([`Supervisor::in_flight`]): its operation, serial,
//!   last phase and whether `writing` was seen. A server that must exit while one runs records it
//!   ([`record_in_flight_at_exit`]), and the next server start reports it once
//!   ([`previous_session_note`], [`acknowledge_previous_session`]).

use std::ffi::OsString;
use std::future::Future;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use nikon_fleet::ops::paths::write_atomic;
use nikon_fleet::ops::worker::{Line, Outcome};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::net::unix::pipe;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// The descriptor number the worker is told to write to.
const CHANNEL_FD: i32 = 3;

/// The phase after which camera writes may have happened. The worker emits it
/// (flushed) before its first write, so its absence proves none did. Defined once, in the
/// worker's own module, so a rename cannot leave the two sides disagreeing.
pub use nikon_fleet::ops::worker::{PHASE_READ_BACK, PHASE_WRITING};

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

/// The call a worker is running right now, as far as the supervisor has seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InFlight {
    /// The worker operation (`restore-apply`, `capture`, ...): the first of the call's arguments.
    pub op: String,
    /// The call's `--serial`, if it has one.
    pub serial: Option<String>,
    /// The call can write to the camera (an apply), whatever phase it has reached.
    pub may_write: bool,
    /// The last progress phase the worker reported.
    pub last_phase: Option<String>,
    /// The worker reported `writing` (or `read_back`): camera writes may have happened.
    pub writing_seen: bool,
    /// RFC 3339, UTC.
    pub started_at: String,
}

impl InFlight {
    fn begin(spec: &Spec) -> Self {
        let serial = spec.op_args.iter().position(|a| a == "--serial").and_then(|i| spec.op_args.get(i + 1)).cloned();
        InFlight {
            op: spec.op_args.first().cloned().unwrap_or_default(),
            serial,
            may_write: spec.cancel == CancelPolicy::UntilWriting,
            last_phase: None,
            writing_seen: false,
            started_at: now_rfc3339(),
        }
    }

    /// A write may have been left half-done if this call is lost: an apply that reached `writing`.
    pub fn write_may_be_incomplete(&self) -> bool {
        self.may_write && self.writing_seen
    }
}

fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|e| format!("unavailable ({e})"))
}

fn lock<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Clears the in-flight slot when a call ends or its future is dropped.
struct InFlightGuard<'a>(&'a StdMutex<Option<InFlight>>);

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        *lock(self.0) = None;
    }
}

pub struct Supervisor {
    program: PathBuf,
    /// Global flags placed before `worker` (`--data-dir`, `--sdk-bundle`, ...).
    global_args: Vec<OsString>,
    queue: Mutex<()>,
    /// How long a worker that has delivered its result may take to exit.
    pub grace: Duration,
    /// How long to keep reading after a kill (the pipe ends as soon as the child is gone).
    drain: Duration,
    /// How long a killed worker may take to be reaped. SIGKILL cannot be caught, but a process
    /// in uninterruptible I/O (a wedged USB transfer) does not die until the I/O returns.
    pub reap: Duration,
    /// The call whose worker is running (calls are serial, so at most one).
    in_flight: StdMutex<Option<InFlight>>,
}

/// A pipe with both ends close-on-exec (read end, write end).
pub fn pipe_pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
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
            reap: Duration::from_secs(5),
            in_flight: StdMutex::new(None),
        }
    }

    /// The call whose worker is running now, with the phase it has reached. `None` between calls
    /// and while a call is still queued behind another.
    pub fn in_flight(&self) -> Option<InFlight> {
        lock(&self.in_flight).clone()
    }

    fn note_progress(&self, last_phase: &Option<String>, writing_seen: bool) {
        if let Some(f) = lock(&self.in_flight).as_mut() {
            f.last_phase = last_phase.clone();
            f.writing_seen = writing_seen;
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
        *lock(&self.in_flight) = Some(InFlight::begin(spec));
        let _in_flight = InFlightGuard(&self.in_flight);
        let mut lines = ChannelLines::new(BufReader::new(rx));
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
                    Ok(Some(l)) => {
                        absorb(&l, &mut last_phase, &mut garbage, &mut writing_seen, &mut answer, on_progress);
                        self.note_progress(&last_phase, writing_seen);
                    }
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
                    self.note_progress(&last_phase, writing_seen);
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
                self.kill_and_reap(&mut child).await;
            }
            return Ok(Finished::Result { outcome, last_phase, garbage_lines: garbage });
        }

        let cause = stop.unwrap_or(NoResultCause::WorkerDied);
        if cause != NoResultCause::WorkerDied {
            self.kill_and_reap(&mut child).await;
        } else {
            let _ = tokio::time::timeout(self.grace, child.wait()).await;
            self.kill_and_reap(&mut child).await;
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

    /// Kill the worker and wait for it, for at most [`Supervisor::reap`]. A failure is logged and
    /// the call goes on: the queue lock is released when `run` returns, and `kill_on_drop` kills
    /// the child again when it is dropped.
    async fn kill_and_reap(&self, child: &mut Child) {
        if let Err(e) = bounded_reap(child.kill(), self.reap).await {
            eprintln!("fleet-mcp: {e}");
        }
    }
}

/// Why a killed worker was not reaped.
#[derive(Debug)]
enum ReapError {
    /// Still not gone after the limit: stuck in a call the kernel will not interrupt.
    TimedOut(Duration),
    Kill(std::io::Error),
}

impl std::fmt::Display for ReapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReapError::TimedOut(limit) => write!(
                f,
                "the worker was killed but was still not reaped after {limit:?} (stuck in uninterruptible I/O?); \
                 abandoning it so the queue is not held"
            ),
            ReapError::Kill(e) => write!(f, "killing the worker: {e}"),
        }
    }
}

/// Run `reap` (the kill and wait of a worker) for at most `limit`. A worker that is already gone
/// (`InvalidInput`) is fine; any other error is returned.
async fn bounded_reap(reap: impl Future<Output = std::io::Result<()>>, limit: Duration) -> Result<(), ReapError> {
    match tokio::time::timeout(limit, reap).await {
        Err(_) => Err(ReapError::TimedOut(limit)),
        Ok(Err(e)) if e.kind() != std::io::ErrorKind::InvalidInput => Err(ReapError::Kill(e)),
        Ok(_) => Ok(()),
    }
}

/// Newline-delimited lines off the result channel as raw bytes. `tokio::io::Lines` fails on the
/// first byte that is not UTF-8, and the caller treated that failure as a dead channel and killed
/// the worker, even mid-apply; here a line is just bytes and [`absorb`] decides what it is.
///
/// Cancellation-safe like `Lines`: bytes read so far stay in `buf` until the line is complete.
struct ChannelLines<R> {
    reader: R,
    buf: Vec<u8>,
}

impl<R: AsyncBufRead + Unpin> ChannelLines<R> {
    fn new(reader: R) -> Self {
        ChannelLines { reader, buf: Vec::new() }
    }

    /// The next line without its terminator (`\n` or `\r\n`), `None` at end of input. A final
    /// line with no terminator is still returned.
    async fn next_line(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        self.reader.read_until(b'\n', &mut self.buf).await?;
        if self.buf.is_empty() {
            return Ok(None);
        }
        let mut line = std::mem::take(&mut self.buf);
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
        }
        Ok(Some(line))
    }
}

/// Absorb the lines that are available right now, without waiting for more.
async fn drain_ready<R: AsyncBufRead + Unpin>(
    lines: &mut ChannelLines<R>,
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
    line: &[u8],
    last_phase: &mut Option<String>,
    garbage: &mut usize,
    writing_seen: &mut bool,
    answer: &mut Option<Outcome>,
    on_progress: &mut (dyn FnMut(&str) + Send),
) {
    let Ok(line) = std::str::from_utf8(line) else {
        *garbage += 1;
        return;
    };
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

// ── a call lost to the server's own exit ─────────────────────────────────

/// Written into the data directory when the server exits while an apply that had reached
/// `writing` is still running: the camera may have been left half-written.
pub const MARKER_FILE: &str = "in-flight-apply.json";
/// What [`MARKER_FILE`] is renamed to once a session has been told about it.
pub const MARKER_SEEN_FILE: &str = "in-flight-apply.seen.json";

/// Why the server is exiting with a call still running. Closed vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// The client closed stdin and the call outlived the wait (`--exit-wait-secs`).
    ClientGoneWaitExpired,
    Sigterm,
    Sigint,
    Sighup,
}

/// What the exiting server knew about the lost call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncompleteMarker {
    pub reason: ExitReason,
    pub recorded_at: String,
    pub in_flight: InFlight,
}

/// Say on stderr which call is in flight as the server exits and, when it is an apply that had
/// reached `writing`, write [`MARKER_FILE`] (atomically; an earlier unreported marker is named on
/// stderr before it is replaced). Returns whether a marker was written. A call that was only
/// reading, or an apply that had not yet begun writing, changed nothing on the camera: stderr
/// only.
pub fn record_in_flight_at_exit(data_dir: &Path, reason: ExitReason, in_flight: Option<&InFlight>) -> std::io::Result<bool> {
    let Some(f) = in_flight else { return Ok(false) };
    eprintln!(
        "fleet-mcp: exiting ({reason:?}) with a camera call in flight: {} serial {} since {}, last phase {}, writing seen: {}",
        f.op,
        f.serial.as_deref().unwrap_or("(none)"),
        f.started_at,
        f.last_phase.as_deref().unwrap_or("(none)"),
        f.writing_seen,
    );
    if !f.write_may_be_incomplete() {
        return Ok(false);
    }
    let path = data_dir.join(MARKER_FILE);
    if let Ok(earlier) = std::fs::read_to_string(&path) {
        eprintln!("fleet-mcp: replacing an earlier marker that no session has reported yet: {}", earlier.trim());
    }
    let marker = IncompleteMarker { reason, recorded_at: now_rfc3339(), in_flight: f.clone() };
    let bytes = serde_json::to_vec_pretty(&marker).map_err(std::io::Error::other)?;
    write_atomic(&path, &bytes)?;
    eprintln!(
        "fleet-mcp: a write to the camera may have been left half-done; recorded in {}",
        path.display()
    );
    Ok(true)
}

/// The typed `previous_session_incomplete` note for a marker left by an earlier server, or `None`
/// when there is none. A marker that cannot be read or parsed is still reported (as
/// `marker_unreadable`): it exists because something went wrong. Does not touch the file; call
/// [`acknowledge_previous_session`] once the note has been delivered.
pub fn previous_session_note(data_dir: &Path) -> Option<Value> {
    let path = data_dir.join(MARKER_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(unreadable_marker(&path, &e.to_string())),
    };
    match serde_json::from_str::<IncompleteMarker>(&text) {
        Ok(m) => Some(json!({
            "reason": m.reason,
            "recorded_at": m.recorded_at,
            "in_flight": m.in_flight,
            "marker_unreadable": false,
            "message": "An earlier fleet-mcp session exited while an apply was running and had reached `writing`: \
                        the camera may have been left half-written. Capture it (capture_snapshot) or check it against its \
                        reference (check_against_reference) to see what state it is in, and tell the user. \
                        This note is shown once; the marker is kept as in-flight-apply.seen.json."
        })),
        Err(e) => Some(unreadable_marker(&path, &e.to_string())),
    }
}

fn unreadable_marker(path: &Path, error: &str) -> Value {
    json!({
        "marker_unreadable": true,
        "file": path.display().to_string(),
        "error": error,
        "message": "An earlier fleet-mcp session left a marker that this one cannot read, which means it exited with an apply in \
                    flight. Treat the camera as possibly half-written: capture it or check it against its reference, and tell the user."
    })
}

/// Mark the marker as reported: rename it to [`MARKER_SEEN_FILE`] (replacing an older acknowledged
/// one). It is kept, not deleted.
pub fn acknowledge_previous_session(data_dir: &Path) -> std::io::Result<()> {
    std::fs::rename(data_dir.join(MARKER_FILE), data_dir.join(MARKER_SEEN_FILE))
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
    use std::sync::Arc;
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
    async fn a_line_that_is_not_utf8_is_garbage_not_a_dead_channel() {
        // `tokio::io::Lines` failed on the first such byte, which ended the read, killed the worker
        // and reported `channel_error`, even for a worker that was mid-apply.
        let d = TempDir::new().unwrap();
        let progress = r#"{"type":"progress","phase":"a"}"#;
        let s = fake(
            &d,
            "w",
            &format!("printf '\\377\\376\\n' >&3\necho '{progress}' >&3\nprintf '{OK_LINE}\\200\\n' >&3\necho '{OK_LINE}' >&3"),
        );
        let (f, phases) = run(&s, &spec(5000, CancelPolicy::Always), &CancellationToken::new()).await;
        // The progress line after the bad one is still read; a result line with a stray byte in it
        // is not a result (it is not UTF-8), so only the clean one answers.
        assert_eq!(phases, ["a"]);
        let Finished::Result { outcome, last_phase, garbage_lines } = f else { panic!("{f:?}") };
        assert_eq!(ok_data(&outcome), Some(&serde_json::json!({"n": 1})));
        assert_eq!((last_phase.as_deref(), garbage_lines), (Some("a"), 2));
    }

    async fn lines_of(input: &[u8]) -> Vec<Vec<u8>> {
        let mut l = ChannelLines::new(BufReader::new(input));
        let mut out = Vec::new();
        while let Some(line) = l.next_line().await.unwrap() {
            out.push(line);
        }
        out
    }

    #[tokio::test]
    async fn channel_lines_split_on_newline_and_keep_every_byte_of_the_line() {
        assert_eq!(lines_of(b"").await, Vec::<Vec<u8>>::new(), "empty input: no lines, not one empty line");
        assert_eq!(lines_of(b"a\nb\n").await, [b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(lines_of(b"a\n\nb\n").await, [b"a".to_vec(), b"".to_vec(), b"b".to_vec()], "an empty line is a line");
        assert_eq!(lines_of(b"a\r\nb\n").await, [b"a".to_vec(), b"b".to_vec()], "CRLF is one terminator");
        assert_eq!(lines_of(b"a\nb").await, [b"a".to_vec(), b"b".to_vec()], "an unterminated last line is still delivered");
        assert_eq!(lines_of(b"\xff\xfe\nok\n").await, [vec![0xff, 0xfe], b"ok".to_vec()], "invalid UTF-8 passes through as bytes");
    }

    #[tokio::test]
    async fn a_partly_read_line_survives_a_cancelled_read() {
        // The run loop reads inside a `select!`, so a read is dropped whenever another branch wins.
        use tokio::io::AsyncWriteExt;
        let (mut tx, rx) = tokio::io::duplex(64);
        let mut l = ChannelLines::new(BufReader::new(rx));
        tx.write_all(b"hel").await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(50), l.next_line()).await.is_err(), "no newline yet");
        tx.write_all(b"lo\n").await.unwrap();
        assert_eq!(l.next_line().await.unwrap(), Some(b"hello".to_vec()), "the bytes read before the cancel are not lost");
        drop(tx);
        assert_eq!(l.next_line().await.unwrap(), None);
    }

    fn io_err(kind: std::io::ErrorKind) -> std::io::Result<()> {
        Err(std::io::Error::new(kind, "x"))
    }

    #[tokio::test]
    async fn a_reap_that_never_completes_is_abandoned_at_the_limit() {
        // A child that ignores SIGKILL (uninterruptible I/O) cannot be made in a test, so the
        // kill-and-wait is injected as a future that never finishes.
        let limit = Duration::from_millis(150);
        let t = Instant::now();
        let r = bounded_reap(std::future::pending::<std::io::Result<()>>(), limit).await;
        assert!(matches!(r, Err(ReapError::TimedOut(l)) if l == limit), "{r:?}");
        assert!(t.elapsed() >= limit, "not given up early: {:?}", t.elapsed());
        assert!(t.elapsed() < Duration::from_secs(5), "released: {:?}", t.elapsed());
        assert!(r.unwrap_err().to_string().contains("not reaped"));
    }

    #[tokio::test]
    async fn a_reap_just_inside_the_limit_succeeds_and_just_outside_does_not() {
        let slow = |ms: u64| async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(())
        };
        assert!(bounded_reap(slow(10), Duration::from_millis(1000)).await.is_ok(), "below the limit");
        assert!(matches!(bounded_reap(slow(1000), Duration::from_millis(50)).await, Err(ReapError::TimedOut(_))), "above the limit");
    }

    #[tokio::test]
    async fn an_already_gone_worker_is_not_an_error_but_a_failed_kill_is() {
        let limit = Duration::from_secs(1);
        assert!(bounded_reap(async { io_err(std::io::ErrorKind::InvalidInput) }, limit).await.is_ok(), "already gone");
        assert!(matches!(bounded_reap(async { io_err(std::io::ErrorKind::PermissionDenied) }, limit).await, Err(ReapError::Kill(_))));
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

    // ── the call in flight, and the marker a lost apply leaves ───────────

    fn apply_spec(deadline_ms: u64) -> Spec {
        Spec {
            op_args: ["restore-apply", "--serial", "S1", "--source", "x.json"].map(String::from).to_vec(),
            deadline: Duration::from_millis(deadline_ms),
            cancel: CancelPolicy::UntilWriting,
        }
    }

    /// A worker that emits each phase only once the test has created the file that gates it, so
    /// the test sees every intermediate state without racing the worker.
    fn gated_worker(d: &TempDir) -> Arc<Supervisor> {
        let dir = d.path().display();
        let wait = |n: u8| format!("while [ ! -f {dir}/g{n} ]; do sleep 0.02; done");
        let body = format!(
            "{}\necho '{{\"type\":\"progress\",\"phase\":\"starting\"}}' >&3\n{}\necho '{{\"type\":\"progress\",\"phase\":\"writing\"}}' >&3\n{}\necho '{OK_LINE}' >&3",
            wait(1),
            wait(2),
            wait(3)
        );
        Arc::new(fake(d, "w", &body))
    }

    async fn until(what: &str, mut ok: impl FnMut() -> bool) {
        for _ in 0..200 {
            if ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("never saw: {what}");
    }

    fn gate(d: &TempDir, n: u8) {
        std::fs::write(d.path().join(format!("g{n}")), "").unwrap();
    }

    #[tokio::test]
    async fn the_call_in_flight_is_visible_with_its_phase_and_gone_when_it_ends() {
        let d = TempDir::new().unwrap();
        let s = gated_worker(&d);
        assert_eq!(s.in_flight(), None, "nothing before a call");
        let (s2, spec) = (s.clone(), apply_spec(20_000));
        let call = tokio::spawn(async move { s2.run(&spec, &CancellationToken::new(), &mut |_| {}).await });

        until("the call is in flight", || s.in_flight().is_some()).await;
        let f = s.in_flight().unwrap();
        assert_eq!((f.op.as_str(), f.serial.as_deref(), f.may_write), ("restore-apply", Some("S1"), true));
        assert_eq!((f.last_phase, f.writing_seen), (None, false), "no phase yet");

        gate(&d, 1);
        until("starting", || s.in_flight().is_some_and(|f| f.last_phase.as_deref() == Some("starting"))).await;
        assert!(!s.in_flight().unwrap().writing_seen && !s.in_flight().unwrap().write_may_be_incomplete());

        gate(&d, 2);
        until("writing", || s.in_flight().is_some_and(|f| f.writing_seen)).await;
        let f = s.in_flight().unwrap();
        assert_eq!(f.last_phase.as_deref(), Some("writing"));
        assert!(f.write_may_be_incomplete());

        gate(&d, 3);
        assert!(matches!(call.await.unwrap().unwrap(), Finished::Result { .. }));
        assert_eq!(s.in_flight(), None, "cleared when the call ends");
    }

    #[tokio::test]
    async fn a_call_whose_future_is_dropped_is_no_longer_in_flight() {
        let d = TempDir::new().unwrap();
        let s = gated_worker(&d);
        let (s2, spec) = (s.clone(), apply_spec(20_000));
        let call = tokio::spawn(async move { s2.run(&spec, &CancellationToken::new(), &mut |_| {}).await });
        until("the call is in flight", || s.in_flight().is_some()).await;
        call.abort();
        let _ = call.await;
        assert_eq!(s.in_flight(), None);
    }

    #[tokio::test]
    async fn a_read_is_never_a_write_that_may_be_incomplete() {
        let d = TempDir::new().unwrap();
        let s = gated_worker(&d);
        let (s2, mut spec) = (s.clone(), apply_spec(20_000));
        spec.cancel = CancelPolicy::Always;
        let call = tokio::spawn(async move { s2.run(&spec, &CancellationToken::new(), &mut |_| {}).await });
        gate(&d, 1);
        gate(&d, 2);
        until("writing", || s.in_flight().is_some_and(|f| f.writing_seen)).await;
        let f = s.in_flight().unwrap();
        assert!(!f.may_write && f.writing_seen && !f.write_may_be_incomplete(), "{f:?}");
        gate(&d, 3);
        let _ = call.await;
    }

    #[test]
    fn the_serial_is_the_value_after_serial_and_absent_otherwise() {
        let begin = |args: &[&str]| {
            InFlight::begin(&Spec {
                op_args: args.iter().map(|a| a.to_string()).collect(),
                deadline: Duration::from_secs(1),
                cancel: CancelPolicy::Always,
            })
        };
        let f = begin(&["capture", "--serial", "S9", "--label", "x"]);
        assert_eq!((f.op.as_str(), f.serial.as_deref()), ("capture", Some("S9")));
        assert_eq!(begin(&["capture"]).serial, None, "no --serial");
        assert_eq!(begin(&["capture", "--serial"]).serial, None, "--serial with no value after it");
        assert_eq!(begin(&["check", "--label", "--serial", "S2"]).serial.as_deref(), Some("S2"));
        let none = begin(&[]);
        assert_eq!((none.op.as_str(), none.serial), ("", None), "an empty argument list is an empty op, not a panic");
    }

    fn lost_apply(writing_seen: bool, may_write: bool) -> InFlight {
        InFlight {
            op: "restore-apply".into(),
            serial: Some("S1".into()),
            may_write,
            last_phase: Some(if writing_seen { "writing" } else { "restore_point_written" }.into()),
            writing_seen,
            started_at: "2026-10-28T10:00:00Z".into(),
        }
    }

    #[test]
    fn only_an_apply_that_reached_writing_leaves_a_marker() {
        for (writing_seen, may_write, marker) in [(true, true, true), (false, true, false), (true, false, false), (false, false, false)] {
            let d = TempDir::new().unwrap();
            let written = record_in_flight_at_exit(d.path(), ExitReason::Sigterm, Some(&lost_apply(writing_seen, may_write))).unwrap();
            assert_eq!(written, marker, "writing_seen={writing_seen} may_write={may_write}");
            assert_eq!(d.path().join(MARKER_FILE).exists(), marker, "writing_seen={writing_seen} may_write={may_write}");
        }
    }

    #[test]
    fn nothing_in_flight_leaves_nothing() {
        let d = TempDir::new().unwrap();
        assert!(!record_in_flight_at_exit(d.path(), ExitReason::ClientGoneWaitExpired, None).unwrap());
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
    }

    #[test]
    fn the_marker_holds_what_was_in_flight_and_why_the_server_left() {
        let d = TempDir::new().unwrap();
        let f = lost_apply(true, true);
        record_in_flight_at_exit(d.path(), ExitReason::ClientGoneWaitExpired, Some(&f)).unwrap();
        let m: IncompleteMarker = serde_json::from_str(&std::fs::read_to_string(d.path().join(MARKER_FILE)).unwrap()).unwrap();
        assert_eq!((m.reason, m.in_flight), (ExitReason::ClientGoneWaitExpired, f));
        assert!(!m.recorded_at.is_empty());
        let leftovers: Vec<_> = std::fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers, [std::ffi::OsString::from(MARKER_FILE)], "written atomically: no staging file left");
    }

    #[test]
    fn the_exit_reasons_serialise_to_the_closed_vocabulary() {
        for (r, want) in [
            (ExitReason::ClientGoneWaitExpired, "client_gone_wait_expired"),
            (ExitReason::Sigterm, "sigterm"),
            (ExitReason::Sigint, "sigint"),
            (ExitReason::Sighup, "sighup"),
        ] {
            assert_eq!(serde_json::to_value(r).unwrap(), json!(want));
            // No wildcard: a new reason must be added to this list.
            match r {
                ExitReason::ClientGoneWaitExpired | ExitReason::Sigterm | ExitReason::Sigint | ExitReason::Sighup => {}
            }
        }
    }

    #[test]
    fn a_marker_that_cannot_be_written_is_an_error_not_a_silent_success() {
        let d = TempDir::new().unwrap();
        let missing = d.path().join("no-such-dir");
        let e = record_in_flight_at_exit(&missing, ExitReason::Sigint, Some(&lost_apply(true, true))).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
        assert!(!missing.exists());
    }

    #[test]
    fn a_second_marker_replaces_an_unreported_first_one() {
        let d = TempDir::new().unwrap();
        record_in_flight_at_exit(d.path(), ExitReason::Sigterm, Some(&lost_apply(true, true))).unwrap();
        let mut second = lost_apply(true, true);
        second.serial = Some("S2".into());
        record_in_flight_at_exit(d.path(), ExitReason::Sighup, Some(&second)).unwrap();
        let note = previous_session_note(d.path()).unwrap();
        assert_eq!((note["reason"].as_str(), note["in_flight"]["serial"].as_str()), (Some("sighup"), Some("S2")));
    }

    #[test]
    fn the_note_is_typed_and_carries_the_recorded_fields() {
        let d = TempDir::new().unwrap();
        assert_eq!(previous_session_note(d.path()), None, "no marker, no note");
        record_in_flight_at_exit(d.path(), ExitReason::ClientGoneWaitExpired, Some(&lost_apply(true, true))).unwrap();
        let note = previous_session_note(d.path()).unwrap();
        assert_eq!(note["marker_unreadable"], json!(false));
        assert_eq!(note["reason"], json!("client_gone_wait_expired"));
        assert_eq!(note["in_flight"]["op"], json!("restore-apply"));
        assert_eq!(note["in_flight"]["serial"], json!("S1"));
        assert_eq!(note["in_flight"]["last_phase"], json!("writing"));
        assert_eq!(note["in_flight"]["writing_seen"], json!(true));
        assert!(note["recorded_at"].is_string() && note["message"].is_string());
        assert!(d.path().join(MARKER_FILE).exists(), "reading the note does not consume the marker");
    }

    #[test]
    fn a_marker_that_cannot_be_parsed_is_reported_as_unreadable_not_ignored() {
        let d = TempDir::new().unwrap();
        std::fs::write(d.path().join(MARKER_FILE), "{ torn").unwrap();
        let note = previous_session_note(d.path()).unwrap();
        assert_eq!(note["marker_unreadable"], json!(true));
        assert!(note["error"].as_str().is_some_and(|e| !e.is_empty()) && note["file"].is_string());
        // And an empty file, the other way a crash can leave it.
        std::fs::write(d.path().join(MARKER_FILE), "").unwrap();
        assert_eq!(previous_session_note(d.path()).unwrap()["marker_unreadable"], json!(true));
    }

    #[test]
    fn a_marker_that_cannot_be_read_at_all_is_reported_too() {
        // A directory where the file should be: a read error that is not "not found".
        let d = TempDir::new().unwrap();
        std::fs::create_dir(d.path().join(MARKER_FILE)).unwrap();
        assert_eq!(previous_session_note(d.path()).unwrap()["marker_unreadable"], json!(true));
    }

    #[test]
    fn acknowledging_renames_the_marker_so_it_is_reported_once_and_kept() {
        let d = TempDir::new().unwrap();
        record_in_flight_at_exit(d.path(), ExitReason::Sigterm, Some(&lost_apply(true, true))).unwrap();
        let before = std::fs::read(d.path().join(MARKER_FILE)).unwrap();
        acknowledge_previous_session(d.path()).unwrap();
        assert_eq!(previous_session_note(d.path()), None, "reported once");
        assert!(!d.path().join(MARKER_FILE).exists());
        assert_eq!(std::fs::read(d.path().join(MARKER_SEEN_FILE)).unwrap(), before, "kept, byte for byte");
        let e = acknowledge_previous_session(d.path()).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "nothing left to acknowledge is an error, not a success");
        // A later marker is reported again, and replaces the older acknowledged one.
        record_in_flight_at_exit(d.path(), ExitReason::Sighup, Some(&lost_apply(true, true))).unwrap();
        assert!(previous_session_note(d.path()).is_some());
        acknowledge_previous_session(d.path()).unwrap();
        let seen: IncompleteMarker = serde_json::from_slice(&std::fs::read(d.path().join(MARKER_SEEN_FILE)).unwrap()).unwrap();
        assert_eq!(seen.reason, ExitReason::Sighup);
    }
}

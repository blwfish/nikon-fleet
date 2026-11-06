//! App-wide diagnostics: what was this process doing when it hung, crashed or panicked?
//!
//! Nothing here knows about cameras. Any part of the app (the CLI, both GUIs, the MCP server, the
//! USB and SDK layers, apply/restore, firmware fetch) wraps an operation in a [`Span`] and the
//! evidence survives the process dying:
//!
//! - **Write-ahead in-flight file.** `inflight-<pid>.json` lists every span that has begun and not
//!   ended, rewritten (temp file + rename, no fsync: it only has to outlive the *process*, not the
//!   machine) on every begin and end. A process that hangs, is killed or aborts leaves it behind;
//!   a process that finishes everything leaves no file. This is the mechanism that worked in
//!   freecad-mcp (a "last op" record written before the risky call and cleared after).
//! - **Bounded ring of completed entries.** An in-memory ring (default [`DEFAULT_RING_CAP`]) mirrored
//!   to `ring-<pid>.jsonl`, append-only, compacted from memory when it reaches twice the capacity, so
//!   the cost of an entry does not grow with the capacity. Eviction is by priority, not age alone:
//!   routine successful leaf spans (one SDK read of two hundred) go first, then successful top-level
//!   spans, then events and failed leaf spans, and failed top-level spans last. A burst of routine
//!   reads therefore cannot push out the failure that matters.
//! - **A report** ([`load`], [`render`]) that reads every process's files, says whether the process
//!   is still running (hung) or gone (killed or crashed), and shows what was in flight and what ran
//!   just before.
//!
//! Records are typed. The subsystem and outcome are closed enums; field keys are `&'static str`
//! chosen in code; field values are [`Val`]s (numbers, flags, a length, a hash, a bounded text).
//! There is deliberately no free-text message that any logic branches on. **Payloads are never
//! recorded**, only their length and a short hash ([`Val::len`], [`Val::hash`]); an FTP profile
//! carries a password and nothing here tries to scrub text, it just never stores the bytes.
//!
//! Failing to write diagnostics never fails the operation being diagnosed. Each failure bumps
//! [`Diag::write_failures`], is reported once on stderr per streak, and appears in the report.
//!
//! A process that never calls [`init`] pays one atomic load per call and records nothing, so the
//! library is usable (and its tests run) without any of this being switched on.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::settings::{data_dir_for, Os, APP_DIR};

/// Environment variable that replaces the diagnostics directory.
pub const DIR_ENV: &str = "NIKON_FLEET_DIAG_DIR";
/// Name of the directory under `{data dir}/net.blw.fleet` when [`DIR_ENV`] is unset.
pub const DIR_NAME: &str = "diag";
/// Default number of completed entries kept in the ring. Not a hard design limit: any value from
/// [`MIN_RING_CAP`] up works, and the disk cost per entry does not depend on it.
pub const DEFAULT_RING_CAP: usize = 1024;
pub const MIN_RING_CAP: usize = 8;
/// How many processes' files are kept when a new process starts (the newest by pid-file mtime).
pub const KEEP_PROCESSES: usize = 20;
/// Longest [`Val::Text`], in characters, before it is cut and marked.
pub const TEXT_MAX_CHARS: usize = 256;
pub const TRUNCATED_MARK: &str = " …[truncated]";
/// The operation name of the entry [`Diag::shutdown`] records, and of the panic hook's.
pub const OP_EXIT: &str = "exit";
pub const OP_PANIC: &str = "panic";

// ─────────────────────────────────────────────────────────────────────────
// Closed vocabularies
// ─────────────────────────────────────────────────────────────────────────

/// Where a record comes from. A closed set: adding a subsystem is a code change, which is the point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subsystem {
    Cli,
    Gui,
    Mcp,
    Usb,
    PtpIp,
    MaidSdk,
    Snapshot,
    Transplant,
    Restore,
    Firmware,
    Settings,
    Lock,
    Diag,
}

impl Subsystem {
    pub const ALL: [Subsystem; 13] = [
        Subsystem::Cli,
        Subsystem::Gui,
        Subsystem::Mcp,
        Subsystem::Usb,
        Subsystem::PtpIp,
        Subsystem::MaidSdk,
        Subsystem::Snapshot,
        Subsystem::Transplant,
        Subsystem::Restore,
        Subsystem::Firmware,
        Subsystem::Settings,
        Subsystem::Lock,
        Subsystem::Diag,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Subsystem::Cli => "cli",
            Subsystem::Gui => "gui",
            Subsystem::Mcp => "mcp",
            Subsystem::Usb => "usb",
            Subsystem::PtpIp => "ptp_ip",
            Subsystem::MaidSdk => "maid_sdk",
            Subsystem::Snapshot => "snapshot",
            Subsystem::Transplant => "transplant",
            Subsystem::Restore => "restore",
            Subsystem::Firmware => "firmware",
            Subsystem::Settings => "settings",
            Subsystem::Lock => "lock",
            Subsystem::Diag => "diag",
        }
    }
}

/// How a span ended. [`Outcome::Abandoned`] is a guard dropped without a result (an early `?`
/// return); [`Outcome::Panicked`] is one dropped while unwinding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Failed,
    Timeout,
    Stalled,
    Indeterminate,
    Panicked,
    Abandoned,
    /// The caller gave up on it (an MCP request cancelled by the client).
    Cancelled,
}

impl Outcome {
    pub const ALL: [Outcome; 8] = [
        Outcome::Ok,
        Outcome::Failed,
        Outcome::Timeout,
        Outcome::Stalled,
        Outcome::Indeterminate,
        Outcome::Panicked,
        Outcome::Abandoned,
        Outcome::Cancelled,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Failed => "failed",
            Outcome::Timeout => "timeout",
            Outcome::Stalled => "stalled",
            Outcome::Indeterminate => "indeterminate",
            Outcome::Panicked => "panicked",
            Outcome::Abandoned => "abandoned",
            Outcome::Cancelled => "cancelled",
        }
    }
}

/// Eviction priority of a span. A [`Class::Leaf`] is high-volume and routine (one SDK read, one PTP
/// transaction); a [`Class::Top`] is a whole command or step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Top,
    Leaf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Span,
    Event,
}

/// A field value. No variant holds an arbitrary payload: bytes are represented only by [`Val::len`]
/// and [`Val::hash`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v", rename_all = "snake_case")]
pub enum Val {
    U64(u64),
    I64(i64),
    Bool(bool),
    /// An operation or property code, shown in hex.
    Hex(u64),
    /// A byte length.
    Len(u64),
    /// The first 8 bytes of a SHA-256, hex. Identifies a payload without storing it.
    Hash(String),
    /// A short identifier (a model name, a serial, a path). Bounded: see [`Val::text`].
    Text(String),
}

impl Val {
    /// A text value, cut to [`TEXT_MAX_CHARS`] characters with [`TRUNCATED_MARK`] appended when it was longer, so
    /// a reader can tell a cut value from a short one.
    pub fn text(s: &str) -> Val {
        if s.chars().count() <= TEXT_MAX_CHARS {
            Val::Text(s.to_string())
        } else {
            let mut cut: String = s.chars().take(TEXT_MAX_CHARS).collect();
            cut.push_str(TRUNCATED_MARK);
            Val::Text(cut)
        }
    }

    pub fn len(n: usize) -> Val {
        Val::Len(n as u64)
    }

    pub fn hash(bytes: &[u8]) -> Val {
        let digest = Sha256::digest(bytes);
        Val::Hash(digest[..8].iter().map(|b| format!("{b:02x}")).collect())
    }

    fn display(&self) -> String {
        match self {
            Val::U64(n) => n.to_string(),
            Val::I64(n) => n.to_string(),
            Val::Bool(b) => b.to_string(),
            Val::Hex(n) => format!("0x{n:x}"),
            Val::Len(n) => format!("{n}B"),
            Val::Hash(h) => format!("#{h}"),
            Val::Text(s) => format!("{s:?}"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Records
// ─────────────────────────────────────────────────────────────────────────

/// A completed span or an event: one line of `ring-<pid>.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: u64,
    /// Unix milliseconds at the start of the span (the moment of an event).
    pub ts_ms: u64,
    pub kind: Kind,
    pub class: Class,
    pub subsystem: Subsystem,
    pub op: String,
    pub fields: Vec<(String, Val)>,
    /// How many spans were already open on the same OS thread when this began. Per thread, not per
    /// operation: async code that moves between threads gets a hint here, not a tree. Use `seq` and
    /// `ts_ms` to order spans.
    pub depth: u32,
    pub thread: u64,
    /// `None` for an event.
    pub outcome: Option<Outcome>,
    pub dur_us: u64,
}

/// A span that has begun and not ended: one element of `inflight-<pid>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenSpan {
    pub seq: u64,
    pub ts_ms: u64,
    pub class: Class,
    pub subsystem: Subsystem,
    pub op: String,
    pub fields: Vec<(String, Val)>,
    pub depth: u32,
    pub thread: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InflightFile {
    pub pid: u32,
    pub role: Subsystem,
    pub process_started_ms: u64,
    pub spans: Vec<OpenSpan>,
}

/// Where a process's in-flight file is. The one definition: the writer and the reader both use it.
pub fn inflight_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("inflight-{pid}.json"))
}

/// Where a process's ring file is. The one definition: the writer and the reader both use it.
pub fn ring_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("ring-{pid}.jsonl"))
}

/// The diagnostics directory for `os` and an environment: [`DIR_ENV`] if set and non-empty, else
/// `{data dir}/net.blw.fleet/diag`. `None` when there is no home directory and no override: the
/// caller decides what to do (the programs run without diagnostics, and say so).
pub fn dir_for(os: Os, env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(p) = env(DIR_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    data_dir_for(os, env).map(|d| d.join(APP_DIR).join(DIR_NAME))
}

/// [`dir_for`] for this process.
pub fn dir() -> Option<PathBuf> {
    dir_for(Os::CURRENT, &|k| std::env::var_os(k))
}

// ─────────────────────────────────────────────────────────────────────────
// Files
// ─────────────────────────────────────────────────────────────────────────

/// The current time as unix milliseconds (0 if the clock is before the epoch).
pub fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// A file only the owner can read where the platform has modes; elsewhere the directory's own
/// permissions apply.
fn private_open(path: &Path, append: bool) -> std::io::Result<fs::File> {
    let mut o = fs::OpenOptions::new();
    o.create(true).write(true);
    if append {
        o.append(true);
    } else {
        o.truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// Replace `path` with `bytes` so a reader sees the old file or the whole new one: a temp file in
/// the same directory, then a rename. Not synced to disk, on purpose: these files have to outlive
/// the process, and an fsync before every span would cost milliseconds per operation.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let written = private_open(&tmp, false).and_then(|mut f| f.write_all(bytes)).and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp); // best effort; `written` is the error to report
    }
    written
}

// ─────────────────────────────────────────────────────────────────────────
// The recorder
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Config {
    pub dir: PathBuf,
    /// What this process is: [`Subsystem::Cli`], [`Subsystem::Gui`] or [`Subsystem::Mcp`].
    pub role: Subsystem,
    /// Completed entries kept; raised to [`MIN_RING_CAP`] if smaller.
    pub ring_cap: usize,
    /// Also append every entry here, uncompacted (`--trace FILE`).
    pub trace_file: Option<PathBuf>,
    /// The process id the files are named by. Always `std::process::id()` outside tests.
    pub pid: u32,
}

impl Config {
    pub fn new(dir: PathBuf, role: Subsystem) -> Config {
        Config { dir, role, ring_cap: DEFAULT_RING_CAP, trace_file: None, pid: std::process::id() }
    }
}

struct State {
    ring: VecDeque<Entry>,
    open: Vec<OpenSpan>,
    ring_lines: usize,
    ring_file: Option<fs::File>,
    trace_file: Option<fs::File>,
}

pub struct Diag {
    cfg: Config,
    started_ms: u64,
    seq: AtomicU64,
    failures: AtomicU64,
    /// True while the last write failed, so one streak warns once.
    in_failure_streak: AtomicBool,
    /// Set by the first [`Diag::shutdown`]; a later one does nothing, so a process that reaches its exit by
    /// two routes (the GUI's `on_exit`, then `main`) records one exit, with the first code.
    exited: AtomicBool,
    state: Mutex<State>,
}

thread_local! {
    static THREAD_NUM: u64 = {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    };
}

fn thread_num() -> u64 {
    THREAD_NUM.with(|n| *n)
}

/// Lower is evicted first.
fn eviction_rank(e: &Entry) -> u8 {
    match (e.kind, e.class, e.outcome) {
        (Kind::Span, Class::Leaf, Some(Outcome::Ok)) => 0,
        (Kind::Span, Class::Top, Some(Outcome::Ok)) => 1,
        (Kind::Event, _, _) => 2,
        (Kind::Span, Class::Leaf, _) => 2,
        (Kind::Span, Class::Top, _) => 3,
    }
}

impl Diag {
    /// Start recording to `cfg.dir`. Creates the directory (owner-only), opens this process's ring
    /// file, and removes the oldest processes' files beyond [`KEEP_PROCESSES`]. An `Err` means
    /// diagnostics could not be set up; nothing was left half-open.
    pub fn new(cfg: Config) -> std::io::Result<Diag> {
        private_dir(&cfg.dir)?;
        let ring_file = private_open(&ring_path(&cfg.dir, cfg.pid), false)?;
        let trace_file = match &cfg.trace_file {
            Some(p) => Some(private_open(p, true)?),
            None => None,
        };
        prune(&cfg.dir, cfg.pid);
        Ok(Diag {
            started_ms: unix_ms(),
            seq: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            in_failure_streak: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            state: Mutex::new(State { ring: VecDeque::new(), open: Vec::new(), ring_lines: 0, ring_file: Some(ring_file), trace_file }),
            cfg,
        })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// How many diagnostics writes have failed. Reported in the report; never an operation error.
    pub fn write_failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cap(&self) -> usize {
        self.cfg.ring_cap.max(MIN_RING_CAP)
    }

    fn note_write(&self, result: std::io::Result<()>) {
        match result {
            Ok(()) => {
                self.in_failure_streak.store(false, Ordering::Relaxed);
            }
            Err(e) => {
                self.failures.fetch_add(1, Ordering::Relaxed);
                if !self.in_failure_streak.swap(true, Ordering::Relaxed) {
                    eprintln!("warning: diagnostics could not be written to {} ({e}); the operation is not affected", self.cfg.dir.display());
                }
            }
        }
    }

    /// Begin a span. `fields` are the values known now; more can be added when it finishes.
    pub fn span(&self, subsystem: Subsystem, op: &str, class: Class, fields: &[(&'static str, Val)]) -> Span<'_> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let thread = thread_num();
        let mut st = self.lock();
        let depth = st.open.iter().filter(|o| o.thread == thread).count() as u32;
        st.open.push(OpenSpan {
            seq,
            ts_ms: unix_ms(),
            class,
            subsystem,
            op: op.to_string(),
            fields: fields.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect(),
            depth,
            thread,
        });
        self.write_inflight(&st);
        Span { diag: Some(self), seq, started: Instant::now(), finished: false }
    }

    /// Record something that happened, with no duration.
    pub fn event(&self, subsystem: Subsystem, op: &str, fields: &[(&'static str, Val)]) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let thread = thread_num();
        let mut st = self.lock();
        let depth = st.open.iter().filter(|o| o.thread == thread).count() as u32;
        let entry = Entry {
            seq,
            ts_ms: unix_ms(),
            kind: Kind::Event,
            class: Class::Top,
            subsystem,
            op: op.to_string(),
            fields: fields.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect(),
            depth,
            thread,
            outcome: None,
            dur_us: 0,
        };
        self.push(&mut st, entry);
    }

    /// Record that the process is exiting with `code`, the entry that tells the report this was a
    /// planned exit and not a kill. Spans still open are closed first: this is called on the paths
    /// that leave without unwinding (`process::exit`, `_exit`), where a command's own span never
    /// gets to finish. A [`Class::Top`] span ends [`Outcome::Ok`] for code 0 and [`Outcome::Failed`]
    /// otherwise; anything else still open ends [`Outcome::Abandoned`]. A process that hangs
    /// *before* this is reached keeps its spans open, which is the whole point.
    pub fn shutdown(&self, code: i32) {
        if self.exited.swap(true, Ordering::SeqCst) {
            return;
        }
        let thread = thread_num();
        let mut st = self.lock();
        let open = std::mem::take(&mut st.open);
        for o in open {
            let outcome = match (o.class, code) {
                (Class::Top, 0) => Outcome::Ok,
                (Class::Top, _) => Outcome::Failed,
                (Class::Leaf, _) => Outcome::Abandoned,
            };
            let dur_us = unix_ms().saturating_sub(o.ts_ms) * 1000;
            self.finish_open(&mut st, o, outcome, &[], dur_us);
        }
        self.write_inflight(&st);
        let entry = Entry {
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            ts_ms: unix_ms(),
            kind: Kind::Event,
            class: Class::Top,
            subsystem: self.cfg.role,
            op: OP_EXIT.to_string(),
            fields: vec![("code".to_string(), Val::I64(i64::from(code)))],
            depth: 0,
            thread,
            outcome: None,
            dur_us: 0,
        };
        self.push(&mut st, entry);
    }

    fn write_inflight(&self, st: &State) {
        let path = inflight_path(&self.cfg.dir, self.cfg.pid);
        if st.open.is_empty() {
            // Nothing in flight: no file. A missing file is how a clean state looks.
            let r = match fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
            self.note_write(r);
            return;
        }
        let file = InflightFile { pid: self.cfg.pid, role: self.cfg.role, process_started_ms: self.started_ms, spans: st.open.clone() };
        let r = match serde_json::to_vec(&file) {
            Ok(bytes) => write_private_atomic(&path, &bytes),
            Err(e) => Err(std::io::Error::other(e)),
        };
        self.note_write(r);
    }

    /// Add a completed entry: ring (with priority eviction), ring file, trace file.
    fn push(&self, st: &mut State, entry: Entry) {
        let cap = self.cap();
        while st.ring.len() >= cap {
            // The lowest rank, oldest first among equals.
            let victim = st
                .ring
                .iter()
                .enumerate()
                .min_by_key(|(i, e)| (eviction_rank(e), *i))
                .map(|(i, _)| i)
                .expect("the ring is non-empty while its length is at least the capacity");
            st.ring.remove(victim);
        }
        st.ring.push_back(entry.clone());

        let line = match serde_json::to_string(&entry) {
            Ok(l) => l,
            Err(e) => {
                self.note_write(Err(std::io::Error::other(e)));
                return;
            }
        };
        let mut result = Ok(());
        if let Some(f) = st.ring_file.as_mut() {
            result = writeln!(f, "{line}");
            st.ring_lines += 1;
        }
        if let Some(f) = st.trace_file.as_mut() {
            let r = writeln!(f, "{line}");
            if result.is_ok() {
                result = r;
            }
        }
        self.note_write(result);
        if st.ring_lines >= cap * 2 {
            self.compact(st);
        }
    }

    /// Rewrite the ring file from memory, so it holds the ring and not everything ever appended.
    fn compact(&self, st: &mut State) {
        let mut out = String::new();
        for e in &st.ring {
            match serde_json::to_string(e) {
                Ok(l) => {
                    out.push_str(&l);
                    out.push('\n');
                }
                Err(err) => {
                    self.note_write(Err(std::io::Error::other(err)));
                    return;
                }
            }
        }
        let path = ring_path(&self.cfg.dir, self.cfg.pid);
        let r = write_private_atomic(&path, out.as_bytes()).and_then(|()| private_open(&path, true));
        match r {
            Ok(f) => {
                st.ring_file = Some(f);
                st.ring_lines = st.ring.len();
                self.note_write(Ok(()));
            }
            Err(e) => {
                // Keep appending to the old handle (it still names the replaced file or the
                // original); the count restarts so this retries at the next threshold.
                st.ring_lines = st.ring.len();
                self.note_write(Err(e));
            }
        }
    }

    fn end(&self, seq: u64, outcome: Outcome, extra: &[(&'static str, Val)], dur_us: u64) {
        let mut st = self.lock();
        let Some(pos) = st.open.iter().position(|o| o.seq == seq) else {
            return;
        };
        let open = st.open.remove(pos);
        self.finish_open(&mut st, open, outcome, extra, dur_us);
        self.write_inflight(&st);
    }

    /// Turn an open span (already removed from `st.open`) into a completed ring entry.
    fn finish_open(&self, st: &mut State, open: OpenSpan, outcome: Outcome, extra: &[(&'static str, Val)], dur_us: u64) {
        let mut fields = open.fields;
        fields.extend(extra.iter().map(|(k, v)| ((*k).to_string(), v.clone())));
        let entry = Entry {
            seq: open.seq,
            ts_ms: open.ts_ms,
            kind: Kind::Span,
            class: open.class,
            subsystem: open.subsystem,
            op: open.op,
            fields,
            depth: open.depth,
            thread: open.thread,
            outcome: Some(outcome),
            dur_us,
        };
        self.push(st, entry);
    }

    /// Replace (or add) one field of a span that is still open, and rewrite the in-flight file so a
    /// reader sees it: how a long operation reports the stage it has reached.
    fn set_field(&self, seq: u64, key: &'static str, v: Val) {
        let mut st = self.lock();
        if let Some(o) = st.open.iter_mut().find(|o| o.seq == seq) {
            match o.fields.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = v,
                None => o.fields.push((key.to_string(), v)),
            }
            self.write_inflight(&st);
        }
    }

    /// A copy of the ring, oldest first.
    pub fn snapshot(&self) -> Vec<Entry> {
        self.lock().ring.iter().cloned().collect()
    }

    /// A copy of the spans in flight, oldest first.
    pub fn open_spans(&self) -> Vec<OpenSpan> {
        self.lock().open.clone()
    }
}

/// Remove the oldest processes' files beyond [`KEEP_PROCESSES`], never this process's. Best effort:
/// a file that cannot be removed is left, and nothing about that is an error for the caller.
fn prune(dir: &Path, own_pid: u32) {
    let Ok(read) = fs::read_dir(dir) else { return };
    let mut pids: std::collections::BTreeMap<u32, std::time::SystemTime> = std::collections::BTreeMap::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(pid) = pid_of_file_name(&name) {
            let m = entry.metadata().and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH);
            let slot = pids.entry(pid).or_insert(m);
            if m > *slot {
                *slot = m;
            }
        }
    }
    if pids.len() <= KEEP_PROCESSES {
        return;
    }
    let mut by_age: Vec<(u32, std::time::SystemTime)> = pids.into_iter().filter(|(p, _)| *p != own_pid).collect();
    by_age.sort_by_key(|(_, m)| *m);
    let excess = by_age.len().saturating_sub(KEEP_PROCESSES.saturating_sub(1));
    for (pid, _) in by_age.into_iter().take(excess) {
        let _ = fs::remove_file(inflight_path(dir, pid));
        let _ = fs::remove_file(ring_path(dir, pid));
    }
}

/// The pid in `inflight-<pid>.json` or `ring-<pid>.jsonl`, else `None`. Built from the same two
/// name shapes as [`inflight_path`] and [`ring_path`].
fn pid_of_file_name(name: &str) -> Option<u32> {
    let stem = name
        .strip_prefix("inflight-")
        .and_then(|r| r.strip_suffix(".json"))
        .or_else(|| name.strip_prefix("ring-").and_then(|r| r.strip_suffix(".jsonl")))?;
    stem.parse().ok()
}

// ─────────────────────────────────────────────────────────────────────────
// Spans
// ─────────────────────────────────────────────────────────────────────────

/// A span that has begun. Finish it with [`Span::finish`] or [`Span::finish_with`]; dropped without
/// either it ends as [`Outcome::Abandoned`], or [`Outcome::Panicked`] when the thread is unwinding.
#[must_use = "a span records its outcome when finished or dropped; binding it to _ ends it at once"]
pub struct Span<'a> {
    diag: Option<&'a Diag>,
    seq: u64,
    started: Instant,
    finished: bool,
}

impl std::fmt::Debug for Span<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Span").field("seq", &self.seq).field("recording", &self.diag.is_some()).finish()
    }
}

impl Span<'static> {
    /// A span that records nothing: what the code uses when diagnostics are not running.
    pub fn none() -> Span<'static> {
        Span { diag: None, seq: 0, started: Instant::now(), finished: false }
    }
}

impl Span<'_> {
    /// Update a field of this open span (for example the stage reached). Visible in the in-flight
    /// file at once, and carried into the completed entry.
    pub fn set(&self, key: &'static str, v: Val) {
        if let Some(d) = self.diag {
            d.set_field(self.seq, key, v);
        }
    }

    pub fn finish(self, outcome: Outcome) {
        self.finish_with(outcome, &[]);
    }

    /// Finish as [`Outcome::Ok`] or [`Outcome::Failed`] from a `Result`, without touching it. For
    /// the common case where the only outcomes are success and failure.
    pub fn done<T, E>(self, result: &Result<T, E>) {
        self.finish(if result.is_ok() { Outcome::Ok } else { Outcome::Failed });
    }

    /// Finish with extra fields known only now (a response code, a byte count).
    pub fn finish_with(mut self, outcome: Outcome, extra: &[(&'static str, Val)]) {
        self.finished = true;
        if let Some(d) = self.diag {
            d.end(self.seq, outcome, extra, self.started.elapsed().as_micros() as u64);
        }
    }
}

impl Drop for Span<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(d) = self.diag {
            let outcome = if std::thread::panicking() { Outcome::Panicked } else { Outcome::Abandoned };
            d.end(self.seq, outcome, &[], self.started.elapsed().as_micros() as u64);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The process-wide recorder
// ─────────────────────────────────────────────────────────────────────────

static GLOBAL: OnceLock<Diag> = OnceLock::new();

/// Install the process-wide recorder. The first call wins; a later one returns the existing
/// recorder untouched (an `Ok`, since diagnostics are on either way).
pub fn init(cfg: Config) -> std::io::Result<&'static Diag> {
    if let Some(d) = GLOBAL.get() {
        return Ok(d);
    }
    let d = Diag::new(cfg)?;
    Ok(GLOBAL.get_or_init(|| d))
}

/// The process-wide recorder, if [`init`] succeeded.
pub fn global() -> Option<&'static Diag> {
    GLOBAL.get()
}

/// Begin a span on the process-wide recorder. A no-op span when diagnostics are not initialised.
pub fn span(subsystem: Subsystem, op: &str, class: Class, fields: &[(&'static str, Val)]) -> Span<'static> {
    match GLOBAL.get() {
        Some(d) => d.span(subsystem, op, class, fields),
        None => Span::none(),
    }
}

/// Record an event on the process-wide recorder. A no-op when diagnostics are not initialised.
pub fn event(subsystem: Subsystem, op: &str, fields: &[(&'static str, Val)]) {
    if let Some(d) = GLOBAL.get() {
        d.event(subsystem, op, fields);
    }
}

/// Record a clean exit on the process-wide recorder, if there is one.
pub fn shutdown(code: i32) {
    if let Some(d) = GLOBAL.get() {
        d.shutdown(code);
    }
}

/// Environment variable a parent sets for a child process so the two sides' records can be joined:
/// the parent records the same value in its span (field `request_id`), and the child records it in
/// its `start` event.
pub const REQUEST_ID_ENV: &str = "NIKON_FLEET_REQUEST_ID";
/// The operation name of the event [`start`] records.
pub const OP_START: &str = "start";

/// A request id unique within this process and, with the pid, across processes: `<pid>-<n>`.
pub fn next_request_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Start diagnostics for this process in the standard way: find the directory, [`init`], install the
/// panic hook (once), and record a `start` event with the parent process id and, when a parent set
/// one, the [`REQUEST_ID_ENV`]. Never fatal: when diagnostics cannot be set up it says so once on
/// stderr and returns `None`, and the program runs without them.
pub fn start(role: Subsystem, trace_file: Option<&Path>) -> Option<&'static Diag> {
    let Some(dir) = dir() else {
        eprintln!("warning: diagnostics are off: no home directory is set and {DIR_ENV} is not set");
        return None;
    };
    let mut cfg = Config::new(dir, role);
    cfg.trace_file = trace_file.map(Path::to_path_buf);
    let d = match init(cfg) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("warning: diagnostics are off: {e}");
            return None;
        }
    };
    install_panic_hook();
    let mut fields: Vec<(&'static str, Val)> = vec![("role", Val::text(role.as_str()))];
    #[cfg(unix)]
    {
        // SAFETY: getppid has no preconditions and cannot fail.
        fields.push(("ppid", Val::U64(unsafe { libc::getppid() } as u64)));
    }
    if let Some(id) = std::env::var(REQUEST_ID_ENV).ok().filter(|v| !v.is_empty()) {
        fields.push(("request_id", Val::text(&id)));
    }
    d.event(Subsystem::Diag, OP_START, &fields);
    Some(d)
}

/// Run `f` inside a span on the process-wide recorder and judge its value: `judge` turns the value into
/// the outcome and the extra fields known only now. For a body with several early returns (or `?`), wrap
/// the body in the closure: a `return` or `?` inside it returns from the closure, so the span always ends
/// through here. A panic in `f` unwinds through the span's drop and ends it [`Outcome::Panicked`]. When
/// diagnostics are not running, `f` simply runs and `judge` is not called.
pub fn scoped_with<T>(
    subsystem: Subsystem,
    op: &str,
    class: Class,
    fields: &[(&'static str, Val)],
    f: impl FnOnce(&Span<'_>) -> T,
    judge: impl FnOnce(&T) -> (Outcome, Vec<(&'static str, Val)>),
) -> T {
    match GLOBAL.get() {
        Some(d) => d.scoped_with(subsystem, op, class, fields, f, judge),
        None => f(&Span::none()),
    }
}

/// [`scoped_with`] for a `Result`: `Ok` ends the span [`Outcome::Ok`], `Err` [`Outcome::Failed`].
pub fn scoped<T, E>(
    subsystem: Subsystem,
    op: &str,
    class: Class,
    fields: &[(&'static str, Val)],
    f: impl FnOnce(&Span<'_>) -> Result<T, E>,
) -> Result<T, E> {
    scoped_with(subsystem, op, class, fields, f, |r| (if r.is_ok() { Outcome::Ok } else { Outcome::Failed }, Vec::new()))
}

impl Diag {
    /// [`scoped_with`] on this recorder.
    pub fn scoped_with<T>(
        &self,
        subsystem: Subsystem,
        op: &str,
        class: Class,
        fields: &[(&'static str, Val)],
        f: impl FnOnce(&Span<'_>) -> T,
        judge: impl FnOnce(&T) -> (Outcome, Vec<(&'static str, Val)>),
    ) -> T {
        let span = self.span(subsystem, op, class, fields);
        let value = f(&span);
        let (outcome, extra) = judge(&value);
        span.finish_with(outcome, &extra);
        value
    }
}

/// Install a panic hook that records the panic (location and a bounded message) before the
/// previous hook runs. With `panic = "abort"` the open spans are never dropped, so the in-flight file
/// keeps them and this entry says why. Safe to call once per process.
pub fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(install_panic_hook_once);
}

fn install_panic_hook_once() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(d) = GLOBAL.get() {
            let location = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_else(|| "unknown".to_string());
            let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "(non-string panic payload)".to_string()
            };
            d.event(d.cfg.role, OP_PANIC, &[("location", Val::text(&location)), ("message", Val::text(&message))]);
        }
        previous(info);
    }));
}

// ─────────────────────────────────────────────────────────────────────────
// Reading it back: the report
// ─────────────────────────────────────────────────────────────────────────

/// What the files of one process say happened to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// It recorded an exit and has nothing in flight.
    ExitedCleanly { code: i64 },
    /// Spans were still open and the process is gone: it was killed or crashed.
    GoneWithSpansOpen,
    /// Spans are open and the process is still running: hung, or simply busy.
    RunningWithSpansOpen,
    /// Nothing open, no exit recorded, the process is gone: killed between operations.
    GoneWithoutExit,
    /// Nothing open and still running (or liveness unknown): idle or between operations.
    RunningIdle,
    /// Spans are open and this platform cannot say whether the process is alive.
    SpansOpenLivenessUnknown,
}

#[derive(Debug, Clone)]
pub struct ProcessTrace {
    pub pid: u32,
    pub inflight: Option<InflightFile>,
    pub entries: Vec<Entry>,
    /// Lines of the ring file that did not parse. A last line cut off by a kill is expected: it is
    /// counted, not hidden.
    pub malformed_lines: usize,
    /// `None` when the platform cannot say.
    pub alive: Option<bool>,
    /// Set when a file of this process could not be read or parsed, with why.
    pub read_problem: Option<String>,
}

impl ProcessTrace {
    pub fn verdict(&self) -> Verdict {
        let exit = self.entries.iter().rev().find(|e| e.kind == Kind::Event && e.op == OP_EXIT);
        let open = self.inflight.as_ref().is_some_and(|f| !f.spans.is_empty());
        match (open, self.alive, exit) {
            (true, Some(false), _) => Verdict::GoneWithSpansOpen,
            (true, Some(true), _) => Verdict::RunningWithSpansOpen,
            (true, None, _) => Verdict::SpansOpenLivenessUnknown,
            (false, _, Some(e)) => Verdict::ExitedCleanly {
                code: e.fields.iter().find_map(|(k, v)| if k == "code" { if let Val::I64(c) = v { Some(*c) } else { None } } else { None }).unwrap_or(0),
            },
            (false, Some(false), None) => Verdict::GoneWithoutExit,
            (false, _, None) => Verdict::RunningIdle,
        }
    }
}

/// Whether process `pid` exists. `None` where this cannot be asked (Windows, today).
pub fn pid_alive(pid: u32) -> Option<bool> {
    #[cfg(unix)]
    {
        // SAFETY: kill with signal 0 only checks that the process exists and may be signalled.
        let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if r == 0 {
            return Some(true);
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Some(false),
            Some(libc::EPERM) => Some(true),
            _ => None,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

/// Read every process's files from `dir`, oldest pid-file first. `alive` says whether a pid is
/// running (pass [`pid_alive`]); it is a parameter so the verdicts are testable. A missing
/// directory is an empty report, not an error.
pub fn load(dir: &Path, alive: &dyn Fn(u32) -> Option<bool>) -> std::io::Result<Vec<ProcessTrace>> {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut pids = std::collections::BTreeSet::new();
    for entry in read.flatten() {
        if let Some(pid) = pid_of_file_name(&entry.file_name().to_string_lossy()) {
            pids.insert(pid);
        }
    }
    let mut out = Vec::new();
    for pid in pids {
        let mut problem = None;
        let inflight = match fs::read(inflight_path(dir, pid)) {
            Ok(bytes) => match serde_json::from_slice::<InflightFile>(&bytes) {
                Ok(f) => Some(f),
                Err(e) => {
                    problem = Some(format!("{} did not parse: {e}", inflight_path(dir, pid).display()));
                    None
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                problem = Some(format!("{} could not be read: {e}", inflight_path(dir, pid).display()));
                None
            }
        };
        let mut entries = Vec::new();
        let mut malformed = 0;
        match fs::read_to_string(ring_path(dir, pid)) {
            Ok(text) => {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    match serde_json::from_str::<Entry>(line) {
                        Ok(e) => entries.push(e),
                        Err(_) => malformed += 1,
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => problem = Some(format!("{} could not be read: {e}", ring_path(dir, pid).display())),
        }
        entries.sort_by_key(|e| e.seq);
        out.push(ProcessTrace { pid, inflight, entries, malformed_lines: malformed, alive: alive(pid), read_problem: problem });
    }
    Ok(out)
}

fn fields_text(fields: &[(String, Val)]) -> String {
    fields.iter().map(|(k, v)| format!("{k}={}", v.display())).collect::<Vec<_>>().join(" ")
}

fn age_text(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

/// Collapse runs of consecutive successful leaf spans of the same subsystem and operation into one
/// line with a count. A failed one is never merged into a run.
fn aggregated(entries: &[Entry]) -> Vec<(Entry, u32, u64)> {
    let mut out: Vec<(Entry, u32, u64)> = Vec::new();
    for e in entries {
        if let Some((last, n, total)) = out.last_mut() {
            let mergeable = |x: &Entry| x.kind == Kind::Span && x.class == Class::Leaf && x.outcome == Some(Outcome::Ok);
            if mergeable(last) && mergeable(e) && last.subsystem == e.subsystem && last.op == e.op && last.thread == e.thread {
                *n += 1;
                *total += e.dur_us;
                continue;
            }
        }
        out.push((e.clone(), 1, e.dur_us));
    }
    out
}

/// The report text. `now_ms` is the current unix time in milliseconds; `last_n` is how many of each
/// process's most recent (aggregated) entries to show.
pub fn render(traces: &[ProcessTrace], now_ms: u64, last_n: usize) -> String {
    if traces.is_empty() {
        return "no diagnostics files found\n".to_string();
    }
    let mut out = String::new();
    for t in traces {
        // The process's role: from its in-flight file if it has one, else from its `start` event (the entry's own
        // subsystem is `diag`, which says nothing), else the first entry's subsystem.
        let start_role = t.entries.iter().find(|e| e.kind == Kind::Event && e.op == OP_START).and_then(|e| {
            e.fields.iter().find_map(|(k, v)| match (k.as_str(), v) {
                ("role", Val::Text(r)) => Some(r.as_str()),
                _ => None,
            })
        });
        let role = t.inflight.as_ref().map(|f| f.role.as_str()).or(start_role).or_else(|| t.entries.first().map(|e| e.subsystem.as_str())).unwrap_or("?");
        out.push_str(&format!("pid {} ({role}): {} entr{}", t.pid, t.entries.len(), if t.entries.len() == 1 { "y" } else { "ies" }));
        if t.malformed_lines > 0 {
            out.push_str(&format!(", {} unreadable line(s) (a line cut off by a kill is normal)", t.malformed_lines));
        }
        out.push('\n');
        if let Some(p) = &t.read_problem {
            out.push_str(&format!("  problem: {p}\n"));
        }
        let verdict = match t.verdict() {
            Verdict::ExitedCleanly { code } => format!("exited cleanly (code {code})"),
            Verdict::GoneWithSpansOpen => "the process is gone and spans were still open: it was killed or crashed".to_string(),
            Verdict::RunningWithSpansOpen => "the process is still running with spans open: hung, or just busy".to_string(),
            Verdict::GoneWithoutExit => "the process is gone, nothing was open and no exit was recorded: killed between operations".to_string(),
            Verdict::RunningIdle => "the process is running (or cannot be checked) and nothing is open".to_string(),
            Verdict::SpansOpenLivenessUnknown => "spans are open; this platform cannot say whether the process is still running".to_string(),
        };
        out.push_str(&format!("  state: {verdict}\n"));
        if let Some(f) = &t.inflight {
            if !f.spans.is_empty() {
                // By start order, which is always right. `depth` (the indent) counts the spans open on the
                // same OS thread, so for async code, where one operation's spans can run on different
                // threads, it is a hint and the order and the "began ... ago" times are the facts.
                out.push_str("  in flight (oldest first):\n");
                let mut spans = f.spans.clone();
                spans.sort_by_key(|s| s.seq);
                for s in &spans {
                    out.push_str(&format!(
                        "    {}{} {} {}  [began {} ago, thread {}]\n",
                        "  ".repeat(s.depth as usize),
                        s.subsystem.as_str(),
                        s.op,
                        fields_text(&s.fields),
                        age_text(now_ms.saturating_sub(s.ts_ms)),
                        s.thread
                    ));
                }
            }
        }
        let agg = aggregated(&t.entries);
        let skipped = agg.len().saturating_sub(last_n);
        if skipped > 0 {
            out.push_str(&format!("  last {last_n} of {} entries (aggregated):\n", agg.len()));
        } else {
            out.push_str("  entries (aggregated):\n");
        }
        for (e, n, total) in agg.iter().skip(skipped) {
            let outcome = e.outcome.map(|o| o.as_str()).unwrap_or("event");
            let count = if *n > 1 { format!(" x{n} ({} total)", age_text(total / 1000)) } else { format!(" ({})", age_text(e.dur_us / 1000)) };
            // A collapsed run shows its first entry's fields, and says so.
            let first = if *n > 1 { "first: " } else { "" };
            out.push_str(&format!("    #{} {} {} {outcome}{count} {first}{}\n", e.seq, e.subsystem.as_str(), e.op, fields_text(&e.fields)));
        }
    }
    out
}

#[cfg(test)]
mod tests;

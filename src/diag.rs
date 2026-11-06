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
//! - **Bounded ring of completed entries.** An in-memory ring (default `DEFAULT_RING_CAP`, 1024 entries) mirrored
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
//! [`Diag::write_failures`], is reported once on stderr per streak (outside the recorder's lock, and
//! never as a panic if stderr is gone), and the count is written into the in-flight file and the exit
//! entry so the report shows it.
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
const DEFAULT_RING_CAP: usize = 1024;
pub const MIN_RING_CAP: usize = 8;
/// How many processes' files are kept when a new process starts: the newest by pid-file mtime, but
/// never the files of a process that is still running (an idle GUI or MCP server must not lose its
/// files to the short-lived processes that come after it), so a running process can push the total
/// above this.
const KEEP_PROCESSES: usize = 20;
/// The version of the in-flight file format (and of the vocabulary both files use). A ring line is one
/// self-contained JSON object and carries no version: a line this build cannot understand is counted
/// separately from a cut line.
///
/// - 1: the first format. Text longer than [`TEXT_MAX_CHARS`] was cut and ended with an in-band
///   ` …[truncated]`; the subsystems `ptp_ip` and `settings` existed in the vocabulary (nothing ever
///   recorded under them).
/// - 2: a cut text is a [`Val::TruncatedText`] that says how long the original was, and `ptp_ip` and
///   `settings` are gone. A version-1 file still loads (the in-band mark is read as ordinary text);
///   a ring line or open span naming a removed subsystem is counted as one this build does not understand.
pub const SCHEMA: u32 = 2;
/// A process whose OS start time is later than the recorded start by more than this is not the
/// process that wrote the files: its pid was reused.
pub const PID_REUSE_SLACK_MS: u64 = 5_000;
/// Field keys of the exit entry, so the writer and the report read one spelling.
pub const FIELD_CODE: &str = "code";
/// Exit entry: how many spans begun on other threads were still open and were recorded abandoned.
pub const FIELD_CUT_OFF: &str = "cut_off";
pub const FIELD_WRITE_FAILURES: &str = "write_failures";
pub const FIELD_EVICTED: &str = "evicted";
/// Longest [`Val::Text`], in characters, before it is cut and becomes a [`Val::TruncatedText`].
pub const TEXT_MAX_CHARS: usize = 256;
/// The operation name of the entry [`Diag::shutdown`] records, and of the panic hook's.
pub const OP_EXIT: &str = "exit";
const OP_PANIC: &str = "panic";

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
    MaidSdk,
    Snapshot,
    Transplant,
    Restore,
    Firmware,
    Lock,
    Diag,
}

impl Subsystem {
    /// Whether this subsystem names what a whole process is (the `role` of a recorder). The exit entry
    /// is recorded under the role, so an entry named `exit` by any other subsystem is not an exit.
    pub const fn is_role(self) -> bool {
        matches!(self, Subsystem::Cli | Subsystem::Gui | Subsystem::Mcp)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Subsystem::Cli => "cli",
            Subsystem::Gui => "gui",
            Subsystem::Mcp => "mcp",
            Subsystem::Usb => "usb",
            Subsystem::MaidSdk => "maid_sdk",
            Subsystem::Snapshot => "snapshot",
            Subsystem::Transplant => "transplant",
            Subsystem::Restore => "restore",
            Subsystem::Firmware => "firmware",
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
    /// A short identifier (a model name, a serial, a path), at most [`TEXT_MAX_CHARS`] characters. A longer
    /// one is a [`Val::TruncatedText`]: see [`Val::text`].
    Text(String),
    /// A text that was longer than [`TEXT_MAX_CHARS`]: its first [`TEXT_MAX_CHARS`] characters, and how
    /// many characters the original had. A typed marker, so it cannot be mistaken for a text that really
    /// ends in a marker-looking string.
    TruncatedText { text: String, original_chars: u64 },
}

impl Val {
    /// A text value: [`Val::Text`] when it fits in [`TEXT_MAX_CHARS`] characters, else a
    /// [`Val::TruncatedText`] holding the first [`TEXT_MAX_CHARS`] and the original length, so a reader
    /// can tell a cut value from a short one without reading the text.
    pub fn text(s: &str) -> Val {
        let original_chars = s.chars().count();
        if original_chars <= TEXT_MAX_CHARS {
            Val::Text(s.to_string())
        } else {
            Val::TruncatedText { text: s.chars().take(TEXT_MAX_CHARS).collect(), original_chars: original_chars as u64 }
        }
    }

    /// The text of a [`Val::Text`] or [`Val::TruncatedText`] (the kept part), else `None`.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Val::Text(s) | Val::TruncatedText { text: s, .. } => Some(s),
            _ => None,
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
            Val::TruncatedText { text, original_chars } => format!("{text:?}[cut, {original_chars} chars in all]"),
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
    /// When it began, on the monotonic clock: what a duration is measured from when [`Diag::shutdown`]
    /// closes the span (the wall-clock `ts_ms` can step). Not written to the file; `None` for a span
    /// read back from one.
    #[serde(skip)]
    started: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InflightFile {
    /// The format version ([`SCHEMA`]); a file without one is version 1.
    #[serde(default = "default_schema")]
    pub schema: u32,
    pub pid: u32,
    pub role: Subsystem,
    pub process_started_ms: u64,
    pub spans: Vec<OpenSpan>,
    /// Diagnostics writes that had failed when this was written ([`Diag::write_failures`]).
    #[serde(default)]
    pub write_failures: u64,
    /// Completed entries pushed out of the ring when this was written.
    #[serde(default)]
    pub evicted: u64,
}

fn default_schema() -> u32 {
    1
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

/// Create `dir` owner-only. A directory that already exists keeps its mode: it may be one the user
/// supplied in [`DIR_ENV`] and shares for other purposes, or one this user does not own.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    let existed = dir.is_dir();
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    if !existed {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = existed;
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
    /// `None` only after a compaction replaced the file and the new one could not be reopened: the
    /// next entry tries again, and until then entries are counted as failed writes, not appended to
    /// a handle that names a file nobody will read.
    ring_file: Option<fs::File>,
    trace_file: Option<fs::File>,
    /// Set by the first [`Diag::shutdown`], under this lock, so a second caller (the teardown watchdog
    /// against `main`) waits for the first to finish writing instead of returning at once, and so a
    /// span begun after the exit does not bring the in-flight file back.
    exited: bool,
    /// Completed entries pushed out of the ring.
    evicted: u64,
}

pub struct Diag {
    cfg: Config,
    started_ms: u64,
    seq: AtomicU64,
    failures: AtomicU64,
    /// `end`/`set` calls whose span was no longer open: one that ended after [`Diag::shutdown`] closed it.
    unmatched: AtomicU64,
    /// True while the last write failed, so one streak warns once.
    in_failure_streak: AtomicBool,
    /// The warning for a new failure streak, printed by the entry point that caused it once it has
    /// released the state lock (a stderr that blocks must not hold the recorder).
    pending_warning: Mutex<Option<String>>,
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
    /// file, and removes the oldest finished processes' files beyond [`KEEP_PROCESSES`]. An `Err`
    /// means diagnostics could not be set up; nothing was left half-open.
    pub fn new(cfg: Config) -> std::io::Result<Diag> {
        private_dir(&cfg.dir)?;
        // A pid is reused. Whatever this pid's earlier process left behind is not this process's, and
        // the ring below is truncated for the same reason: an in-flight file kept from before would be
        // read as this process's hung state.
        match fs::remove_file(inflight_path(&cfg.dir, cfg.pid)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
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
            unmatched: AtomicU64::new(0),
            in_failure_streak: AtomicBool::new(false),
            pending_warning: Mutex::new(None),
            state: Mutex::new(State {
                ring: VecDeque::new(),
                open: Vec::new(),
                ring_lines: 0,
                ring_file: Some(ring_file),
                trace_file,
                exited: false,
                evicted: 0,
            }),
            cfg,
        })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// How many diagnostics writes have failed. Written into the in-flight file and the exit entry, so
    /// the report shows it; never an operation error.
    pub fn write_failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// How many `end`/`set` calls found their span already closed (by [`Diag::shutdown`]).
    pub fn unmatched_ops(&self) -> u64 {
        self.unmatched.load(Ordering::Relaxed)
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
                    let w = format!("warning: diagnostics could not be written to {} ({e}); the operation is not affected", self.cfg.dir.display());
                    *self.pending_warning.lock().unwrap_or_else(|e| e.into_inner()) = Some(w);
                }
            }
        }
    }

    /// Print the warning [`Diag::note_write`] queued. Called with the state lock released. A stderr that
    /// is closed is not an error here, and must not panic (`eprintln!` would).
    fn flush_warning(&self) {
        let w = self.pending_warning.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(w) = w {
            let _ = writeln!(std::io::stderr(), "{w}");
        }
    }

    /// Begin a span. `fields` are the values known now; more can be added when it finishes.
    pub fn span(&self, subsystem: Subsystem, op: &str, class: Class, fields: &[(&'static str, Val)]) -> Span<'_> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let thread = thread_num();
        let started = Instant::now();
        {
            let mut st = self.lock();
            let depth = st.open.iter().filter(|o| o.thread == thread).count() as u32;
            st.open.push(OpenSpan {
                seq,
                ts_ms: unix_ms(),
                class,
                subsystem,
                op: op.to_string(),
                fields: own_fields(fields),
                depth,
                thread,
                started: Some(started),
            });
            self.write_inflight(&st);
        }
        self.flush_warning();
        Span { diag: Some(self), seq, started }
    }

    /// Record something that happened, with no duration.
    pub fn event(&self, subsystem: Subsystem, op: &str, fields: &[(&'static str, Val)]) {
        {
            let mut st = self.lock();
            self.record_event(&mut st, subsystem, op, fields);
        }
        self.flush_warning();
    }

    /// [`Diag::event`] for the panic hook: never waits for the lock. A panic on a thread that already
    /// holds it (a write that panicked mid-record) would otherwise wait for itself. `false` (counted as
    /// a failed write) when the event could not be recorded.
    fn try_event(&self, subsystem: Subsystem, op: &str, fields: &[(&'static str, Val)]) -> bool {
        let mut st = match self.state.try_lock() {
            Ok(g) => g,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                self.failures.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        };
        self.record_event(&mut st, subsystem, op, fields);
        true
    }

    fn record_event(&self, st: &mut State, subsystem: Subsystem, op: &str, fields: &[(&'static str, Val)]) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let thread = thread_num();
        let depth = st.open.iter().filter(|o| o.thread == thread).count() as u32;
        let entry = Entry {
            seq,
            ts_ms: unix_ms(),
            kind: Kind::Event,
            class: Class::Top,
            subsystem,
            op: op.to_string(),
            fields: own_fields(fields),
            depth,
            thread,
            outcome: None,
            dur_us: 0,
        };
        self.push(st, entry);
    }

    /// Record that the process is exiting with `code`, the entry that tells the report this was a
    /// planned exit and not a kill. Spans still open are closed first: this is called on the paths
    /// that leave without unwinding (`process::exit`, `_exit`), where a command's own span never
    /// gets to finish.
    ///
    /// What a span ends as depends on **which thread calls this**, because the caller is the thread
    /// that is leaving:
    /// - a span begun on the calling thread is part of what is finishing: a [`Class::Top`] one ends
    ///   [`Outcome::Ok`] for code 0 and [`Outcome::Failed`] otherwise, a [`Class::Leaf`] one
    ///   [`Outcome::Abandoned`];
    /// - a span begun on **any other thread** is work the exit interrupts, whatever its class and
    ///   whatever the code: it ends [`Outcome::Abandoned`], and the exit entry carries
    ///   [`FIELD_CUT_OFF`] with how many there were, so the report does not call the exit clean.
    ///
    /// So the teardown watchdog (its own thread) records the hung main thread's `disconnect_device`
    /// and command spans as abandoned, and a GUI's `on_exit` on the UI thread records its running
    /// `cli_child` spans (begun on worker threads) as abandoned and its own `gui` span as finished.
    ///
    /// Only the first call records; a later one returns once the first has finished writing (it takes
    /// the same lock), so a second route to the exit never gets ahead of the first's records. A
    /// process that hangs *before* this is reached keeps its spans open, which is the whole point.
    pub fn shutdown(&self, code: i32) {
        let thread = thread_num();
        {
            let mut st = self.lock();
            if st.exited {
                return;
            }
            st.exited = true;
            let open = std::mem::take(&mut st.open);
            let mut cut_off = 0u64;
            for o in open {
                let outcome = if o.thread != thread {
                    cut_off += 1;
                    Outcome::Abandoned
                } else {
                    match (o.class, code) {
                        (Class::Top, 0) => Outcome::Ok,
                        (Class::Top, _) => Outcome::Failed,
                        (Class::Leaf, _) => Outcome::Abandoned,
                    }
                };
                // Monotonic, like every other span's duration: a wall-clock step must not make it negative or huge.
                let dur_us = o.started.map_or(0, |s| s.elapsed().as_micros() as u64);
                self.finish_open(&mut st, o, outcome, &[], dur_us);
            }
            self.write_inflight(&st);
            let mut fields = vec![(FIELD_CODE.to_string(), Val::I64(i64::from(code)))];
            if cut_off > 0 {
                fields.push((FIELD_CUT_OFF.to_string(), Val::U64(cut_off)));
            }
            let failures = self.failures.load(Ordering::Relaxed);
            if failures > 0 {
                fields.push((FIELD_WRITE_FAILURES.to_string(), Val::U64(failures)));
            }
            if st.evicted > 0 {
                fields.push((FIELD_EVICTED.to_string(), Val::U64(st.evicted)));
            }
            let entry = Entry {
                seq: self.seq.fetch_add(1, Ordering::Relaxed),
                ts_ms: unix_ms(),
                kind: Kind::Event,
                class: Class::Top,
                subsystem: self.cfg.role,
                op: OP_EXIT.to_string(),
                fields,
                depth: 0,
                thread,
                outcome: None,
                dur_us: 0,
            };
            self.push(&mut st, entry);
        }
        self.flush_warning();
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
        if st.exited {
            // A span begun after the exit (a thread that outlived it): the exit entry is the record, and
            // a file listing this span would make a finished process read as hung.
            return;
        }
        let file = InflightFile {
            schema: SCHEMA,
            pid: self.cfg.pid,
            role: self.cfg.role,
            process_started_ms: self.started_ms,
            spans: st.open.clone(),
            write_failures: self.failures.load(Ordering::Relaxed),
            evicted: st.evicted,
        };
        let r = match serde_json::to_vec(&file) {
            Ok(bytes) => write_private_atomic(&path, &bytes),
            Err(e) => Err(std::io::Error::other(e)),
        };
        if r.is_err() {
            // The file still on disk lists the state before this change, which may include spans that have
            // since ended: it would read as a hang that is not one. Removing it says "nothing recorded"
            // instead of something false (best effort; the failure is counted and warned about either way).
            let _ = fs::remove_file(&path);
        }
        self.note_write(r);
    }

    /// Append `line` to the ring file, reopening it first if a failed compaction left no handle.
    fn ring_append(&self, st: &mut State, line: &str) -> std::io::Result<()> {
        if st.ring_file.is_none() {
            st.ring_file = Some(private_open(&ring_path(&self.cfg.dir, self.cfg.pid), true)?);
        }
        st.ring_lines += 1;
        match st.ring_file.as_mut() {
            Some(f) => f.write_all(line.as_bytes()),
            None => Err(std::io::Error::other("the ring file is not open")),
        }
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
            st.evicted += 1;
        }
        st.ring.push_back(entry.clone());

        // One `write` per line (the newline is part of it): a trace file several processes append to
        // would otherwise see their halves interleaved.
        let mut line = match serde_json::to_string(&entry) {
            Ok(l) => l,
            Err(e) => {
                self.note_write(Err(std::io::Error::other(e)));
                return;
            }
        };
        line.push('\n');
        let ring_result = self.ring_append(st, &line);
        let trace_result = st.trace_file.as_mut().map(|f| f.write_all(line.as_bytes())).unwrap_or(Ok(()));
        match (ring_result, trace_result) {
            (Ok(()), Ok(())) => self.note_write(Ok(())),
            (a, b) => {
                // Both failures count; neither hides the other.
                for e in [a, b].into_iter().filter_map(Result::err) {
                    self.note_write(Err(e));
                }
            }
        }
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
        st.ring_lines = st.ring.len(); // restarts either way; a failure retries at the next threshold
        if let Err(e) = write_private_atomic(&path, out.as_bytes()) {
            // Nothing was replaced (the temp file or the rename failed): the old handle still names the file
            // a reader will open, so appending to it stays right.
            self.note_write(Err(e));
            return;
        }
        match private_open(&path, true) {
            Ok(f) => {
                st.ring_file = Some(f);
                self.note_write(Ok(()));
            }
            Err(e) => {
                // The file was replaced and the old handle now names an unlinked inode: appends to it would
                // "succeed" and be lost. Drop it; the next entry reopens, and until then writes fail visibly.
                st.ring_file = None;
                self.note_write(Err(e));
            }
        }
    }

    fn end(&self, seq: u64, outcome: Outcome, extra: &[(&'static str, Val)], dur_us: u64) {
        {
            let mut st = self.lock();
            let Some(pos) = st.open.iter().position(|o| o.seq == seq) else {
                self.unmatched.fetch_add(1, Ordering::Relaxed);
                return;
            };
            let open = st.open.remove(pos);
            self.finish_open(&mut st, open, outcome, extra, dur_us);
            self.write_inflight(&st);
        }
        self.flush_warning();
    }

    /// Turn an open span (already removed from `st.open`) into a completed ring entry.
    fn finish_open(&self, st: &mut State, open: OpenSpan, outcome: Outcome, extra: &[(&'static str, Val)], dur_us: u64) {
        let mut fields = open.fields;
        for (k, v) in extra {
            put_field(&mut fields, k, v.clone());
        }
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
        {
            let mut st = self.lock();
            if let Some(o) = st.open.iter_mut().find(|o| o.seq == seq) {
                put_field(&mut o.fields, key, v);
                self.write_inflight(&st);
            } else {
                self.unmatched.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.flush_warning();
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

/// Set `key` to `v`: replace the value if the key is there, else append. The one place fields are added,
/// so a key given at the start and again at the finish (or set mid-flight) is one field with the latest
/// value, not two entries of which a reader finds the first.
fn put_field(fields: &mut Vec<(String, Val)>, key: &str, v: Val) {
    match fields.iter_mut().find(|(k, _)| k == key) {
        Some(slot) => slot.1 = v,
        None => fields.push((key.to_string(), v)),
    }
}

fn own_fields(fields: &[(&'static str, Val)]) -> Vec<(String, Val)> {
    let mut out = Vec::with_capacity(fields.len());
    for (k, v) in fields {
        put_field(&mut out, k, v.clone());
    }
    out
}

/// Whether the process that wrote `pid`'s files is still running: its pid exists and is not a later
/// process that reused the number (judged by the start time in its in-flight file).
fn live_process(dir: &Path, pid: u32) -> bool {
    if pid_alive(pid) != Some(true) {
        return false;
    }
    let recorded = fs::read(inflight_path(dir, pid))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("process_started_ms").and_then(serde_json::Value::as_u64));
    match (recorded, process_start_ms(pid)) {
        (Some(recorded), Some(os)) => !pid_recycled(os, recorded),
        _ => true,
    }
}

/// Remove the oldest finished processes' files beyond [`KEEP_PROCESSES`], never this process's and never
/// those of a process that is still running. Best effort: a file that cannot be removed is left, and
/// nothing about that is an error for the caller.
fn prune(dir: &Path, own_pid: u32) {
    prune_with(dir, own_pid, KEEP_PROCESSES, &|pid| live_process(dir, pid));
}

/// [`prune`] with the count and the liveness test given, so the boundary and the live-process case can be tested.
fn prune_with(dir: &Path, own_pid: u32, keep: usize, is_live: &dyn Fn(u32) -> bool) {
    let Ok(read) = fs::read_dir(dir) else { return };
    let mut pids: std::collections::BTreeMap<u32, std::time::SystemTime> = std::collections::BTreeMap::new();
    let mut temps: Vec<(PathBuf, u32)> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(pid) = pid_of_file_name(&name) {
            let m = entry.metadata().and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH);
            let slot = pids.entry(pid).or_insert(m);
            if m > *slot {
                *slot = m;
            }
        } else if let Some(owner) = temp_owner_pid(&name) {
            temps.push((entry.path(), owner));
        }
    }
    // A temp file whose writer died between creating and renaming it is never read; its writer, if alive, is mid-write.
    for (path, owner) in temps {
        if owner != own_pid && !is_live(owner) {
            let _ = fs::remove_file(path);
        }
    }
    if pids.len() <= keep {
        return;
    }
    let excess = pids.len() - keep;
    let mut removable: Vec<(u32, std::time::SystemTime)> = pids.into_iter().filter(|(p, _)| *p != own_pid && !is_live(*p)).collect();
    removable.sort_by_key(|(_, m)| *m);
    for (pid, _) in removable.into_iter().take(excess) {
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

/// The writer's pid in a temp file name `.<target file name>.<writer pid>.<n>.tmp` (what
/// [`write_private_atomic`] makes), when `<target file name>` is one of ours.
fn temp_owner_pid(name: &str) -> Option<u32> {
    let body = name.strip_prefix('.')?.strip_suffix(".tmp")?;
    let mut parts = body.rsplitn(3, '.');
    let _n: u64 = parts.next()?.parse().ok()?;
    let owner: u32 = parts.next()?.parse().ok()?;
    pid_of_file_name(parts.next()?)?;
    Some(owner)
}

// ─────────────────────────────────────────────────────────────────────────
// Spans
// ─────────────────────────────────────────────────────────────────────────

/// A span that has begun. Finish it with [`Span::finish`] or [`Span::finish_with`]; dropped without
/// either it ends as [`Outcome::Abandoned`], or [`Outcome::Panicked`] when the thread is unwinding.
#[must_use = "a span records its outcome when finished or dropped; binding it to _ ends it at once"]
pub struct Span<'a> {
    /// `None` for a span that records nothing, and once it has ended (taken by whoever ends it).
    diag: Option<&'a Diag>,
    seq: u64,
    started: Instant,
}

impl std::fmt::Debug for Span<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Span").field("seq", &self.seq).field("recording", &self.diag.is_some()).finish()
    }
}

impl Span<'static> {
    /// A span that records nothing: what the code uses when diagnostics are not running.
    pub fn none() -> Span<'static> {
        Span { diag: None, seq: 0, started: Instant::now() }
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
        if let Some(d) = self.diag.take() {
            d.end(self.seq, outcome, extra, self.started.elapsed().as_micros() as u64);
        }
    }
}

impl Drop for Span<'_> {
    fn drop(&mut self) {
        // Already ended (finish_with took it), or a span that records nothing.
        if let Some(d) = self.diag.take() {
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
    if let Some(id) = std::env::var_os(REQUEST_ID_ENV).filter(|v| !v.is_empty()) {
        // Lossy, not dropped: a non-UTF-8 id still joins the two sides' records if both write it the same way.
        fields.push(("request_id", Val::text(&id.to_string_lossy())));
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

/// Install a panic hook that records the panic (its location, and the message only as a length and a
/// hash: an `unwrap` on an `Err` carries the error's `Debug` text, which can embed a path, URL or payload)
/// before the previous hook runs, which still prints the message for the user. The hook never waits for
/// the recorder's lock ([`Diag::try_event`]): a panic on a thread that holds it would wait for itself.
/// With `panic = "abort"` the open spans are never dropped, so the in-flight file keeps them and this
/// entry says why. Safe to call more than once (only the first call installs).
fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(install_panic_hook_once);
}

/// The fields of a panic entry: where it happened, and the message only as a length and a hash.
fn panic_fields(location: &str, message: &str) -> Vec<(&'static str, Val)> {
    vec![("location", Val::text(location)), ("message_len", Val::len(message.len())), ("message_hash", Val::hash(message.as_bytes()))]
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
            d.try_event(d.cfg.role, OP_PANIC, &panic_fields(&location, &message));
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
    /// It recorded an exit with a code and has nothing in flight.
    ExitedCleanly { code: i64 },
    /// It recorded an exit, but `cut_off` spans begun on other threads were still open and the exit
    /// interrupted them (recorded `abandoned`): the teardown watchdog ending a hung camera session, or a
    /// GUI quitting with a child still running. Not a clean exit.
    ExitedWithWorkCutOff { code: i64, cut_off: u64 },
    /// An exit entry is there but carries no usable code.
    ExitCodeUnknown,
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
    /// An in-flight file exists and could not be read at all, so what was open is unknown. Says nothing
    /// about the process's state; `read_problems` says why.
    InflightUnreadable,
}

#[derive(Debug, Clone)]
pub struct ProcessTrace {
    pub pid: u32,
    pub inflight: Option<InflightFile>,
    pub entries: Vec<Entry>,
    /// Lines of the ring file that are not JSON. A last line cut off by a kill is expected: it is
    /// counted, not hidden.
    pub malformed_lines: usize,
    /// Lines that are JSON but not an entry this build understands (a value or field another build
    /// added): counted apart from a cut line, because the cause is different.
    pub unrecognised_lines: usize,
    /// `None` when the platform cannot say. Already `Some(false)` when the pid now belongs to a later process.
    pub alive: Option<bool>,
    /// The pid is running, but it started after this process's files were written: a different process.
    pub pid_recycled: bool,
    /// Everything that could not be read or understood, each with why. All are kept.
    pub read_problems: Vec<String>,
    /// Open spans in the in-flight file that this build could not read (they count as open).
    pub unreadable_open_spans: usize,
    /// The in-flight file exists and nothing of it could be read.
    pub inflight_unusable: bool,
}

/// An exit entry: recorded under the process's role, named [`OP_EXIT`]. Another subsystem's event that
/// happens to be called `exit` is not one.
fn is_exit(e: &Entry) -> bool {
    e.kind == Kind::Event && e.op == OP_EXIT && e.subsystem.is_role()
}

fn field_of<'a>(fields: &'a [(String, Val)], key: &str) -> Option<&'a Val> {
    fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

impl ProcessTrace {
    pub fn verdict(&self) -> Verdict {
        if self.inflight_unusable {
            return Verdict::InflightUnreadable;
        }
        let exit = self.entries.iter().rev().find(|e| is_exit(e));
        let open = self.inflight.as_ref().is_some_and(|f| !f.spans.is_empty()) || self.unreadable_open_spans > 0;
        match (open, self.alive, exit) {
            (true, Some(false), _) => Verdict::GoneWithSpansOpen,
            (true, Some(true), _) => Verdict::RunningWithSpansOpen,
            (true, None, _) => Verdict::SpansOpenLivenessUnknown,
            (false, _, Some(e)) => match field_of(&e.fields, FIELD_CODE) {
                Some(Val::I64(code)) => match field_of(&e.fields, FIELD_CUT_OFF) {
                    Some(Val::U64(n)) if *n > 0 => Verdict::ExitedWithWorkCutOff { code: *code, cut_off: *n },
                    _ => Verdict::ExitedCleanly { code: *code },
                },
                _ => Verdict::ExitCodeUnknown,
            },
            (false, Some(false), None) => Verdict::GoneWithoutExit,
            (false, _, None) => Verdict::RunningIdle,
        }
    }

    /// When the process began, as well as the files say: its in-flight file's start, else its first entry; the
    /// earlier of the two when both exist.
    pub fn started_ms(&self) -> Option<u64> {
        let from_file = self.inflight.as_ref().map(|f| f.process_started_ms).filter(|ms| *ms > 0);
        let from_entries = self.entries.first().map(|e| e.ts_ms);
        match (from_file, from_entries) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Diagnostics writes that failed, from the in-flight file or the exit entry (whichever saw more).
    pub fn write_failures(&self) -> u64 {
        self.counter(FIELD_WRITE_FAILURES, self.inflight.as_ref().map_or(0, |f| f.write_failures))
    }

    /// Completed entries pushed out of the ring, from the in-flight file or the exit entry.
    pub fn evicted(&self) -> u64 {
        self.counter(FIELD_EVICTED, self.inflight.as_ref().map_or(0, |f| f.evicted))
    }

    fn counter(&self, key: &str, from_file: u64) -> u64 {
        let from_exit = self
            .entries
            .iter()
            .rev()
            .find(|e| is_exit(e))
            .and_then(|e| match field_of(&e.fields, key) {
                Some(Val::U64(n)) => Some(*n),
                _ => None,
            })
            .unwrap_or(0);
        from_file.max(from_exit)
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

/// When process `pid` started, in unix milliseconds, where the OS can say (macOS today; `None`
/// elsewhere, and for a pid that does not exist). Used with [`pid_recycled`] to tell a running process
/// from a later one that reused its pid.
pub fn process_start_ms(pid: u32) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: proc_pidinfo fills at most `size` bytes of the zeroed struct we pass.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast::<libc::c_void>(), size) };
        if n != size {
            return None;
        }
        Some(info.pbi_start_tvsec * 1000 + info.pbi_start_tvusec / 1000)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        None
    }
}

/// Whether a running process that the OS says started at `os_start_ms` is not the one that recorded
/// `recorded_ms` as its start: it started later than that by more than [`PID_REUSE_SLACK_MS`]. (A process
/// always exists before it starts recording, so an OS start *earlier* than the record is never a mismatch.)
pub fn pid_recycled(os_start_ms: u64, recorded_ms: u64) -> bool {
    os_start_ms > recorded_ms.saturating_add(PID_REUSE_SLACK_MS)
}

/// What reading `inflight-<pid>.json` found.
struct InflightRead {
    file: Option<InflightFile>,
    unreadable_spans: usize,
    unusable: bool,
    problems: Vec<String>,
}

fn read_inflight(dir: &Path, pid: u32) -> InflightRead {
    let path = inflight_path(dir, pid);
    let mut r = InflightRead { file: None, unreadable_spans: 0, unusable: false, problems: Vec::new() };
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return r,
        Err(e) => {
            r.unusable = true;
            r.problems.push(format!("{} could not be read: {e}", path.display()));
            return r;
        }
    };
    if let Ok(f) = serde_json::from_slice::<InflightFile>(&bytes) {
        if f.schema > SCHEMA {
            r.problems.push(format!("{} has schema {} and this build reads schema {SCHEMA}: it was written by a newer build", path.display(), f.schema));
        }
        r.file = Some(f);
        return r;
    }
    let value = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(v) => v,
        Err(e) => {
            r.unusable = true;
            r.problems.push(format!("{} did not parse: {e}", path.display()));
            return r;
        }
    };
    // Valid JSON that is not the format this build writes: another build's file. Keep every open span that can be
    // read, and count the rest as open, rather than losing the process's whole hung state to one new value.
    let Some(spans) = value.get("spans").and_then(serde_json::Value::as_array) else {
        r.unusable = true;
        r.problems.push(format!("{} is JSON but has no list of spans (a different format?)", path.display()));
        return r;
    };
    let mut ok = Vec::new();
    for s in spans {
        match serde_json::from_value::<OpenSpan>(s.clone()) {
            Ok(s) => ok.push(s),
            Err(_) => r.unreadable_spans += 1,
        }
    }
    let num = |k: &str| value.get(k).and_then(serde_json::Value::as_u64);
    let role = match value.get("role").map(|v| serde_json::from_value::<Subsystem>(v.clone())) {
        Some(Ok(role)) => role,
        _ => {
            r.problems.push(format!("{} names a role this build does not know (or none): shown as `diag`", path.display()));
            Subsystem::Diag
        }
    };
    r.problems.push(format!(
        "{} was not fully understood (written by a different build?): {} open span(s) could not be read",
        path.display(),
        r.unreadable_spans
    ));
    r.file = Some(InflightFile {
        schema: num("schema").map_or(1, |n| n as u32),
        pid: num("pid").map_or(pid, |n| n as u32),
        role,
        process_started_ms: num("process_started_ms").unwrap_or(0),
        spans: ok,
        write_failures: num("write_failures").unwrap_or(0),
        evicted: num("evicted").unwrap_or(0),
    });
    r
}

/// Read every process's files from `dir`, oldest process first (by when it started, then pid). `alive`
/// says whether a pid is running (pass [`pid_alive`]); it is a parameter so the verdicts are testable. A
/// running pid that the OS says started after the files were written is reported as gone: its pid was reused
/// ([`process_start_ms`], [`pid_recycled`]). A missing directory is an empty report, not an error.
pub fn load(dir: &Path, alive: &dyn Fn(u32) -> Option<bool>) -> std::io::Result<Vec<ProcessTrace>> {
    load_with(dir, alive, &process_start_ms)
}

/// [`load`] with the OS start-time lookup given, so reuse detection is testable with made-up pids.
pub fn load_with(dir: &Path, alive: &dyn Fn(u32) -> Option<bool>, start_of: &dyn Fn(u32) -> Option<u64>) -> std::io::Result<Vec<ProcessTrace>> {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let pids = pids_in(read.map(|e| e.map(|e| e.file_name())))
        .map_err(|e| std::io::Error::new(e.kind(), format!("{} could not be listed completely, so processes may be missing from a report: {e}", dir.display())))?;
    let mut out = Vec::new();
    for pid in pids {
        let inflight = read_inflight(dir, pid);
        let mut problems = inflight.problems;
        let mut entries = Vec::new();
        let (mut malformed, mut unrecognised) = (0, 0);
        match fs::read(ring_path(dir, pid)) {
            // Bytes, not a string: one line cut in the middle of a character must not make the whole file unreadable.
            Ok(bytes) => {
                for raw in bytes.split(|b| *b == b'\n') {
                    let line = String::from_utf8_lossy(raw);
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Entry>(&line) {
                        Ok(e) => entries.push(e),
                        Err(_) if serde_json::from_str::<serde_json::Value>(&line).is_ok() => unrecognised += 1,
                        Err(_) => malformed += 1,
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => problems.push(format!("{} could not be read: {e}", ring_path(dir, pid).display())),
        }
        entries.sort_by_key(|e| e.seq);
        let mut trace = ProcessTrace {
            pid,
            inflight: inflight.file,
            entries,
            malformed_lines: malformed,
            unrecognised_lines: unrecognised,
            alive: alive(pid),
            pid_recycled: false,
            read_problems: problems,
            unreadable_open_spans: inflight.unreadable_spans,
            inflight_unusable: inflight.unusable,
        };
        if trace.alive == Some(true) {
            if let (Some(recorded), Some(os)) = (trace.started_ms(), start_of(pid)) {
                if pid_recycled(os, recorded) {
                    trace.alive = Some(false);
                    trace.pid_recycled = true;
                }
            }
        }
        out.push(trace);
    }
    out.sort_by_key(|t| (t.started_ms().unwrap_or(0), t.pid));
    Ok(out)
}

/// The pids named by a directory listing's file names. A listing error is an error, not a skipped name:
/// a report that silently left a process out would read as "that process never ran".
fn pids_in(names: impl Iterator<Item = std::io::Result<OsString>>) -> std::io::Result<std::collections::BTreeSet<u32>> {
    let mut pids = std::collections::BTreeSet::new();
    for name in names {
        if let Some(pid) = pid_of_file_name(&name?.to_string_lossy()) {
            pids.insert(pid);
        }
    }
    Ok(pids)
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

/// One line of the report: an entry, or a run of consecutive successful leaf spans collapsed into it.
struct Run<'a> {
    first: &'a Entry,
    /// The newest member: the one nearest a hang, so its fields are shown too. Equal to `first` for a run of one.
    last: &'a Entry,
    count: u32,
    /// The members' durations summed.
    total_us: u64,
}

/// Collapse runs of consecutive successful leaf spans of the same subsystem and operation into one
/// line with a count. A failed one is never merged into a run.
fn aggregated(entries: &[Entry]) -> Vec<Run<'_>> {
    let mut out: Vec<Run<'_>> = Vec::new();
    for e in entries {
        if let Some(run) = out.last_mut() {
            let mergeable = |x: &Entry| x.kind == Kind::Span && x.class == Class::Leaf && x.outcome == Some(Outcome::Ok);
            if mergeable(run.last) && mergeable(e) && run.last.subsystem == e.subsystem && run.last.op == e.op && run.last.thread == e.thread {
                run.last = e;
                run.count += 1;
                run.total_us += e.dur_us;
                continue;
            }
        }
        out.push(Run { first: e, last: e, count: 1, total_us: e.dur_us });
    }
    out
}

/// One completed entry (or collapsed run) as a report line: its sequence number (the range for a run),
/// what it was, how it ended and how long it took, then when it began, on which thread, its class and
/// nesting depth, then its fields. A run shows the fields of its first and of its last member.
fn run_line(run: &Run<'_>, now_ms: u64) -> String {
    let (f, l) = (run.first, run.last);
    let outcome = f.outcome.map_or("event", Outcome::as_str);
    let seq = if run.count > 1 { format!("#{}..#{}", f.seq, l.seq) } else { format!("#{}", f.seq) };
    let took = if run.count > 1 { format!(" x{} ({} total)", run.count, age_text(run.total_us / 1000)) } else { format!(" ({})", age_text(f.dur_us / 1000)) };
    let class = match f.class {
        Class::Top => "top",
        Class::Leaf => "leaf",
    };
    let began = format!("began {} ago, thread {}, {class}, depth {}", age_text(now_ms.saturating_sub(f.ts_ms)), f.thread, f.depth);
    let fields = if run.count > 1 {
        format!("first: {} last: {}", fields_text(&f.fields), fields_text(&l.fields))
    } else {
        fields_text(&f.fields)
    };
    format!("    {seq} {} {} {outcome}{took} [{began}] {fields}\n", f.subsystem.as_str(), f.op)
}

/// The report text. `now_ms` is the current unix time in milliseconds; `last_n` is how many of each
/// process's most recent (aggregated) entries to show.
pub fn render(traces: &[ProcessTrace], now_ms: u64, last_n: usize) -> String {
    if traces.is_empty() {
        return "no diagnostics files found\n".to_string();
    }
    let mut out = String::new();
    for t in traces {
        // The process's role: from its `start` event (the entry's own subsystem is `diag`, which says nothing), else
        // its in-flight file, else the first entry's subsystem.
        let start_role = t.entries.iter().find(|e| e.kind == Kind::Event && e.op == OP_START).and_then(|e| {
            e.fields.iter().find_map(|(k, v)| if k == "role" { v.as_text() } else { None })
        });
        let role = start_role.or(t.inflight.as_ref().map(|f| f.role.as_str())).or_else(|| t.entries.first().map(|e| e.subsystem.as_str())).unwrap_or("?");
        out.push_str(&format!("pid {} ({role}): {} entr{}", t.pid, t.entries.len(), if t.entries.len() == 1 { "y" } else { "ies" }));
        if t.malformed_lines > 0 {
            out.push_str(&format!(", {} unreadable line(s) (a line cut off by a kill is normal)", t.malformed_lines));
        }
        if t.unrecognised_lines > 0 {
            out.push_str(&format!(", {} line(s) this build does not understand (written by a different build?)", t.unrecognised_lines));
        }
        out.push('\n');
        for p in &t.read_problems {
            out.push_str(&format!("  problem: {p}\n"));
        }
        let failures = t.write_failures();
        if failures > 0 {
            out.push_str(&format!("  warning: {failures} diagnostics write(s) failed in this process; this record may be incomplete\n"));
        }
        let verdict = match t.verdict() {
            Verdict::ExitedCleanly { code } => format!("exited cleanly (code {code})"),
            Verdict::ExitedWithWorkCutOff { code, cut_off } => format!(
                "exited (code {code}) but cut work short: {cut_off} span(s) on other threads were still open and are recorded abandoned (the teardown watchdog ending a hung session, or a quit with work running)"
            ),
            Verdict::ExitCodeUnknown => "an exit was recorded without a readable code".to_string(),
            Verdict::InflightUnreadable => "the in-flight file could not be read, so what was in flight is unknown (see the problem line)".to_string(),
            Verdict::GoneWithSpansOpen => "the process is gone and spans were still open: it was killed or crashed".to_string(),
            Verdict::RunningWithSpansOpen => "the process is still running with spans open: hung, or just busy".to_string(),
            Verdict::GoneWithoutExit => "the process is gone, nothing was open and no exit was recorded: killed between operations".to_string(),
            Verdict::RunningIdle => "the process is running (or cannot be checked) and nothing is open".to_string(),
            Verdict::SpansOpenLivenessUnknown => "spans are open; this platform cannot say whether the process is still running".to_string(),
        };
        out.push_str(&format!("  state: {verdict}\n"));
        if t.pid_recycled {
            out.push_str(&format!("  note: pid {} is running now, but it started after these files were written: it is a different process\n", t.pid));
        }
        if t.unreadable_open_spans > 0 {
            out.push_str(&format!("  note: {} open span(s) in the in-flight file could not be read and are not listed\n", t.unreadable_open_spans));
        }
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
        if t.evicted() > 0 {
            out.push_str(&format!("  note: {} older entries were evicted from the ring and are not shown\n", t.evicted()));
        }
        let agg = aggregated(&t.entries);
        let skipped = agg.len().saturating_sub(last_n);
        if skipped > 0 {
            out.push_str(&format!("  last {last_n} of {} retained entries (aggregated):\n", agg.len()));
        } else {
            out.push_str("  retained entries (aggregated):\n");
        }
        for run in agg.iter().skip(skipped) {
            out.push_str(&run_line(run, now_ms));
        }
    }
    out
}

#[cfg(test)]
mod tests;

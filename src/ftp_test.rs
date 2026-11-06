//! Trying an FTP or SFTP profile from this computer: does the server answer, accept the login, have the folder, and
//! take, give back and delete a small file?
//!
//! The steps, in order, each with its own outcome: look the host name up; open a connection; (SFTP) the SSH
//! handshake, which also yields the server's host key fingerprint; log in; go to the folder; and, if asked, put a
//! small uniquely-named file there, fetch it, compare it with what was sent, and delete it. A step that fails ends the
//! test, except that a file that was put (or that a failed upload may have left) is still deleted, and one that could
//! not be deleted is named in [`TestReport::file_left_behind`] so it can be removed by hand.
//!
//! What a failure says: the step, the address and port that were tried, the server's own words, and a hint in terms of
//! the camera's profile. It cannot tell a wrong host name from a server on a network this computer is not on; both are
//! "could not reach", and the hint says so. The server's reply code, when it sent one, is also a typed field
//! ([`StepOutcome::Failed::reply_code`]); what a code MEANS goes through one table ([`classify_reply`]), not through
//! the words of the reply.
//!
//! This tests the SERVER side only. Whether the camera can join the Wi-Fi network, and whether it behaves the same
//! (its passive-mode setting, its FTP dialect), is not tested here. Plain FTP only: the profile has no known FTPS
//! setting. The SFTP host key is not checked against anything (there is nothing to check it against): it is reported.
//!
//! Time limits: `step_timeout` applies to each step, and to each address tried while connecting, so a name with
//! several addresses can take that long for each one. For FTP it also applies to the control connection's reads and
//! writes and to the data connection (connecting to the passive address, and every read and write on it); the data
//! connection is made by a builder this module supplies, because the client library's default one has no limit.
//!
//! The password is scrubbed from the report: it is not stored, and any place it appears in text the server or the
//! client library produced (a server may echo it in a reply) is replaced by `<password hidden>` before the report is
//! returned. This removes the password as typed; a server that echoes it altered (re-cased, encoded) is not caught.
//!
//! Nothing here has been run against a real camera, a real FTP server or a real SFTP server; the tests use servers
//! written for them.

use std::io::{Cursor, Read};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;

pub use crate::ftp_edit::Protocol;

/// The most of the test file that is read back. The file is a couple of hundred bytes; a server that sends more has
/// not given back what was sent, and reading without a limit would let a server make this computer hold any amount.
const MAX_TEST_FILE_BYTES: u64 = 64 * 1024;

/// What is written over any copy of the password found in a report.
const PASSWORD_HIDDEN: &str = "<password hidden>";

impl TestRequest {
    /// Test a stored profile with its own settings (`ftp_edit::profile_view`). The one place that maps a profile's
    /// fields to a request, so the command line and the GUI cannot disagree about which field is the server's port.
    pub fn from_profile(v: &crate::ftp_edit::ProfileView, file_check: bool, step_timeout: Duration) -> TestRequest {
        TestRequest {
            host: v.host.clone(),
            port: v.port,
            protocol: v.protocol,
            username: v.username.clone(),
            password: v.password.clone(),
            folder: v.folder.clone(),
            file_check,
            step_timeout,
        }
    }
}

/// What to try. The password is used and never kept in a report.
#[derive(Clone)]
pub struct TestRequest {
    pub host: String,
    pub port: u16,
    pub protocol: Protocol,
    pub username: String,
    pub password: String,
    /// The folder to go to; empty for the login directory.
    pub folder: String,
    /// Also put, fetch, compare and delete a small file.
    pub file_check: bool,
    /// How long each step may take (and each address tried while connecting; see the module doc).
    pub step_timeout: Duration,
}

impl std::fmt::Debug for TestRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestRequest")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("protocol", &self.protocol)
            .field("username", &self.username)
            .field("password", &format_args!("<{} byte(s) hidden>", self.password.len()))
            .field("folder", &self.folder)
            .field("file_check", &self.file_check)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Resolve,
    Connect,
    /// SFTP: the SSH key exchange.
    Handshake,
    Login,
    Folder,
    Upload,
    Download,
    Compare,
    Delete,
}

impl Step {
    pub fn words(self) -> &'static str {
        match self {
            Step::Resolve => "look up the host",
            Step::Connect => "connect",
            Step::Handshake => "SSH handshake",
            Step::Login => "log in",
            Step::Folder => "go to the folder",
            Step::Upload => "put a test file",
            Step::Download => "fetch it back",
            Step::Compare => "compare it",
            Step::Delete => "delete it",
        }
    }

    /// Whether this step comes before the server has accepted a login. A connection that dies or sends rubbish up to
    /// here points at the wrong protocol or port; after it, at a network fault in the middle of a session.
    fn before_login_accepted(self) -> bool {
        matches!(self, Step::Resolve | Step::Connect | Step::Handshake | Step::Login)
    }
}

/// The steps a protocol's report lists, in order: the one place that says which steps there are. Everything that
/// skips "the rest" skips what this list says is not yet recorded, so a step added here is skipped everywhere it
/// should be without anyone counting positions.
fn plan(protocol: Protocol) -> &'static [Step] {
    match protocol {
        Protocol::Ftp => &[Step::Resolve, Step::Connect, Step::Login, Step::Folder, Step::Upload, Step::Download, Step::Compare, Step::Delete],
        Protocol::Sftp => &[Step::Resolve, Step::Connect, Step::Handshake, Step::Login, Step::Folder, Step::Upload, Step::Download, Step::Compare, Step::Delete],
    }
}

/// Why a step failed, as a closed vocabulary. Anything that branches on a failure uses this; the message is for a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailKind {
    /// The name does not resolve.
    NameNotFound,
    /// The address answered and refused the connection.
    ConnectionRefused,
    /// No route, or the network is unreachable.
    Unreachable,
    TimedOut,
    /// Something answered but it did not speak the protocol.
    NotThisProtocol,
    /// The SSH key exchange failed.
    HandshakeFailed,
    LoginRefused,
    /// SFTP only: the server's own list of login methods does not include a password, which is the only way this
    /// test signs in. The message names what the server does offer.
    LoginMethodNotOffered,
    FolderMissing,
    PermissionDenied,
    /// The server said it cannot serve this connection now (FTP 421: too many users, shutting down, ...).
    ServerUnavailable,
    /// The transfer or the delete failed for another reason.
    TransferFailed,
    /// What came back is not what was sent.
    DataMismatch,
    /// None of the above; also what a reply this program has no entry for becomes. The message has the server's words.
    Other,
}

impl FailKind {
    /// What to try, in terms of the camera's profile.
    pub fn hint(self) -> &'static str {
        match self {
            FailKind::NameNotFound => "This computer could not find that host name. Check the spelling (and that this computer is online); if it is a name only the camera's network knows, try its IP address.",
            FailKind::ConnectionRefused => "The address answered but nothing accepted the connection on that port. Check the port (21 for FTP, 22 for SFTP is usual) and that the server is running.",
            FailKind::Unreachable => "There is no route from this computer to that address. If the camera will be on another network, this computer may simply not be on it; that is not necessarily a mistake in the profile.",
            FailKind::TimedOut => "Nothing answered in time. A wrong address, a firewall, or a network this computer is not on all look like this; it cannot tell them apart.",
            FailKind::NotThisProtocol => "Something answered but it is not an FTP/SFTP server of the kind chosen. Check the protocol and port.",
            FailKind::HandshakeFailed => "The SSH handshake failed. The server may require key types or ciphers this test does not offer; the message says what it reported.",
            FailKind::LoginRefused => "The server refused this user name and password. Check both; the password is not shown here.",
            FailKind::LoginMethodNotOffered => "This test can only sign in with a password and the server does not offer password login (the message lists what it does offer). That does not show the profile is wrong; this test cannot sign in to this server. Unverified against a real server.",
            FailKind::FolderMissing => "The folder does not exist or this user cannot enter it. Check the spelling and capitals; it is relative to the login directory unless it starts with /.",
            FailKind::PermissionDenied => "This user is not allowed to do that in the folder (the test file could not be written, read or deleted). The camera needs to write there.",
            FailKind::ServerUnavailable => "The server is there but would not serve this connection just now (it may be full, shutting down, or limiting logins). Try again later; this is not a mistake in the profile.",
            FailKind::TransferFailed => "The transfer failed. In FTP this is often the data connection (passive mode) being blocked between this computer and the server.",
            FailKind::DataMismatch => "The file came back different from what was sent; something between here and the server altered it.",
            FailKind::Other => "See the message.",
        }
    }
}

/// What an FTP reply code means for the step it answered, as a closed table. A code this table does not list is
/// [`FailKind::Other`]: the message still carries the server's words, and nothing guesses what an unlisted code
/// meant from the step it arrived in (a 421 on the login is not a wrong password).
///
/// Codes are RFC 959's: 421 service not available; 430/530 not logged in / bad login; 450/550 file or folder
/// unavailable; 451, 452, 552, 425, 426 a transfer that could not be done; 553 name not allowed.
pub fn classify_reply(step: Step, code: u16) -> FailKind {
    match (step, code) {
        (_, 421) => FailKind::ServerUnavailable,
        (Step::Login, 430 | 530) => FailKind::LoginRefused,
        (Step::Folder, 450 | 550) => FailKind::FolderMissing,
        (Step::Upload | Step::Download | Step::Delete, 550 | 553) => FailKind::PermissionDenied,
        (Step::Upload | Step::Download | Step::Delete, 425 | 426 | 450 | 451 | 452 | 552) => FailKind::TransferFailed,
        _ => FailKind::Other,
    }
}

/// What a failure that did not come with a reply code (a library error that is neither an I/O error nor a reply)
/// means for the step it happened in.
fn unreplied_kind(step: Step) -> FailKind {
    match step {
        Step::Upload | Step::Download | Step::Compare | Step::Delete => FailKind::TransferFailed,
        Step::Resolve | Step::Connect | Step::Handshake | Step::Login | Step::Folder => FailKind::Other,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum StepOutcome {
    Ok {
        detail: String,
    },
    Failed {
        kind: FailKind,
        message: String,
        hint: String,
        /// The FTP reply code the server answered with, when it answered with one. (SFTP statuses are not FTP codes
        /// and are in the message only.)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_code: Option<u16>,
    },
    Skipped {
        because: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepResult {
    pub step: Step,
    #[serde(flatten)]
    pub outcome: StepOutcome,
    pub millis: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestReport {
    pub host: String,
    pub port: u16,
    pub protocol: Protocol,
    pub folder: String,
    pub steps: Vec<StepResult>,
    /// Every step that was tried succeeded (and none was skipped for a failure). A step skipped because the file check
    /// was not asked for does not make this false; see [`TestReport::fully_tested`].
    pub ok: bool,
    /// The server's host key fingerprint (SFTP), as `SHA256:…`.
    pub host_key: Option<String>,
    /// The name of a test file that was put (or that a failed upload may have left) on the server and could not be
    /// deleted.
    pub file_left_behind: Option<String>,
}

impl TestReport {
    /// The first step that failed.
    pub fn failure(&self) -> Option<&StepResult> {
        self.steps.iter().find(|s| matches!(s.outcome, StepOutcome::Failed { .. }))
    }

    /// Every step passed and none was skipped: the server took the login and the folder AND a file was put, fetched,
    /// compared and deleted. A pass with the file check off is `ok` but not fully tested.
    pub fn fully_tested(&self) -> bool {
        self.ok && !self.steps.iter().any(|s| matches!(s.outcome, StepOutcome::Skipped { .. }))
    }

    /// The last line of the text report, worded for what was actually tried.
    pub fn verdict(&self) -> &'static str {
        if !self.ok {
            "the test did not pass"
        } else if self.fully_tested() {
            "the server accepted this profile"
        } else {
            "the login and the folder passed; no test file was put, so writing to the folder is not tested"
        }
    }

    /// For a person, one line per step.
    pub fn to_text(&self) -> String {
        let proto = if self.protocol == Protocol::Sftp { "SFTP" } else { "FTP" };
        let mut s = format!("{proto} {}:{}{}\n", self.host, self.port, if self.folder.is_empty() { String::new() } else { format!("  folder {}", self.folder) });
        for r in &self.steps {
            match &r.outcome {
                StepOutcome::Ok { detail } => s.push_str(&format!("  ok       {}: {detail}\n", r.step.words())),
                StepOutcome::Failed { message, hint, .. } => s.push_str(&format!("  FAILED   {}: {message}\n           {hint}\n", r.step.words())),
                StepOutcome::Skipped { because } => s.push_str(&format!("  skipped  {}: {because}\n", r.step.words())),
            }
        }
        if let Some(k) = &self.host_key {
            s.push_str(&format!("  server host key {k}\n"));
        }
        if let Some(f) = &self.file_left_behind {
            let place = if self.folder.trim().is_empty() { "the login directory".to_string() } else { format!("folder {}", self.folder.trim()) };
            s.push_str(&format!("  WARNING: the test file {f:?} in {place} may still be on the server (it could not be deleted); delete it by hand\n"));
        }
        s.push_str(&format!("result: {}\n", self.verdict()));
        s
    }

    /// Replace every copy of `secret` in the report's text. Applied to the password before a report leaves
    /// [`run_test`]: the client library and the server both produce text (an error message, a reply) this module does
    /// not control, and either may contain what was sent.
    fn scrub(&mut self, secret: &str) {
        if secret.is_empty() {
            return;
        }
        let clean = |t: &mut String| {
            if t.contains(secret) {
                *t = t.replace(secret, PASSWORD_HIDDEN);
            }
        };
        for r in &mut self.steps {
            match &mut r.outcome {
                StepOutcome::Ok { detail } => clean(detail),
                StepOutcome::Failed { message, .. } => clean(message),
                StepOutcome::Skipped { because } => clean(because),
            }
        }
        if let Some(k) = &mut self.host_key {
            clean(k);
        }
        if let Some(f) = &mut self.file_left_behind {
            clean(f);
        }
    }
}

// ── recording ────────────────────────────────────────────────────────────

struct Failure {
    kind: FailKind,
    message: String,
    /// The FTP reply code, when the failure is a reply.
    code: Option<u16>,
}

fn fail(kind: FailKind, message: impl Into<String>) -> Failure {
    Failure { kind, message: message.into(), code: None }
}

impl Failure {
    /// The same failure with `what` in front of its message (an address, the thing that was being done).
    fn in_context(self, what: &str) -> Failure {
        Failure { message: format!("{what}: {}", self.message), ..self }
    }
}

/// The failures of trying each address in turn: all of them are reported, not just the last.
#[derive(Default)]
struct ConnectTries(Vec<Failure>);

impl ConnectTries {
    fn note(&mut self, f: Failure) {
        self.0.push(f);
    }

    /// One failure: the kind and code of the last attempt, the messages of all of them in the order tried.
    fn into_failure(mut self) -> Failure {
        match self.0.pop() {
            None => fail(FailKind::Other, "no address could be tried"),
            Some(last) if self.0.is_empty() => last,
            Some(last) => {
                let earlier: Vec<String> = self.0.into_iter().map(|f| f.message).collect();
                Failure { message: format!("{}; {}", earlier.join("; "), last.message), ..last }
            }
        }
    }
}

struct Recorder {
    plan: &'static [Step],
    steps: Vec<StepResult>,
    failed: bool,
    started: Instant,
}

impl Recorder {
    fn new(protocol: Protocol) -> Recorder {
        Recorder { plan: plan(protocol), steps: Vec::new(), failed: false, started: Instant::now() }
    }

    fn lap(&mut self) -> u64 {
        let ms = self.started.elapsed().as_millis() as u64;
        self.started = Instant::now();
        ms
    }

    fn ok(&mut self, step: Step, detail: impl Into<String>) {
        let millis = self.lap();
        self.steps.push(StepResult { step, outcome: StepOutcome::Ok { detail: detail.into() }, millis });
    }

    fn fail(&mut self, step: Step, f: Failure) {
        let millis = self.lap();
        self.failed = true;
        self.steps.push(StepResult { step, outcome: StepOutcome::Failed { kind: f.kind, message: f.message, hint: f.kind.hint().to_string(), reply_code: f.code }, millis });
    }

    fn skip(&mut self, step: Step, because: impl Into<String>) {
        self.steps.push(StepResult { step, outcome: StepOutcome::Skipped { because: because.into() }, millis: 0 });
    }

    fn recorded(&self, step: Step) -> bool {
        self.steps.iter().any(|r| r.step == step)
    }

    /// Skip every step of the plan that has no outcome yet, saying why for each. Which steps are "the rest" is
    /// whatever the plan lists and the report lacks, not a position someone counted.
    fn skip_unrecorded(&mut self, why: impl Fn(Step) -> &'static str) {
        for s in self.plan {
            if !self.recorded(*s) {
                self.skip(*s, why(*s));
            }
        }
    }

    /// An earlier step failed: the rest is not tried.
    fn skip_rest(&mut self) {
        self.skip_unrecorded(|_| "an earlier step failed");
    }

    /// The file check was not asked for.
    fn skip_unasked(&mut self) {
        self.skip_unrecorded(|_| "not asked for");
    }

    /// The upload failed in a way that says the server took nothing (a refusal), so there is nothing to fetch or delete.
    fn skip_after_refused_upload(&mut self) {
        self.skip_unrecorded(|s| if s == Step::Delete { "nothing was put there" } else { "an earlier step failed" });
    }
}

fn io_kind(step: Step, e: &std::io::Error) -> FailKind {
    use std::io::ErrorKind as K;
    match e.kind() {
        K::ConnectionRefused => FailKind::ConnectionRefused,
        K::TimedOut | K::WouldBlock => FailKind::TimedOut,
        K::HostUnreachable | K::NetworkUnreachable | K::NetworkDown => FailKind::Unreachable,
        // A connection that dies or sends rubbish before the login is accepted points at the wrong protocol or port;
        // the same thing in the middle of a session is a network fault, and "check the protocol" would be wrong.
        K::UnexpectedEof | K::InvalidData | K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe => {
            if step.before_login_accepted() {
                FailKind::NotThisProtocol
            } else {
                FailKind::TransferFailed
            }
        }
        _ => FailKind::Other,
    }
}

/// A name for the test file that no other run, and no file already there, is likely to share.
fn test_file_name() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("nikon-fleet-test-{}-{nanos}.txt", std::process::id())
}

fn test_file_body(name: &str) -> Vec<u8> {
    format!("nikon-fleet connection test file {name}\nIt was put here and will be deleted by the same test; if you can read this it was left behind.\n").into_bytes()
}

/// `bytes` limited to `cap`, and whether there was more: one byte past the cap is enough to know, so a reader only
/// ever has to produce `cap + 1`.
fn limit_to(mut bytes: Vec<u8>, cap: u64) -> (Vec<u8>, bool) {
    let over = bytes.len() as u64 > cap;
    if over {
        bytes.truncate(cap as usize);
    }
    (bytes, over)
}

/// Read at most `cap` bytes (and learn whether there were more) without ever holding more than `cap + 1`.
fn read_capped(reader: &mut dyn Read, cap: u64) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    reader.take(cap + 1).read_to_end(&mut buf)?;
    Ok(limit_to(buf, cap))
}

/// [`read_capped`] for an async reader.
async fn read_capped_async<R: tokio::io::AsyncRead + Unpin>(reader: &mut R, cap: u64) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    reader.take(cap + 1).read_to_end(&mut buf).await?;
    Ok(limit_to(buf, cap))
}

/// What the comparison step records, from what was sent and what came back (`more` when the server sent more than
/// the cap). Shared by both protocols.
fn record_comparison(rec: &mut Recorder, sent: &[u8], got: &[u8], more: bool) {
    if more {
        rec.fail(Step::Compare, fail(FailKind::DataMismatch, format!("more than {} bytes came back for {} sent", got.len(), sent.len())));
    } else if got == sent {
        rec.ok(Step::Compare, "identical");
    } else {
        rec.fail(Step::Compare, fail(FailKind::DataMismatch, format!("{} bytes came back for {} sent", got.len(), sent.len())));
    }
}

fn downloaded_words(got: &[u8], more: bool) -> String {
    if more { format!("more than {} bytes", got.len()) } else { format!("{} bytes", got.len()) }
}

/// A failed upload leaves nothing to delete only if the server refused the write itself; any other failure (a
/// timeout, a dropped data connection, an aborted transfer) may have left some of the file.
fn upload_may_have_left_a_file(kind: FailKind) -> bool {
    kind != FailKind::PermissionDenied
}

/// A limit in words for a message: whole seconds as "15", a shorter one as "0.4" (never "0").
fn limit_words(d: Duration) -> String {
    if d.subsec_nanos() == 0 { d.as_secs().to_string() } else { format!("{:.1}", d.as_secs_f64()) }
}

/// Resolve `host:port`, with a limit: the system resolver itself has none.
fn resolve(host: &str, port: u16, timeout: Duration) -> Result<Vec<SocketAddr>, Failure> {
    // An empty name is not looked up: the resolver answers it differently on each platform (Windows returns this
    // computer's own addresses for ":21", so the test would go on to "connect" to itself and wait for the timeout).
    if host.trim().is_empty() {
        return Err(fail(FailKind::NameNotFound, "no server name was given"));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let h = host.to_string();
    std::thread::spawn(move || {
        // The receiver has gone if the wait below timed out; there is nobody to tell.
        let _ = tx.send((h.as_str(), port).to_socket_addrs().map(|i| i.collect::<Vec<_>>()));
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(addrs)) if !addrs.is_empty() => Ok(addrs),
        Ok(Ok(_)) => Err(fail(FailKind::NameNotFound, format!("{host:?} resolved to no address"))),
        // std gives the resolver's failure only as text; there is no code to tell "no such name" from "no network",
        // so the kind says what was asked (a name that did not resolve) and the hint covers both.
        Ok(Err(e)) => Err(fail(FailKind::NameNotFound, format!("{host:?} could not be resolved ({e})"))),
        Err(_) => Err(fail(FailKind::TimedOut, format!("looking up {host:?} took more than {} s", limit_words(timeout)))),
    }
}

fn show_addrs(addrs: &[SocketAddr]) -> String {
    addrs.iter().map(SocketAddr::to_string).collect::<Vec<_>>().join(", ")
}

/// The first step, the same for both protocols: the addresses, or the report with everything else skipped.
fn resolve_step(req: &TestRequest, rec: &mut Recorder) -> Option<Vec<SocketAddr>> {
    match resolve(&req.host, req.port, req.step_timeout) {
        Ok(a) => {
            rec.ok(Step::Resolve, format!("{} -> {}", req.host, show_addrs(&a)));
            Some(a)
        }
        Err(f) => {
            rec.fail(Step::Resolve, f);
            rec.skip_rest();
            None
        }
    }
}

/// Run the test. Blocks; call it off any UI thread. Never panics on a server's behaviour.
pub fn run_test(req: &TestRequest) -> TestReport {
    let mut rec = Recorder::new(req.protocol);
    let mut host_key = None;
    let mut left_behind = None;
    match req.protocol {
        Protocol::Ftp => ftp(req, &mut rec, &mut left_behind),
        Protocol::Sftp => sftp(req, &mut rec, &mut host_key, &mut left_behind),
    }
    let ok = !rec.failed && rec.steps.iter().all(|s| !matches!(s.outcome, StepOutcome::Failed { .. }));
    let mut report = TestReport { host: req.host.clone(), port: req.port, protocol: req.protocol, folder: req.folder.clone(), steps: rec.steps, ok, host_key, file_left_behind: left_behind };
    report.scrub(&req.password);
    report
}

// ── FTP ──────────────────────────────────────────────────────────────────

/// A TCP connection with the step limit on the connect AND on every later read and write. Every result is checked: a
/// socket whose limits could not be set is not returned, since it would be waited on for ever.
fn timed_connect(addr: &SocketAddr, limit: Duration) -> std::io::Result<TcpStream> {
    let socket = TcpStream::connect_timeout(addr, limit)?;
    socket.set_read_timeout(Some(limit))?;
    socket.set_write_timeout(Some(limit))?;
    Ok(socket)
}

fn ftp_failure(step: Step, e: &suppaftp::FtpError) -> Failure {
    use suppaftp::FtpError;
    match e {
        FtpError::ConnectionError(io) => fail(io_kind(step, io), format!("{io}")),
        FtpError::UnexpectedResponse(r) => {
            let raw = r.status as u32;
            let text = String::from_utf8_lossy(&r.body).trim().to_string();
            let code = u16::try_from(raw).ok();
            Failure { kind: code.map_or(FailKind::Other, |c| classify_reply(step, c)), message: format!("the server answered {raw} {text}"), code }
        }
        FtpError::BadResponse => fail(if step.before_login_accepted() { FailKind::NotThisProtocol } else { FailKind::Other }, "the server's reply was not valid FTP"),
        other => fail(unreplied_kind(step), other.to_string()),
    }
}

/// What a server's STAT of a path says about it. A server that sends no permissions field, or one with none of the
/// file-type bits set (some send only `rwx` bits), has given no file type: `TypeNotGiven` is not an answer either way.
#[derive(Debug, PartialEq, Eq)]
enum FolderProbe {
    Folder,
    /// Not a folder: what it is, worded to follow "is".
    NotAFolder(&'static str),
    TypeNotGiven,
}

/// The POSIX file-type bits of a mode (`S_IFMT`) and the ones this reads.
const MODE_TYPE_MASK: u32 = 0o170000;
const MODE_DIR: u32 = 0o040000;
const MODE_FILE: u32 = 0o100000;
const MODE_LINK: u32 = 0o120000;

fn folder_probe(attrs: &russh_sftp::client::fs::Metadata) -> FolderProbe {
    match attrs.permissions.map(|p| p & MODE_TYPE_MASK) {
        None | Some(0) => FolderProbe::TypeNotGiven,
        Some(MODE_DIR) => FolderProbe::Folder,
        Some(MODE_FILE) => FolderProbe::NotAFolder("a file, not a folder"),
        Some(MODE_LINK) => FolderProbe::NotAFolder("a link to something that is not a folder"),
        Some(_) => FolderProbe::NotAFolder("a special file, not a folder"),
    }
}

fn ftp(req: &TestRequest, rec: &mut Recorder, left_behind: &mut Option<String>) {
    use suppaftp::FtpStream;
    let t = req.step_timeout;
    let Some(addrs) = resolve_step(req, rec) else { return };
    let mut tries = ConnectTries::default();
    let mut stream = None;
    for a in &addrs {
        // The timeouts go on the socket BEFORE the greeting is read: `FtpStream::connect_timeout` limits only the
        // connect, so a server that accepts and never speaks would be waited on for ever.
        let socket = match timed_connect(a, t) {
            Ok(sock) => sock,
            Err(e) => {
                tries.note(fail(io_kind(Step::Connect, &e), format!("{a}: {e}")));
                continue;
            }
        };
        match FtpStream::connect_with_stream(socket) {
            Ok(s) => {
                stream = Some((s, *a));
                break;
            }
            Err(e) => tries.note(ftp_failure(Step::Connect, &e).in_context(&a.to_string())),
        }
    }
    let Some((s, addr)) = stream else {
        rec.fail(Step::Connect, tries.into_failure());
        rec.skip_rest();
        return;
    };
    // The library's own passive-mode connection has no limit at all: a data address that never answers blocks for the
    // operating system's connect timeout, and a data connection that goes quiet blocks for ever. This one has the
    // step limit on the connect and on every read and write.
    let mut s = s.passive_stream_builder(move |data_addr| timed_connect(&data_addr, t).map_err(suppaftp::FtpError::ConnectionError));
    let banner = s.get_welcome_msg().map(|m| m.trim().replace('\n', " "));
    rec.ok(Step::Connect, format!("{addr}{}", banner.filter(|b| !b.is_empty()).map_or_else(String::new, |b| format!(" said {b:?}"))));

    if let Err(e) = s.login(req.username.as_str(), req.password.as_str()) {
        rec.fail(Step::Login, ftp_failure(Step::Login, &e));
        rec.skip_rest();
        // The verdict is in; the goodbye is courtesy and its failure changes nothing.
        let _ = s.quit();
        return;
    }
    rec.ok(Step::Login, format!("accepted {:?}", req.username));
    // Binary, because the camera sends images. (Whether the camera itself sends binary is not something this test
    // knows.) The client's default is ASCII, which rewrites line endings and would make a faithful server look like
    // it altered the file.
    if let Err(e) = s.transfer_type(suppaftp::types::FileType::Binary) {
        rec.fail(Step::Folder, ftp_failure(Step::Folder, &e).in_context("could not switch to binary transfer"));
        rec.skip_rest();
        let _ = s.quit();
        return;
    }

    let folder = req.folder.trim();
    let at = if folder.is_empty() { s.pwd().map(|p| format!("staying in the login directory {p}")) } else { s.cwd(folder).map(|()| format!("in {folder}")) };
    match at {
        Ok(d) => rec.ok(Step::Folder, d),
        Err(e) => {
            rec.fail(Step::Folder, ftp_failure(Step::Folder, &e));
            rec.skip_rest();
            let _ = s.quit();
            return;
        }
    }
    if !req.file_check {
        rec.skip_unasked();
        let _ = s.quit();
        return;
    }

    let name = test_file_name();
    let body = test_file_body(&name);
    let upload_failed = match s.put_file(name.as_str(), &mut Cursor::new(body.clone())) {
        Ok(n) => {
            rec.ok(Step::Upload, format!("{name} ({n} bytes)"));
            false
        }
        Err(e) => {
            let f = ftp_failure(Step::Upload, &e);
            let may_have_left_a_file = upload_may_have_left_a_file(f.kind);
            rec.fail(Step::Upload, f);
            if !may_have_left_a_file {
                rec.skip_after_refused_upload();
                let _ = s.quit();
                return;
            }
            true
        }
    };
    if upload_failed {
        rec.skip(Step::Download, "an earlier step failed");
        rec.skip(Step::Compare, "an earlier step failed");
    } else {
        match s.retr(name.as_str(), |r| read_capped(r, MAX_TEST_FILE_BYTES).map_err(suppaftp::FtpError::ConnectionError)) {
            Ok((got, more)) => {
                rec.ok(Step::Download, downloaded_words(&got, more));
                record_comparison(rec, &body, &got, more);
            }
            Err(e) => {
                rec.fail(Step::Download, ftp_failure(Step::Download, &e));
                rec.skip(Step::Compare, "an earlier step failed");
            }
        }
    }
    // Whatever happened above, try to remove what may be there; a failed upload can leave part of a file.
    match s.rm(name.as_str()) {
        Ok(()) => rec.ok(Step::Delete, format!("{name} removed")),
        Err(e) => {
            rec.fail(Step::Delete, ftp_failure(Step::Delete, &e));
            *left_behind = Some(name);
        }
    }
    let _ = s.quit();
}

// ── SFTP ─────────────────────────────────────────────────────────────────

struct KeyCapture(Arc<Mutex<Option<String>>>);

/// The `SHA256:…` fingerprint of a server's host key; for a host certificate, of the key the certificate carries.
fn host_key_fingerprint(key: &russh::keys::PublicKeyOrCertificate) -> String {
    use russh::keys::{HashAlg, PublicKey, PublicKeyOrCertificate as K};
    match key {
        K::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256).to_string(),
        K::Certificate(c) => PublicKey::from(c.public_key().clone()).fingerprint(HashAlg::Sha256).to_string(),
    }
}

impl russh::client::Handler for KeyCapture {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &russh::keys::PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        // The only way the lock is held is by this closure and by the one read after the handshake; neither panics.
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(host_key_fingerprint(key));
        }
        // Nothing to check it against: reported, not trusted.
        Ok(true)
    }
}

/// What an SFTP error means. `default` is what the step it happened in means by "failed"; a server's "no such file"
/// is a missing folder only when the step was looking for the folder.
fn sftp_status_failure(default: FailKind, e: &russh_sftp::client::error::Error) -> Failure {
    use russh_sftp::client::error::Error as E;
    use russh_sftp::protocol::StatusCode as C;
    match e {
        E::Status(st) => {
            let kind = match st.status_code {
                C::PermissionDenied => FailKind::PermissionDenied,
                C::NoConnection | C::ConnectionLost => FailKind::Other,
                _ => default,
            };
            fail(kind, format!("the server answered {:?}: {}", st.status_code, st.error_message))
        }
        E::Timeout => fail(FailKind::TimedOut, "the server did not answer in time"),
        other => fail(default, other.to_string()),
    }
}

fn sftp(req: &TestRequest, rec: &mut Recorder, host_key: &mut Option<String>, left_behind: &mut Option<String>) {
    let Some(addrs) = resolve_step(req, rec) else { return };
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            rec.fail(Step::Connect, fail(FailKind::Other, format!("could not start the network runtime: {e}")));
            rec.skip_rest();
            return;
        }
    };
    rt.block_on(sftp_steps(req, &addrs, rec, host_key, left_behind));
}

async fn within<T>(limit: Duration, what: &str, f: impl std::future::Future<Output = T>) -> Result<T, Failure> {
    tokio::time::timeout(limit, f).await.map_err(|_| fail(FailKind::TimedOut, format!("{what} took more than {} s", limit_words(limit))))
}

/// A failure of the SSH connection (the handshake, or the login that rides on it). The library reports a closed or refused connection and a bad key exchange alike; only
/// a peer that sent no SSH version line at all (`Version`) is evidence of the wrong protocol.
fn ssh_failure(e: &russh::Error) -> Failure {
    let kind = match e {
        russh::Error::IO(io) => io_kind(Step::Handshake, io),
        russh::Error::Version => FailKind::NotThisProtocol,
        _ => FailKind::HandshakeFailed,
    };
    fail(kind, format!("{e}"))
}

/// What a refused SFTP password means, decided from typed data only: the methods the server listed when it was asked
/// (`offered`, before the password was sent), and from the refusal itself whether more authentication is needed
/// (`partial_success`) and what the server says to go on with (`next`). No message text is read.
///
/// - the password was accepted but the server wants a further method: [`FailKind::Other`] (this test cannot do the rest),
///   naming `next`;
/// - "password" among `offered`: a wrong user name or password ([`FailKind::LoginRefused`]);
/// - otherwise, with at least one method listed: [`FailKind::LoginMethodNotOffered`], naming them;
/// - nothing listed at all (an empty list says nothing about passwords): the plain refusal, as before.
///
/// The "none" entry some servers list is not a login method and is left out of what is named. `next` is not used to
/// decide whether a password is taken: russh's own server leaves out the method just tried.
fn refused_password(offered: &russh::MethodSet, next: &russh::MethodSet, partial_success: bool) -> Failure {
    use russh::MethodKind as M;
    let names = |set: &russh::MethodSet| -> Vec<&'static str> { set.iter().filter(|m| **m != M::None).map(<&'static str>::from).collect() };
    if partial_success {
        let next = names(next);
        let more = if next.is_empty() { "a further method".to_string() } else { format!("a further method (the server lists: {})", next.join(", ")) };
        return fail(FailKind::Other, format!("the server accepted the password but wants {more}; this test can only send a password"));
    }
    let named = names(offered);
    if named.is_empty() || offered.contains(&M::Password) {
        return fail(FailKind::LoginRefused, "the server did not accept that user name and password (SFTP here tries a password only)");
    }
    fail(FailKind::LoginMethodNotOffered, format!("the server does not offer password login; it offers: {} (SFTP here tries a password only)", named.join(", ")))
}

async fn sftp_steps(req: &TestRequest, addrs: &[SocketAddr], rec: &mut Recorder, host_key: &mut Option<String>, left_behind: &mut Option<String>) {
    use russh_sftp::client::SftpSession;
    let t = req.step_timeout;
    // connect
    let mut tries = ConnectTries::default();
    let mut socket = None;
    for a in addrs {
        match within(t, &format!("connecting to {a}"), tokio::net::TcpStream::connect(*a)).await {
            Ok(Ok(s)) => {
                socket = Some((s, *a));
                break;
            }
            Ok(Err(e)) => tries.note(fail(io_kind(Step::Connect, &e), format!("{a}: {e}"))),
            Err(f) => tries.note(f),
        }
    }
    let Some((socket, addr)) = socket else {
        rec.fail(Step::Connect, tries.into_failure());
        rec.skip_rest();
        return;
    };
    rec.ok(Step::Connect, addr.to_string());

    // handshake
    let seen = Arc::new(Mutex::new(None));
    let config = Arc::new(russh::client::Config { inactivity_timeout: Some(t * 4), ..Default::default() });
    let handle = within(t, "the SSH handshake", russh::client::connect_stream(config, socket, KeyCapture(seen.clone()))).await;
    let mut handle = match handle {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            rec.fail(Step::Handshake, ssh_failure(&e));
            rec.skip_rest();
            return;
        }
        Err(f) => {
            rec.fail(Step::Handshake, f);
            rec.skip_rest();
            return;
        }
    };
    *host_key = seen.lock().ok().and_then(|k| k.clone());
    rec.ok(Step::Handshake, host_key.clone().map_or("done".to_string(), |k| format!("host key {k}")));

    // login: first ask which methods the server offers (the standard "none" probe, before any password is sent), then
    // try the password. The list in a refusal of the password itself is not used: russh's own server leaves out the
    // method just tried, so it cannot say whether a password is taken at all.
    let offered = match within(t, "asking which login methods the server offers", handle.authenticate_none(req.username.as_str())).await {
        Ok(Ok(russh::client::AuthResult::Success)) => None,
        Ok(Ok(russh::client::AuthResult::Failure { remaining_methods, .. })) => Some(remaining_methods),
        Ok(Err(e)) => {
            rec.fail(Step::Login, ssh_failure(&e));
            rec.skip_rest();
            return;
        }
        Err(f) => {
            rec.fail(Step::Login, f);
            rec.skip_rest();
            return;
        }
    };
    match offered {
        None => rec.ok(Step::Login, format!("accepted {:?} (the server asked for no authentication; no password was sent)", req.username)),
        Some(offered) => match within(t, "logging in", handle.authenticate_password(req.username.as_str(), req.password.as_str())).await {
            Ok(Ok(russh::client::AuthResult::Success)) => rec.ok(Step::Login, format!("accepted {:?}", req.username)),
            Ok(Ok(russh::client::AuthResult::Failure { remaining_methods, partial_success })) => {
                rec.fail(Step::Login, refused_password(&offered, &remaining_methods, partial_success));
                rec.skip_rest();
                return;
            }
            Ok(Err(e)) => {
                rec.fail(Step::Login, ssh_failure(&e));
                rec.skip_rest();
                return;
            }
            Err(f) => {
                rec.fail(Step::Login, f);
                rec.skip_rest();
                return;
            }
        },
    }

    // sftp subsystem
    let opened = async {
        let channel = handle.channel_open_session().await.map_err(|e| fail(FailKind::Other, format!("could not open a channel: {e}")))?;
        channel.request_subsystem(true, "sftp").await.map_err(|e| fail(FailKind::NotThisProtocol, format!("the server has no SFTP subsystem for this user ({e})")))?;
        SftpSession::new(channel.into_stream()).await.map_err(|e| sftp_status_failure(FailKind::NotThisProtocol, &e))
    };
    let sftp = match within(t, "starting SFTP", opened).await {
        Ok(Ok(s)) => s,
        Ok(Err(f)) | Err(f) => {
            // There is no step of its own for opening the SFTP subsystem; the message says what failed.
            rec.fail(Step::Folder, f);
            rec.skip_rest();
            return;
        }
    };

    // folder
    let wanted = if req.folder.trim().is_empty() { ".".to_string() } else { req.folder.trim().to_string() };
    // The question is only "is there a folder here": a STAT answers it without reading what is in it (a camera's upload
    // folder can hold thousands of files, and the library's `read_dir` reads every one of them before it returns). A
    // server that gives no file type in its answer is asked the old way, by opening the folder and reading it.
    let folder = match within(t, "finding the folder", async {
        let real = sftp.canonicalize(wanted.as_str()).await?;
        let attrs = sftp.metadata(real.as_str()).await?;
        match folder_probe(&attrs) {
            FolderProbe::Folder => {}
            FolderProbe::NotAFolder(what) => return Ok::<Result<String, String>, russh_sftp::client::error::Error>(Err(format!("{real} is {what}"))),
            FolderProbe::TypeNotGiven => {
                sftp.read_dir(real.as_str()).await?;
            }
        }
        Ok(Ok(real))
    })
    .await
    {
        Ok(Ok(Ok(real))) => {
            rec.ok(Step::Folder, format!("in {real}"));
            real
        }
        Ok(Ok(Err(message))) => {
            rec.fail(Step::Folder, fail(FailKind::FolderMissing, message));
            rec.skip_rest();
            return;
        }
        Ok(Err(e)) => {
            rec.fail(Step::Folder, sftp_status_failure(FailKind::FolderMissing, &e));
            rec.skip_rest();
            return;
        }
        Err(f) => {
            rec.fail(Step::Folder, f);
            rec.skip_rest();
            return;
        }
    };
    if !req.file_check {
        rec.skip_unasked();
        return;
    }

    let name = test_file_name();
    let path = format!("{}/{name}", folder.trim_end_matches('/'));
    let body = test_file_body(&name);
    let upload_failure = match within(t, "putting the file", sftp.write(path.as_str(), &body)).await {
        Ok(Ok(())) => {
            rec.ok(Step::Upload, format!("{name} ({} bytes)", body.len()));
            None
        }
        Ok(Err(e)) => Some(sftp_status_failure(FailKind::TransferFailed, &e)),
        Err(f) => Some(f),
    };
    let upload_failed = upload_failure.is_some();
    if let Some(f) = upload_failure {
        let may_have_left_a_file = upload_may_have_left_a_file(f.kind);
        rec.fail(Step::Upload, f);
        if !may_have_left_a_file {
            rec.skip_after_refused_upload();
            return;
        }
    }
    if upload_failed {
        rec.skip(Step::Download, "an earlier step failed");
        rec.skip(Step::Compare, "an earlier step failed");
    } else {
        let read = within(t, "fetching the file", async {
            let mut file = sftp.open(path.as_str()).await?;
            let got = read_capped_async(&mut file, MAX_TEST_FILE_BYTES).await.map_err(|e| russh_sftp::client::error::Error::IO(e.to_string()));
            // Reading is done and judged; failing to close a file that was only read changes nothing.
            let _ = file.close().await;
            got
        })
        .await;
        match read {
            Ok(Ok((got, more))) => {
                rec.ok(Step::Download, downloaded_words(&got, more));
                record_comparison(rec, &body, &got, more);
            }
            Ok(Err(e)) => {
                rec.fail(Step::Download, sftp_status_failure(FailKind::TransferFailed, &e));
                rec.skip(Step::Compare, "an earlier step failed");
            }
            Err(f) => {
                rec.fail(Step::Download, f);
                rec.skip(Step::Compare, "an earlier step failed");
            }
        }
    }
    match within(t, "deleting the file", sftp.remove_file(path.as_str())).await {
        Ok(Ok(())) => rec.ok(Step::Delete, format!("{name} removed")),
        Ok(Err(e)) => {
            rec.fail(Step::Delete, sftp_status_failure(FailKind::TransferFailed, &e));
            *left_behind = Some(name);
        }
        Err(f) => {
            rec.fail(Step::Delete, f);
            *left_behind = Some(name);
        }
    }
    // The verdict is in; closing the session is courtesy.
    let _ = sftp.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    // ── an FTP server that does just enough, and can misbehave ──

    #[derive(Clone, Default)]
    struct FtpBehaviour {
        user: &'static str,
        pass: &'static str,
        dirs: Vec<&'static str>,
        deny_store: bool,
        corrupt_retr: bool,
        deny_delete: bool,
        greeting: Option<&'static str>,
        /// Answer to PASS (instead of the real one).
        pass_reply: Option<&'static str>,
        /// Answer to CWD (instead of the real one).
        cwd_reply: Option<&'static str>,
        /// Answer a refused login with the password it was given in the text.
        echo_password: bool,
        /// After the 150 for a RETR, take the data connection and say nothing for this long.
        stall_retr: bool,
        /// After the 150 for a STOR, take the data connection and never read it or answer.
        stall_stor: bool,
        /// Keep what arrived of a STOR, then answer 426 (an aborted transfer that leaves a partial file).
        partial_stor: bool,
        /// Send far more than was stored for a RETR.
        oversize_retr: bool,
        /// Close the control connection when this verb arrives.
        hang_up_on: Option<&'static str>,
    }

    /// How long a stalled fake holds its data connection: far longer than any timeout in these tests, short enough
    /// that a test that fails to time out ends in a few seconds instead of hanging.
    const STALL: Duration = Duration::from_secs(6);

    #[derive(Default)]
    struct FtpState {
        files: BTreeMap<String, Vec<u8>>,
        commands: Vec<String>,
    }

    struct FakeFtp {
        port: u16,
        state: Arc<Mutex<FtpState>>,
    }

    fn reply(w: &mut impl Write, text: &str) {
        let _ = w.write_all(format!("{text}\r\n").as_bytes());
    }

    fn fake_ftp(b: FtpBehaviour) -> FakeFtp {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(FtpState::default()));
        let st = state.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let (b, st) = (b.clone(), st.clone());
                std::thread::spawn(move || ftp_session(conn, b, st));
            }
        });
        FakeFtp { port, state }
    }

    fn ftp_session(conn: std::net::TcpStream, b: FtpBehaviour, st: Arc<Mutex<FtpState>>) {
        let mut w = conn.try_clone().unwrap();
        let mut r = BufReader::new(conn);
        reply(&mut w, b.greeting.unwrap_or("220 fake ftp ready"));
        // RFC 959: a session starts in ASCII type. In ASCII a server stores and sends text with its own line ending
        // (here LF), so a client that forgot to ask for binary gets different bytes back than it sent.
        let (mut user_ok, mut logged_in, mut cwd, mut binary) = (false, false, String::from("/"), false);
        let mut data: Option<TcpListener> = None;
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let cmd = line.trim_end().to_string();
            // The password is kept out of the log so that a failing assertion cannot print it; nothing asserts on the
            // log's absence of it (the report is what must not hold it, and the tests below check the report).
            st.lock().unwrap().commands.push(if verb_is_pass(&cmd) { "PASS <pw>".to_string() } else { cmd.clone() });
            let (verb, arg) = cmd.split_once(' ').map_or((cmd.as_str(), ""), |(v, a)| (v, a));
            if b.hang_up_on.is_some_and(|h| h.eq_ignore_ascii_case(verb)) {
                return;
            }
            match verb.to_uppercase().as_str() {
                "USER" => {
                    user_ok = arg == b.user;
                    reply(&mut w, "331 password please");
                }
                "PASS" => {
                    logged_in = user_ok && arg == b.pass;
                    if let Some(r) = b.pass_reply {
                        reply(&mut w, r);
                    } else if logged_in {
                        reply(&mut w, "230 logged in");
                    } else if b.echo_password {
                        reply(&mut w, &format!("530 Login incorrect for password {arg}"));
                    } else {
                        reply(&mut w, "530 Login incorrect.");
                    }
                }
                "TYPE" => match arg {
                    "I" => {
                        binary = true;
                        reply(&mut w, "200 Type set to I");
                    }
                    "A" => {
                        binary = false;
                        reply(&mut w, "200 Type set to A");
                    }
                    _ => reply(&mut w, "504 type not supported"),
                },
                "SYST" => reply(&mut w, "215 UNIX Type: L8"),
                "PWD" => reply(&mut w, &format!("257 \"{cwd}\" is the current directory")),
                "CWD" => {
                    if let Some(r) = b.cwd_reply {
                        reply(&mut w, r);
                    } else if b.dirs.contains(&arg) {
                        cwd = format!("/{}", arg.trim_start_matches('/'));
                        reply(&mut w, "250 ok");
                    } else {
                        reply(&mut w, "550 No such file or directory.");
                    }
                }
                "PASV" => {
                    let l = TcpListener::bind("127.0.0.1:0").unwrap();
                    let p = l.local_addr().unwrap().port();
                    reply(&mut w, &format!("227 Entering Passive Mode (127,0,0,1,{},{})", p / 256, p % 256));
                    data = Some(l);
                }
                "STOR" => {
                    if b.deny_store {
                        reply(&mut w, "550 Permission denied.");
                        data = None;
                        continue;
                    }
                    reply(&mut w, "150 go ahead");
                    let mut buf = Vec::new();
                    if let Some(l) = data.take() {
                        if let Ok((mut c, _)) = l.accept() {
                            if b.stall_stor {
                                std::thread::sleep(STALL);
                                return;
                            }
                            let _ = c.read_to_end(&mut buf);
                        }
                    }
                    if !binary {
                        buf = String::from_utf8_lossy(&buf).replace("\r\n", "\n").into_bytes();
                    }
                    st.lock().unwrap().files.insert(arg.to_string(), buf);
                    if b.partial_stor {
                        reply(&mut w, "426 Connection closed; transfer aborted.");
                    } else {
                        reply(&mut w, "226 transfer complete");
                    }
                }
                "RETR" => {
                    let content = st.lock().unwrap().files.get(arg).cloned();
                    match content {
                        None => {
                            reply(&mut w, "550 No such file.");
                            data = None;
                        }
                        Some(mut bytes) => {
                            reply(&mut w, "150 sending");
                            if b.corrupt_retr {
                                bytes.push(b'!');
                            }
                            if b.oversize_retr {
                                bytes = vec![b'x'; 4 * MAX_TEST_FILE_BYTES as usize];
                            }
                            if !binary {
                                bytes = String::from_utf8_lossy(&bytes).replace('\n', "\r\n").into_bytes();
                            }
                            if let Some(l) = data.take() {
                                if let Ok((mut c, _)) = l.accept() {
                                    if b.stall_retr {
                                        std::thread::sleep(STALL);
                                        return;
                                    }
                                    let _ = c.write_all(&bytes);
                                }
                            }
                            reply(&mut w, "226 transfer complete");
                        }
                    }
                }
                "DELE" => {
                    if b.deny_delete {
                        reply(&mut w, "550 Permission denied.");
                    } else if st.lock().unwrap().files.remove(arg).is_some() {
                        reply(&mut w, "250 deleted");
                    } else {
                        reply(&mut w, "550 No such file.");
                    }
                }
                "QUIT" => {
                    reply(&mut w, "221 bye");
                    return;
                }
                _ => reply(&mut w, "502 not implemented"),
            }
            let _ = logged_in;
        }
    }

    fn verb_is_pass(cmd: &str) -> bool {
        cmd.split(' ').next().is_some_and(|v| v.eq_ignore_ascii_case("PASS"))
    }

    fn good_ftp() -> FtpBehaviour {
        FtpBehaviour { user: "fleet", pass: "s3cret!", dirs: vec!["incoming", "/incoming/raw"], ..Default::default() }
    }

    fn request(port: u16, protocol: Protocol) -> TestRequest {
        TestRequest {
            host: "127.0.0.1".into(),
            port,
            protocol,
            username: "fleet".into(),
            password: "s3cret!".into(),
            folder: "incoming".into(),
            file_check: true,
            step_timeout: Duration::from_secs(5),
        }
    }

    /// The steps of a report, with every report held to listing each step of its protocol once and in order: a step
    /// that goes missing from a report (not recorded, not skipped) is a bug, not a pass.
    fn outcomes(r: &TestReport) -> Vec<(Step, String)> {
        let canonical: &[Step] = match r.protocol {
            Protocol::Ftp => &[Step::Resolve, Step::Connect, Step::Login, Step::Folder, Step::Upload, Step::Download, Step::Compare, Step::Delete],
            Protocol::Sftp => &[Step::Resolve, Step::Connect, Step::Handshake, Step::Login, Step::Folder, Step::Upload, Step::Download, Step::Compare, Step::Delete],
        };
        assert_eq!(r.steps.iter().map(|s| s.step).collect::<Vec<_>>(), canonical, "every step appears once, in order:\n{}", r.to_text());
        r.steps
            .iter()
            .map(|s| {
                let tag = match &s.outcome {
                    StepOutcome::Ok { .. } => "ok".to_string(),
                    StepOutcome::Failed { kind, .. } => format!("failed:{}", serde_json::to_value(kind).unwrap().as_str().unwrap()),
                    StepOutcome::Skipped { .. } => "skipped".to_string(),
                };
                (s.step, tag)
            })
            .collect()
    }

    fn all(step_outcomes: &[(Step, &str)]) -> Vec<(Step, String)> {
        step_outcomes.iter().map(|(s, o)| (*s, o.to_string())).collect()
    }

    // ── FTP ──

    #[test]
    fn a_good_ftp_server_passes_every_step_and_the_test_file_is_gone_afterwards() {
        let srv = fake_ftp(good_ftp());
        let r = run_test(&request(srv.port, Protocol::Ftp));
        assert!(r.ok, "{}", r.to_text());
        assert_eq!(
            outcomes(&r),
            all(&[(Step::Resolve, "ok"), (Step::Connect, "ok"), (Step::Login, "ok"), (Step::Folder, "ok"), (Step::Upload, "ok"), (Step::Download, "ok"), (Step::Compare, "ok"), (Step::Delete, "ok")])
        );
        assert!(srv.state.lock().unwrap().files.is_empty(), "the file that was put was deleted");
        assert!(r.file_left_behind.is_none());
        let log = srv.state.lock().unwrap().commands.clone();
        let commands = log.join(" | ");
        assert!(commands.contains("CWD incoming") && commands.contains("STOR nikon-fleet-test-") && commands.contains("DELE nikon-fleet-test-"), "{commands}");
        // Binary transfer is asked for, and before any file moves: in ASCII this fake (like a real server) rewrites line
        // endings, so the comparison below passing is also evidence that the type was set in time.
        let at = |verb: &str| log.iter().position(|c| c.starts_with(verb)).unwrap_or_else(|| panic!("no {verb} in {commands}"));
        assert!(log.iter().any(|c| c == "TYPE I") && at("TYPE I") < at("STOR"), "{commands}");
        let text = r.to_text();
        assert!(text.contains("result: the server accepted this profile") && !text.contains("s3cret!"), "{text}");
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("s3cret!"), "no password in the report");
    }

    #[test]
    fn a_client_that_forgot_binary_transfer_would_be_caught_by_the_fake_server() {
        // Proves the fake is not byte-transparent: the same file put and fetched in ASCII type does not come back equal,
        // which is what `transfer_type(Binary)` in the client prevents. (Without this, "the comparison passed" would say
        // nothing about the type.)
        let srv = fake_ftp(good_ftp());
        let mut s = suppaftp::FtpStream::connect(("127.0.0.1", srv.port)).unwrap();
        s.login("fleet", "s3cret!").unwrap();
        let body = test_file_body("x");
        s.put_file("ascii.txt", &mut Cursor::new(body.clone())).unwrap();
        let back = s.retr_as_buffer("ascii.txt").unwrap().into_inner();
        assert_ne!(back, body, "ASCII type rewrote the line endings");
        let _ = s.quit();
    }

    #[test]
    fn without_the_file_check_no_file_is_put_and_the_file_steps_say_so() {
        let srv = fake_ftp(good_ftp());
        let r = run_test(&TestRequest { file_check: false, ..request(srv.port, Protocol::Ftp) });
        assert!(r.ok, "{}", r.to_text());
        assert!(srv.state.lock().unwrap().commands.iter().all(|c| !c.starts_with("STOR")));
        assert_eq!(outcomes(&r).iter().filter(|(_, o)| o == "skipped").count(), 4);
        assert!(r.steps.iter().any(|s| matches!(&s.outcome, StepOutcome::Skipped { because } if because == "not asked for")));
        // A pass that never put a file must not claim the server accepted the profile.
        assert!(r.ok && !r.fully_tested());
        let text = r.to_text();
        assert!(!text.contains("result: the server accepted this profile") && text.contains("result: the login and the folder passed") && text.contains("writing to the folder is not tested"), "{text}");
    }

    #[test]
    fn a_refused_login_is_login_refused_and_stops_there_with_the_servers_words() {
        let srv = fake_ftp(good_ftp());
        let r = run_test(&TestRequest { password: "wrong".into(), ..request(srv.port, Protocol::Ftp) });
        assert!(!r.ok);
        assert_eq!(outcomes(&r)[2], (Step::Login, "failed:login_refused".to_string()));
        assert!(outcomes(&r)[3..].iter().all(|(_, o)| o == "skipped"), "{:?}", outcomes(&r));
        let StepOutcome::Failed { message, hint, .. } = &r.failure().unwrap().outcome else { panic!() };
        assert!(message.contains("530") && message.contains("Login incorrect"), "{message}");
        assert!(hint.contains("user name and password") && !hint.contains("wrong"), "{hint}");
        assert!(srv.state.lock().unwrap().files.is_empty());
    }

    #[test]
    fn a_missing_folder_is_folder_missing_and_the_folder_boundaries_are_the_servers() {
        let srv = fake_ftp(good_ftp());
        let r = run_test(&TestRequest { folder: "nope".into(), ..request(srv.port, Protocol::Ftp) });
        assert_eq!(outcomes(&r)[3], (Step::Folder, "failed:folder_missing".to_string()));
        assert!(outcomes(&r)[4..].iter().all(|(_, o)| o == "skipped"));
        // an absolute path the server has; and no folder at all means the login directory
        assert!(run_test(&TestRequest { folder: "/incoming/raw".into(), ..request(srv.port, Protocol::Ftp) }).ok);
        let r = run_test(&TestRequest { folder: "".into(), ..request(srv.port, Protocol::Ftp) });
        assert!(r.ok, "{}", r.to_text());
        let StepOutcome::Ok { detail } = &r.steps[3].outcome else { panic!() };
        assert!(detail.contains("login directory"), "{detail}");
        assert!(run_test(&TestRequest { folder: "   ".into(), ..request(srv.port, Protocol::Ftp) }).ok, "a blank folder is no folder");
        // surrounding blanks are not part of the folder name (the same for SFTP, which trims)
        let r = run_test(&TestRequest { folder: " incoming ".into(), ..request(srv.port, Protocol::Ftp) });
        assert!(r.ok, "{}", r.to_text());
        assert!(srv.state.lock().unwrap().commands.iter().any(|c| c == "CWD incoming"), "{:?}", srv.state.lock().unwrap().commands);
    }

    #[test]
    fn a_folder_that_cannot_be_written_to_is_permission_denied_and_nothing_is_left() {
        let srv = fake_ftp(FtpBehaviour { deny_store: true, ..good_ftp() });
        let r = run_test(&request(srv.port, Protocol::Ftp));
        assert_eq!(outcomes(&r)[4], (Step::Upload, "failed:permission_denied".to_string()));
        assert_eq!(outcomes(&r)[5..], all(&[(Step::Download, "skipped"), (Step::Compare, "skipped"), (Step::Delete, "skipped")])[..]);
        assert!(r.file_left_behind.is_none());
        // "Nothing left" is checked from the server's side, and a refused write is not followed by a delete it would
        // also refuse: the server saw a STOR and no DELE.
        let st = srv.state.lock().unwrap();
        assert!(st.files.is_empty() && st.commands.iter().any(|c| c.starts_with("STOR")) && !st.commands.iter().any(|c| c.starts_with("DELE")), "{:?}", st.commands);
    }

    #[test]
    fn a_file_that_comes_back_different_is_a_data_mismatch_and_is_still_deleted() {
        let srv = fake_ftp(FtpBehaviour { corrupt_retr: true, ..good_ftp() });
        let r = run_test(&request(srv.port, Protocol::Ftp));
        assert_eq!(outcomes(&r)[6], (Step::Compare, "failed:data_mismatch".to_string()));
        assert_eq!(outcomes(&r)[7], (Step::Delete, "ok".to_string()), "the cleanup still runs after a failed comparison");
        assert!(srv.state.lock().unwrap().files.is_empty());
    }

    #[test]
    fn a_file_that_cannot_be_deleted_is_named_so_it_can_be_removed_by_hand() {
        let srv = fake_ftp(FtpBehaviour { deny_delete: true, ..good_ftp() });
        let r = run_test(&request(srv.port, Protocol::Ftp));
        assert_eq!(outcomes(&r)[7], (Step::Delete, "failed:permission_denied".to_string()));
        let left = r.file_left_behind.clone().expect("named");
        assert!(left.starts_with("nikon-fleet-test-") && srv.state.lock().unwrap().files.contains_key(&left));
        assert!(r.to_text().contains(&left) && r.to_text().contains("delete it by hand"));
        assert!(r.to_text().contains("folder incoming"), "the warning says where: {}", r.to_text());
    }

    fn kind_of(r: &TestReport, step: Step) -> Option<FailKind> {
        r.steps.iter().find(|s| s.step == step).and_then(|s| match &s.outcome {
            StepOutcome::Failed { kind, .. } => Some(*kind),
            _ => None,
        })
    }

    fn code_of(r: &TestReport, step: Step) -> Option<u16> {
        r.steps.iter().find(|s| s.step == step).and_then(|s| match &s.outcome {
            StepOutcome::Failed { reply_code, .. } => *reply_code,
            _ => None,
        })
    }

    #[test]
    fn a_failed_upload_that_may_have_left_part_of_a_file_is_cleaned_up_or_named() {
        // The server keeps what arrived and then aborts the transfer (426): a partial file is on the server. The upload
        // failed, and the test still removes it.
        let srv = fake_ftp(FtpBehaviour { partial_stor: true, ..good_ftp() });
        let r = run_test(&request(srv.port, Protocol::Ftp));
        assert_eq!(outcomes(&r)[4], (Step::Upload, "failed:transfer_failed".to_string()), "{}", r.to_text());
        assert_eq!(outcomes(&r)[5..], all(&[(Step::Download, "skipped"), (Step::Compare, "skipped"), (Step::Delete, "ok")])[..], "{}", r.to_text());
        assert!(srv.state.lock().unwrap().files.is_empty(), "the partial file was removed");
        assert!(r.file_left_behind.is_none());

        // The same, but the server will not delete it: the file is named so it can be removed by hand.
        let srv = fake_ftp(FtpBehaviour { partial_stor: true, deny_delete: true, ..good_ftp() });
        let r = run_test(&request(srv.port, Protocol::Ftp));
        assert_eq!(outcomes(&r)[7], (Step::Delete, "failed:permission_denied".to_string()), "{}", r.to_text());
        let left = r.file_left_behind.clone().expect("a partial upload that cannot be deleted is named");
        assert!(srv.state.lock().unwrap().files.contains_key(&left));
    }

    #[test]
    fn an_upload_that_stalls_is_a_timeout_and_the_file_it_may_have_left_is_named() {
        let srv = fake_ftp(FtpBehaviour { stall_stor: true, ..good_ftp() });
        let started = Instant::now();
        let r = run_test(&TestRequest { step_timeout: Duration::from_millis(500), ..request(srv.port, Protocol::Ftp) });
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert_eq!(kind_of(&r, Step::Upload), Some(FailKind::TimedOut), "{}", r.to_text());
        // the clean-up was tried, the stalled server did not answer it, and so the name is given
        assert!(r.file_left_behind.as_deref().is_some_and(|f| f.starts_with("nikon-fleet-test-")), "{}", r.to_text());
    }

    // ── time limits on the data connection ──

    #[test]
    fn a_data_connection_that_goes_quiet_is_a_timeout_within_the_step_limit() {
        // The server accepts the data connection for a fetch and then sends nothing. The library's own connection has no
        // read limit, so without ours this waits for the server to close it (STALL).
        let srv = fake_ftp(FtpBehaviour { stall_retr: true, ..good_ftp() });
        let limit = Duration::from_millis(500);
        let started = Instant::now();
        let r = run_test(&TestRequest { step_timeout: limit, ..request(srv.port, Protocol::Ftp) });
        let took = started.elapsed();
        assert!(took < Duration::from_secs(5) && took < STALL, "{took:?}");
        assert_eq!(kind_of(&r, Step::Download), Some(FailKind::TimedOut), "{}", r.to_text());
        assert_eq!(outcomes(&r)[6], (Step::Compare, "skipped".to_string()));
    }

    #[test]
    fn the_sockets_this_module_opens_carry_the_step_limit_in_both_directions() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let s = timed_connect(&addr, Duration::from_millis(1234)).unwrap();
        assert_eq!((s.read_timeout().unwrap(), s.write_timeout().unwrap()), (Some(Duration::from_millis(1234)), Some(Duration::from_millis(1234))));
        // a zero limit is refused by the operating system's API rather than meaning "none": no socket comes back
        assert!(timed_connect(&addr, Duration::ZERO).is_err());
        // and a run with such a limit fails at once instead of waiting
        let started = Instant::now();
        let r = run_test(&TestRequest { step_timeout: Duration::ZERO, ..request(addr.port(), Protocol::Ftp) });
        assert!(!r.ok && started.elapsed() < Duration::from_secs(3), "{}", r.to_text());
    }

    // ── the test file is read back with a limit ──

    #[test]
    fn what_is_read_back_is_limited_at_the_cap_and_the_cap_itself_is_not_too_much() {
        // below, at and above the cap; and empty
        for (len, over) in [(0usize, false), (9, false), (10, false), (11, true), (12, true)] {
            let (kept, more) = limit_to(vec![7; len], 10);
            assert_eq!((kept.len(), more), (len.min(10), over), "{len} bytes against a cap of 10");
        }
        // a reader that would give 100 times the cap gives up what it was asked for and no more
        let mut flood = std::io::repeat(b'x').take(1000);
        let (kept, more) = read_capped(&mut flood, 10).unwrap();
        assert_eq!((kept.len(), more), (10, true));
        assert_eq!(flood.limit(), 989, "only cap + 1 = 11 bytes were taken from the source");
        // exactly the cap is not "more"
        let (kept, more) = read_capped(&mut Cursor::new(vec![1u8; 10]), 10).unwrap();
        assert_eq!((kept.len(), more), (10, false));
    }

    #[test]
    fn a_server_that_sends_far_more_than_was_stored_is_a_data_mismatch_not_a_big_buffer() {
        let srv = fake_ftp(FtpBehaviour { oversize_retr: true, ..good_ftp() });
        let r = run_test(&request(srv.port, Protocol::Ftp));
        let StepOutcome::Failed { kind, message, .. } = &r.steps[6].outcome else { panic!("{}", r.to_text()) };
        assert_eq!((r.steps[6].step, *kind), (Step::Compare, FailKind::DataMismatch));
        assert!(message.contains(&format!("more than {MAX_TEST_FILE_BYTES} bytes")), "{message}");
    }

    // ── what a reply means ──

    #[test]
    fn reply_codes_mean_what_the_table_says_and_an_unlisted_one_is_other() {
        use FailKind as K;
        use Step::*;
        for (step, code, want) in [
            // login: only 430/530 are a refused name or password; 421 is a server that will not serve
            (Login, 530, K::LoginRefused),
            (Login, 430, K::LoginRefused),
            (Login, 421, K::ServerUnavailable),
            (Login, 531, K::Other),
            (Login, 332, K::Other),
            (Login, 500, K::Other),
            // folder
            (Folder, 550, K::FolderMissing),
            (Folder, 450, K::FolderMissing),
            (Folder, 421, K::ServerUnavailable),
            (Folder, 530, K::Other),
            (Folder, 551, K::Other),
            // a transfer step: 550/553 a refusal, 450-452/552/425/426 a failed transfer
            (Upload, 550, K::PermissionDenied),
            (Upload, 553, K::PermissionDenied),
            (Upload, 552, K::TransferFailed),
            (Upload, 452, K::TransferFailed),
            (Upload, 426, K::TransferFailed),
            (Upload, 421, K::ServerUnavailable),
            (Upload, 530, K::Other),
            (Download, 550, K::PermissionDenied),
            (Download, 451, K::TransferFailed),
            (Delete, 550, K::PermissionDenied),
            (Delete, 450, K::TransferFailed),
            // the greeting
            (Connect, 421, K::ServerUnavailable),
            (Connect, 530, K::Other),
            (Connect, 550, K::Other),
        ] {
            assert_eq!(classify_reply(step, code), want, "{step:?} {code}");
        }
    }

    #[test]
    fn a_421_is_a_server_that_will_not_serve_not_a_wrong_password_and_not_a_missing_folder() {
        let srv = fake_ftp(FtpBehaviour { pass_reply: Some("421 Too many users logged in."), ..good_ftp() });
        let r = run_test(&request(srv.port, Protocol::Ftp));
        assert_eq!(outcomes(&r)[2], (Step::Login, "failed:server_unavailable".to_string()), "{}", r.to_text());
        assert_eq!(code_of(&r, Step::Login), Some(421));
        let StepOutcome::Failed { hint, message, .. } = &r.failure().unwrap().outcome else { panic!() };
        assert!(!hint.contains("user name and password") && message.contains("Too many users"), "{hint} / {message}");

        let srv = fake_ftp(FtpBehaviour { cwd_reply: Some("421 Service not available, closing control connection."), ..good_ftp() });
        let r = run_test(&request(srv.port, Protocol::Ftp));
        assert_eq!(outcomes(&r)[3], (Step::Folder, "failed:server_unavailable".to_string()), "{}", r.to_text());

        // an unlisted code at the login is Other, with the server's words and its code
        let srv = fake_ftp(FtpBehaviour { pass_reply: Some("503 Bad sequence of commands."), ..good_ftp() });
        let r = run_test(&request(srv.port, Protocol::Ftp));
        assert_eq!((kind_of(&r, Step::Login), code_of(&r, Step::Login)), (Some(FailKind::Other), Some(503)), "{}", r.to_text());
    }

    #[test]
    fn the_reply_code_is_a_field_in_the_json_and_absent_when_there_was_no_reply() {
        let srv = fake_ftp(good_ftp());
        let r = run_test(&TestRequest { password: "wrong".into(), ..request(srv.port, Protocol::Ftp) });
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["steps"][2]["reply_code"], 530, "{v}");
        assert!(v["steps"][0].get("reply_code").is_none() && v["steps"][1].get("reply_code").is_none());
        // refused connection: a failure with no reply, so no code
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let v = serde_json::to_value(run_test(&request(port, Protocol::Ftp))).unwrap();
        assert!(v["steps"][1].get("reply_code").is_none(), "{v}");
    }

    #[test]
    fn a_connection_that_dies_is_the_wrong_protocol_before_the_login_and_a_network_fault_after_it() {
        let before = fake_ftp(FtpBehaviour { hang_up_on: Some("USER"), ..good_ftp() });
        let r = run_test(&request(before.port, Protocol::Ftp));
        assert_eq!(kind_of(&r, Step::Login), Some(FailKind::NotThisProtocol), "{}", r.to_text());
        let after = fake_ftp(FtpBehaviour { hang_up_on: Some("CWD"), ..good_ftp() });
        let r = run_test(&request(after.port, Protocol::Ftp));
        assert_eq!(kind_of(&r, Step::Folder), Some(FailKind::TransferFailed), "{}", r.to_text());
    }

    // ── the password ──

    #[test]
    fn a_server_that_echoes_the_password_does_not_get_it_into_the_report() {
        let srv = fake_ftp(FtpBehaviour { echo_password: true, ..good_ftp() });
        let r = run_test(&TestRequest { password: "hunter2-not-the-real-one".into(), ..request(srv.port, Protocol::Ftp) });
        let StepOutcome::Failed { message, .. } = &r.failure().expect("refused").outcome else { panic!() };
        assert!(message.contains("530") && message.contains(PASSWORD_HIDDEN), "{message}");
        let everything = format!("{}{}{r:?}", r.to_text(), serde_json::to_string(&r).unwrap());
        assert!(!everything.contains("hunter2"), "{everything}");
    }

    #[test]
    fn scrubbing_replaces_every_copy_and_does_nothing_for_an_empty_password() {
        let mut r = TestReport {
            host: "h".into(),
            port: 21,
            protocol: Protocol::Ftp,
            folder: String::new(),
            steps: vec![
                StepResult { step: Step::Connect, outcome: StepOutcome::Ok { detail: "said pw pw".into() }, millis: 0 },
                StepResult { step: Step::Login, outcome: StepOutcome::Failed { kind: FailKind::Other, message: "pw!".into(), hint: "h".into(), reply_code: None }, millis: 0 },
                StepResult { step: Step::Folder, outcome: StepOutcome::Skipped { because: "pw".into() }, millis: 0 },
            ],
            ok: false,
            host_key: Some("pw".into()),
            file_left_behind: Some("pw".into()),
        };
        let untouched = r.clone();
        r.scrub("");
        assert_eq!(r, untouched, "an empty password is not a pattern");
        r.scrub("pw");
        let all = format!("{r:?}");
        assert!(!all.contains("\"pw\"") && !all.contains("pw pw") && all.matches(PASSWORD_HIDDEN).count() == 6, "{all}");
    }

    // ── the report's own words ──

    #[test]
    fn the_verdict_says_what_was_tried() {
        let step = |outcome| StepResult { step: Step::Upload, outcome, millis: 0 };
        let mut r = TestReport { host: "h".into(), port: 1, protocol: Protocol::Ftp, folder: String::new(), steps: vec![step(StepOutcome::Ok { detail: String::new() })], ok: true, host_key: None, file_left_behind: None };
        assert!((r.fully_tested(), r.verdict()) == (true, "the server accepted this profile"));
        r.steps.push(step(StepOutcome::Skipped { because: "x".into() }));
        assert!(r.ok && !r.fully_tested() && r.verdict().contains("not tested"));
        r.ok = false;
        assert_eq!((r.fully_tested(), r.verdict()), (false, "the test did not pass"));
    }

    // ── choreography ──

    #[test]
    fn skipping_the_rest_skips_exactly_the_steps_with_no_outcome_for_every_prefix_of_both_plans() {
        for protocol in [Protocol::Ftp, Protocol::Sftp] {
            let p = plan(protocol);
            for done in 0..=p.len() {
                let mut rec = Recorder::new(protocol);
                for s in &p[..done] {
                    rec.ok(*s, "d");
                }
                rec.skip_rest();
                let got: Vec<(Step, bool)> = rec.steps.iter().map(|r| (r.step, matches!(r.outcome, StepOutcome::Skipped { .. }))).collect();
                let want: Vec<(Step, bool)> = p.iter().enumerate().map(|(i, s)| (*s, i >= done)).collect();
                assert_eq!(got, want, "{protocol:?}, {done} done");
            }
        }
        // a step recorded out of the usual run (the clean-up after a failed compare) is not skipped again
        let mut rec = Recorder::new(Protocol::Ftp);
        for s in [Step::Resolve, Step::Connect, Step::Login, Step::Folder, Step::Upload] {
            rec.ok(s, "d");
        }
        rec.skip_after_refused_upload();
        assert_eq!(rec.steps.iter().filter(|r| r.step == Step::Upload).count(), 1);
        assert!(matches!(&rec.steps.last().unwrap().outcome, StepOutcome::Skipped { because } if because == "nothing was put there"));
        assert!(matches!(&rec.steps[rec.steps.len() - 2].outcome, StepOutcome::Skipped { because } if because == "an earlier step failed"));
    }

    #[test]
    fn every_address_that_was_tried_is_in_the_failure_and_the_last_one_decides_the_kind() {
        let mut t = ConnectTries::default();
        assert!(matches!(t.0.len(), 0));
        t.note(fail(FailKind::Unreachable, "[::1]:21: no route"));
        t.note(fail(FailKind::ConnectionRefused, "127.0.0.1:21: refused"));
        let f = t.into_failure();
        assert_eq!(f.kind, FailKind::ConnectionRefused);
        assert_eq!(f.message, "[::1]:21: no route; 127.0.0.1:21: refused");
        let one = { let mut t = ConnectTries::default(); t.note(fail(FailKind::TimedOut, "a")); t.into_failure() };
        assert_eq!((one.kind, one.message.as_str()), (FailKind::TimedOut, "a"));
        assert_eq!(ConnectTries::default().into_failure().kind, FailKind::Other);
    }

    #[test]
    fn nothing_listening_is_connection_refused_and_an_unknown_name_is_name_not_found() {
        // a port that was just free
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        for protocol in [Protocol::Ftp, Protocol::Sftp] {
            let r = run_test(&request(port, protocol));
            assert_eq!(outcomes(&r)[0], (Step::Resolve, "ok".to_string()));
            assert_eq!(outcomes(&r)[1], (Step::Connect, "failed:connection_refused".to_string()), "{protocol:?}: {}", r.to_text());
            assert!(outcomes(&r)[2..].iter().all(|(_, o)| o == "skipped"));
            let StepOutcome::Failed { message, .. } = &r.failure().unwrap().outcome else { panic!() };
            assert!(message.contains("127.0.0.1"), "the address that was tried is named: {message}");
        }
        for protocol in [Protocol::Ftp, Protocol::Sftp] {
            let r = run_test(&TestRequest { host: "no-such-host.invalid".into(), ..request(port, protocol) });
            assert_eq!(outcomes(&r)[0], (Step::Resolve, "failed:name_not_found".to_string()), "{protocol:?}");
            assert!(outcomes(&r)[1..].iter().all(|(_, o)| o == "skipped"));
        }
    }

    #[test]
    fn a_server_that_says_nothing_is_a_timeout_not_a_hang() {
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = silent.local_addr().unwrap().port();
        let _keep = std::thread::spawn(move || {
            let held: Vec<_> = silent.incoming().flatten().collect();
            std::thread::sleep(Duration::from_secs(8));
            drop(held);
        });
        let started = Instant::now();
        let r = run_test(&TestRequest { step_timeout: Duration::from_millis(400), ..request(port, Protocol::Ftp) });
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
        assert!(!r.ok);
        // A read that gives up is reported as a timeout, never as "not this protocol".
        assert_eq!(outcomes(&r)[1], (Step::Connect, "failed:timed_out".to_string()), "{}", r.to_text());
        // and the same for SSH: a socket that connects and never speaks
        let started = Instant::now();
        let r = run_test(&TestRequest { step_timeout: Duration::from_millis(400), ..request(port, Protocol::Sftp) });
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
        assert_eq!(outcomes(&r)[2], (Step::Handshake, "failed:timed_out".to_string()), "{}", r.to_text());
        let StepOutcome::Failed { message, .. } = &r.failure().unwrap().outcome else { panic!() };
        assert!(message.contains("more than 0.4 s"), "a limit under a second is not shown as 0: {message}");
    }

    #[test]
    fn limits_are_worded_as_they_are() {
        for (d, want) in [(Duration::from_secs(15), "15"), (Duration::from_secs(1), "1"), (Duration::from_millis(400), "0.4"), (Duration::from_millis(1500), "1.5"), (Duration::ZERO, "0")] {
            assert_eq!(limit_words(d), want, "{d:?}");
        }
    }

    #[test]
    fn something_that_is_not_an_ftp_server_is_reported_as_such() {
        let srv = fake_ftp(FtpBehaviour { greeting: Some("SSH-2.0-OpenSSH_9.0"), ..good_ftp() });
        let r = run_test(&request(srv.port, Protocol::Ftp));
        assert!(!r.ok);
        // exactly this kind, at the greeting: not "Other", and not a login failure
        assert_eq!(outcomes(&r)[1], (Step::Connect, "failed:not_this_protocol".to_string()), "{}", r.to_text());
        // a server that accepts and hangs up at once is the same
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || l.incoming().flatten().for_each(drop));
        let r = run_test(&request(port, Protocol::Ftp));
        assert_eq!(outcomes(&r)[1], (Step::Connect, "failed:not_this_protocol".to_string()), "{}", r.to_text());
    }

    #[test]
    fn every_failure_kind_has_a_hint_that_names_what_to_check() {
        // Each kind's hint must say the thing that sets it apart from the others. The match has no wildcard: a new kind
        // has to be listed here with the word its hint is about, and every hint has to differ from every other.
        let kinds = [
            FailKind::NameNotFound,
            FailKind::ConnectionRefused,
            FailKind::Unreachable,
            FailKind::TimedOut,
            FailKind::NotThisProtocol,
            FailKind::HandshakeFailed,
            FailKind::LoginRefused,
            FailKind::LoginMethodNotOffered,
            FailKind::FolderMissing,
            FailKind::PermissionDenied,
            FailKind::ServerUnavailable,
            FailKind::TransferFailed,
            FailKind::DataMismatch,
            FailKind::Other,
        ];
        for k in kinds {
            let about = match k {
                FailKind::NameNotFound => "host name",
                FailKind::ConnectionRefused => "port",
                FailKind::Unreachable => "no route",
                FailKind::TimedOut => "in time",
                FailKind::NotThisProtocol => "protocol",
                FailKind::HandshakeFailed => "handshake",
                FailKind::LoginRefused => "user name and password",
                FailKind::LoginMethodNotOffered => "does not offer password login",
                FailKind::FolderMissing => "folder",
                FailKind::PermissionDenied => "not allowed",
                FailKind::ServerUnavailable => "would not serve",
                FailKind::TransferFailed => "data connection",
                FailKind::DataMismatch => "different from what was sent",
                FailKind::Other => "See the message",
            };
            assert!(k.hint().contains(about), "{k:?}: {:?} should be about {about:?}", k.hint());
        }
        let hints: BTreeSet<&str> = kinds.iter().map(|k| k.hint()).collect();
        assert_eq!(hints.len(), kinds.len(), "two kinds share a hint");
        assert!(FailKind::Unreachable.hint().contains("may simply not be on it") && FailKind::TimedOut.hint().contains("cannot tell them apart"), "the hints say what this test cannot know");
        assert!(FailKind::NameNotFound.hint().contains("online"), "an offline computer is told so, since a resolver failure cannot be told from a missing name");
    }

    #[test]
    fn a_request_from_a_stored_profile_takes_the_servers_port_not_the_second_port() {
        use crate::ftp_edit::ProfileView;
        let v = ProfileView {
            name: "n".into(),
            wifi_name: "w".into(),
            wifi_key: "k".into(),
            host: "h.example".into(),
            folder: "/in".into(),
            username: "u".into(),
            password: "p".into(),
            port: 2222,
            protocol: Protocol::Sftp,
            proxy_port: 8080,
        };
        let r = TestRequest::from_profile(&v, true, Duration::from_secs(9));
        assert_eq!((r.host.as_str(), r.port, r.protocol, r.folder.as_str(), r.username.as_str(), r.password.as_str(), r.file_check), ("h.example", 2222, Protocol::Sftp, "/in", "u", "p", true));
        assert_eq!(r.step_timeout, Duration::from_secs(9));
        assert!(!TestRequest::from_profile(&v, false, Duration::from_secs(1)).file_check);
    }

    #[test]
    fn the_request_never_shows_its_password_in_debug_output() {
        let d = format!("{:?}", request(21, Protocol::Ftp));
        assert!(!d.contains("s3cret!") && d.contains("hidden"), "{d}");
    }

    // ── SFTP, against an in-process server ──

    #[derive(Clone)]
    struct SftpBehaviour {
        user: &'static str,
        pass: &'static str,
        dirs: Vec<&'static str>,
        deny_write: bool,
        corrupt_read: bool,
        deny_delete: bool,
        /// Give far more than was stored when the file is read.
        oversize_read: bool,
        /// Answer STAT with a file type (a folder, a file, or no such file). Off: the answer carries a size and no type.
        report_types: bool,
        /// The login methods the server lists and honours (its russh `methods`). A password is only checked when
        /// `Password` is among them, as a real server would.
        methods: &'static [russh::MethodKind],
        /// Accept the "none" probe: a server that asks for no authentication.
        accept_none: bool,
        /// Answer a correct password with "accepted, but more authentication is needed" (partial success), offering
        /// `methods` as the way on.
        partial_after_password: bool,
    }

    #[derive(Default)]
    struct SftpFs {
        files: BTreeMap<String, Vec<u8>>,
        ops: Vec<String>,
    }

    struct FakeSftp {
        port: u16,
        fs: Arc<Mutex<SftpFs>>,
        _runtime: std::thread::JoinHandle<()>,
    }

    struct SshConn {
        b: SftpBehaviour,
        fs: Arc<Mutex<SftpFs>>,
        channels: Arc<tokio::sync::Mutex<std::collections::HashMap<russh::ChannelId, russh::Channel<russh::server::Msg>>>>,
    }

    impl russh::server::Handler for SshConn {
        type Error = anyhow::Error;

        async fn auth_none(&mut self, _user: &str) -> Result<russh::server::Auth, Self::Error> {
            Ok(if self.b.accept_none { russh::server::Auth::Accept } else { russh::server::Auth::reject() })
        }

        async fn auth_password(&mut self, user: &str, password: &str) -> Result<russh::server::Auth, Self::Error> {
            let offered = self.b.methods.contains(&russh::MethodKind::Password);
            Ok(if offered && user == self.b.user && password == self.b.pass {
                if self.b.partial_after_password {
                    russh::server::Auth::Reject { proceed_with_methods: Some(russh::MethodSet::from(&[russh::MethodKind::KeyboardInteractive][..])), partial_success: true }
                } else {
                    russh::server::Auth::Accept
                }
            } else {
                russh::server::Auth::Reject { proceed_with_methods: None, partial_success: false }
            })
        }

        async fn channel_open_session(&mut self, channel: russh::Channel<russh::server::Msg>, reply: russh::server::ChannelOpenHandle, _s: &mut russh::server::Session) -> Result<(), Self::Error> {
            self.channels.lock().await.insert(channel.id(), channel);
            reply.accept().await;
            Ok(())
        }

        async fn subsystem_request(&mut self, id: russh::ChannelId, name: &str, session: &mut russh::server::Session) -> Result<(), Self::Error> {
            if name == "sftp" {
                let channel = self.channels.lock().await.remove(&id).unwrap();
                session.channel_success(id)?;
                let h = SftpConn { b: self.b.clone(), fs: self.fs.clone(), open: BTreeSet::new() };
                russh_sftp::server::run(channel.into_stream(), h).await;
            } else {
                session.channel_failure(id)?;
            }
            Ok(())
        }
    }

    struct SftpConn {
        b: SftpBehaviour,
        fs: Arc<Mutex<SftpFs>>,
        open: BTreeSet<String>,
    }

    fn ok_status(id: u32) -> russh_sftp::protocol::Status {
        russh_sftp::protocol::Status { id, status_code: russh_sftp::protocol::StatusCode::Ok, error_message: "Ok".into(), language_tag: "en-US".into() }
    }

    impl russh_sftp::server::Handler for SftpConn {
        type Error = russh_sftp::protocol::StatusCode;

        fn unimplemented(&self) -> Self::Error {
            russh_sftp::protocol::StatusCode::OpUnsupported
        }

        async fn realpath(&mut self, id: u32, path: String) -> Result<russh_sftp::protocol::Name, Self::Error> {
            let p = if path == "." || path.is_empty() { "/".to_string() } else if path.starts_with('/') { path } else { format!("/{path}") };
            Ok(russh_sftp::protocol::Name { id, files: vec![russh_sftp::protocol::File::dummy(p)] })
        }

        async fn opendir(&mut self, id: u32, path: String) -> Result<russh_sftp::protocol::Handle, Self::Error> {
            self.fs.lock().unwrap().ops.push(format!("opendir {path}"));
            let bare = path.trim_matches('/');
            if bare.is_empty() || self.b.dirs.contains(&bare) {
                Ok(russh_sftp::protocol::Handle { id, handle: path })
            } else {
                Err(russh_sftp::protocol::StatusCode::NoSuchFile)
            }
        }

        async fn readdir(&mut self, _id: u32, _handle: String) -> Result<russh_sftp::protocol::Name, Self::Error> {
            Err(russh_sftp::protocol::StatusCode::Eof)
        }

        async fn open(&mut self, id: u32, filename: String, pflags: russh_sftp::protocol::OpenFlags, _a: russh_sftp::protocol::FileAttributes) -> Result<russh_sftp::protocol::Handle, Self::Error> {
            let writing = pflags.contains(russh_sftp::protocol::OpenFlags::WRITE) || pflags.contains(russh_sftp::protocol::OpenFlags::CREATE);
            if writing {
                if self.b.deny_write {
                    return Err(russh_sftp::protocol::StatusCode::PermissionDenied);
                }
                self.fs.lock().unwrap().files.insert(filename.clone(), Vec::new());
            } else if !self.fs.lock().unwrap().files.contains_key(&filename) {
                return Err(russh_sftp::protocol::StatusCode::NoSuchFile);
            }
            self.open.insert(filename.clone());
            self.fs.lock().unwrap().ops.push(format!("open {filename}"));
            Ok(russh_sftp::protocol::Handle { id, handle: filename })
        }

        async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<russh_sftp::protocol::Status, Self::Error> {
            let mut fs = self.fs.lock().unwrap();
            let f = fs.files.entry(handle).or_default();
            let end = offset as usize + data.len();
            if f.len() < end {
                f.resize(end, 0);
            }
            f[offset as usize..end].copy_from_slice(&data);
            Ok(ok_status(id))
        }

        async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<russh_sftp::protocol::Data, Self::Error> {
            let fs = self.fs.lock().unwrap();
            let f = fs.files.get(&handle).ok_or(russh_sftp::protocol::StatusCode::NoSuchFile)?;
            let mut f = f.clone();
            if self.b.corrupt_read {
                f.push(b'!');
            }
            if self.b.oversize_read {
                f = vec![b'x'; 4 * MAX_TEST_FILE_BYTES as usize];
            }
            if offset as usize >= f.len() {
                return Err(russh_sftp::protocol::StatusCode::Eof);
            }
            let end = (offset as usize + len as usize).min(f.len());
            Ok(russh_sftp::protocol::Data { id, data: f[offset as usize..end].to_vec() })
        }

        async fn close(&mut self, id: u32, handle: String) -> Result<russh_sftp::protocol::Status, Self::Error> {
            self.open.remove(&handle);
            Ok(ok_status(id))
        }

        async fn remove(&mut self, id: u32, filename: String) -> Result<russh_sftp::protocol::Status, Self::Error> {
            if self.b.deny_delete {
                return Err(russh_sftp::protocol::StatusCode::PermissionDenied);
            }
            self.fs.lock().unwrap().ops.push(format!("remove {filename}"));
            match self.fs.lock().unwrap().files.remove(&filename) {
                Some(_) => Ok(ok_status(id)),
                None => Err(russh_sftp::protocol::StatusCode::NoSuchFile),
            }
        }

        async fn stat(&mut self, id: u32, path: String) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
            let size = self.fs.lock().unwrap().files.get(&path).map(|f| f.len() as u64);
            let mut attrs = russh_sftp::protocol::FileAttributes { size, ..Default::default() };
            if self.b.report_types {
                let bare = path.trim_matches('/');
                if bare.is_empty() || self.b.dirs.contains(&bare) {
                    attrs.permissions = Some(0o040755);
                } else if size.is_some() {
                    attrs.permissions = Some(0o100644);
                } else {
                    return Err(russh_sftp::protocol::StatusCode::NoSuchFile);
                }
            }
            self.fs.lock().unwrap().ops.push(format!("stat {path}"));
            Ok(russh_sftp::protocol::Attrs { id, attrs })
        }

        async fn lstat(&mut self, id: u32, path: String) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
            self.stat(id, path).await
        }

        async fn fstat(&mut self, id: u32, handle: String) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
            self.stat(id, handle).await
        }
    }

    #[derive(Clone)]
    struct SshServer {
        b: SftpBehaviour,
        fs: Arc<Mutex<SftpFs>>,
    }

    impl russh::server::Server for SshServer {
        type Handler = SshConn;
        fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> SshConn {
            SshConn { b: self.b.clone(), fs: self.fs.clone(), channels: Default::default() }
        }
    }

    fn fake_sftp(b: SftpBehaviour) -> FakeSftp {
        let fs = Arc::new(Mutex::new(SftpFs::default()));
        let (tx, rx) = std::sync::mpsc::channel();
        let server = SshServer { b, fs: fs.clone() };
        let runtime = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap().port()).unwrap();
                let config = Arc::new(russh::server::Config {
                    methods: russh::MethodSet::from(server.b.methods),
                    auth_rejection_time: Duration::from_millis(0),
                    auth_rejection_time_initial: Some(Duration::from_millis(0)),
                    keys: vec![russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).unwrap()],
                    ..Default::default()
                });
                let mut server = server;
                use russh::server::Server as _;
                let _ = server.run_on_socket(config, &listener).await;
            });
        });
        FakeSftp { port: rx.recv().unwrap(), fs, _runtime: runtime }
    }

    fn good_sftp() -> SftpBehaviour {
        SftpBehaviour { user: "fleet", pass: "s3cret!", dirs: vec!["incoming", "incoming/raw"], deny_write: false, corrupt_read: false, deny_delete: false, oversize_read: false, report_types: true, methods: &[russh::MethodKind::Password], accept_none: false, partial_after_password: false }
    }

    #[test]
    fn a_good_sftp_server_passes_every_step_reports_its_host_key_and_the_file_is_gone() {
        let srv = fake_sftp(good_sftp());
        let r = run_test(&request(srv.port, Protocol::Sftp));
        assert!(r.ok, "{}", r.to_text());
        assert_eq!(
            outcomes(&r),
            all(&[(Step::Resolve, "ok"), (Step::Connect, "ok"), (Step::Handshake, "ok"), (Step::Login, "ok"), (Step::Folder, "ok"), (Step::Upload, "ok"), (Step::Download, "ok"), (Step::Compare, "ok"), (Step::Delete, "ok")])
        );
        let key = r.host_key.clone().expect("the host key is reported");
        assert!(key.starts_with("SHA256:"), "{key}");
        assert!(srv.fs.lock().unwrap().files.is_empty(), "the file that was put was deleted");
        assert!(srv.fs.lock().unwrap().ops.iter().any(|o| o.starts_with("open /incoming/nikon-fleet-test-")), "{:?}", srv.fs.lock().unwrap().ops);
        assert!(!serde_json::to_string(&r).unwrap().contains("s3cret!"));
        assert!(r.to_text().contains("server host key SHA256:"));
    }

    fn mode(permissions: Option<u32>) -> russh_sftp::client::fs::Metadata {
        russh_sftp::protocol::FileAttributes { permissions, ..Default::default() }
    }

    #[test]
    fn an_empty_or_blank_server_name_is_never_looked_up_on_any_platform() {
        for host in ["", " ", "\t\n"] {
            let f = super::resolve(host, 21, Duration::from_secs(5)).unwrap_err();
            assert_eq!(f.kind, FailKind::NameNotFound, "{host:?}");
            assert_eq!(f.message, "no server name was given", "{host:?}: the resolver was not asked");
        }
        // a name that is there is looked up as before (no network: an address literal)
        assert!(super::resolve("127.0.0.1", 21, Duration::from_secs(5)).is_ok());
        // and the whole test stops at the first step with nothing else attempted
        let r = run_test(&TestRequest { host: "".into(), ..request(1, Protocol::Ftp) });
        assert!(!r.ok && outcomes(&r)[0] == (Step::Resolve, "failed:name_not_found".to_string()), "{}", r.to_text());
        assert!(outcomes(&r)[1..].iter().all(|(_, o)| o == "skipped"));
    }

    #[test]
    fn a_stat_is_read_for_the_file_type_bits_and_a_mode_without_them_says_nothing() {
        use super::FolderProbe::*;
        // at the type: a folder, a file, a link, and the others
        assert_eq!(super::folder_probe(&mode(Some(0o040755))), Folder);
        assert_eq!(super::folder_probe(&mode(Some(0o040000))), Folder, "no permission bits at all is still a folder");
        assert_eq!(super::folder_probe(&mode(Some(0o100644))), NotAFolder("a file, not a folder"));
        assert_eq!(super::folder_probe(&mode(Some(0o120777))), NotAFolder("a link to something that is not a folder"));
        for other in [0o020644, 0o060644, 0o010644, 0o140755] {
            assert_eq!(super::folder_probe(&mode(Some(other))), NotAFolder("a special file, not a folder"), "{other:o}");
        }
        // below: no type given at all. Some servers send only the rwx bits; that is not "not a folder".
        assert_eq!(super::folder_probe(&mode(Some(0o755))), TypeNotGiven);
        assert_eq!(super::folder_probe(&mode(Some(0))), TypeNotGiven);
        assert_eq!(super::folder_probe(&mode(None)), TypeNotGiven);
    }

    #[test]
    fn the_sftp_folder_step_asks_what_the_path_is_and_does_not_read_what_is_in_it() {
        let srv = fake_sftp(good_sftp());
        let r = run_test(&TestRequest { folder: "incoming".into(), ..request(srv.port, Protocol::Sftp) });
        assert!(r.ok, "{}", r.to_text());
        let ops = srv.fs.lock().unwrap().ops.clone();
        assert!(ops.iter().any(|o| o == "stat /incoming"), "{ops:?}");
        assert!(!ops.iter().any(|o| o.starts_with("opendir")), "the folder was opened and listed: {ops:?}");
    }

    #[test]
    fn a_server_that_gives_no_file_type_is_asked_the_old_way_by_opening_the_folder() {
        let srv = fake_sftp(SftpBehaviour { report_types: false, ..good_sftp() });
        let r = run_test(&TestRequest { folder: "incoming".into(), ..request(srv.port, Protocol::Sftp) });
        assert!(r.ok, "{}", r.to_text());
        assert!(srv.fs.lock().unwrap().ops.iter().any(|o| o == "opendir /incoming"), "{:?}", srv.fs.lock().unwrap().ops);
        // and the fallback still tells a missing folder from a good one
        let missing = run_test(&TestRequest { folder: "nope".into(), ..request(srv.port, Protocol::Sftp) });
        assert_eq!(outcomes(&missing)[4], (Step::Folder, "failed:folder_missing".to_string()), "{}", missing.to_text());
    }

    #[test]
    fn a_path_that_is_a_file_is_not_a_folder_and_nothing_is_uploaded() {
        let srv = fake_sftp(good_sftp());
        srv.fs.lock().unwrap().files.insert("/incoming/notes.txt".to_string(), b"x".to_vec());
        let r = run_test(&TestRequest { folder: "incoming/notes.txt".into(), ..request(srv.port, Protocol::Sftp) });
        assert_eq!(outcomes(&r)[4], (Step::Folder, "failed:folder_missing".to_string()), "{}", r.to_text());
        let StepOutcome::Failed { message, .. } = &r.steps[4].outcome else { panic!("{}", r.to_text()) };
        assert!(message.contains("/incoming/notes.txt is a file, not a folder"), "{message}");
        assert!(outcomes(&r)[5..].iter().all(|(_, o)| o == "skipped"));
        assert!(!srv.fs.lock().unwrap().ops.iter().any(|o| o.starts_with("open /incoming/notes.txt/")), "nothing was put inside a file");
    }

    #[test]
    fn sftp_login_folder_write_read_and_delete_failures_are_each_their_own_kind() {
        let wrong = run_test(&TestRequest { password: "wrong".into(), ..request(fake_sftp(good_sftp()).port, Protocol::Sftp) });
        assert_eq!(outcomes(&wrong)[3], (Step::Login, "failed:login_refused".to_string()), "{}", wrong.to_text());
        assert!(outcomes(&wrong)[4..].iter().all(|(_, o)| o == "skipped"));

        let folder = run_test(&TestRequest { folder: "missing".into(), ..request(fake_sftp(good_sftp()).port, Protocol::Sftp) });
        assert_eq!(outcomes(&folder)[4], (Step::Folder, "failed:folder_missing".to_string()), "{}", folder.to_text());

        let ro = run_test(&request(fake_sftp(SftpBehaviour { deny_write: true, ..good_sftp() }).port, Protocol::Sftp));
        assert_eq!(outcomes(&ro)[5], (Step::Upload, "failed:permission_denied".to_string()), "{}", ro.to_text());
        assert_eq!(outcomes(&ro)[6..], all(&[(Step::Download, "skipped"), (Step::Compare, "skipped"), (Step::Delete, "skipped")])[..]);

        let srv = fake_sftp(SftpBehaviour { corrupt_read: true, ..good_sftp() });
        let bad = run_test(&request(srv.port, Protocol::Sftp));
        assert_eq!(outcomes(&bad)[7], (Step::Compare, "failed:data_mismatch".to_string()), "{}", bad.to_text());
        assert_eq!(outcomes(&bad)[8], (Step::Delete, "ok".to_string()));

        let srv = fake_sftp(SftpBehaviour { deny_delete: true, ..good_sftp() });
        let stuck = run_test(&request(srv.port, Protocol::Sftp));
        assert_eq!(outcomes(&stuck)[8], (Step::Delete, "failed:permission_denied".to_string()));
        assert!(stuck.file_left_behind.as_deref().is_some_and(|f| srv.fs.lock().unwrap().files.contains_key(&format!("/incoming/{f}"))), "{:?}", stuck.file_left_behind);
    }

    #[test]
    fn a_refused_sftp_password_says_what_was_tried() {
        let r = run_test(&TestRequest { password: "wrong".into(), ..request(fake_sftp(good_sftp()).port, Protocol::Sftp) });
        let StepOutcome::Failed { message, .. } = &r.failure().unwrap().outcome else { panic!() };
        assert!(message.contains("did not accept that user name and password") && message.contains("password only"), "{message}");
    }

    fn login_of(r: &TestReport) -> (FailKind, String, String) {
        let StepOutcome::Failed { kind, message, hint, .. } = &r.steps[3].outcome else { panic!("the login did not fail: {}", r.to_text()) };
        assert_eq!(r.steps[3].step, Step::Login);
        (*kind, message.clone(), hint.clone())
    }

    #[test]
    fn a_wrong_password_on_a_server_that_offers_a_password_is_login_refused_whatever_else_it_offers() {
        use russh::MethodKind as M;
        for methods in [&[M::Password][..], &[M::Password, M::KeyboardInteractive][..], &[M::PublicKey, M::KeyboardInteractive, M::Password][..]] {
            let srv = fake_sftp(SftpBehaviour { methods: Box::leak(methods.to_vec().into_boxed_slice()), ..good_sftp() });
            let r = run_test(&TestRequest { password: "wrong".into(), ..request(srv.port, Protocol::Sftp) });
            let (kind, message, hint) = login_of(&r);
            assert_eq!(kind, FailKind::LoginRefused, "{methods:?}: {message}");
            assert!(message.contains("did not accept that user name and password"), "{message}");
            assert!(hint.contains("user name and password"), "{hint}");
            assert!(outcomes(&r)[4..].iter().all(|(_, o)| o == "skipped"), "{:?}", outcomes(&r));
        }
    }

    #[test]
    fn a_server_that_does_not_offer_a_password_is_login_method_not_offered_and_names_what_it_offers() {
        use russh::MethodKind as M;
        // the right user and password: the refusal is not about them, and the report must not say it is
        for (methods, named) in [
            (&[M::KeyboardInteractive][..], "keyboard-interactive"),
            (&[M::PublicKey][..], "publickey"),
            (&[M::PublicKey, M::KeyboardInteractive][..], "publickey, keyboard-interactive"),
        ] {
            let srv = fake_sftp(SftpBehaviour { methods: Box::leak(methods.to_vec().into_boxed_slice()), ..good_sftp() });
            let r = run_test(&request(srv.port, Protocol::Sftp));
            assert!(!r.ok);
            assert_eq!(outcomes(&r)[3], (Step::Login, "failed:login_method_not_offered".to_string()), "{}", r.to_text());
            let (_, message, hint) = login_of(&r);
            assert!(message.contains(&format!("it offers: {named} ")), "{message}");
            assert!(!message.contains("did not accept that user name"), "{message}");
            assert!(!message.contains("none"), "the 'none' entry is not a login method and is not named: {message}");
            assert!(hint.contains("does not offer password login") && hint.contains("Unverified"), "{hint}");
            assert!(outcomes(&r)[4..].iter().all(|(_, o)| o == "skipped"), "{:?}", outcomes(&r));
            assert!(!serde_json::to_string(&r).unwrap().contains("s3cret!"));
        }
    }

    #[test]
    fn a_server_that_asks_for_no_authentication_is_accepted_without_sending_the_password() {
        // the password given is wrong on purpose: it would be refused if it were sent
        let srv = fake_sftp(SftpBehaviour { accept_none: true, methods: &[russh::MethodKind::KeyboardInteractive], ..good_sftp() });
        let r = run_test(&TestRequest { password: "wrong".into(), ..request(srv.port, Protocol::Sftp) });
        assert!(r.ok, "{}", r.to_text());
        let StepOutcome::Ok { detail } = &r.steps[3].outcome else { panic!() };
        assert!(detail.contains("accepted") && detail.contains("asked for no authentication") && detail.contains("no password was sent"), "{detail}");
        assert!(!serde_json::to_string(&r).unwrap().contains("wrong"), "the password is not in the report");
    }

    #[test]
    fn a_server_that_lists_no_methods_and_then_drops_the_connection_is_never_reported_as_a_refused_password() {
        // russh's own server closes the connection after a probe answered with an empty list, and russh's client
        // reports a connection that closed during authentication as a refusal with an empty list (not as an error), so
        // an empty list cannot be told from a dropped connection: the password is still tried, the send then fails,
        // and the report is a failed login that does not blame the user name or password
        let srv = fake_sftp(SftpBehaviour { methods: &[], ..good_sftp() });
        let r = run_test(&request(srv.port, Protocol::Sftp));
        assert!(!r.ok);
        let (kind, message, _) = login_of(&r);
        assert_ne!(kind, FailKind::LoginRefused, "{message}");
        assert!(outcomes(&r)[4..].iter().all(|(_, o)| o == "skipped"), "{:?}", outcomes(&r));
    }

    #[test]
    fn a_password_that_is_accepted_but_needs_a_further_method_is_other_and_says_so() {
        let srv = fake_sftp(SftpBehaviour { partial_after_password: true, ..good_sftp() });
        let r = run_test(&request(srv.port, Protocol::Sftp));
        let (kind, message, _) = login_of(&r);
        assert_eq!(kind, FailKind::Other, "{message}");
        assert!(message.contains("accepted the password but wants a further method") && message.contains("keyboard-interactive"), "{message}");
        assert!(!message.contains("did not accept"), "{message}");
    }

    #[test]
    fn refused_password_decides_from_the_offered_methods_alone() {
        use russh::MethodKind as M;
        let set = |m: &[M]| russh::MethodSet::from(m);
        let k = |m: &[M], partial: bool| refused_password(&set(m), &set(&[]), partial);
        // password listed (in any position, with anything else) -> a wrong password
        assert_eq!(k(&[M::Password], false).kind, FailKind::LoginRefused);
        assert_eq!(k(&[M::PublicKey, M::Password], false).kind, FailKind::LoginRefused);
        // password not listed -> not offered, and the others are named in the server's order
        let f = k(&[M::KeyboardInteractive, M::PublicKey], false);
        assert_eq!(f.kind, FailKind::LoginMethodNotOffered);
        assert!(f.message.contains("it offers: keyboard-interactive, publickey "), "{}", f.message);
        // 'none' is not a login method: left out of the names, and a list of nothing else says nothing
        let f = k(&[M::None, M::KeyboardInteractive], false);
        assert_eq!(f.kind, FailKind::LoginMethodNotOffered);
        assert!(f.message.contains("it offers: keyboard-interactive ") && !f.message.contains("none"), "{}", f.message);
        assert_eq!(k(&[M::None], false).kind, FailKind::LoginRefused);
        assert_eq!(k(&[], false).kind, FailKind::LoginRefused);
        // partial success wins over everything, with or without a list
        assert_eq!(k(&[M::Password], true).kind, FailKind::Other);
        assert_eq!(k(&[], true).kind, FailKind::Other);
        assert!(k(&[], true).message.contains("wants a further method;"), "{}", k(&[], true).message);
        assert_eq!(k(&[M::KeyboardInteractive], true).kind, FailKind::Other);
        // what it says to go on with is named from the refusal's own list, not the probe's
        let f = refused_password(&set(&[M::Password]), &set(&[M::None, M::PublicKey]), true);
        assert!(f.message.contains("(the server lists: publickey)"), "{}", f.message);
    }

    #[test]
    fn an_sftp_server_that_sends_far_more_than_was_stored_is_a_data_mismatch() {
        let srv = fake_sftp(SftpBehaviour { oversize_read: true, ..good_sftp() });
        let r = run_test(&request(srv.port, Protocol::Sftp));
        let StepOutcome::Failed { kind, message, .. } = &r.steps[7].outcome else { panic!("{}", r.to_text()) };
        assert_eq!((r.steps[7].step, *kind), (Step::Compare, FailKind::DataMismatch));
        assert!(message.contains(&format!("more than {MAX_TEST_FILE_BYTES} bytes")), "{message}");
        assert_eq!(outcomes(&r)[8], (Step::Delete, "ok".to_string()));
    }

    #[test]
    fn the_async_read_back_holds_at_most_the_cap_plus_one_like_the_blocking_one() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        // a source 100 times the cap: the answer is the cap, and the source was not drained
        let mut flood = tokio::io::repeat(b'x').take(1000);
        let (kept, more) = rt.block_on(read_capped_async(&mut flood, 10)).unwrap();
        assert_eq!((kept.len(), more), (10, true));
        assert_eq!(flood.limit(), 989, "only cap + 1 = 11 bytes were taken from the source");
        // parity with the blocking reader, at and around the cap
        for len in [0usize, 9, 10, 11, 12] {
            let a = rt.block_on(read_capped_async(&mut Cursor::new(vec![3u8; len]), 10)).unwrap();
            let b = read_capped(&mut Cursor::new(vec![3u8; len]), 10).unwrap();
            assert_eq!(a, b, "{len} bytes");
        }
    }

    #[test]
    fn host_key_fingerprints_are_sha256_of_the_key() {
        let key = russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).unwrap();
        let public = key.public_key().clone();
        let expect = public.fingerprint(russh::keys::HashAlg::Sha256).to_string();
        let got = host_key_fingerprint(&russh::keys::PublicKeyOrCertificate::PublicKey { key: public, hash_alg: None });
        assert!(got.starts_with("SHA256:") && got == expect, "{got}");
    }

    #[test]
    fn an_sftp_test_without_the_file_check_puts_nothing_there() {
        let srv = fake_sftp(good_sftp());
        let r = run_test(&TestRequest { file_check: false, ..request(srv.port, Protocol::Sftp) });
        // The folder is looked up (a stat), but no file is opened, written or removed.
        let ops = srv.fs.lock().unwrap().ops.clone();
        assert!(r.ok && ops.iter().all(|o| o.starts_with("stat ") || o.starts_with("opendir ")), "{ops:?}\n{}", r.to_text());
        assert!(ops.iter().any(|o| o.starts_with("stat ")), "the folder was looked up: {ops:?}");
    }

    #[test]
    fn an_ftp_port_given_to_an_sftp_test_is_reported_not_hung_on() {
        let srv = fake_ftp(good_ftp());
        let started = Instant::now();
        let r = run_test(&TestRequest { step_timeout: Duration::from_millis(600), ..request(srv.port, Protocol::Sftp) });
        assert!(!r.ok && started.elapsed() < Duration::from_secs(6), "{:?}\n{}", started.elapsed(), r.to_text());
        let f = r.failure().unwrap();
        assert_eq!(f.step, Step::Handshake, "the FTP server's greeting is not an SSH banner: {}", r.to_text());
        // What actually happens (russh skips lines that are not an SSH version line and keeps waiting for one, and an
        // FTP server is waiting for a command), so the handshake times out. That is what is pinned; the hint's "nothing
        // answered" is not accurate here, and this test is where a change to a better answer would show.
        assert!(matches!(&f.outcome, StepOutcome::Failed { kind: FailKind::TimedOut, .. }), "{f:?}");
    }

    #[test]
    fn an_ssh_server_given_to_an_ftp_test_is_not_this_protocol_at_the_greeting() {
        let ssh = fake_sftp(good_sftp());
        let r = run_test(&TestRequest { step_timeout: Duration::from_millis(600), ..request(ssh.port, Protocol::Ftp) });
        assert_eq!(outcomes(&r)[1], (Step::Connect, "failed:not_this_protocol".to_string()), "{}", r.to_text());
    }

    #[test]
    fn ssh_failures_are_kinds_by_what_the_library_reports() {
        use std::io::ErrorKind as IO;
        let kind = |e: russh::Error| ssh_failure(&e).kind;
        // only a peer with no SSH version line is "not this protocol"; a closed or refused handshake is a failed one
        assert_eq!(kind(russh::Error::Version), FailKind::NotThisProtocol);
        assert_eq!(kind(russh::Error::HUP), FailKind::HandshakeFailed);
        assert_eq!(kind(russh::Error::Disconnect), FailKind::HandshakeFailed);
        assert_eq!(kind(russh::Error::Kex), FailKind::HandshakeFailed);
        assert_eq!(kind(russh::Error::NoAuthMethod), FailKind::HandshakeFailed);
        // transport errors keep their own kind
        assert_eq!(kind(russh::Error::IO(IO::ConnectionRefused.into())), FailKind::ConnectionRefused);
        assert_eq!(kind(russh::Error::IO(IO::TimedOut.into())), FailKind::TimedOut);
        assert_eq!(kind(russh::Error::IO(IO::ConnectionReset.into())), FailKind::NotThisProtocol);
    }

    #[test]
    fn io_errors_are_kinds_by_the_step_they_happened_in() {
        use std::io::ErrorKind as IO;
        let e = |k: IO| std::io::Error::from(k);
        // a connection that dies: wrong protocol until the login is accepted, a network fault after
        for k in [IO::UnexpectedEof, IO::InvalidData, IO::ConnectionReset, IO::ConnectionAborted, IO::BrokenPipe] {
            for step in [Step::Resolve, Step::Connect, Step::Handshake, Step::Login] {
                assert_eq!(io_kind(step, &e(k)), FailKind::NotThisProtocol, "{k:?} in {step:?}");
            }
            for step in [Step::Folder, Step::Upload, Step::Download, Step::Compare, Step::Delete] {
                assert_eq!(io_kind(step, &e(k)), FailKind::TransferFailed, "{k:?} in {step:?}");
            }
        }
        // the kinds that do not depend on the step
        for step in [Step::Connect, Step::Download] {
            assert_eq!(io_kind(step, &e(IO::ConnectionRefused)), FailKind::ConnectionRefused);
            assert_eq!(io_kind(step, &e(IO::TimedOut)), FailKind::TimedOut);
            assert_eq!(io_kind(step, &e(IO::WouldBlock)), FailKind::TimedOut);
            assert_eq!(io_kind(step, &e(IO::HostUnreachable)), FailKind::Unreachable);
            assert_eq!(io_kind(step, &e(IO::PermissionDenied)), FailKind::Other);
        }
    }

    #[test]
    fn an_sftp_status_means_what_the_step_that_got_it_means_by_failed() {
        use russh_sftp::client::error::Error as E;
        use russh_sftp::protocol::{Status, StatusCode as C};
        let st = |c: C| E::Status(Status { id: 1, status_code: c, error_message: "words".into(), language_tag: "en".into() });
        // a missing file is a missing FOLDER only when the step was looking for the folder
        assert_eq!(sftp_status_failure(FailKind::FolderMissing, &st(C::NoSuchFile)).kind, FailKind::FolderMissing);
        assert_eq!(sftp_status_failure(FailKind::TransferFailed, &st(C::NoSuchFile)).kind, FailKind::TransferFailed);
        assert_eq!(sftp_status_failure(FailKind::NotThisProtocol, &st(C::NoSuchFile)).kind, FailKind::NotThisProtocol);
        // a refusal is a refusal wherever it came
        for d in [FailKind::FolderMissing, FailKind::TransferFailed] {
            assert_eq!(sftp_status_failure(d, &st(C::PermissionDenied)).kind, FailKind::PermissionDenied);
            assert_eq!(sftp_status_failure(d, &st(C::ConnectionLost)).kind, FailKind::Other);
            assert_eq!(sftp_status_failure(d, &st(C::NoConnection)).kind, FailKind::Other);
        }
        assert_eq!(sftp_status_failure(FailKind::TransferFailed, &E::Timeout).kind, FailKind::TimedOut);
        let f = sftp_status_failure(FailKind::TransferFailed, &st(C::Failure));
        assert_eq!(f.kind, FailKind::TransferFailed);
        assert!(f.message.contains("words") && f.code.is_none(), "{}", f.message);
    }
}

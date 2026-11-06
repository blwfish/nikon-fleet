//! Trying an FTP or SFTP profile from this computer: does the server answer, accept the login, have the folder, and
//! take, give back and delete a small file?
//!
//! The steps, in order, each with its own outcome: look the host name up; open a connection; (SFTP) the SSH
//! handshake, which also yields the server's host key fingerprint; log in; go to the folder; and, if asked, put a
//! small uniquely-named file there, fetch it, compare it with what was sent, and delete it. A step that fails ends the
//! test, except that a file that was put is still deleted, and one that could not be deleted is named in
//! [`TestReport::file_left_behind`] so it can be removed by hand.
//!
//! What a failure says: the step, the address and port that were tried, the server's own words, and a hint in terms of
//! the camera's profile. It cannot tell a wrong host name from a server on a network this computer is not on; both are
//! "could not reach", and the hint says so.
//!
//! This tests the SERVER side only. Whether the camera can join the Wi-Fi network, and whether it behaves the same
//! (its passive-mode setting, its FTP dialect), is not tested here. Plain FTP only: the profile has no known FTPS
//! setting. The SFTP host key is not checked against anything (there is nothing to check it against): it is reported.
//!
//! The report never holds the password.

use std::io::Cursor;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub use crate::ftp_edit::Protocol;

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
    /// How long each step may take.
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
    FolderMissing,
    PermissionDenied,
    /// The transfer or the delete failed for another reason.
    TransferFailed,
    /// What came back is not what was sent.
    DataMismatch,
    Other,
}

impl FailKind {
    /// What to try, in terms of the camera's profile.
    pub fn hint(self) -> &'static str {
        match self {
            FailKind::NameNotFound => "This computer could not find that host name. Check the spelling; if it is a name only the camera's network knows, try its IP address.",
            FailKind::ConnectionRefused => "The address answered but nothing accepted the connection on that port. Check the port (21 for FTP, 22 for SFTP is usual) and that the server is running.",
            FailKind::Unreachable => "There is no route from this computer to that address. If the camera will be on another network, this computer may simply not be on it; that is not necessarily a mistake in the profile.",
            FailKind::TimedOut => "Nothing answered in time. A wrong address, a firewall, or a network this computer is not on all look like this; it cannot tell them apart.",
            FailKind::NotThisProtocol => "Something answered but it is not an FTP/SFTP server of the kind chosen. Check the protocol and port.",
            FailKind::HandshakeFailed => "The SSH handshake failed. The server may require key types or ciphers this test does not offer; the message says what it reported.",
            FailKind::LoginRefused => "The server refused this user name and password. Check both; the password is not shown here.",
            FailKind::FolderMissing => "The folder does not exist or this user cannot enter it. Check the spelling and capitals; it is relative to the login directory unless it starts with /.",
            FailKind::PermissionDenied => "This user is not allowed to do that in the folder (the test file could not be written, read or deleted). The camera needs to write there.",
            FailKind::TransferFailed => "The transfer failed. In FTP this is often the data connection (passive mode) being blocked between this computer and the server.",
            FailKind::DataMismatch => "The file came back different from what was sent; something between here and the server altered it.",
            FailKind::Other => "See the message.",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum StepOutcome {
    Ok { detail: String },
    Failed { kind: FailKind, message: String, hint: String },
    Skipped { because: String },
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
    /// Every step that was tried succeeded (and none was skipped for a failure).
    pub ok: bool,
    /// The server's host key fingerprint (SFTP), as `SHA256:…`.
    pub host_key: Option<String>,
    /// The name of a test file put on the server that could not be deleted.
    pub file_left_behind: Option<String>,
}

impl TestReport {
    /// The first step that failed.
    pub fn failure(&self) -> Option<&StepResult> {
        self.steps.iter().find(|s| matches!(s.outcome, StepOutcome::Failed { .. }))
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
            s.push_str(&format!("  WARNING: the test file {f:?} could not be deleted and is still on the server; delete it by hand\n"));
        }
        s.push_str(if self.ok { "result: the server accepted this profile\n" } else { "result: the test did not pass\n" });
        s
    }
}

// ── recording ────────────────────────────────────────────────────────────

struct Failure {
    kind: FailKind,
    message: String,
}

fn fail(kind: FailKind, message: impl Into<String>) -> Failure {
    Failure { kind, message: message.into() }
}

struct Recorder {
    steps: Vec<StepResult>,
    failed: bool,
    started: Instant,
}

impl Recorder {
    fn new() -> Recorder {
        Recorder { steps: Vec::new(), failed: false, started: Instant::now() }
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
        self.steps.push(StepResult { step, outcome: StepOutcome::Failed { kind: f.kind, message: f.message, hint: f.kind.hint().to_string() }, millis });
    }

    fn skip(&mut self, step: Step, because: impl Into<String>) {
        self.steps.push(StepResult { step, outcome: StepOutcome::Skipped { because: because.into() }, millis: 0 });
    }

    fn skip_rest(&mut self, steps: &[Step]) {
        for s in steps {
            self.skip(*s, "an earlier step failed");
        }
    }
}

fn io_kind(e: &std::io::Error) -> FailKind {
    use std::io::ErrorKind as K;
    match e.kind() {
        K::ConnectionRefused => FailKind::ConnectionRefused,
        K::TimedOut | K::WouldBlock => FailKind::TimedOut,
        K::HostUnreachable | K::NetworkUnreachable | K::NetworkDown => FailKind::Unreachable,
        K::UnexpectedEof | K::InvalidData | K::ConnectionReset | K::ConnectionAborted => FailKind::NotThisProtocol,
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

/// Resolve `host:port`, with a limit: the system resolver itself has none.
fn resolve(host: &str, port: u16, timeout: Duration) -> Result<Vec<SocketAddr>, Failure> {
    let (tx, rx) = std::sync::mpsc::channel();
    let h = host.to_string();
    std::thread::spawn(move || {
        let _ = tx.send((h.as_str(), port).to_socket_addrs().map(|i| i.collect::<Vec<_>>()));
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(addrs)) if !addrs.is_empty() => Ok(addrs),
        Ok(Ok(_)) => Err(fail(FailKind::NameNotFound, format!("{host:?} resolved to no address"))),
        Ok(Err(e)) => Err(fail(FailKind::NameNotFound, format!("{host:?} could not be resolved ({e})"))),
        Err(_) => Err(fail(FailKind::TimedOut, format!("looking up {host:?} took more than {} s", timeout.as_secs()))),
    }
}

fn show_addrs(addrs: &[SocketAddr]) -> String {
    addrs.iter().map(SocketAddr::to_string).collect::<Vec<_>>().join(", ")
}

/// Run the test. Blocks; call it off any UI thread. Never panics on a server's behaviour.
pub fn run_test(req: &TestRequest) -> TestReport {
    let mut rec = Recorder::new();
    let mut host_key = None;
    let mut left_behind = None;
    match req.protocol {
        Protocol::Ftp => ftp(req, &mut rec, &mut left_behind),
        Protocol::Sftp => sftp(req, &mut rec, &mut host_key, &mut left_behind),
    }
    let ok = !rec.failed && rec.steps.iter().all(|s| !matches!(s.outcome, StepOutcome::Failed { .. }));
    TestReport { host: req.host.clone(), port: req.port, protocol: req.protocol, folder: req.folder.clone(), steps: rec.steps, ok, host_key, file_left_behind: left_behind }
}

// ── FTP ──────────────────────────────────────────────────────────────────

fn ftp_failure(step_default: FailKind, e: &suppaftp::FtpError) -> Failure {
    use suppaftp::FtpError;
    match e {
        FtpError::ConnectionError(io) => fail(io_kind(io), format!("{io}")),
        FtpError::UnexpectedResponse(r) => {
            let code = r.status as u32;
            let text = String::from_utf8_lossy(&r.body).trim().to_string();
            let kind = match (step_default, code) {
                (FailKind::LoginRefused, _) => FailKind::LoginRefused,
                (FailKind::FolderMissing, _) => FailKind::FolderMissing,
                (_, 550 | 553) => FailKind::PermissionDenied,
                (other, _) => other,
            };
            fail(kind, format!("the server answered {code} {text}"))
        }
        FtpError::BadResponse => fail(FailKind::NotThisProtocol, "the server's reply was not valid FTP"),
        other => fail(step_default, other.to_string()),
    }
}

fn ftp(req: &TestRequest, rec: &mut Recorder, left_behind: &mut Option<String>) {
    use suppaftp::FtpStream;
    let all = [Step::Resolve, Step::Connect, Step::Login, Step::Folder, Step::Upload, Step::Download, Step::Compare, Step::Delete];
    let file_steps = [Step::Upload, Step::Download, Step::Compare, Step::Delete];
    let addrs = match resolve(&req.host, req.port, req.step_timeout) {
        Ok(a) => {
            rec.ok(Step::Resolve, format!("{} -> {}", req.host, show_addrs(&a)));
            a
        }
        Err(f) => {
            rec.fail(Step::Resolve, f);
            rec.skip_rest(&all[1..]);
            return;
        }
    };
    let mut last: Option<Failure> = None;
    let mut stream = None;
    for a in &addrs {
        // The timeouts go on the socket BEFORE the greeting is read: `FtpStream::connect_timeout` limits only the
        // connect, so a server that accepts and never speaks would be waited on for ever.
        let socket = match std::net::TcpStream::connect_timeout(a, req.step_timeout) {
            Ok(sock) => sock,
            Err(e) => {
                last = Some(fail(io_kind(&e), format!("{a}: {e}")));
                continue;
            }
        };
        let _ = socket.set_read_timeout(Some(req.step_timeout));
        let _ = socket.set_write_timeout(Some(req.step_timeout));
        match FtpStream::connect_with_stream(socket) {
            Ok(s) => {
                stream = Some((s, *a));
                break;
            }
            Err(e) => last = Some(Failure { message: format!("{a}: {}", ftp_failure(FailKind::Other, &e).message), ..ftp_failure(FailKind::Other, &e) }),
        }
    }
    let Some((mut s, addr)) = stream else {
        rec.fail(Step::Connect, last.unwrap_or_else(|| fail(FailKind::Other, "no address could be tried")));
        rec.skip_rest(&all[2..]);
        return;
    };
    let banner = s.get_welcome_msg().map(|m| m.trim().replace('\n', " ")).unwrap_or_default();
    rec.ok(Step::Connect, format!("{addr}{}", if banner.is_empty() { String::new() } else { format!(" said {banner:?}") }));

    if let Err(e) = s.login(req.username.as_str(), req.password.as_str()) {
        rec.fail(Step::Login, ftp_failure(FailKind::LoginRefused, &e));
        rec.skip_rest(&all[3..]);
        let _ = s.quit();
        return;
    }
    rec.ok(Step::Login, format!("accepted {:?}", req.username));
    // Binary, as the camera sends images. The client's default is ASCII, which rewrites line endings and would make a
    // faithful server look like it altered the file.
    if let Err(e) = s.transfer_type(suppaftp::types::FileType::Binary) {
        rec.fail(Step::Folder, ftp_failure(FailKind::TransferFailed, &e));
        rec.skip_rest(&file_steps);
        let _ = s.quit();
        return;
    }

    let at = if req.folder.trim().is_empty() {
        s.pwd().map(|p| format!("staying in the login directory {p}"))
    } else {
        s.cwd(req.folder.as_str()).map(|()| format!("in {}", req.folder))
    };
    match at {
        Ok(d) => rec.ok(Step::Folder, d),
        Err(e) => {
            rec.fail(Step::Folder, ftp_failure(FailKind::FolderMissing, &e));
            rec.skip_rest(&file_steps);
            let _ = s.quit();
            return;
        }
    }
    if !req.file_check {
        for st in file_steps {
            rec.skip(st, "not asked for");
        }
        let _ = s.quit();
        return;
    }

    let name = test_file_name();
    let body = test_file_body(&name);
    let mut uploaded = false;
    match s.put_file(name.as_str(), &mut Cursor::new(body.clone())) {
        Ok(n) => {
            uploaded = true;
            rec.ok(Step::Upload, format!("{name} ({n} bytes)"));
        }
        Err(e) => {
            rec.fail(Step::Upload, ftp_failure(FailKind::TransferFailed, &e));
            rec.skip(Step::Download, "an earlier step failed");
            rec.skip(Step::Compare, "an earlier step failed");
            rec.skip(Step::Delete, "nothing was put there");
        }
    }
    if uploaded {
        match s.retr_as_buffer(name.as_str()) {
            Ok(got) => {
                rec.ok(Step::Download, format!("{} bytes", got.get_ref().len()));
                if got.get_ref() == &body {
                    rec.ok(Step::Compare, "identical");
                } else {
                    rec.fail(Step::Compare, fail(FailKind::DataMismatch, format!("{} bytes came back for {} sent", got.get_ref().len(), body.len())));
                }
            }
            Err(e) => {
                rec.fail(Step::Download, ftp_failure(FailKind::TransferFailed, &e));
                rec.skip(Step::Compare, "an earlier step failed");
            }
        }
        match s.rm(name.as_str()) {
            Ok(()) => rec.ok(Step::Delete, format!("{name} removed")),
            Err(e) => {
                rec.fail(Step::Delete, ftp_failure(FailKind::TransferFailed, &e));
                *left_behind = Some(name);
            }
        }
    }
    let _ = s.quit();
}

// ── SFTP ─────────────────────────────────────────────────────────────────

struct KeyCapture(Arc<Mutex<Option<String>>>);

impl russh::client::Handler for KeyCapture {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &russh::keys::PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let fp = match key {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => Some(key.fingerprint(russh::keys::HashAlg::Sha256).to_string()),
            russh::keys::PublicKeyOrCertificate::Certificate(_) => None,
        };
        if let Ok(mut slot) = self.0.lock() {
            *slot = fp.or_else(|| Some("(a certificate)".to_string()));
        }
        // Nothing to check it against: reported, not trusted.
        Ok(true)
    }
}

fn sftp_status_failure(default: FailKind, e: &russh_sftp::client::error::Error) -> Failure {
    use russh_sftp::client::error::Error as E;
    use russh_sftp::protocol::StatusCode as C;
    match e {
        E::Status(st) => {
            let kind = match (st.status_code, default) {
                (C::NoSuchFile, FailKind::FolderMissing) | (C::NoSuchFile, _) => FailKind::FolderMissing,
                (C::PermissionDenied, _) => FailKind::PermissionDenied,
                (_, d) => d,
            };
            fail(kind, format!("the server answered {:?}: {}", st.status_code, st.error_message))
        }
        E::Timeout => fail(FailKind::TimedOut, "the server did not answer in time"),
        other => fail(default, other.to_string()),
    }
}

fn sftp(req: &TestRequest, rec: &mut Recorder, host_key: &mut Option<String>, left_behind: &mut Option<String>) {
    let all = [Step::Resolve, Step::Connect, Step::Handshake, Step::Login, Step::Folder, Step::Upload, Step::Download, Step::Compare, Step::Delete];
    let file_steps = [Step::Upload, Step::Download, Step::Compare, Step::Delete];
    let addrs = match resolve(&req.host, req.port, req.step_timeout) {
        Ok(a) => {
            rec.ok(Step::Resolve, format!("{} -> {}", req.host, show_addrs(&a)));
            a
        }
        Err(f) => {
            rec.fail(Step::Resolve, f);
            rec.skip_rest(&all[1..]);
            return;
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            rec.fail(Step::Connect, fail(FailKind::Other, format!("could not start the network runtime: {e}")));
            rec.skip_rest(&all[2..]);
            return;
        }
    };
    rt.block_on(sftp_steps(req, &addrs, rec, host_key, left_behind, &all, &file_steps));
}

async fn within<T>(limit: Duration, what: &str, f: impl std::future::Future<Output = T>) -> Result<T, Failure> {
    tokio::time::timeout(limit, f).await.map_err(|_| fail(FailKind::TimedOut, format!("{what} took more than {} s", limit.as_secs())))
}

async fn sftp_steps(req: &TestRequest, addrs: &[SocketAddr], rec: &mut Recorder, host_key: &mut Option<String>, left_behind: &mut Option<String>, all: &[Step], file_steps: &[Step]) {
    use russh_sftp::client::SftpSession;
    let t = req.step_timeout;
    // connect
    let mut last: Option<Failure> = None;
    let mut socket = None;
    for a in addrs {
        match within(t, &format!("connecting to {a}"), tokio::net::TcpStream::connect(*a)).await {
            Ok(Ok(s)) => {
                socket = Some((s, *a));
                break;
            }
            Ok(Err(e)) => last = Some(fail(io_kind(&e), format!("{a}: {e}"))),
            Err(f) => last = Some(f),
        }
    }
    let Some((socket, addr)) = socket else {
        rec.fail(Step::Connect, last.unwrap_or_else(|| fail(FailKind::Other, "no address could be tried")));
        rec.skip_rest(&all[2..]);
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
            let kind = match &e {
                russh::Error::IO(io) => io_kind(io),
                russh::Error::Disconnect | russh::Error::HUP => FailKind::NotThisProtocol,
                _ => FailKind::HandshakeFailed,
            };
            rec.fail(Step::Handshake, fail(kind, format!("{e}")));
            rec.skip_rest(&all[3..]);
            return;
        }
        Err(f) => {
            rec.fail(Step::Handshake, f);
            rec.skip_rest(&all[3..]);
            return;
        }
    };
    *host_key = seen.lock().ok().and_then(|k| k.clone());
    rec.ok(Step::Handshake, host_key.clone().map_or("done".to_string(), |k| format!("host key {k}")));

    // login
    match within(t, "logging in", handle.authenticate_password(req.username.as_str(), req.password.as_str())).await {
        Ok(Ok(r)) if r.success() => rec.ok(Step::Login, format!("accepted {:?}", req.username)),
        Ok(Ok(_)) => {
            rec.fail(Step::Login, fail(FailKind::LoginRefused, "the server did not accept that user name and password (SFTP here tries a password only)"));
            rec.skip_rest(&all[4..]);
            return;
        }
        Ok(Err(e)) => {
            rec.fail(Step::Login, fail(FailKind::HandshakeFailed, e.to_string()));
            rec.skip_rest(&all[4..]);
            return;
        }
        Err(f) => {
            rec.fail(Step::Login, f);
            rec.skip_rest(&all[4..]);
            return;
        }
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
            rec.fail(Step::Folder, f);
            rec.skip_rest(file_steps);
            return;
        }
    };

    // folder
    let wanted = if req.folder.trim().is_empty() { ".".to_string() } else { req.folder.trim().to_string() };
    let folder = match within(t, "finding the folder", async {
        let real = sftp.canonicalize(wanted.as_str()).await?;
        sftp.read_dir(real.as_str()).await?;
        Ok::<String, russh_sftp::client::error::Error>(real)
    })
    .await
    {
        Ok(Ok(real)) => {
            rec.ok(Step::Folder, format!("in {real}"));
            real
        }
        Ok(Err(e)) => {
            rec.fail(Step::Folder, sftp_status_failure(FailKind::FolderMissing, &e));
            rec.skip_rest(file_steps);
            return;
        }
        Err(f) => {
            rec.fail(Step::Folder, f);
            rec.skip_rest(file_steps);
            return;
        }
    };
    if !req.file_check {
        for st in file_steps {
            rec.skip(*st, "not asked for");
        }
        return;
    }

    let name = test_file_name();
    let path = format!("{}/{name}", folder.trim_end_matches('/'));
    let body = test_file_body(&name);
    let mut uploaded = false;
    match within(t, "putting the file", sftp.write(path.as_str(), &body)).await {
        Ok(Ok(())) => {
            uploaded = true;
            rec.ok(Step::Upload, format!("{name} ({} bytes)", body.len()));
        }
        Ok(Err(e)) => rec.fail(Step::Upload, sftp_status_failure(FailKind::TransferFailed, &e)),
        Err(f) => rec.fail(Step::Upload, f),
    }
    if !uploaded {
        rec.skip(Step::Download, "an earlier step failed");
        rec.skip(Step::Compare, "an earlier step failed");
        rec.skip(Step::Delete, "nothing was put there");
        return;
    }
    match within(t, "fetching the file", sftp.read(path.as_str())).await {
        Ok(Ok(got)) => {
            rec.ok(Step::Download, format!("{} bytes", got.len()));
            if got == body {
                rec.ok(Step::Compare, "identical");
            } else {
                rec.fail(Step::Compare, fail(FailKind::DataMismatch, format!("{} bytes came back for {} sent", got.len(), body.len())));
            }
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
    }

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
        let (mut user_ok, mut logged_in, mut cwd) = (false, false, String::from("/"));
        let mut data: Option<TcpListener> = None;
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let cmd = line.trim_end().to_string();
            st.lock().unwrap().commands.push(cmd.replace(b.pass, "<pw>"));
            let (verb, arg) = cmd.split_once(' ').map_or((cmd.as_str(), ""), |(v, a)| (v, a));
            match verb.to_uppercase().as_str() {
                "USER" => {
                    user_ok = arg == b.user;
                    reply(&mut w, "331 password please");
                }
                "PASS" => {
                    logged_in = user_ok && arg == b.pass;
                    reply(&mut w, if logged_in { "230 logged in" } else { "530 Login incorrect." });
                }
                "TYPE" => reply(&mut w, "200 ok"),
                "SYST" => reply(&mut w, "215 UNIX Type: L8"),
                "PWD" => reply(&mut w, &format!("257 \"{cwd}\" is the current directory")),
                "CWD" => {
                    if b.dirs.contains(&arg) {
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
                            let _ = c.read_to_end(&mut buf);
                        }
                    }
                    st.lock().unwrap().files.insert(arg.to_string(), buf);
                    reply(&mut w, "226 transfer complete");
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
                            if let Some(l) = data.take() {
                                if let Ok((mut c, _)) = l.accept() {
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
        let commands = srv.state.lock().unwrap().commands.join(" | ");
        assert!(commands.contains("CWD incoming") && commands.contains("STOR nikon-fleet-test-") && commands.contains("DELE nikon-fleet-test-"), "{commands}");
        assert!(!commands.contains("s3cret!"), "the test's own log replaced the password: {commands}");
        let text = r.to_text();
        assert!(text.contains("result: the server accepted this profile") && !text.contains("s3cret!"), "{text}");
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("s3cret!"), "no password in the report");
    }

    #[test]
    fn without_the_file_check_no_file_is_put_and_the_file_steps_say_so() {
        let srv = fake_ftp(good_ftp());
        let r = run_test(&TestRequest { file_check: false, ..request(srv.port, Protocol::Ftp) });
        assert!(r.ok, "{}", r.to_text());
        assert!(srv.state.lock().unwrap().commands.iter().all(|c| !c.starts_with("STOR")));
        assert_eq!(outcomes(&r).iter().filter(|(_, o)| o == "skipped").count(), 4);
        assert!(r.steps.iter().any(|s| matches!(&s.outcome, StepOutcome::Skipped { because } if because == "not asked for")));
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
    }

    #[test]
    fn a_folder_that_cannot_be_written_to_is_permission_denied_and_nothing_is_left() {
        let srv = fake_ftp(FtpBehaviour { deny_store: true, ..good_ftp() });
        let r = run_test(&request(srv.port, Protocol::Ftp));
        assert_eq!(outcomes(&r)[4], (Step::Upload, "failed:permission_denied".to_string()));
        assert_eq!(outcomes(&r)[5..], all(&[(Step::Download, "skipped"), (Step::Compare, "skipped"), (Step::Delete, "skipped")])[..]);
        assert!(r.file_left_behind.is_none());
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
        let k = r.failure().expect("a failure");
        assert!(matches!(&k.outcome, StepOutcome::Failed { kind: FailKind::TimedOut | FailKind::NotThisProtocol, .. }), "{:?}", k);
        // and the same for SSH: a socket that connects and never speaks
        let started = Instant::now();
        let r = run_test(&TestRequest { step_timeout: Duration::from_millis(400), ..request(port, Protocol::Sftp) });
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
        assert_eq!(outcomes(&r)[2], (Step::Handshake, "failed:timed_out".to_string()), "{}", r.to_text());
    }

    #[test]
    fn something_that_is_not_an_ftp_server_is_reported_as_such() {
        let srv = fake_ftp(FtpBehaviour { greeting: Some("SSH-2.0-OpenSSH_9.0"), ..good_ftp() });
        let r = run_test(&request(srv.port, Protocol::Ftp));
        assert!(!r.ok);
        assert!(matches!(&r.failure().unwrap().outcome, StepOutcome::Failed { kind: FailKind::NotThisProtocol | FailKind::Other, .. }), "{}", r.to_text());
    }

    #[test]
    fn every_failure_kind_has_a_hint_that_names_what_to_check() {
        for k in [
            FailKind::NameNotFound,
            FailKind::ConnectionRefused,
            FailKind::Unreachable,
            FailKind::TimedOut,
            FailKind::NotThisProtocol,
            FailKind::HandshakeFailed,
            FailKind::LoginRefused,
            FailKind::FolderMissing,
            FailKind::PermissionDenied,
            FailKind::TransferFailed,
            FailKind::DataMismatch,
            FailKind::Other,
        ] {
            // No wildcard in this match: a new kind has to be listed above and worded.
            match k {
                FailKind::NameNotFound | FailKind::ConnectionRefused | FailKind::Unreachable | FailKind::TimedOut | FailKind::NotThisProtocol | FailKind::HandshakeFailed | FailKind::LoginRefused | FailKind::FolderMissing | FailKind::PermissionDenied | FailKind::TransferFailed | FailKind::DataMismatch | FailKind::Other => {
                    assert!(k.hint().len() > 10, "{k:?}");
                }
            }
        }
        assert!(FailKind::Unreachable.hint().contains("may simply not be on it") && FailKind::TimedOut.hint().contains("cannot tell them apart"), "the hints say what this test cannot know");
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

        async fn auth_password(&mut self, user: &str, password: &str) -> Result<russh::server::Auth, Self::Error> {
            Ok(if user == self.b.user && password == self.b.pass {
                russh::server::Auth::Accept
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
            Ok(russh_sftp::protocol::Attrs { id, attrs: russh_sftp::protocol::FileAttributes { size, ..Default::default() } })
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
        SftpBehaviour { user: "fleet", pass: "s3cret!", dirs: vec!["incoming", "incoming/raw"], deny_write: false, corrupt_read: false, deny_delete: false }
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
    fn an_sftp_test_without_the_file_check_puts_nothing_there() {
        let srv = fake_sftp(good_sftp());
        let r = run_test(&TestRequest { file_check: false, ..request(srv.port, Protocol::Sftp) });
        assert!(r.ok && srv.fs.lock().unwrap().ops.is_empty(), "{}", r.to_text());
    }

    #[test]
    fn an_ftp_port_given_to_an_sftp_test_is_reported_not_hung_on() {
        let srv = fake_ftp(good_ftp());
        let started = Instant::now();
        let r = run_test(&TestRequest { step_timeout: Duration::from_millis(600), ..request(srv.port, Protocol::Sftp) });
        assert!(!r.ok && started.elapsed() < Duration::from_secs(6), "{:?}\n{}", started.elapsed(), r.to_text());
        let f = r.failure().unwrap();
        assert_eq!(f.step, Step::Handshake, "the FTP server's greeting is not an SSH banner: {}", r.to_text());
        assert!(matches!(&f.outcome, StepOutcome::Failed { kind: FailKind::NotThisProtocol | FailKind::HandshakeFailed | FailKind::TimedOut, .. }), "{f:?}");
    }
}

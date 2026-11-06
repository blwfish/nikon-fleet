//! `fleet-mcp`: the optional MCP server for nikon-fleet. Speaks MCP on stdio and
//! logs to stderr. Stdout is the protocol channel: nothing else may write to it.
//!
//! ```text
//! fleet-mcp --data-dir ~/nikon-fleet-data [--enable-writes]
//! ```
//!
//! Without `--enable-writes` the `*_apply` tools do not exist.

use std::ffi::OsString;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use clap::Parser;
use fleet_mcp::server::{Config, Deadlines, FleetMcp};
use fleet_mcp::supervisor::{ExitReason, Supervisor, pipe_pair, record_in_flight_at_exit};
use rmcp::ServiceExt;
use tokio::io::AsyncReadExt;

#[derive(Parser, Debug)]
#[command(name = "fleet-mcp", about = "MCP server for nikon-fleet (stdio)")]
struct Args {
    /// Directory holding snapshots/ and references/.
    #[arg(long)]
    data_dir: PathBuf,

    /// The `nikon-fleet` binary to run as the camera worker (default: beside this executable).
    #[arg(long)]
    worker: Option<PathBuf>,

    /// Passed to the worker as --sdk-bundle (default: the worker's own).
    #[arg(long)]
    sdk_bundle: Option<PathBuf>,

    /// MaidLayer.config, passed to the worker and used for diffs (default: the repository's).
    #[arg(long)]
    schema: Option<PathBuf>,

    /// Register the tools that write to cameras (transplant_apply, restore_apply, restore_extended_apply).
    #[arg(long)]
    enable_writes: bool,

    /// Give every camera call this many seconds instead of the per-tool defaults.
    #[arg(long, value_name = "SECS")]
    deadline_secs: Option<u64>,

    /// After the client goes away, wait this long for an apply that is still running before
    /// exiting. If one is still running then, it is recorded (stderr, and a marker file in the
    /// data directory when it had reached `writing`) so the next session can report it.
    #[arg(long, value_name = "SECS", default_value_t = DEFAULT_EXIT_WAIT_SECS)]
    exit_wait_secs: u64,
}

/// How long, by default, the server waits at exit for an apply still running: the longest apply deadline
/// (`Deadlines::extended_apply`, the FTP rebuild) plus the worker's teardown watchdog and a margin. A test pins
/// that it outlasts every apply deadline, so a call that is merely slow is not abandoned and recorded as lost.
const DEFAULT_EXIT_WAIT_SECS: u64 = 700;

/// The write end of the self-pipe the termination signals are reported through.
static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_termination_signal(sig: libc::c_int) {
    report_signal(SIGNAL_PIPE.load(Ordering::Relaxed), sig);
}

/// Async-signal-safe: one write(2) of the signal number to `fd`. A failed or short write (a full pipe can only
/// mean a signal is already pending) cannot be handled or reported from here. `errno` is saved and restored:
/// a handler runs between any two instructions of the interrupted thread, so a failing write(2) here would
/// otherwise change the `errno` that thread is about to read.
fn report_signal(fd: libc::c_int, sig: libc::c_int) {
    let byte = sig as u8;
    unsafe {
        let errno = errno_location();
        let saved = *errno;
        libc::write(fd, (&byte as *const u8).cast(), 1);
        *errno = saved;
    }
}

/// The calling thread's `errno`.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
unsafe fn errno_location() -> *mut libc::c_int {
    libc::__error()
}
#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe fn errno_location() -> *mut libc::c_int {
    libc::__errno_location()
}

/// Route SIGTERM, SIGINT and SIGHUP to a pipe the runtime can wait on. (tokio's `signal` feature
/// is not enabled in this crate, and the self-pipe is the whole of what it would do here.)
fn termination_signals() -> anyhow::Result<tokio::net::unix::pipe::Receiver> {
    let (r, w) = pipe_pair()?;
    // The handler must never block: a full pipe means a signal is already waiting to be read.
    if unsafe { libc::fcntl(w.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    SIGNAL_PIPE.store(w.as_raw_fd(), Ordering::Relaxed);
    std::mem::forget(w); // the handler uses the descriptor for the life of the process
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_termination_signal as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0
            || unsafe { libc::sigaction(sig, &action, std::ptr::null_mut()) } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(tokio::net::unix::pipe::Receiver::from_owned_fd(r)?)
}

/// The next termination signal, as the reason and the signal number.
async fn next_signal(rx: &mut tokio::net::unix::pipe::Receiver) -> anyhow::Result<(ExitReason, i32)> {
    loop {
        let mut byte = [0u8; 1];
        rx.read_exact(&mut byte).await?;
        match i32::from(byte[0]) {
            libc::SIGTERM => return Ok((ExitReason::Sigterm, libc::SIGTERM)),
            libc::SIGINT => return Ok((ExitReason::Sigint, libc::SIGINT)),
            libc::SIGHUP => return Ok((ExitReason::Sighup, libc::SIGHUP)),
            other => eprintln!("fleet-mcp: ignoring unexpected byte {other} on the signal pipe"),
        }
    }
}

fn default_worker() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or_else(|| anyhow::anyhow!("{} has no parent directory", exe.display()))?;
    Ok(dir.join("nikon-fleet"))
}

#[tokio::main]
async fn main() {
    use nikon_fleet::diag::{self, Class, Outcome, Subsystem};
    let args = Args::parse();
    // Diagnostics for the whole server process: the in-flight file, the ring, the panic hook. They are off,
    // with one warning, if their directory cannot be made; the server runs either way.
    diag::start(Subsystem::Mcp, None);
    let span = diag::span(Subsystem::Mcp, "server", Class::Top, &[]);
    // `main` used to return the Result, which printed `Error: {e:?}` and exited 1; keep exactly that,
    // after the exit is recorded.
    match run(args).await {
        Ok(()) => {
            span.finish(Outcome::Ok);
            diag::shutdown(0);
        }
        Err(e) => {
            span.finish(Outcome::Failed);
            diag::shutdown(1);
            eprintln!("Error: {e:?}");
            std::process::exit(1);
        }
    }
}

async fn run(args: Args) -> anyhow::Result<()> {
    let worker = match args.worker {
        Some(w) => w,
        None => default_worker()?,
    };
    if !worker.is_file() {
        anyhow::bail!("worker binary not found at {} (use --worker)", worker.display());
    }
    let schema = args.schema.clone().unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../sdk-runtime/MaidLayer.config")));

    let mut global: Vec<OsString> = vec!["--data-dir".into(), args.data_dir.clone().into()];
    if let Some(b) = &args.sdk_bundle {
        global.extend(["--sdk-bundle".into(), b.clone().into()]);
    }
    if let Some(s) = &args.schema {
        global.extend(["--schema".into(), s.clone().into()]);
    }

    let cfg = Config {
        data_dir: args.data_dir,
        schema,
        enable_writes: args.enable_writes,
        deadlines: args.deadline_secs.map(|s| Deadlines::all(Duration::from_secs(s))).unwrap_or_default(),
    };
    eprintln!(
        "fleet-mcp: data dir {}, worker {}, camera writes {}",
        cfg.data_dir.display(),
        worker.display(),
        if cfg.enable_writes { "ENABLED" } else { "disabled" }
    );
    let data_dir = cfg.data_dir.clone();
    let server = FleetMcp::new(cfg, Supervisor::new(worker, global));
    let tasks = server.tasks.clone();
    let sup = server.supervisor();
    let mut signals = termination_signals()?;
    let service = server.serve(rmcp::transport::stdio()).await?;
    // The service task ending abnormally (a join error) is the client side being gone too, with a call possibly still
    // in flight: it is handled as the client going, below (the in-flight apply is waited for and, if it outlasts the
    // wait, recorded), and only then returned as the error. It used to be returned at once, and `main`'s `exit(1)` then
    // left the worker running with no marker.
    let mut service_failed: Option<tokio::task::JoinError> = None;
    let signal = tokio::select! {
        done = service.waiting() => {
            if let Err(e) = done {
                eprintln!("fleet-mcp: the MCP service ended abnormally: {e}");
                service_failed = Some(e);
            }
            None
        }
        sig = next_signal(&mut signals) => Some(sig?),
    };

    if let Some((reason, signo)) = signal {
        // Told to stop. Say what was in flight and, if an apply had begun writing, leave the
        // marker; then exit without unwinding, which leaves the worker running to finish its
        // writes instead of killing it half way (dropping the runtime would kill it).
        if let Err(e) = record_in_flight_at_exit(&data_dir, reason, sup.in_flight().as_ref()) {
            eprintln!("fleet-mcp: could not record the call that was in flight: {e}");
        }
        // Not unwinding on purpose (the worker must finish), so the exit is recorded here, and the
        // server span closes with it.
        nikon_fleet::diag::shutdown(128 + signo);
        std::process::exit(128 + signo);
    }

    // The client is gone. A worker that is mid-apply must be allowed to finish (or hit its
    // own deadline): killing it half way would leave the camera half written.
    tasks.close();
    if let Some(f) = sup.in_flight() {
        eprintln!(
            "fleet-mcp: the client is gone with {} still running (last phase {}, writing seen: {}); waiting up to {} s for it",
            f.op,
            f.last_phase.as_deref().unwrap_or("(none)"),
            f.writing_seen,
            args.exit_wait_secs
        );
    }
    if tokio::time::timeout(Duration::from_secs(args.exit_wait_secs), tasks.wait()).await.is_err() {
        eprintln!("fleet-mcp: a camera call was still running after {} s; exiting anyway", args.exit_wait_secs);
        if let Err(e) = record_in_flight_at_exit(&data_dir, ExitReason::ClientGoneWaitExpired, sup.in_flight().as_ref()) {
            eprintln!("fleet-mcp: could not record the call that was in flight: {e}");
        }
    }
    match service_failed {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}

#[cfg(all(test, unix))]
mod signal_tests {
    use super::*;

    /// The signal handler must leave the interrupted thread's errno alone even when its write fails.
    #[test]
    fn the_signal_report_leaves_errno_as_it_found_it() {
        unsafe {
            *errno_location() = 77;
            report_signal(-1, libc::SIGTERM); // EBADF: the write fails and would set errno
            assert_eq!(*errno_location(), 77);
        }
    }
}

#[cfg(test)]
mod exit_wait_tests {
    use super::*;
    use fleet_mcp::server::ApplyOp;

    #[test]
    fn the_default_exit_wait_outlasts_every_apply_deadline_by_more_than_the_teardown_watchdog() {
        let d = Deadlines::default();
        let wait = Duration::from_secs(DEFAULT_EXIT_WAIT_SECS);
        for op in [ApplyOp::Transplant, ApplyOp::Restore, ApplyOp::RestoreExtended] {
            let deadline = d.apply_for(op);
            assert!(
                wait >= deadline + nikon_fleet::process_exit::TEARDOWN_WORST_CASE,
                "{op:?}: waiting {wait:?} at exit would abandon an apply still inside its {deadline:?} deadline"
            );
        }
    }
}

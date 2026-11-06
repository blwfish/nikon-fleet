//! Leaving the process without closing the camera session (docs/mcp-hardware-findings.md F24).
//!
//! The SDK's `DisconnectDevice` hangs on both bodies at a rate of roughly 7% even with the
//! run loop serviced first, and a read-only command has nothing left to do once its result
//! is out. [`exit_now`] flushes the standard streams and calls `_exit`, so neither the
//! disconnect nor any exit handler (the SDK dylib's included) runs; the OS closes the USB
//! handles. 60 consecutive opens after sessions left that way needed no replug (F24).
//!
//! Use it only after the result has been written and flushed, and only for commands that
//! wrote nothing to the camera: a write path keeps the ordinary disconnect.

use std::io::Write;
use std::time::Duration;

/// Flush stdout and stderr, then end the process with `code` without running destructors,
/// `atexit` handlers or `Drop` impls.
pub fn exit_now(code: i32) -> ! {
    // The one place every `_exit` path passes: record the planned exit and close the spans still
    // open, since no destructor will run to do it. (A write to the diagnostics files is a plain
    // `write`, so it is in the OS's hands before `_exit`.)
    crate::diag::shutdown(code);
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    #[cfg(unix)]
    unsafe {
        libc::_exit(code)
    }
    #[cfg(not(unix))]
    std::process::exit(code)
}

/// How long the process may take to exit after a write command has written its result. Measured on a Z 9
/// (docs/mcp-hardware-findings.md F24 and F26), `DisconnectDevice` takes one of three times: **1.55 to 1.58 s**
/// (healthy), **4.70 s** (slow, but it finishes: twice in 10 runs, 4.703 and 4.704 s, which is 5.0 s after the 0.3 s
/// pump) or **never** (10 of 20 write applies in one batch were still in it when killed after 90 s, and one after
/// 279 s). The limit sits above the slow-but-finishing case, so a teardown that is only slow is never reported as a
/// hang, and far below the real hang: 10 s is twice the slow case. (The MCP supervisor kills a worker 3 s after its
/// result for the same reason; there it only costs a retry of the exit.)
pub const TEARDOWN_LIMIT: Duration = Duration::from_secs(10);

/// After the result of a write has been written and flushed, give the camera session `limit` to close. If the process
/// has not exited by then, say so, record a `teardown_watchdog` diagnostics event, and end it with `code`: the status it
/// was already going to have. The caller works `code` out *before* arming this, which is why it is a parameter: when
/// the timer fires, nothing else decides it. `error_line` is the line `main` would have printed for a failed result
/// (`Error: ...`), printed here because `main` is never reached.
///
/// Arm it only after the result is out. It does not delay a process that exits normally (the thread dies with the
/// process), and it never cuts anything short before `limit`.
pub fn arm_teardown_watchdog(limit: Duration, code: i32, error_line: Option<String>) {
    spawn_watchdog(limit, move || {
        use crate::diag::{Subsystem, Val};
        crate::diag::event(
            Subsystem::MaidSdk,
            "teardown_watchdog",
            &[("limit_ms", Val::U64(limit.as_millis() as u64)), ("code", Val::I64(i64::from(code)))],
        );
        eprintln!(
            "warning: the camera session did not close within {} s after the result was written (the SDK's DisconnectDevice \
             hangs on some runs); ending the process now with exit status {code}. The result above is complete.",
            limit.as_secs_f64()
        );
        if let Some(line) = &error_line {
            eprintln!("{line}");
        }
        exit_now(code)
    });
}

/// Run `fire` on a thread of its own after `limit`. A thread that cannot be started is reported, not ignored: the
/// teardown then simply is not bounded, as it was before.
fn spawn_watchdog(limit: Duration, fire: impl FnOnce() + Send + 'static) {
    let started = std::thread::Builder::new().name("teardown-watchdog".to_string()).spawn(move || {
        std::thread::sleep(limit);
        fire();
    });
    if let Err(e) = started {
        eprintln!("warning: could not start the teardown watchdog ({e}); a hung disconnect will not be cut short");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHILD_ENV: &str = "NIKON_FLEET_EXIT_NOW_CHILD";
    const MARKER: &str = "written-before-exit-no-newline";

    /// Runs only as the child of `exit_now_flushes_what_was_written_and_ends_with_the_code`.
    #[test]
    fn exit_now_child() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        // No newline: a line-buffered stdout keeps this until it is flushed.
        print!("{MARKER}");
        eprint!("{MARKER}-err");
        exit_now(7)
    }

    #[test]
    fn exit_now_flushes_what_was_written_and_ends_with_the_code() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "process_exit::tests::exit_now_child", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .output()
            .expect("run the test binary as a child");
        assert_eq!(out.status.code(), Some(7), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(String::from_utf8_lossy(&out.stdout).contains(MARKER), "stdout was not flushed: {:?}", out.stdout);
        assert!(String::from_utf8_lossy(&out.stderr).contains(&format!("{MARKER}-err")), "stderr was not flushed");
        // the process ended at exit_now: the harness never got to print its summary line
        assert!(!String::from_utf8_lossy(&out.stdout).contains("test result"), "the process kept running past exit_now");
    }

    #[test]
    fn the_watchdog_fires_after_the_limit_and_not_before() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let fired = Arc::new(AtomicBool::new(false));
        let f = Arc::clone(&fired);
        spawn_watchdog(Duration::from_millis(600), move || f.store(true, Ordering::SeqCst));
        std::thread::sleep(Duration::from_millis(150));
        assert!(!fired.load(Ordering::SeqCst), "it must not fire a quarter of the way through the limit");
        let end = std::time::Instant::now() + Duration::from_secs(10);
        while !fired.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < end, "it never fired");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_limit_of_zero_fires_at_once_and_the_limit_is_well_above_a_healthy_teardown() {
        use std::sync::mpsc;
        let (tx, rx) = mpsc::channel();
        spawn_watchdog(Duration::ZERO, move || tx.send(()).unwrap());
        rx.recv_timeout(Duration::from_secs(10)).expect("a zero limit fires immediately");
        // The slowest teardown that still finishes was 5.0 s (4.7 s of DisconnectDevice after the 0.3 s pump): the limit must
        // clear it with room to spare, and stay far below the hang (85 s and more).
        assert!(TEARDOWN_LIMIT >= Duration::from_secs(8) && TEARDOWN_LIMIT <= Duration::from_secs(30), "{TEARDOWN_LIMIT:?}");
    }
}

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

/// Flush stdout and stderr, then end the process with `code` without running destructors,
/// `atexit` handlers or `Drop` impls.
pub fn exit_now(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    #[cfg(unix)]
    unsafe {
        libc::_exit(code)
    }
    #[cfg(not(unix))]
    std::process::exit(code)
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
}

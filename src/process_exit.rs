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

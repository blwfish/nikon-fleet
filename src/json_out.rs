//! Clean JSON on stdout from a process that loads the Nikon SDK.
//!
//! The SDK dylib prints `InitializeSDK Execution duration: N` to stdout itself (F14 in
//! docs/mcp-hardware-findings.md), so a `--json` command that opens the SDK would print that line
//! before its JSON. [`JsonOut::redirect`] moves what is written to stdout over to stderr for the
//! rest of the process and keeps a private handle to the real stdout; [`JsonOut::emit`] writes the
//! JSON there. Everything else, ours or the SDK's, lands on stderr.

use std::fs::File;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::fd::{FromRawFd, RawFd};

pub struct JsonOut {
    /// The real stdout, kept aside. `None` when JSON output was not asked for.
    saved: Option<File>,
}

impl JsonOut {
    /// When `enabled`, point stdout at stderr and keep the original stdout for [`emit`](Self::emit).
    /// When not, do nothing and `emit` writes to stdout as usual.
    pub fn redirect(enabled: bool) -> io::Result<Self> {
        if !enabled {
            return Ok(JsonOut { saved: None });
        }
        #[cfg(unix)]
        {
            Self::redirect_fds(1, 2)
        }
        // The SDK is macOS-only in practice; elsewhere there is nothing to protect stdout from.
        #[cfg(not(unix))]
        {
            Ok(JsonOut { saved: None })
        }
    }

    /// [`redirect`](Self::redirect) on arbitrary descriptors, so it can be tested without touching the process's own.
    #[cfg(unix)]
    pub fn redirect_fds(stdout_fd: RawFd, stderr_fd: RawFd) -> io::Result<Self> {
        // SAFETY: plain descriptor calls on descriptors the caller owns; the duplicate is wrapped
        // in a `File` exactly once.
        unsafe {
            let saved = libc::dup(stdout_fd);
            if saved < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::dup2(stderr_fd, stdout_fd) < 0 {
                let e = io::Error::last_os_error();
                libc::close(saved);
                return Err(e);
            }
            Ok(JsonOut { saved: Some(File::from_raw_fd(saved)) })
        }
    }

    /// Write one JSON document and a newline to the real stdout, flushed.
    pub fn emit(&mut self, json: &str) -> io::Result<()> {
        match &mut self.saved {
            Some(f) => {
                f.write_all(json.as_bytes())?;
                f.write_all(b"\n")?;
                f.flush()
            }
            None => {
                let mut o = io::stdout().lock();
                o.write_all(json.as_bytes())?;
                o.write_all(b"\n")?;
                o.flush()
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Read;

    fn pipe() -> (File, File) {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }

    fn drain(mut r: File) -> String {
        let mut s = String::new();
        r.read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn noise_written_to_stdout_after_the_redirect_lands_on_stderr_and_the_json_stays_clean() {
        let (out_r, out_w) = pipe();
        let (err_r, err_w) = pipe();
        {
            use std::os::fd::AsRawFd;
            let mut j = JsonOut::redirect_fds(out_w.as_raw_fd(), err_w.as_raw_fd()).unwrap();
            // What the SDK does: write to the descriptor that was stdout.
            let mut sdk = unsafe { File::from_raw_fd(libc::dup(out_w.as_raw_fd())) };
            sdk.write_all(b"InitializeSDK Execution duration: 180\n").unwrap();
            j.emit("{\"ok\":true}").unwrap();
        }
        drop(out_w);
        drop(err_w);
        assert_eq!(drain(out_r), "{\"ok\":true}\n", "stdout holds only the JSON");
        assert_eq!(drain(err_r), "InitializeSDK Execution duration: 180\n");
    }

    #[test]
    fn a_json_write_before_the_noise_is_still_only_on_stdout() {
        let (out_r, out_w) = pipe();
        let (err_r, err_w) = pipe();
        {
            use std::os::fd::AsRawFd;
            let mut j = JsonOut::redirect_fds(out_w.as_raw_fd(), err_w.as_raw_fd()).unwrap();
            j.emit("1").unwrap();
            j.emit("2").unwrap();
        }
        drop(out_w);
        drop(err_w);
        assert_eq!(drain(out_r), "1\n2\n");
        assert_eq!(drain(err_r), "");
    }

    #[test]
    fn not_enabled_leaves_the_descriptors_alone() {
        let j = JsonOut::redirect(false).unwrap();
        assert!(j.saved.is_none());
    }
}

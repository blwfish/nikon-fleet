//! The worker's wire protocol: line-delimited JSON on a dedicated file
//! descriptor, never stdout (the SDK dylib is third-party code and writes to
//! stdout itself: F14 in docs/mcp-hardware-findings.md).
//!
//! ```text
//! {"type":"progress","phase":"connected"}
//! {"type":"result","ok":true,"data":{...}}
//! {"type":"result","ok":false,"error":{"tag":"camera_busy","message":"..."}}
//! ```
//!
//! Rules (docs/mcp-plan.md §3):
//! - zero or more `progress` lines, then exactly one `result`, which is last;
//! - every line is flushed as it is written, and the `result` is written and
//!   flushed **before** the SDK is torn down, so a teardown hang (F3) cannot
//!   lose it;
//! - once a reader has the `result`, it is final: the worker's exit status,
//!   a hang afterwards, or a kill are not errors;
//! - end of input with no `result` means the worker died or was killed, and the
//!   last `progress` phase says how far it got.

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A worker's final answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Outcome {
    Ok { ok: OkMarker, data: Value },
    Err { ok: ErrMarker, error: WorkerError },
}

/// Serialises as `true`; refuses anything else on read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OkMarker;
/// Serialises as `false`; refuses anything else on read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrMarker;

impl Serialize for OkMarker {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bool(true)
    }
}
impl Serialize for ErrMarker {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bool(false)
    }
}
impl<'de> Deserialize<'de> for OkMarker {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if bool::deserialize(d)? { Ok(OkMarker) } else { Err(serde::de::Error::custom("expected ok: true")) }
    }
}
impl<'de> Deserialize<'de> for ErrMarker {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if !bool::deserialize(d)? { Ok(ErrMarker) } else { Err(serde::de::Error::custom("expected ok: false")) }
    }
}

/// A typed failure: a closed `tag` (what callers branch on), a message for a
/// human, and optional typed detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerError {
    pub tag: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

impl Outcome {
    pub fn ok(data: Value) -> Self {
        Outcome::Ok { ok: OkMarker, data }
    }
    pub fn err(tag: &str, message: impl Into<String>, detail: Option<Value>) -> Self {
        Outcome::Err { ok: ErrMarker, error: WorkerError { tag: tag.into(), message: message.into(), detail } }
    }
}

/// One line on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Line {
    Progress { phase: String },
    Result(Outcome),
}

/// Why an [`Emitter`] refused a line.
#[derive(Debug)]
pub enum EmitError {
    /// A `result` was already written; it is the last line.
    AfterResult,
    Io(io::Error),
}

impl std::fmt::Display for EmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmitError::AfterResult => f.write_str("a result line was already written; it must be the last line"),
            EmitError::Io(e) => write!(f, "writing to the result channel: {e}"),
        }
    }
}

impl std::error::Error for EmitError {}

/// Writes protocol lines, flushing after each.
pub struct Emitter<W: Write> {
    w: W,
    result_sent: bool,
}

impl<W: Write> Emitter<W> {
    pub fn new(w: W) -> Self {
        Emitter { w, result_sent: false }
    }

    fn write_line(&mut self, line: &Line) -> Result<(), EmitError> {
        // serde_json escapes newlines inside strings, so one value is one line.
        let mut text = serde_json::to_string(line).map_err(|e| EmitError::Io(io::Error::other(e)))?;
        text.push('\n');
        self.w.write_all(text.as_bytes()).map_err(EmitError::Io)?;
        self.w.flush().map_err(EmitError::Io)
    }

    pub fn progress(&mut self, phase: &str) -> Result<(), EmitError> {
        if self.result_sent {
            return Err(EmitError::AfterResult);
        }
        self.write_line(&Line::Progress { phase: phase.to_string() })
    }

    /// Write the one `result`. A second call is refused.
    pub fn result(&mut self, outcome: Outcome) -> Result<(), EmitError> {
        if self.result_sent {
            return Err(EmitError::AfterResult);
        }
        self.write_line(&Line::Result(outcome))?;
        self.result_sent = true;
        Ok(())
    }

    pub fn result_sent(&self) -> bool {
        self.result_sent
    }
}

/// Why the result channel could not be opened.
#[derive(Debug, PartialEq, Eq)]
pub enum FdError {
    /// 0, 1 and 2 are stdin, stdout and stderr: stdout is exactly what the
    /// protocol must stay off.
    Reserved(i32),
    /// Not an open descriptor in this process.
    NotOpen(i32),
}

impl std::fmt::Display for FdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FdError::Reserved(fd) => write!(f, "result fd {fd} is a standard stream; the protocol must not share stdout"),
            FdError::NotOpen(fd) => write!(f, "result fd {fd} is not open"),
        }
    }
}

impl std::error::Error for FdError {}

/// Take ownership of an inherited file descriptor as the result channel.
///
/// The parent opens a pipe and passes its write end as a descriptor number on
/// the command line; this checks the number is open and not a standard stream
/// before wrapping it. The caller must pass a descriptor it owns: nothing else
/// in the process may close or use it afterwards.
#[cfg(unix)]
pub fn open_result_fd(fd: i32) -> Result<std::fs::File, FdError> {
    use std::os::fd::FromRawFd;
    if (0..=2).contains(&fd) {
        return Err(FdError::Reserved(fd));
    }
    // SAFETY: F_GETFD only inspects the descriptor.
    if fd < 0 || unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
        return Err(FdError::NotOpen(fd));
    }
    // SAFETY: `fd` is open (just checked) and, by this function's contract,
    // owned by the caller, who hands it over here.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// How reading a worker's output ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadEnd {
    /// The result line: final.
    Result(Outcome),
    /// End of input with no result: the worker died or was killed.
    NoResult { last_phase: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    pub end: ReadEnd,
    /// Every progress phase seen, in order.
    pub progress: Vec<String>,
    /// Non-empty lines that were not valid protocol lines. Counted, never
    /// silently dropped; a well-behaved worker has none.
    pub garbage_lines: usize,
}

/// Read lines until the `result` (then stop: it is final) or end of input.
/// `on_progress` is called for each progress line as it arrives.
pub fn read_until_result<R: BufRead>(r: R, mut on_progress: impl FnMut(&str)) -> io::Result<Read> {
    let mut out = Read { end: ReadEnd::NoResult { last_phase: None }, progress: Vec::new(), garbage_lines: 0 };
    for line in r.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Line>(&line) {
            Ok(Line::Progress { phase }) => {
                on_progress(&phase);
                out.progress.push(phase);
            }
            Ok(Line::Result(o)) => {
                out.end = ReadEnd::Result(o);
                return Ok(out);
            }
            Err(_) => out.garbage_lines += 1,
        }
    }
    out.end = ReadEnd::NoResult { last_phase: out.progress.last().cloned() };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    /// A writer that records bytes and the byte count at each flush.
    #[derive(Default)]
    struct Spy {
        bytes: Vec<u8>,
        flushed_at: Vec<usize>,
    }
    impl Write for Spy {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushed_at.push(self.bytes.len());
            Ok(())
        }
    }

    fn emit_all(f: impl FnOnce(&mut Emitter<Spy>)) -> Spy {
        let mut e = Emitter::new(Spy::default());
        f(&mut e);
        e.w
    }

    #[test]
    fn lines_are_json_objects_one_per_newline_with_the_documented_shapes() {
        let spy = emit_all(|e| {
            e.progress("connected").unwrap();
            e.result(Outcome::ok(json!({"file": "x.json"}))).unwrap();
        });
        let text = String::from_utf8(spy.bytes).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(text.ends_with('\n'));
        assert_eq!(serde_json::from_str::<Value>(lines[0]).unwrap(), json!({"type": "progress", "phase": "connected"}));
        assert_eq!(
            serde_json::from_str::<Value>(lines[1]).unwrap(),
            json!({"type": "result", "ok": true, "data": {"file": "x.json"}})
        );
    }

    #[test]
    fn an_error_result_carries_tag_message_and_optional_detail() {
        let spy = emit_all(|e| e.result(Outcome::err("camera_busy", "held elsewhere", None)).unwrap());
        let v: Value = serde_json::from_str(String::from_utf8(spy.bytes).unwrap().trim()).unwrap();
        assert_eq!(v, json!({"type": "result", "ok": false, "error": {"tag": "camera_busy", "message": "held elsewhere"}}));
        let spy = emit_all(|e| e.result(Outcome::err("capture_incomplete", "m", Some(json!({"missing": [1]})))).unwrap());
        let v: Value = serde_json::from_str(String::from_utf8(spy.bytes).unwrap().trim()).unwrap();
        assert_eq!(v["error"]["detail"], json!({"missing": [1]}));
    }

    #[test]
    fn every_line_is_flushed_as_it_is_written_not_at_the_end() {
        let spy = emit_all(|e| {
            e.progress("a").unwrap();
            e.progress("b").unwrap();
            e.result(Outcome::ok(json!(null))).unwrap();
        });
        assert_eq!(spy.flushed_at.len(), 3);
        // Each flush happened at a line boundary, so a hang after the result loses nothing.
        let text = String::from_utf8(spy.bytes.clone()).unwrap();
        let mut boundaries = Vec::new();
        let mut n = 0;
        for l in text.split_inclusive('\n') {
            n += l.len();
            boundaries.push(n);
        }
        assert_eq!(spy.flushed_at, boundaries);
        assert_eq!(*spy.flushed_at.last().unwrap(), spy.bytes.len());
    }

    #[test]
    fn newlines_and_quotes_in_text_stay_inside_one_line() {
        let spy = emit_all(|e| e.result(Outcome::err("write_failed", "bad \"x\"\nline two\r\n– é", None)).unwrap());
        let text = String::from_utf8(spy.bytes).unwrap();
        assert_eq!(text.matches('\n').count(), 1, "exactly one newline: the terminator");
        let back = read_until_result(Cursor::new(text), |_| {}).unwrap();
        match back.end {
            ReadEnd::Result(Outcome::Err { error, .. }) => assert_eq!(error.message, "bad \"x\"\nline two\r\n– é"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_result_is_last_a_second_result_or_a_progress_after_it_is_refused() {
        let mut e = Emitter::new(Spy::default());
        e.progress("a").unwrap();
        assert!(!e.result_sent());
        e.result(Outcome::ok(json!(1))).unwrap();
        assert!(e.result_sent());
        assert!(matches!(e.result(Outcome::ok(json!(2))), Err(EmitError::AfterResult)));
        assert!(matches!(e.progress("late"), Err(EmitError::AfterResult)));
        assert_eq!(String::from_utf8(e.w.bytes).unwrap().lines().count(), 2, "nothing was written after the result");
    }

    #[test]
    fn a_write_error_is_reported_and_does_not_mark_the_result_as_sent() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "gone"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut e = Emitter::new(Broken);
        assert!(matches!(e.result(Outcome::ok(json!(1))), Err(EmitError::Io(_))));
        assert!(!e.result_sent(), "a result that was not delivered is not a sent result");
    }

    // ── reading ──────────────────────────────────────────────────────────

    fn read(text: &str) -> (Read, Vec<String>) {
        let mut seen = Vec::new();
        let r = read_until_result(Cursor::new(text.to_string()), |p| seen.push(p.to_string())).unwrap();
        (r, seen)
    }

    #[test]
    fn progress_is_reported_as_it_arrives_then_the_result_ends_the_read() {
        let (r, seen) = read("{\"type\":\"progress\",\"phase\":\"a\"}\n{\"type\":\"progress\",\"phase\":\"b\"}\n{\"type\":\"result\",\"ok\":true,\"data\":7}\n");
        assert_eq!(seen, ["a", "b"]);
        assert_eq!(r.progress, ["a", "b"]);
        assert_eq!(r.end, ReadEnd::Result(Outcome::ok(json!(7))));
        assert_eq!(r.garbage_lines, 0);
    }

    #[test]
    fn nothing_after_the_result_is_read() {
        let (r, seen) = read("{\"type\":\"result\",\"ok\":true,\"data\":1}\n{\"type\":\"progress\",\"phase\":\"late\"}\n{\"type\":\"result\",\"ok\":true,\"data\":2}\ngarbage\n");
        assert_eq!(r.end, ReadEnd::Result(Outcome::ok(json!(1))), "the first result is final");
        assert!(seen.is_empty());
        assert_eq!(r.garbage_lines, 0, "later lines were never looked at");
    }

    #[test]
    fn end_of_input_without_a_result_reports_how_far_the_worker_got() {
        let (r, _) = read("{\"type\":\"progress\",\"phase\":\"connected\"}\n{\"type\":\"progress\",\"phase\":\"writing\"}\n");
        assert_eq!(r.end, ReadEnd::NoResult { last_phase: Some("writing".into()) });
        let (r, _) = read("");
        assert_eq!(r.end, ReadEnd::NoResult { last_phase: None });
    }

    #[test]
    fn a_truncated_final_line_is_garbage_not_a_result() {
        let (r, _) = read("{\"type\":\"progress\",\"phase\":\"a\"}\n{\"type\":\"result\",\"ok\":tr");
        assert_eq!(r.end, ReadEnd::NoResult { last_phase: Some("a".into()) });
        assert_eq!(r.garbage_lines, 1);
    }

    #[test]
    fn unparseable_lines_are_counted_blank_lines_are_not() {
        let (r, _) = read("\nnot json\n   \n{\"type\":\"other\"}\n{\"type\":\"result\",\"ok\":true,\"data\":null}\n");
        assert_eq!(r.garbage_lines, 2);
        assert_eq!(r.end, ReadEnd::Result(Outcome::ok(Value::Null)));
    }

    #[test]
    fn a_result_whose_ok_flag_disagrees_with_its_body_is_garbage() {
        // ok:true must carry data, ok:false must carry error: never guessed.
        let (r, _) = read("{\"type\":\"result\",\"ok\":true,\"error\":{\"tag\":\"x\",\"message\":\"m\"}}\n{\"type\":\"result\",\"ok\":false,\"data\":1}\n");
        assert_eq!(r.garbage_lines, 2);
        assert_eq!(r.end, ReadEnd::NoResult { last_phase: None });
    }

    #[test]
    fn round_trip_of_both_outcomes() {
        for o in [Outcome::ok(json!({"a": [1, 2]})), Outcome::err("t", "m", Some(json!(3)))] {
            let spy = emit_all(|e| e.result(o.clone()).unwrap());
            let (r, _) = read(&String::from_utf8(spy.bytes).unwrap());
            assert_eq!(r.end, ReadEnd::Result(o));
        }
    }
    #[cfg(unix)]
    #[test]
    fn the_result_fd_must_be_open_and_not_a_standard_stream() {
        for fd in [0, 1, 2] {
            assert_eq!(open_result_fd(fd).unwrap_err(), FdError::Reserved(fd));
        }
        for fd in [-1, 3_000_000] {
            assert_eq!(open_result_fd(fd).unwrap_err(), FdError::NotOpen(fd));
        }
        assert!(FdError::Reserved(1).to_string().contains("stdout"));
    }

    #[cfg(unix)]
    #[test]
    fn an_open_pipe_end_becomes_a_writable_channel() {
        use std::io::Read;
        use std::os::fd::FromRawFd;
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let mut reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let w = open_result_fd(fds[1]).unwrap();
        let mut e = Emitter::new(w);
        e.progress("hello").unwrap();
        e.result(Outcome::ok(json!(1))).unwrap();
        drop(e); // closes the write end
        let mut text = String::new();
        reader.read_to_string(&mut text).unwrap();
        let (r, _) = read(&text);
        assert_eq!(r.progress, ["hello"]);
        assert_eq!(r.end, ReadEnd::Result(Outcome::ok(json!(1))));
    }
}

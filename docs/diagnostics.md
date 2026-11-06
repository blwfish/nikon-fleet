# Diagnostics: what was it doing when it hung or died?

Every nikon-fleet process (the CLI, the GUI, the MCP server, and the `worker` the MCP server starts) keeps a small,
bounded, crash-proof record of what it was doing. Nothing here needs an AI assistant or a network: it is files on disk
and one command to read them. The code is `src/diag.rs`; this page is the map.

## Reading it

```bash
nikon-fleet diag-report              # every recent process, oldest first
nikon-fleet diag-report --last 50    # show more of each process's recent entries
nikon-fleet diag-report --dir DIR    # a different diagnostics directory
```

For each process it says one of:

| State | Meaning |
|---|---|
| exited cleanly (code N) | It recorded a planned exit and nothing was open. |
| **gone, spans still open** | Killed or crashed. The spans listed under "in flight" are what it was doing, oldest first, each with how long ago it began. |
| **running, spans open** | Hung, or just busy. Same list. |
| gone, nothing open, no exit | Killed between operations. |

"In flight" is the evidence: the operation that never finished, the stage it had reached (`phase`), and for a camera
call the child's pid. A line cut off by a kill is counted, not hidden.

## Where the files are

`{data dir}/net.blw.fleet/diag/` (on macOS `~/Library/Application Support/net.blw.fleet/diag/`), or the directory in
`NIKON_FLEET_DIAG_DIR`. Per process, named by pid, owner-only:

- `inflight-<pid>.json`: the spans that have begun and not ended. Written **before** each operation and removed when
  nothing is open, with a temp file and a rename (not fsynced: it has to outlive the process, not the machine). A
  process that hangs or is killed leaves it behind.
- `ring-<pid>.jsonl`: completed spans and events, newest last, in a ring of 1024 entries. Append-only, rewritten from
  memory when it reaches twice that, so the cost of an entry does not grow with the size. When the ring is full the
  routine successes (one SDK read of two hundred) go first, then other successes, then events and failed routine
  calls, and **failed top-level steps last**, so a burst of reads cannot push out the failure that matters.

The newest 20 processes' files are kept. `cargo run` and `cargo test` write to `target/diag-dev` instead
(`.cargo/config.toml`), so test runs never touch the real directory. `--trace FILE` (CLI only) also appends every
entry to FILE, uncompacted.

## What is recorded

Spans are typed: a subsystem from a closed list, an operation name, small typed fields, an outcome (`ok`, `failed`,
`timeout`, `stalled`, `indeterminate`, `panicked`, `abandoned`, `cancelled`). `indeterminate` means the effect on the
camera is unknown (a write whose data went out and whose reply never came; an apply that could not be verified).

| Subsystem | Spans (class) | Fields worth knowing |
|---|---|---|
| cli | `command` (top) | `name`, the subcommand clap matched |
| gui | `gui` (process), `gui_cmd` (top), `cli_child` (top) | `cmd` (a closed list), `program`, `argc`, `timeout_ms`, `child_pid`, `request_id`, `exit_code`, `stdin_len`, output lengths |
| mcp | `server` (process), `tool_call` (top), `worker_call` (top), `kill_worker` (top) | `tool`, `worker_op`, `deadline_ms`, `queue_wait_ms`, `child_pid`, `phase`, `cause`, `request_id` |
| usb | `ptp_<op code>` (leaf, one per PTP transaction), `usb_scan`, `usb_reset` | `op`, `txid`, `p0`..`p4`, `phase` (`command_sent`, `data_sent`), `response`, `data_len`, `data_hash`, `reply_len`, `reply_hash` (`data_hash`/`reply_hash` are omitted for FTP-profile payloads, `0x90EE` and `0x90ED`); `usb_scan`: `cameras`, `unreadable`, `enumeration_error` (ends `failed` if the bus could not be listed or a device on it could not be read; a device whose descriptor will not read is counted in `unreadable`); `usb_reset`: `devices`, `reset`, `open_failed`, `reset_failed`, `enumeration_failed` (ends `failed` if any body was not reset) |
| maid_sdk | `sdk_load`, `initialize_sdk`, `enum_devices` (with `attempt`), `connect_device`, `disconnect_device` (all top); `read_cap` (leaf); `write_cap` (top) | `cap`, `kind`, `value_hash` (or `value_unhashable` if the value could not be serialised), `code`, `capabilities`, `attempts` (`enum_devices`), `data_type`, `placeholder` (the type has no decoder), `values_missing` and `array_code` (an enum read whose options list could not be had); a failure carries `error_kind` (a closed tag) and, for an SDK call, `call` and `code`; a load failure also `load_error` (the loader's reason) |
| lock | `camera_lock_held` (top, open for the whole hold); event `lock_refused` | `reason` |
| snapshot | `capture_snapshot`, `restore_point`, `check`, `save_new`, `capture_reads` | `gate_waived`; `capture_reads`: `read_ok`, `read_err`, `skipped_no_get`, `unknown_schema` (ends `failed` only if reads were attempted and none came back; some unreadable properties are normal) |
| transplant, restore | `plan`, `apply`, `execute_writes` | `plan_digest` (hashed), `status`, `written`, `write_errors`, `no_op`, `steps`, `dry_run`, `error` (a refusal's closed tag); `apply` ends by its status: `ok` only if verified, `indeterminate` if unverified or unknown, `failed` if failed or refused; `execute_writes` ends `failed` if any write errored (also `unsupported_kind`); a transplant `plan` records `outcomes`, `copied`, `would_change`, `target_read_failed` and ends `failed` only if every outcome is an unreadable target |
| firmware | `http_get`, `http_download`, `fetch`, `ingest` | `host`, `bytes`, `error_kind`, `model`, `version` |
| diag / cli events | `start` (role, `ppid`, `request_id`), `phase` (worker progress), `exit`, `panic` (location and bounded message) | |
| maid_sdk event | `teardown_watchdog` (`limit_ms`, `code`): a write command's camera session did not close within the limit after the result was written, and the process was ended with its decided status; the `disconnect_device` span it cut short is recorded `abandoned` | |

A hang in the SDK's `DisconnectDevice` shows as an open `disconnect_device` span in a process that is still running. On a
write command it is bounded: after the result is written the process is ended 10 s later if it has not exited (see F26 in
`mcp-hardware-findings.md`), and the report then shows a `teardown_watchdog` event, an `abandoned` disconnect span and a clean exit. A worker the MCP server killed at its deadline shows as a `worker_call` that ended `timeout` with the
`phase` it was stuck at, and a `kill_worker` span for the kill.

## Joining the sides of one request

A parent that starts a child process sets `NIKON_FLEET_REQUEST_ID=<pid>-<n>` and records the same value in its own span
(`request_id`); the child records it in its `start` event, together with `ppid`. So a GUI `cli_child` span, or an MCP
`worker_call`, can be matched to the child process's own files.

## What is never recorded

Payload bytes, passwords, argument lists, file contents, URLs, and error text from places where it can embed any of those. SDK
errors are recorded as typed fields (`error_kind`, `call`, `code`), never as their `Display` text.
Instead: lengths, a short hash (`data_hash`, `value_hash`, `plan_digest`), the host of a URL, closed tags. An FTP profile
carries a password; only its length reaches a diagnostics file, no hash either (a 64-bit hash of a blob that is mostly known structure
plus a short password would let whoever holds the file test password guesses offline). There is no regex scrubbing anywhere: the rule is that
the bytes are never stored. Tests check this for the PTP layer, the GUI's `cli_child` (stdin data) and the URL host.

## Limits worth knowing

- `depth` (the indent) counts spans open on the same OS thread. In async code (the MCP server) one request's spans can run
  on different threads, so use the start order and the "began ... ago" times, not the indent.
- WiFi (PTP/IP) is not covered: there is no Rust PTP/IP client yet. The Python probe `scripts/ptpip_read_ftp.py` writes
  nothing here.
- Not instrumented: SDK callbacks (they run on SDK-owned threads), `firmware browse`, settings, snapshot loads.
- The GUI records its exit in `on_exit` and after `run_native` returns; if the window system ends the process some other
  way the report will call it killed.
- A write to the diagnostics files that fails is counted and warned about once on stderr; it never fails the operation.

## Adding a span

```rust
use nikon_fleet::diag::{self, Class, Outcome, Subsystem, Val};

// A body with early returns or `?`: run it inside the span, which always ends.
diag::scoped(Subsystem::Snapshot, "save_new", Class::Top, &[("bytes", Val::len(n))], |_span| {
    /* ... */
})

// A long operation reporting how far it got:
let span = diag::span(Subsystem::Usb, "ptp_1016", Class::Leaf, &[("op", Val::Hex(0x1016))]);
span.set("phase", Val::text("data_sent"));
span.finish_with(Outcome::Failed, &[("response", Val::Hex(0x2002))]);
```

Use `Class::Leaf` for anything that happens hundreds of times, with the fewest fields that identify it; record a payload only
as `Val::len` and `Val::hash`. Spans in a process that never called `diag::start` do nothing.

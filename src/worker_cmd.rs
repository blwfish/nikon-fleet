//! `nikon-fleet worker`: run one operation in this process and report it on a
//! dedicated file descriptor (internal; the MCP server's child process).
//!
//! The SDK runs here, on the process's main thread, which is how the CLI
//! has always used it. The parent can kill this process when it hangs
//! (F3 in docs/mcp-hardware-findings.md). Protocol: `nikon_fleet::ops::worker`.
//! The result line is written and flushed while the SDK session is still
//! alive, before it is torn down, so a teardown hang cannot lose it.
//!
//! Exit status: 0 for an `ok` result, 1 for an error result, 2 when the
//! result channel could not be established. Callers must not rely on it
//! (a killed or hung worker has none); the result line is the answer.

use std::fs::File;
use std::path::Path;

use clap::{Args, Subcommand};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use nikon_fleet::camera_lock::LockError;
use nikon_fleet::maid_layer::MaidLayerConfig;
use nikon_fleet::ops::capture_snapshot::{CaptureError, CaptureRequest, capture_snapshot};
use nikon_fleet::ops::cameras::require_unambiguous;
use nikon_fleet::ops::target::select_by_usb_serial;
use nikon_fleet::ops::worker::{Emitter, Outcome, open_result_fd};
use nikon_fleet::snapshot::Camera;

use super::{enrich_devices, ensure_capabilities_present, open_sdk, snapshots_dir};

#[derive(Args, Debug)]
pub struct WorkerArgs {
    /// Inherited file descriptor to write protocol lines to (not 0, 1 or 2).
    #[arg(long)]
    result_fd: i32,
    #[command(subcommand)]
    op: WorkerOp,
}

#[derive(Subcommand, Debug)]
pub enum WorkerOp {
    /// Protocol self-test, no SDK and no camera: emit two progress lines and a
    /// result, optionally failing, hanging or exiting oddly afterwards.
    Selftest {
        /// Emit an error result instead of an ok one.
        #[arg(long)]
        fail: bool,
        /// After the result, sleep forever (a teardown hang).
        #[arg(long)]
        hang_after_result: bool,
        /// Exit with this status after the result.
        #[arg(long, default_value_t = 0)]
        exit_code: i32,
    },
    /// Capture one body and write a new snapshot (`mcp-` label, cap, baseline gate).
    Capture {
        /// USB serial of the body. `id-N` forms and SDK ids are refused.
        #[arg(long)]
        serial: String,
        /// Label before the `mcp-` prefix.
        #[arg(long)]
        label: Option<String>,
    },
}

/// Run the worker; returns the process exit status.
pub fn run(data_dir: &Path, bundle: &Path, schema: &Path, args: &WorkerArgs) -> i32 {
    let file = match open_result_fd(args.result_fd) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("worker: {e}");
            return 2;
        }
    };
    let mut em = Emitter::new(file);
    match &args.op {
        WorkerOp::Selftest { fail, hang_after_result, exit_code } => {
            selftest(&mut em, *fail, *hang_after_result, *exit_code)
        }
        WorkerOp::Capture { serial, label } => capture(&mut em, data_dir, bundle, schema, serial, label.as_deref()),
    }
}

/// Write the result and turn it into an exit status. A result that could not be
/// written is a status of 2: the parent will see end-of-input with no result.
fn finish(em: &mut Emitter<File>, outcome: Outcome) -> i32 {
    let code = if matches!(outcome, Outcome::Ok { .. }) { 0 } else { 1 };
    match em.result(outcome) {
        Ok(()) => code,
        Err(e) => {
            eprintln!("worker: {e}");
            2
        }
    }
}

fn progress(em: &mut Emitter<File>, phase: &str) {
    if let Err(e) = em.progress(phase) {
        eprintln!("worker: {e}");
    }
}

fn selftest(em: &mut Emitter<File>, fail: bool, hang_after_result: bool, exit_code: i32) -> i32 {
    progress(em, "starting");
    progress(em, "working");
    let outcome = if fail {
        Outcome::err("selftest_failed", "asked to fail", Some(json!({"why": "test"})))
    } else {
        Outcome::ok(json!({"selftest": true}))
    };
    let code = finish(em, outcome);
    if hang_after_result {
        // Stands in for a hang in DisconnectDevice after the work is done.
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
    if exit_code != 0 { exit_code } else { code }
}

fn capture(
    em: &mut Emitter<File>,
    data_dir: &Path,
    bundle: &Path,
    schema_path: &Path,
    serial: &str,
    label: Option<&str>,
) -> i32 {
    macro_rules! fail {
        ($tag:expr, $msg:expr) => {
            return finish(em, Outcome::err($tag, $msg, None))
        };
        ($tag:expr, $msg:expr, $detail:expr) => {
            return finish(em, Outcome::err($tag, $msg, Some($detail)))
        };
    }

    progress(em, "starting");
    // Reject what can be rejected before the SDK or the lock are touched.
    if let Err(e) = nikon_fleet::ops::target::require_usb_serial(serial) {
        fail!(e.tag(), e.to_string());
    }
    if !bundle.exists() {
        fail!("sdk_bundle_missing", format!("SDK bundle not found at {}", bundle.display()));
    }
    let schema = match MaidLayerConfig::parse_file(schema_path) {
        Ok(s) => s,
        Err(e) => fail!("schema_unreadable", format!("{}: {e}", schema_path.display())),
    };

    // The worker never resets USB: another process may hold a body (F2).
    let mut sdk = match open_sdk(bundle, true) {
        Ok(s) => s,
        Err(e) => match e.downcast_ref::<LockError>() {
            Some(lock) => fail!(lock.tag(), lock.to_string()),
            None => fail!("sdk_open_failed", format!("{e:#}")),
        },
    };
    progress(em, "sdk_ready");

    let devices = match sdk.devices() {
        Ok(d) => d,
        Err(e) => fail!("sdk_enumerate_failed", e.to_string()),
    };
    let enriched = enrich_devices(devices);
    let (target, usb) = match select_by_usb_serial(enriched.clone(), serial) {
        Ok(t) => t,
        Err(e) => fail!(e.tag(), e.to_string()),
    };
    if let Err(e) = require_unambiguous(&enriched, &target.name) {
        fail!(e.tag(), e.to_string(), json!({"model": e.model, "count": e.count}));
    }

    let device = match sdk.connect(target.id) {
        Ok(d) => d,
        Err(e) => fail!("sdk_connect_failed", e.to_string()),
    };
    progress(em, "connected");
    if let Err(e) = ensure_capabilities_present(device.capabilities.len()) {
        fail!("no_capabilities", e.to_string());
    }

    let name_lookup = schema.name_map_for_model(&target.name);
    let captured_at = match OffsetDateTime::now_utc().format(&Rfc3339) {
        Ok(t) => t,
        Err(e) => fail!("clock_failed", e.to_string()),
    };
    let snapshots = snapshots_dir(data_dir);
    let req = CaptureRequest {
        snapshots_dir: &snapshots,
        camera: Camera { model: target.name.clone(), serial: usb.serial.clone(), firmware: usb.firmware.clone() },
        label,
        name_lookup: &name_lookup,
        captured_at,
    };
    let mut phases: Vec<String> = Vec::new();
    let result = capture_snapshot(&device, &req, &mut |p| phases.push(p.to_string()));
    for p in &phases {
        progress(em, p);
    }
    match result {
        Ok(outcome) => finish(em, Outcome::ok(serde_json::to_value(&outcome).unwrap_or(json!(null)))),
        Err(e) => {
            let detail = match &e {
                CaptureError::Incomplete { missing, stats } => {
                    Some(json!({"missing": missing, "read_ok": stats.read_ok, "read_err": stats.read_err}))
                }
                _ => None,
            };
            finish(em, Outcome::err(e.tag(), e.to_string(), detail))
        }
    }
    // `device` and `sdk` drop here, after the result line was written and flushed.
}

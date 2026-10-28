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

use std::collections::BTreeSet;
use std::fs::File;
use std::path::Path;

use clap::{Args, Subcommand};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use nikon_fleet::camera_lock::LockError;
use nikon_fleet::maid_layer::MaidLayerConfig;
use nikon_fleet::ops::apply::{ApplyError, ApplyRequest, apply_transplant, require_confirmation};
use nikon_fleet::ops::cameras::require_unambiguous;
use nikon_fleet::ops::capture_snapshot::{CaptureError, CaptureRequest, capture_snapshot};
use nikon_fleet::ops::check::{CheckError, CheckRequest, check};
use nikon_fleet::ops::list::reference_filename;
use nikon_fleet::ops::paths::resolve_existing;
use nikon_fleet::ops::plan::{PlanInput, build_plan};
use nikon_fleet::ops::restore::resolve_only;
use nikon_fleet::ops::baseline::GatePolicy;
use nikon_fleet::ops::restore_flow::{
    BodyPolicy, RestoreApplyError, RestoreApplyRequest, RestorePlanInput, apply_restore, build_restore_plan,
};
use nikon_fleet::ops::target::select_by_usb_serial;
use nikon_fleet::ops::worker::{Emitter, Outcome, PHASE_WRITING, open_result_fd};
use nikon_fleet::ops::port::CameraPort;
use nikon_fleet::sdk::Device;
use nikon_fleet::snapshot::{Camera, Snapshot};
use nikon_fleet::transplant::{CodeNameGuard, TransplantOptions, resolve_allow_raw_code};

use super::{enrich_devices, ensure_capabilities_present, open_sdk, references_dir, snapshots_dir};

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
        /// After the result, leave through `process_exit::exit_now` (the read-only teardown),
        /// before any `--hang-after-result` takes effect.
        #[arg(long)]
        exit_now: bool,
    },
    /// List the attached bodies from the USB descriptors. No SDK, no lock, no
    /// camera session: works with no SDK bundle installed.
    ListCameras,
    /// Capture one body and write a new snapshot (`mcp-` label, cap, baseline gate).
    Capture {
        /// USB serial of the body. `id-N` forms and SDK ids are refused.
        #[arg(long)]
        serial: String,
        /// Label before the `mcp-` prefix.
        #[arg(long)]
        label: Option<String>,
    },
    /// Capture a body live and diff it against its reference (writes nothing).
    Check {
        #[arg(long)]
        serial: String,
    },
    /// Dry-run a transplant of a stored snapshot onto a body; returns a digest and
    /// summary to echo to `transplant-apply`. Writes nothing to the camera or the disk.
    TransplantPlan {
        #[arg(long)]
        serial: String,
        /// Snapshot name inside the data directory's snapshots/ (a name, not a path).
        #[arg(long)]
        source: String,
        /// Accept a raw-code enum / numeric literal for this property. Repeatable.
        #[arg(long = "allow-raw-code", value_name = "PROPERTY")]
        allow_raw_code: Vec<String>,
    },
    /// Apply a planned transplant: restore point, writes, read-back. WRITES TO THE CAMERA.
    TransplantApply {
        #[arg(long)]
        serial: String,
        #[arg(long)]
        source: String,
        #[arg(long = "allow-raw-code", value_name = "PROPERTY")]
        allow_raw_code: Vec<String>,
        /// The plan's digest, copied from the plan result.
        #[arg(long)]
        digest: String,
        /// The plan's summary line, copied from the plan result.
        #[arg(long)]
        summary: String,
    },
    /// Dry-run restoring a body from a stored snapshot of the same body; returns a digest
    /// and summary to echo to `restore-apply`. Reads the body; writes nothing.
    RestorePlan {
        #[arg(long)]
        serial: String,
        /// Snapshot name inside the data directory's snapshots/ (a name, not a path).
        #[arg(long)]
        source: String,
        /// Restore only this property (short or full name). Repeatable; default is all.
        #[arg(long = "only", value_name = "PROPERTY")]
        only: Vec<String>,
    },
    /// Apply a planned restore: restore point, writes, read-back. WRITES TO THE CAMERA.
    RestoreApply {
        #[arg(long)]
        serial: String,
        #[arg(long)]
        source: String,
        #[arg(long = "only", value_name = "PROPERTY")]
        only: Vec<String>,
        /// The plan's digest, copied from the plan result.
        #[arg(long)]
        digest: String,
        /// The plan's summary line, copied from the plan result.
        #[arg(long)]
        summary: String,
    },
}

/// What the worker does with the camera session once the result is out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Teardown {
    /// Drop the `Device` and `Sdk` as usual: `DisconnectDevice`, after the run-loop pump.
    Disconnect,
    /// Leave the session open and end the process with `_exit` (F24): the SDK's disconnect hangs on
    /// both bodies, and an operation that wrote nothing has nothing to close cleanly.
    ExitAfterResult,
}

impl Teardown {
    /// The one place that says which operations may skip the disconnect. Exhaustive on purpose: a
    /// new operation has to be classified here, and anything that can write to the camera keeps
    /// the ordinary disconnect.
    fn for_op(op: &WorkerOp) -> Teardown {
        match op {
            WorkerOp::Capture { .. }
            | WorkerOp::Check { .. }
            | WorkerOp::TransplantPlan { .. }
            | WorkerOp::RestorePlan { .. } => Teardown::ExitAfterResult,
            WorkerOp::TransplantApply { .. } | WorkerOp::RestoreApply { .. } => Teardown::Disconnect,
            // No camera session at all.
            WorkerOp::Selftest { .. } | WorkerOp::ListCameras => Teardown::Disconnect,
        }
    }
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
    let teardown = Teardown::for_op(&args.op);
    match &args.op {
        WorkerOp::Selftest { fail, hang_after_result, exit_code, exit_now } => {
            selftest(&mut em, *fail, *hang_after_result, *exit_code, *exit_now)
        }
        WorkerOp::ListCameras => {
            progress(&mut em, "starting");
            let list = nikon_fleet::ops::cameras::list_cameras();
            finish(&mut em, Outcome::ok_report(&list))
        }
        WorkerOp::Capture { serial, label } => capture(&mut em, teardown, data_dir, bundle, schema, serial, label.as_deref()),
        WorkerOp::Check { serial } => op_check(&mut em, teardown, data_dir, bundle, schema, serial),
        WorkerOp::TransplantPlan { serial, source, allow_raw_code } => {
            op_transplant(&mut em, teardown, data_dir, bundle, schema, serial, source, allow_raw_code, None)
        }
        WorkerOp::TransplantApply { serial, source, allow_raw_code, digest, summary } => op_transplant(
            &mut em,
            teardown,
            data_dir,
            bundle,
            schema,
            serial,
            source,
            allow_raw_code,
            Some((digest.as_str(), summary.as_str())),
        ),
        WorkerOp::RestorePlan { serial, source, only } => {
            op_restore(&mut em, teardown, data_dir, bundle, schema, serial, source, only, None)
        }
        WorkerOp::RestoreApply { serial, source, only, digest, summary } => op_restore(
            &mut em,
            teardown,
            data_dir,
            bundle,
            schema,
            serial,
            source,
            only,
            Some((digest.as_str(), summary.as_str())),
        ),
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

/// Progress for an apply. The `writing` line is the supervisor's proof that no write happened if it
/// never arrives, so if it cannot be delivered the apply must not go on to write: returns `false`
/// for that case. Any other line that cannot be delivered is only logged, as before.
fn progress_for_apply<W: std::io::Write>(em: &mut Emitter<W>, phase: &str) -> bool {
    match em.progress(phase) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("worker: {e}");
            phase != PHASE_WRITING
        }
    }
}

/// The progress callback an apply is given: ends the process before the first write if the line
/// announcing writes could not be sent.
fn apply_progress<'a>(em: &'a mut Emitter<File>) -> impl FnMut(&str) + 'a {
    move |phase| {
        if !progress_for_apply(em, phase) {
            eprintln!("worker: the `writing` progress line could not be sent; stopping before any write");
            nikon_fleet::process_exit::exit_now(2);
        }
    }
}

fn selftest(em: &mut Emitter<File>, fail: bool, hang_after_result: bool, exit_code: i32, exit_now: bool) -> i32 {
    progress(em, "starting");
    progress(em, "working");
    let outcome = if fail {
        Outcome::err("selftest_failed", "asked to fail", Some(json!({"why": "test"})))
    } else {
        Outcome::ok(json!({"selftest": true}))
    };
    let code = finish(em, outcome);
    if exit_now {
        nikon_fleet::process_exit::exit_now(if exit_code != 0 { exit_code } else { code });
    }
    if hang_after_result {
        // Stands in for a hang in DisconnectDevice after the work is done.
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
    if exit_code != 0 { exit_code } else { code }
}

/// What an op gets once a body is chosen and connected.
struct Body<'a, 'sdk> {
    device: &'a Device<'sdk>,
    /// Identity from the USB descriptors.
    camera: Camera,
    schema: &'a MaidLayerConfig,
}

/// Validate the serial and bundle before touching the SDK, take the lock, open
/// the SDK without a USB reset (another process may hold a body: F2), choose the
/// body by USB serial only, refuse same-model ambiguity, connect, and run `f`
/// **while the session is alive** so its result is written before teardown.
fn with_body(
    em: &mut Emitter<File>,
    teardown: Teardown,
    bundle: &Path,
    schema_path: &Path,
    serial: &str,
    f: impl FnOnce(&mut Emitter<File>, &Body<'_, '_>) -> Outcome,
) -> i32 {
    macro_rules! fail {
        ($tag:expr, $msg:expr) => {
            return finish(em, Outcome::err($tag, $msg, None))
        };
        ($tag:expr, $msg:expr, $detail:expr) => {
            return finish(em, Outcome::err($tag, $msg, Some($detail)))
        };
    }

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
    if let Err(e) = ensure_capabilities_present(device.capabilities().len()) {
        fail!("no_capabilities", e.to_string());
    }
    let body = Body {
        device: &device,
        camera: Camera { model: target.name.clone(), serial: usb.serial.clone(), firmware: usb.firmware.clone() },
        schema: &schema,
    };
    let outcome = f(em, &body);
    let code = finish(em, outcome);
    if teardown == Teardown::ExitAfterResult {
        // The result line is out and flushed; nothing was written to the camera.
        nikon_fleet::process_exit::exit_now(code);
    }
    code
    // Otherwise `device` and `sdk` drop here, after the result line was written and flushed.
}

fn now() -> Result<String, Outcome> {
    OffsetDateTime::now_utc().format(&Rfc3339).map_err(|e| Outcome::err("clock_failed", e.to_string(), None))
}

fn capture(
    em: &mut Emitter<File>,
    teardown: Teardown,
    data_dir: &Path,
    bundle: &Path,
    schema_path: &Path,
    serial: &str,
    label: Option<&str>,
) -> i32 {
    progress(em, "starting");
    with_body(em, teardown, bundle, schema_path, serial, |em, body| {
        let name_lookup = body.schema.name_map_for_model(&body.camera.model);
        let captured_at = match now() {
            Ok(t) => t,
            Err(o) => return o,
        };
        let snapshots = snapshots_dir(data_dir);
        let req = CaptureRequest {
            snapshots_dir: &snapshots,
            camera: body.camera.clone(),
            label,
            name_lookup: &name_lookup,
            captured_at,
        };
        let mut phases: Vec<String> = Vec::new();
        let result = capture_snapshot(body.device, &req, &mut |p| phases.push(p.to_string()));
        for p in &phases {
            progress(em, p);
        }
        match result {
            Ok(outcome) => Outcome::ok_report(&outcome),
            Err(e) => {
                let detail = match &e {
                    CaptureError::Incomplete { missing, stats } => {
                        Some(json!({"missing": missing, "read_ok": stats.read_ok, "read_err": stats.read_err}))
                    }
                    _ => None,
                };
                Outcome::err(e.tag(), e.to_string(), detail)
            }
        }
    })
}

fn op_check(em: &mut Emitter<File>, teardown: Teardown, data_dir: &Path, bundle: &Path, schema_path: &Path, serial: &str) -> i32 {
    progress(em, "starting");
    with_body(em, teardown, bundle, schema_path, serial, |em, body| {
        let ref_path = references_dir(data_dir).join(reference_filename(&body.camera.model, &body.camera.serial));
        let reference = match Snapshot::load_from_file(&ref_path) {
            Ok(r) => r,
            Err(e) => {
                let missing = !ref_path.exists();
                return Outcome::err(
                    if missing { "no_reference" } else { "reference_unreadable" },
                    format!("{}: {e}", ref_path.display()),
                    None,
                );
            }
        };
        let name_lookup = body.schema.name_map_for_model(&body.camera.model);
        let captured_at = match now() {
            Ok(t) => t,
            Err(o) => return o,
        };
        let req = CheckRequest {
            live_camera: body.camera.clone(),
            reference: &reference,
            name_lookup: &name_lookup,
            schema: body.schema,
            captured_at,
        };
        progress(em, "reading");
        match check(body.device, &req) {
            Ok(report) => Outcome::ok_report(&report),
            Err(e) => {
                let detail = match &e {
                    CheckError::Incomplete { missing, stats } => {
                        Some(json!({"missing": missing, "read_ok": stats.read_ok, "read_err": stats.read_err}))
                    }
                    _ => None,
                };
                Outcome::err(e.tag(), e.to_string(), detail)
            }
        }
    })
}

/// `transplant-plan` (confirmation `None`) and `transplant-apply` (`Some((digest, summary))`).
#[allow(clippy::too_many_arguments)]
fn op_transplant(
    em: &mut Emitter<File>,
    teardown: Teardown,
    data_dir: &Path,
    bundle: &Path,
    schema_path: &Path,
    serial: &str,
    source_name: &str,
    allow_raw_code: &[String],
    confirmation: Option<(&str, &str)>,
) -> i32 {
    progress(em, "starting");
    // Everything that can be refused without a camera is refused first.
    if let Some((digest, summary)) = confirmation {
        if let Err(e) = require_confirmation(digest, summary) {
            return finish(em, Outcome::err(e.tag(), e.to_string(), None));
        }
    }
    let snapshots = snapshots_dir(data_dir);
    let source_path = match resolve_existing(&snapshots, source_name) {
        Ok(p) => p,
        Err(e) => return finish(em, Outcome::err(e.tag(), e.to_string(), None)),
    };
    let source_bytes = match std::fs::read(&source_path) {
        Ok(b) => b,
        Err(e) => return finish(em, Outcome::err("source_unreadable", format!("{}: {e}", source_path.display()), None)),
    };
    let source = match std::str::from_utf8(&source_bytes)
        .map_err(|e| e.to_string())
        .and_then(|t| Snapshot::from_json(t).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => return finish(em, Outcome::err("source_unreadable", format!("{}: {e}", source_path.display()), None)),
    };
    let allow = match resolve_allow_raw_code(allow_raw_code, &source.properties) {
        Ok(a) => a,
        Err(msg) => return finish(em, Outcome::err("bad_opt_in", msg, None)),
    };
    let options = TransplantOptions { allow_raw_code: allow };

    with_body(em, teardown, bundle, schema_path, serial, |em, body| {
        let guard = match CodeNameGuard::new(body.schema, &source.camera.model, &body.camera.model) {
            Ok(g) => g,
            Err(e) => return Outcome::err("unsupported_model", e.to_string(), None),
        };
        let input = PlanInput { source: &source, source_bytes: &source_bytes, target: &body.camera, options: &options };
        match confirmation {
            None => {
                progress(em, "planning");
                let report = build_plan(body.device, &guard, &input);
                Outcome::ok_report(&report)
            }
            Some((digest, summary)) => {
                let name_lookup = body.schema.name_map_for_model(&body.camera.model);
                let captured_at = match now() {
                    Ok(t) => t,
                    Err(o) => return o,
                };
                let req = ApplyRequest {
                    plan: input,
                    guard: &guard,
                    snapshots_dir: &snapshots,
                    name_lookup: &name_lookup,
                    expected_digest: digest,
                    expected_summary: summary,
                    captured_at,
                    gate_policy: GatePolicy::Strict,
                };
                // Each phase is emitted as it happens: the parent decides what a cancellation
                // can still do from the last one it has seen, and "writing" is the last safe point.
                let result = apply_transplant(body.device, &req, &mut apply_progress(em));
                match result {
                    Ok(report) => Outcome::ok_report(&report),
                    Err(e) => {
                        let detail = match &e {
                            ApplyError::RestorePointIncomplete { missing, stats } => {
                                Some(json!({"missing": missing, "read_ok": stats.read_ok, "read_err": stats.read_err}))
                            }
                            _ => None,
                        };
                        Outcome::err(e.tag(), e.to_string(), detail)
                    }
                }
            }
        }
    })
}

/// `restore-plan` (confirmation `None`) and `restore-apply` (`Some((digest, summary))`).
#[allow(clippy::too_many_arguments)]
fn op_restore(
    em: &mut Emitter<File>,
    teardown: Teardown,
    data_dir: &Path,
    bundle: &Path,
    schema_path: &Path,
    serial: &str,
    source_name: &str,
    only_names: &[String],
    confirmation: Option<(&str, &str)>,
) -> i32 {
    progress(em, "starting");
    // Everything that can be refused without a camera is refused first.
    if let Some((digest, summary)) = confirmation {
        if let Err(e) = require_confirmation(digest, summary) {
            return finish(em, Outcome::err(e.tag(), e.to_string(), None));
        }
    }
    let snapshots = snapshots_dir(data_dir);
    let source_path = match resolve_existing(&snapshots, source_name) {
        Ok(p) => p,
        Err(e) => return finish(em, Outcome::err(e.tag(), e.to_string(), None)),
    };
    let source_bytes = match std::fs::read(&source_path) {
        Ok(b) => b,
        Err(e) => return finish(em, Outcome::err("source_unreadable", format!("{}: {e}", source_path.display()), None)),
    };
    let source = match std::str::from_utf8(&source_bytes)
        .map_err(|e| e.to_string())
        .and_then(|t| Snapshot::from_json(t).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => return finish(em, Outcome::err("source_unreadable", format!("{}: {e}", source_path.display()), None)),
    };
    let only: Option<BTreeSet<String>> = if only_names.is_empty() {
        None
    } else {
        match resolve_only(only_names, &source.properties) {
            Ok(set) => Some(set),
            Err(e) => return finish(em, Outcome::err(e.tag(), e.to_string(), None)),
        }
    };

    with_body(em, teardown, bundle, schema_path, serial, |em, body| {
        let input = RestorePlanInput {
            source: &source,
            source_name: &source_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            source_bytes: &source_bytes,
            target: &body.camera,
            only: only.as_ref(),
            body_policy: BodyPolicy::SameBody,
        };
        match confirmation {
            None => {
                progress(em, "planning");
                match build_restore_plan(body.device, &input) {
                    Ok(report) => Outcome::ok_report(&report),
                    Err(e) => Outcome::err(e.tag(), e.to_string(), None),
                }
            }
            Some((digest, summary)) => {
                let name_lookup = body.schema.name_map_for_model(&body.camera.model);
                let captured_at = match now() {
                    Ok(t) => t,
                    Err(o) => return o,
                };
                let req = RestoreApplyRequest {
                    plan: input,
                    snapshots_dir: &snapshots,
                    name_lookup: &name_lookup,
                    expected_digest: digest,
                    expected_summary: summary,
                    captured_at,
                    gate_policy: GatePolicy::Strict,
                };
                // As for the transplant: "writing" is the last phase at which cancelling is safe.
                match apply_restore(body.device, &req, &mut apply_progress(em)) {
                    Ok(report) => Outcome::ok_report(&report),
                    Err(e) => {
                        let detail = match &e {
                            RestoreApplyError::Apply(ApplyError::RestorePointIncomplete { missing, stats }) => {
                                Some(json!({"missing": missing, "read_ok": stats.read_ok, "read_err": stats.read_err}))
                            }
                            _ => None,
                        };
                        Outcome::err(e.tag(), e.to_string(), detail)
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that fails every write, like a pipe whose reader has gone.
    struct Broken;
    impl std::io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("broken pipe"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn an_apply_must_not_write_if_the_line_announcing_writes_could_not_be_sent() {
        let mut em = Emitter::new(Broken);
        assert!(!progress_for_apply(&mut em, PHASE_WRITING), "the supervisor would read silence as 'nothing was written'");
        assert!(progress_for_apply(&mut em, "plan_verified"), "other lines are only logged");
        assert!(progress_for_apply(&mut em, "read_back"));
        let mut ok = Emitter::new(Vec::new());
        assert!(progress_for_apply(&mut ok, PHASE_WRITING), "delivered: go ahead");
    }


    fn s(v: &str) -> String {
        v.to_string()
    }

    #[test]
    fn only_operations_that_cannot_write_may_skip_the_disconnect() {
        let (serial, source) = (s("123"), s("a.json"));
        let table: Vec<(WorkerOp, Teardown)> = vec![
            (WorkerOp::Capture { serial: serial.clone(), label: None }, Teardown::ExitAfterResult),
            (WorkerOp::Check { serial: serial.clone() }, Teardown::ExitAfterResult),
            (
                WorkerOp::TransplantPlan { serial: serial.clone(), source: source.clone(), allow_raw_code: vec![] },
                Teardown::ExitAfterResult,
            ),
            (
                WorkerOp::RestorePlan { serial: serial.clone(), source: source.clone(), only: vec![] },
                Teardown::ExitAfterResult,
            ),
            (
                WorkerOp::TransplantApply {
                    serial: serial.clone(),
                    source: source.clone(),
                    allow_raw_code: vec![],
                    digest: s("d"),
                    summary: s("s"),
                },
                Teardown::Disconnect,
            ),
            (
                WorkerOp::RestoreApply { serial, source, only: vec![], digest: s("d"), summary: s("s") },
                Teardown::Disconnect,
            ),
            (WorkerOp::ListCameras, Teardown::Disconnect),
            (
                WorkerOp::Selftest { fail: false, hang_after_result: false, exit_code: 0, exit_now: false },
                Teardown::Disconnect,
            ),
        ];
        for (op, want) in table {
            assert_eq!(Teardown::for_op(&op), want, "{op:?}");
        }
    }
}

//! Raw PTP-over-USB client for Nikon vendor operations the MAID SDK cannot
//! reach — most importantly `0x943B`, the vendor-property read wrapper.
//!
//! This talks PTP directly over the camera's USB Still Image class (PIMA
//! 15740) bulk pipes, using the same Bulk-Only Transport container framing
//! as `libgphoto2`/`gphoto2`: a 12-byte header (4-byte little-endian length,
//! 2-byte container type, 2-byte operation/response code, 4-byte transaction
//! ID) followed by up to 5 little-endian `u32` params (Command) or raw bytes
//! (Data). It is a completely separate path from `sdk.rs`'s MAID FFI bridge
//! and does not need the Nikon SDK bundle at all.
//!
//! ## Background
//!
//! `0x943B` and friends (`0x9413`, `0x90E8`, `0x90EE`) were reverse-engineered
//! from an NX Field WiFi/PTP-IP capture (`docs/nx-field-session-2026-07-09.md`)
//! and originally assumed to be WiFi-only. A follow-up session on 2026-09-06
//! confirmed all four also work over plain USB PTP once called with the
//! correct wire format — see that doc for the full story, including several
//! wire-format bugs the original by-eye pcap reading introduced.
//!
//! Wired up here: `0x943B` (vendor-property reads) and `0x90EE` (FTP profile
//! writes, built from scratch via [`FtpProfile`]/[`encode_ftp_profile`] —
//! its data-blob layout was fully mapped by diffing three real captured
//! instances, and the encoder is verified byte-for-byte against two of them
//! in tests). `0x9413` (IPTC profile write) is not — its data-blob preamble
//! couldn't be decoded from existing captures (only two instances exist
//! anywhere and they're byte-identical), so there's no way to construct
//! arbitrary IPTC content safely yet. See the doc's open items.
//!
//! ## macOS's own PTP claim
//!
//! macOS's Image Capture subsystem (`ptpcamerad`, `icdd`) claims a camera's
//! PTP interface as soon as it's touched, and SIP blocks disabling those
//! LaunchAgents permanently — so claiming the interface here means racing
//! them: kill, then claim immediately, retrying if they win the race.

use std::time::Duration;

use rusb::{Direction, GlobalContext, TransferType};
use thiserror::Error;

use crate::sdk::{model_from_product_string, nikon_usb_devices, read_usb_string};

// USB Still Image class (PIMA 15740): class 0x06, subclass 0x01, protocol 0x01.
const STILL_IMAGE_CLASS: u8 = 0x06;
const STILL_IMAGE_SUBCLASS: u8 = 0x01;
const STILL_IMAGE_PROTOCOL: u8 = 0x01;

const CONTAINER_COMMAND: u16 = 1;
const CONTAINER_DATA: u16 = 2;
const CONTAINER_RESPONSE: u16 = 3;
const CONTAINER_EVENT: u16 = 4;

const OP_GET_DEVICE_INFO: u16 = 0x1001;
const OP_OPEN_SESSION: u16 = 0x1002;
const OP_CLOSE_SESSION: u16 = 0x1003;

/// `0x943B` — generic vendor-property read wrapper. Param is
/// `0x10000 | propcode`; response data is the property's raw bytes.
const OP_VENDOR_PROP_READ: u16 = 0x943B;

/// `0x90E8` — FTP setup/status step. Not preceded by a distinct params list;
/// carries a fixed 3-byte data-out payload. A *second* `0x90EE` write in the
/// same PTP session hangs without this sent immediately before it (confirmed
/// 2026-09-06); the first write in a fresh session doesn't need it, but this
/// module always sends it anyway rather than tracking write count.
const OP_FTP_SETUP: u16 = 0x90E8;
const FTP_SETUP_DATA: [u8; 3] = [0x01, 0x00, 0x00];

/// `0x90EE` — FTP profile write. Two params, both `0` in every captured
/// instance (slot index? unconfirmed — never varied). See
/// `docs/nx-field-session-2026-07-09.md`'s field table for the data blob.
const OP_FTP_PROFILE_WRITE: u16 = 0x90EE;

const RESP_OK: u16 = 0x2001;
const RESP_SESSION_ALREADY_OPEN: u16 = 0x201E;

/// One fact this module's iterative reverse-engineering effort (see module
/// doc) has established about a vendor PTP operation. Previously this
/// knowledge lived only as prose scattered across per-const doc comments
/// and `docs/nx-field-session-2026-07-09.md`, with no single place to check
/// "is op X confirmed over USB" as more ops get added — this table is that
/// place.
#[derive(Debug, Clone, Copy)]
struct VendorOpInfo {
    code: u16,
    name: &'static str,
    /// Confirmed to work over plain USB PTP, not just WiFi/PTP-IP (the
    /// 2026-09-06 follow-up session — see module doc).
    usb_confirmed: bool,
    /// Whether this crate currently exposes a way to call it.
    wired_up: bool,
}

const VENDOR_OPS: &[VendorOpInfo] = &[
    VendorOpInfo { code: OP_GET_DEVICE_INFO, name: "GetDeviceInfo", usb_confirmed: true, wired_up: true },
    VendorOpInfo { code: OP_OPEN_SESSION, name: "OpenSession", usb_confirmed: true, wired_up: true },
    VendorOpInfo { code: OP_CLOSE_SESSION, name: "CloseSession", usb_confirmed: true, wired_up: true },
    VendorOpInfo { code: OP_VENDOR_PROP_READ, name: "VendorPropRead", usb_confirmed: true, wired_up: true },
    VendorOpInfo { code: OP_FTP_SETUP, name: "FtpSetup", usb_confirmed: true, wired_up: true },
    VendorOpInfo { code: OP_FTP_PROFILE_WRITE, name: "FtpProfileWrite", usb_confirmed: true, wired_up: true },
    // 0x9413 (IPTC profile write): confirmed to work over USB PTP alongside
    // the ops above, but not wired up here — its data-blob preamble
    // couldn't be decoded from existing captures (only two byte-identical
    // instances exist anywhere). See the module doc's open items.
    VendorOpInfo { code: 0x9413, name: "IptcProfileWrite", usb_confirmed: true, wired_up: false },
];

/// Look up what's confirmed about vendor op `code`, if this module knows
/// anything about it at all.
fn vendor_op_info(code: u16) -> Option<&'static VendorOpInfo> {
    VENDOR_OPS.iter().find(|op| op.code == code)
}

/// What an OpenSession response code means for `open_ptp_session`'s retry
/// decision — pulled out as its own pure classification so it's unit
/// testable independent of the real USB round trip. See
/// `PtpUsbSession::open_ptp_session`'s doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenSessionResponse {
    Ok,
    RetryOnce,
    Failed,
}

fn classify_open_session_response(code: u16) -> OpenSessionResponse {
    if code == RESP_OK {
        OpenSessionResponse::Ok
    } else if code == RESP_SESSION_ALREADY_OPEN {
        OpenSessionResponse::RetryOnce
    } else {
        OpenSessionResponse::Failed
    }
}

/// Camera models these reverse-engineered wire formats have been confirmed
/// against (see the module doc for the reverse-engineering history). Model
/// strings match `firmware::model_from_usb_product`'s output exactly (e.g.
/// `"Z 9"`, `"Z6_3"` for Z6III — space vs. underscore is not a typo, it's
/// this codebase's existing inconsistent USB product-string convention).
/// Sending an unconfirmed model a vendor write risks writing a malformed
/// blob to hardware whose exact protocol implementation was never checked.
const CONFIRMED_MODELS: &[&str] = &["Z 9", "Z6_3"];

fn model_confirmed(model: &str) -> bool {
    CONFIRMED_MODELS.contains(&model)
}

const CLAIM_RETRY_ATTEMPTS: u32 = 40;
const CLAIM_RETRY_DELAY: Duration = Duration::from_millis(50);
const BULK_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_ID: u32 = 1;

/// Aggregate ceiling on one `transact()` call's whole read loop. Each
/// individual `read_bulk` is already capped at `BULK_TIMEOUT`, but the loop
/// around it (waiting for a Response container amid Data/Event containers)
/// previously had no bound of its own — a malfunctioning device, or a
/// misparsed stream, could hang it indefinitely. Generous relative to real
/// transactions (a handful of `BULK_TIMEOUT`-capped reads at most) so it
/// only ever fires on a genuinely stuck transaction.
const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling on one `transact()` call's total accumulated Data-phase payload.
/// Nothing in the PTP container format itself bounds how many Data
/// containers a misbehaving/malicious device could send before ever sending
/// a Response — without this, `data_in` would grow unboundedly. Generous
/// relative to anything this module's own vendor ops actually transfer
/// (the FTP profile blob is a few hundred bytes; even an unusually large
/// vendor-property read is expected to be well under this).
const MAX_TRANSACTION_DATA_BYTES: usize = 16 * 1024 * 1024;

// ─────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PtpUsbError {
    #[error("no Nikon USB camera found")]
    NoDevice,
    #[error("multiple Nikon USB cameras found ({0}); pass a serial to disambiguate")]
    AmbiguousDevice(usize),
    #[error("no Nikon USB camera with serial {0:?}")]
    SerialNotFound(String),
    #[error("USB error: {0}")]
    Usb(#[from] rusb::Error),
    #[error("could not find a Still Image (PTP) class interface on this device")]
    NoPtpInterface,
    #[error("could not claim the PTP USB interface after {0} attempts (macOS's ptpcamerad/icdd may be holding it)")]
    ClaimFailed(u32),
    #[error("PTP container too short ({0} bytes)")]
    ShortContainer(usize),
    #[error("PTP container declared {declared} bytes but only {got} were received")]
    TruncatedContainer { declared: usize, got: usize },
    #[error("PTP response params ({0} bytes) are not a whole number of 4-byte words")]
    MisalignedResponseParams(usize),
    #[error("PTP transaction did not complete within {0:?}")]
    TransactionTimedOut(Duration),
    #[error("PTP transaction's accumulated data ({got} bytes) exceeds the {limit}-byte limit")]
    TransactionDataTooLarge { got: usize, limit: usize },
    #[error("OpenSession failed: {0}")]
    OpenSessionFailed(PtpResponseCode),
    #[error("PTP operation 0x{op:04x} failed: {code}")]
    OperationFailed { op: u16, code: PtpResponseCode },
    #[error("FTP profile field {field} is {len} bytes, exceeds the {max}-byte limit")]
    FtpFieldTooLong { field: &'static str, len: usize, max: usize },
    #[error("FTP profile field {field} contains non-ASCII characters, which this encoding does not support")]
    FtpFieldNotAscii { field: &'static str },
    #[error("camera model {0:?} is not in this module's confirmed-safe list for vendor writes; wire format may not match")]
    UnconfirmedModel(String),
}

/// A PTP response code, with `Display` naming the common ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtpResponseCode(pub u16);

impl std::fmt::Display for PtpResponseCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.0 {
            0x2001 => "OK",
            0x2002 => "GeneralError",
            0x2003 => "SessionNotOpen",
            0x2004 => "InvalidTransactionID",
            0x2005 => "OperationNotSupported",
            0x2006 => "ParameterNotSupported",
            0x2007 => "IncompleteTransfer",
            0x200A => "DevicePropNotSupported",
            0x200F => "AccessDenied",
            0x2017 => "UnknownVendorCode",
            0x2019 => "DeviceBusy",
            0x201D => "InvalidParameter",
            0x201E => "SessionAlreadyOpen",
            _ => "",
        };
        if name.is_empty() {
            write!(f, "0x{:04x}", self.0)
        } else {
            write!(f, "{name} (0x{:04x})", self.0)
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// macOS's own PTP claimants
// ─────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn kill_macos_ptp_claimants() {
    for name in ["ptpcamerad", "icdd"] {
        // -9 (SIGKILL) matters here, not just -x: the reference Python
        // prototype used `kill -9`, and a plain pkill (SIGTERM) measurably
        // lost the reclaim race in testing — these daemons likely don't die
        // (and release the USB interface) immediately on SIGTERM. Best-
        // effort: pkill exits non-zero when nothing matches, the common case
        // after the first kill. Fixed argv, no shell.
        let _ = std::process::Command::new("pkill").args(["-9", "-x", name]).status();
    }
}

#[cfg(not(target_os = "macos"))]
fn kill_macos_ptp_claimants() {}

// ─────────────────────────────────────────────────────────────────────────
// Device discovery + interface claiming
// ─────────────────────────────────────────────────────────────────────────

/// The identifier `find_device` matches `--serial` against for one
/// candidate: the real USB `iSerialNumber` string if the descriptor had one,
/// else a fallback derived purely from USB topology (bus/address) so a
/// camera with no readable serial string is still individually addressable
/// instead of colliding with every other serial-less camera on `""`.
///
/// This is deliberately NOT the same fallback format as `main.rs`'s
/// `snapshot_serial`'s `"id-{sdk_id}"` (used by `fleet discover`/`snapshot`
/// output) — this module has no MAID SDK dependency at all (see the module
/// doc), so it has no access to the SDK's device ids to reproduce that
/// scheme. A `--serial id-5` copied from `fleet discover` output will not
/// match here; use `--serial usb-<bus>:<addr>` (printed by this crate's own
/// discovery, when a device has no real serial) or the real serial instead.
fn candidate_serial(device: &rusb::Device<GlobalContext>, real_serial: &str) -> String {
    if !real_serial.is_empty() {
        return real_serial.to_string();
    }
    fallback_serial(device.bus_number(), device.address())
}

/// The `"usb-<bus>:<addr>"` fallback format itself, split out from
/// [`candidate_serial`] so it's testable without needing a real
/// `rusb::Device` (which nothing in this crate can construct outside live
/// hardware) — the device-touching half of `candidate_serial` is a single
/// pass-through call into this, not a second copy of the format string.
fn fallback_serial(bus: u8, address: u8) -> String {
    format!("usb-{bus}:{address}")
}

/// Returns `(candidates, skipped_count)` — `skipped_count` is how many
/// Nikon USB devices were seen but couldn't be opened (e.g. macOS's
/// `ptpcamerad` still holding the interface) and so couldn't be identified;
/// they're silently absent from `candidates` otherwise, which previously
/// gave no signal that a real second camera was skipped rather than absent.
fn candidate_devices() -> (Vec<(rusb::Device<GlobalContext>, String, String)>, usize) {
    let mut candidates = Vec::new();
    let mut skipped = 0usize;
    for device in nikon_usb_devices() {
        let Ok(desc) = device.device_descriptor() else {
            skipped += 1;
            continue;
        };
        let Ok(handle) = device.open() else {
            skipped += 1;
            continue;
        };
        let real_serial = read_usb_string(&handle, desc.serial_number_string_index().unwrap_or(0));
        let serial = candidate_serial(&device, &real_serial);
        let product = read_usb_string(&handle, desc.product_string_index().unwrap_or(0));
        let model = model_from_product_string(&product);
        candidates.push((device, serial, model));
    }
    (candidates, skipped)
}

/// Pick one item from a `(serial, item)` candidate list, by serial (exact
/// match) or — if none given — the sole candidate. Extracted as a pure,
/// hardware-independent function so tests exercise this exact selection
/// logic directly instead of a hand-copied re-implementation that could
/// silently drift from what `find_device` actually runs (confirmed to have
/// already happened once: `find_device`'s real serial-matching didn't
/// recognize a fallback-serial form the test suite's old parallel `select()`
/// helper wasn't exercising either, so the mismatch went uncaught).
fn select_candidate<T>(candidates: Vec<(String, T)>, serial: Option<&str>) -> Result<T, PtpUsbError> {
    match serial {
        Some(s) => candidates
            .into_iter()
            .find(|(ser, _)| ser == s)
            .map(|(_, item)| item)
            .ok_or_else(|| PtpUsbError::SerialNotFound(s.to_string())),
        None => match candidates.len() {
            0 => Err(PtpUsbError::NoDevice),
            1 => Ok(candidates.into_iter().next().unwrap().1),
            n => Err(PtpUsbError::AmbiguousDevice(n)),
        },
    }
}

/// Resolves a device by serial (or the sole candidate), returning it
/// together with its model string so callers can gate on confirmed-safe
/// models before sending reverse-engineered wire formats.
fn find_device(serial: Option<&str>) -> Result<(rusb::Device<GlobalContext>, String), PtpUsbError> {
    let (candidates, skipped) = candidate_devices();
    if skipped > 0 {
        eprintln!(
            "warning: {skipped} Nikon USB device(s) were seen but could not be opened/identified \
             (commonly macOS's ptpcamerad/icdd still holding the interface) — not counted as candidates"
        );
    }
    let items = candidates.into_iter().map(|(d, ser, model)| (ser, (d, model))).collect();
    select_candidate(items, serial)
}

/// The PTP (Still Image class) interface number and its bulk IN/OUT endpoint
/// addresses, found by walking the active config's interface descriptors.
struct PtpEndpoints {
    interface: u8,
    ep_out: u8,
    ep_in: u8,
}

fn find_ptp_endpoints(device: &rusb::Device<GlobalContext>) -> Result<PtpEndpoints, PtpUsbError> {
    let config = device.active_config_descriptor()?;
    for interface in config.interfaces() {
        for desc in interface.descriptors() {
            if desc.class_code() != STILL_IMAGE_CLASS
                || desc.sub_class_code() != STILL_IMAGE_SUBCLASS
                || desc.protocol_code() != STILL_IMAGE_PROTOCOL
            {
                continue;
            }
            let mut ep_out = None;
            let mut ep_in = None;
            for ep in desc.endpoint_descriptors() {
                if ep.transfer_type() != TransferType::Bulk {
                    continue;
                }
                match ep.direction() {
                    Direction::Out => ep_out = ep_out.or(Some(ep.address())),
                    Direction::In => ep_in = ep_in.or(Some(ep.address())),
                }
            }
            if let (Some(ep_out), Some(ep_in)) = (ep_out, ep_in) {
                return Ok(PtpEndpoints { interface: desc.interface_number(), ep_out, ep_in });
            }
        }
    }
    Err(PtpUsbError::NoPtpInterface)
}

/// Claim the PTP interface, retrying against macOS's own PTP claimants.
///
/// Killing `ptpcamerad`/`icdd` once is not enough — they re-claim the
/// interface as soon as it's touched, and SIP blocks disabling their
/// LaunchAgents outright. So this kills them and attempts the claim in a
/// tight loop, which reliably wins the race in practice (confirmed
/// interactively on a Z6III 2026-09-06).
fn claim_with_retry(
    handle: &rusb::DeviceHandle<GlobalContext>,
    interface: u8,
) -> Result<(), PtpUsbError> {
    let mut last_err = None;
    for _ in 0..CLAIM_RETRY_ATTEMPTS {
        kill_macos_ptp_claimants();
        // Re-asserted every attempt, not just once up front: macOS's PTP
        // claimants reset config state when they re-grab the device, so a
        // one-time set_active_configuration before the loop isn't enough —
        // confirmed against the reference Python prototype, which also
        // re-issues this every iteration.
        let _ = handle.set_active_configuration(1);
        match handle.claim_interface(interface) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(CLAIM_RETRY_DELAY);
            }
        }
    }
    let _ = last_err;
    Err(PtpUsbError::ClaimFailed(CLAIM_RETRY_ATTEMPTS))
}

// ─────────────────────────────────────────────────────────────────────────
// Container framing
// ─────────────────────────────────────────────────────────────────────────

fn encode_command(code: u16, transaction_id: u32, params: &[u32]) -> Vec<u8> {
    let length = 12 + 4 * params.len() as u32;
    let mut buf = Vec::with_capacity(length as usize);
    buf.extend_from_slice(&length.to_le_bytes());
    buf.extend_from_slice(&CONTAINER_COMMAND.to_le_bytes());
    buf.extend_from_slice(&code.to_le_bytes());
    buf.extend_from_slice(&transaction_id.to_le_bytes());
    for p in params {
        buf.extend_from_slice(&p.to_le_bytes());
    }
    buf
}

fn encode_data(code: u16, transaction_id: u32, data: &[u8]) -> Vec<u8> {
    let length = 12 + data.len() as u32;
    let mut buf = Vec::with_capacity(length as usize);
    buf.extend_from_slice(&length.to_le_bytes());
    buf.extend_from_slice(&CONTAINER_DATA.to_le_bytes());
    buf.extend_from_slice(&code.to_le_bytes());
    buf.extend_from_slice(&transaction_id.to_le_bytes());
    buf.extend_from_slice(data);
    buf
}

/// A parsed container header + whatever payload followed it (params for
/// Command/Response, raw bytes for Data).
#[derive(Debug)]
struct Container {
    kind: u16,
    #[allow(dead_code)] // code/txn round-trip for future ops; not needed by read_vendor_property yet
    code: u16,
    #[allow(dead_code)]
    transaction_id: u32,
    payload: Vec<u8>,
}

fn decode_container(raw: &[u8]) -> Result<Container, PtpUsbError> {
    if raw.len() < 12 {
        return Err(PtpUsbError::ShortContainer(raw.len()));
    }
    let length = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as usize;
    // The declared length is wire-controlled data (a malformed/corrupt
    // response is a real input on this reverse-engineered, live-hardware
    // path, not theoretical) — floor it at the header size before using it
    // to index, or a declared length under 12 panics the slice below.
    if length < 12 {
        return Err(PtpUsbError::ShortContainer(length));
    }
    let kind = u16::from_le_bytes(raw[4..6].try_into().unwrap());
    let code = u16::from_le_bytes(raw[6..8].try_into().unwrap());
    let transaction_id = u32::from_le_bytes(raw[8..12].try_into().unwrap());
    // `raw` may legitimately be longer than `length` (a single bulk read can
    // capture more than one container's worth of bytes) — that's fine, only
    // the declared portion is the payload. But `raw` being *shorter* than
    // declared means the caller didn't actually receive the full container
    // (e.g. a >16KB response needing more than one bulk read) — silently
    // truncating that down to what's on hand would desync every subsequent
    // read on this connection, so it must be a hard error instead.
    if raw.len() < length {
        return Err(PtpUsbError::TruncatedContainer { declared: length, got: raw.len() });
    }
    let payload = raw[12..length].to_vec();
    Ok(Container { kind, code, transaction_id, payload })
}

fn decode_response_params(payload: &[u8]) -> Result<Vec<u32>, PtpUsbError> {
    if !payload.len().is_multiple_of(4) {
        return Err(PtpUsbError::MisalignedResponseParams(payload.len()));
    }
    Ok(payload
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect())
}

// ─────────────────────────────────────────────────────────────────────────
// 0x90EE FTP-profile blob
//
// Field-by-field map from docs/nx-field-session-2026-07-09.md (found by
// diffing three real captured instances, not guessed). Several structural
// bytes (a header, a per-field "tag", an 82-byte block, a trailing byte)
// have no known semantic meaning — they're copied verbatim from a real
// captured template rather than reconstructed, since the camera presumably
// cares about their exact value and we don't need to know why.
// ─────────────────────────────────────────────────────────────────────────

const FTP_BLOB_HEADER: [u8; 2] = [0xf4, 0x01];

/// 82 bytes between the profile-name field and the SSID section. Contains
/// two 16-UTF16-unit strings and a value shaped like a locally-administered
/// (randomized) MAC address, all byte-identical across every real capture
/// we have — never observed to vary, so treated as an opaque constant.
const FTP_BLOB_MYSTERY_BLOCK: [u8; 82] = [
    0x10, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30,
    0x00, 0x54, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x00,
    0x00, 0x10, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00,
    0x30, 0x00, 0x54, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00,
    0x00, 0x00, 0x02, 0x03, 0x03, 0x57, 0x42, 0xa8, 0xc0, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00,
];

const FTP_TAG_SSID_24GHZ: [u8; 4] = [0x03, 0x00, 0x01, 0x00];
const FTP_TAG_SSID_5GHZ: [u8; 2] = [0x04, 0x04];
const FTP_TAG_HOST: u8 = 0x01;
const FTP_TAG_USERNAME: [u8; 3] = [0x00, 0x15, 0x00];
const FTP_TAG_PORT_SECTION: [u8; 2] = [0x05, 0x00];
const FTP_PORT_SECTION_PAD: [u8; 5] = [0, 0, 0, 0, 0];
const FTP_IPV6_PLACEHOLDER: &str = "0000:0000:0000:0000:0000:0000:0000:0000";
/// Appears once, immediately after the *first* of the three IPv6-placeholder
/// fields only (not after the second) — asymmetric in every real capture we
/// have. Meaning unknown; copied verbatim.
const FTP_IPV6_FIELD1_SEPARATOR: u8 = 0x40;
const FTP_BLOB_TRAILER: u8 = 0x81;

const FTP_ASCII_FIELD_MAX: usize = 64; // generous; real SSIDs/usernames/passwords are far shorter
const FTP_HOST_MAX_UNITS: usize = 200; // 1-byte length field caps this at 255 units anyway

/// Fields for an `0x90EE` FTP profile write. All required; see the module
/// docs for which structural bytes are opaque-but-fixed instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpProfile {
    /// Human-readable profile name shown in camera menus, e.g. `"CLAUDE"`.
    pub profile_name: String,
    pub ssid_24ghz: String,
    pub ssid_5ghz: String,
    /// Hostname or IP address of the FTP server.
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl FtpProfile {
    /// A copy of this profile with the password replaced by same-length
    /// asterisks — for previewing the encoded blob (dry-run output) without
    /// the plaintext password becoming readable in the hex dump, which
    /// would defeat the password-masking on any UI field the value came
    /// from. Same length as the real password so the previewed blob's size
    /// and byte layout still match what an actual write would send.
    pub fn with_password_redacted(&self) -> Self {
        FtpProfile { password: "*".repeat(self.password.chars().count()), ..self.clone() }
    }
}

fn ascii_bytes(field: &'static str, s: &str, max: usize) -> Result<Vec<u8>, PtpUsbError> {
    if !s.is_ascii() {
        return Err(PtpUsbError::FtpFieldNotAscii { field });
    }
    if s.len() > max {
        return Err(PtpUsbError::FtpFieldTooLong { field, len: s.len(), max });
    }
    Ok(s.as_bytes().to_vec())
}

/// `[u32 length in bytes, no null][ascii content]` — the convention used by
/// SSID/username/password fields.
fn encode_ascii_field(field: &'static str, s: &str, max: usize) -> Result<Vec<u8>, PtpUsbError> {
    let bytes = ascii_bytes(field, s, max)?;
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&bytes);
    Ok(out)
}

/// `[u8 length in UTF-16 units incl. null][utf16le content + null]` — the
/// host field's convention (distinct from the profile-name field, which
/// shares this encoding but is otherwise unrelated).
fn encode_utf16_field_1byte_len(field: &'static str, s: &str, max_units: usize) -> Result<Vec<u8>, PtpUsbError> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let len_incl_null = units.len() + 1;
    if len_incl_null > max_units || len_incl_null > u8::MAX as usize {
        return Err(PtpUsbError::FtpFieldTooLong { field, len: s.len(), max: max_units });
    }
    let mut out = Vec::with_capacity(1 + len_incl_null * 2);
    out.push(len_incl_null as u8);
    for u in units {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes()); // null terminator
    Ok(out)
}

fn encode_ipv6_placeholder() -> Vec<u8> {
    let content = FTP_IPV6_PLACEHOLDER.as_bytes();
    let mut out = Vec::with_capacity(4 + content.len());
    out.extend_from_slice(&(content.len() as u32).to_le_bytes());
    out.extend_from_slice(content);
    out
}

/// Build the `0x90EE` data-phase blob for `profile`. Reproduces the real
/// captured wire format byte-for-byte (verified in tests against the
/// original 2026-07-09 capture) rather than a best guess.
pub fn encode_ftp_profile(profile: &FtpProfile) -> Result<Vec<u8>, PtpUsbError> {
    let mut out = Vec::new();
    out.extend_from_slice(&FTP_BLOB_HEADER);
    out.extend_from_slice(&encode_utf16_field_1byte_len("profile_name", &profile.profile_name, 254)?);
    out.extend_from_slice(&FTP_BLOB_MYSTERY_BLOCK);
    out.extend_from_slice(&FTP_TAG_SSID_24GHZ);
    out.extend_from_slice(&encode_ascii_field("ssid_24ghz", &profile.ssid_24ghz, FTP_ASCII_FIELD_MAX)?);
    out.extend_from_slice(&FTP_TAG_SSID_5GHZ);
    out.extend_from_slice(&encode_ascii_field("ssid_5ghz", &profile.ssid_5ghz, FTP_ASCII_FIELD_MAX)?);
    out.push(FTP_TAG_HOST);
    out.extend_from_slice(&encode_utf16_field_1byte_len("host", &profile.host, FTP_HOST_MAX_UNITS)?);
    out.extend_from_slice(&FTP_TAG_USERNAME);
    out.extend_from_slice(&encode_ascii_field("username", &profile.username, FTP_ASCII_FIELD_MAX)?);
    // password has no tag bytes of its own — it immediately follows username's content.
    out.extend_from_slice(&encode_ascii_field("password", &profile.password, FTP_ASCII_FIELD_MAX)?);
    out.extend_from_slice(&FTP_TAG_PORT_SECTION);
    out.extend_from_slice(&profile.port.to_le_bytes());
    out.extend_from_slice(&FTP_PORT_SECTION_PAD);
    out.extend_from_slice(&encode_ipv6_placeholder());
    out.push(FTP_IPV6_FIELD1_SEPARATOR);
    out.extend_from_slice(&encode_ipv6_placeholder());
    out.extend_from_slice(&encode_ipv6_placeholder());
    out.push(FTP_BLOB_TRAILER);
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────
// Session
// ─────────────────────────────────────────────────────────────────────────

/// An open raw-PTP-over-USB session. Closes the PTP session and releases the
/// USB interface on drop (best-effort — errors are ignored, matching
/// `sdk.rs`'s `Device::drop` policy of not panicking on teardown).
pub struct PtpUsbSession {
    handle: rusb::DeviceHandle<GlobalContext>,
    interface: u8,
    ep_out: u8,
    ep_in: u8,
    transaction_id: u32,
    session_open: bool,
    model: String,
    /// Reused scratch buffer for `read_container`'s bulk-IN reads, so a
    /// session doing many reads (every PTP transaction needs at least one,
    /// often more for a Data phase) doesn't allocate+zero a fresh 16384-byte
    /// `Vec` on every single one.
    read_scratch: Vec<u8>,
}

impl PtpUsbSession {
    /// Find the Nikon camera (by serial if given, else the sole one
    /// attached), claim its PTP interface (racing macOS's own claimants),
    /// and open a PTP session.
    pub fn open(serial: Option<&str>) -> Result<Self, PtpUsbError> {
        let (device, model) = find_device(serial)?;
        let endpoints = find_ptp_endpoints(&device)?;
        let handle = device.open()?;
        claim_with_retry(&handle, endpoints.interface)?;

        let mut session = PtpUsbSession {
            handle,
            interface: endpoints.interface,
            ep_out: endpoints.ep_out,
            ep_in: endpoints.ep_in,
            transaction_id: 0,
            session_open: false,
            model,
            read_scratch: vec![0u8; 16384],
        };
        session.open_ptp_session()?;
        Ok(session)
    }

    /// The connected camera's model string (e.g. `"Z 9"`, `"Z6_3"`), as read
    /// from the USB product-string descriptor.
    pub fn model(&self) -> &str {
        &self.model
    }

    fn send_command(&mut self, code: u16, params: &[u32]) -> Result<(), PtpUsbError> {
        let buf = encode_command(code, self.transaction_id, params);
        self.handle.write_bulk(self.ep_out, &buf, BULK_TIMEOUT)?;
        Ok(())
    }

    fn send_data(&mut self, code: u16, data: &[u8]) -> Result<(), PtpUsbError> {
        let buf = encode_data(code, self.transaction_id, data);
        self.handle.write_bulk(self.ep_out, &buf, BULK_TIMEOUT)?;
        Ok(())
    }

    fn read_container(&mut self) -> Result<Container, PtpUsbError> {
        self.read_scratch.resize(16384, 0); // no-op after the first call — reuses existing capacity
        let n = self.handle.read_bulk(self.ep_in, &mut self.read_scratch, BULK_TIMEOUT)?;
        let mut buf = self.read_scratch[..n].to_vec();
        if buf.len() < 12 {
            return decode_container(&buf); // let decode_container report ShortContainer
        }
        let declared_length = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        // A single 16384-byte bulk read isn't guaranteed to capture the whole
        // logical container — this camera's own protocol family has vendor
        // ops documented (docs/nx-field-session-2026-07-09.md) needing more
        // than one chunk. Keep reading until the declared length is on hand;
        // decode_container() still errors instead of silently truncating if
        // the device stops sending before that (n == 0 breaks out early).
        while buf.len() < declared_length {
            let n = self.handle.read_bulk(self.ep_in, &mut self.read_scratch, BULK_TIMEOUT)?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&self.read_scratch[..n]);
        }
        decode_container(&buf)
    }

    /// Send a command (with an optional data-out phase), collect any
    /// data-in, and return `(response_code, response_params, data)`.
    /// Advances the transaction ID only after a full round trip completes,
    /// matching the reference Python prototype's behavior.
    fn transact(
        &mut self,
        op: u16,
        params: &[u32],
        data_out: Option<&[u8]>,
    ) -> Result<(u16, Vec<u32>, Vec<u8>), PtpUsbError> {
        self.send_command(op, params)?;
        if let Some(data) = data_out {
            self.send_data(op, data)?;
        }
        let mut data_in = Vec::new();
        let deadline = std::time::Instant::now() + TRANSACTION_TIMEOUT;
        loop {
            if std::time::Instant::now() >= deadline {
                return Err(PtpUsbError::TransactionTimedOut(TRANSACTION_TIMEOUT));
            }
            let container = self.read_container()?;
            match container.kind {
                CONTAINER_DATA => {
                    data_in.extend(container.payload);
                    if data_in.len() > MAX_TRANSACTION_DATA_BYTES {
                        return Err(PtpUsbError::TransactionDataTooLarge {
                            got: data_in.len(),
                            limit: MAX_TRANSACTION_DATA_BYTES,
                        });
                    }
                }
                CONTAINER_RESPONSE => {
                    let resp_params = decode_response_params(&container.payload)?;
                    self.transaction_id += 1;
                    return Ok((container.code, resp_params, data_in));
                }
                CONTAINER_EVENT => continue, // shouldn't arrive on the bulk pipe; ignore defensively
                other => {
                    // An unrecognized container kind is a protocol violation
                    // (or a misparse — see decode_container's own
                    // truncation/desync guards). Previously silently looped
                    // forever; now at least visible, and bounded by the
                    // deadline check above regardless.
                    eprintln!("warning: PTP transaction received unrecognized container kind 0x{other:04x}, ignoring");
                }
            }
        }
    }

    /// Live-hardware-only from here down: `transact`/`open_ptp_session` and
    /// everything built on them need a real USB device (`self.handle`) to
    /// exercise at all, and this crate deliberately has no mock/fake
    /// transport layer (see the repo's testing conventions — no mocking
    /// anywhere in this codebase's Rust tests). The one piece of decision
    /// logic that doesn't need real I/O — classifying an OpenSession
    /// response — is pulled out into [`classify_open_session_response`]
    /// below specifically so it has its own unit tests; the surrounding
    /// retry *sequencing* (send, read, close, resend) is exercised only by
    /// live hardware, same as `read_container`'s accumulation loop and
    /// `claim_with_retry`'s race-retry loop.
    fn open_ptp_session(&mut self) -> Result<(), PtpUsbError> {
        let (code, _, _) = self.transact(OP_OPEN_SESSION, &[SESSION_ID], None)?;
        match classify_open_session_response(code) {
            OpenSessionResponse::Ok => {}
            OpenSessionResponse::RetryOnce => {
                // Leftover session from a prior claimant (typically macOS's
                // own ptpcamerad, which we just evicted) — close it and
                // retry once.
                self.transaction_id = 0;
                let _ = self.transact(OP_CLOSE_SESSION, &[], None);
                self.transaction_id = 0;
                let (code, _, _) = self.transact(OP_OPEN_SESSION, &[SESSION_ID], None)?;
                // Deliberately not re-matching classify_open_session_response
                // here: a second RESP_SESSION_ALREADY_OPEN on the retry
                // itself is treated as fatal, not looped again.
                if code != RESP_OK {
                    return Err(PtpUsbError::OpenSessionFailed(PtpResponseCode(code)));
                }
            }
            OpenSessionResponse::Failed => {
                return Err(PtpUsbError::OpenSessionFailed(PtpResponseCode(code)));
            }
        }
        self.session_open = true;
        Ok(())
    }

    /// `Err(OperationFailed)` if `code` isn't `RESP_OK`, else `Ok(())`.
    /// Centralizes the response-code check every op below needs, instead of
    /// each hand-copying the same `if code != RESP_OK { ... }` — this
    /// module is an explicitly-growing reverse-engineering effort (more ops
    /// expected per the module doc), so a shared check is one less place
    /// for a future op to get the check wrong or skip it.
    fn require_ok(op: u16, code: u16) -> Result<(), PtpUsbError> {
        if code != RESP_OK {
            return Err(PtpUsbError::OperationFailed { op, code: PtpResponseCode(code) });
        }
        Ok(())
    }

    /// Confirm the session/pipe is still responsive. Exposed mainly for
    /// tests/diagnostics — `read_vendor_property` doesn't need this itself.
    pub fn ping(&mut self) -> Result<(), PtpUsbError> {
        let (code, _, _) = self.transact(OP_GET_DEVICE_INFO, &[], None)?;
        Self::require_ok(OP_GET_DEVICE_INFO, code)
    }

    /// Read a vendor property via the `0x943B` wrapper. `propcode` is the
    /// raw `0xD0xx`/`0x5xxx` property code (without the `0x10000` flag —
    /// this adds it). Returns the property's raw bytes on success.
    pub fn read_vendor_property(&mut self, propcode: u16) -> Result<Vec<u8>, PtpUsbError> {
        if !model_confirmed(&self.model) {
            eprintln!(
                "warning: camera model {:?} is not in this module's confirmed-safe list ({:?}) \
                 for vendor-property reads; wire format may not match",
                self.model, CONFIRMED_MODELS
            );
        }
        let param = 0x10000 | propcode as u32;
        let (code, _, data) = self.transact(OP_VENDOR_PROP_READ, &[param], None)?;
        Self::require_ok(OP_VENDOR_PROP_READ, code)?;
        Ok(data)
    }

    /// Write an FTP profile via `0x90EE`. Always sends the `0x90E8` setup
    /// call immediately before it — the first write in a fresh session
    /// doesn't strictly need this, but a second one hangs without it
    /// (confirmed 2026-09-06), and sending it unconditionally is simpler and
    /// safer than tracking write count.
    ///
    /// Refuses to write to a camera model outside [`CONFIRMED_MODELS`] unless
    /// `force_unconfirmed_model` is set — see the module doc and
    /// [`PtpUsbError::UnconfirmedModel`].
    pub fn write_ftp_profile(
        &mut self,
        profile: &FtpProfile,
        force_unconfirmed_model: bool,
    ) -> Result<(), PtpUsbError> {
        if !force_unconfirmed_model && !model_confirmed(&self.model) {
            return Err(PtpUsbError::UnconfirmedModel(self.model.clone()));
        }
        let blob = encode_ftp_profile(profile)?;

        let (code, _, _) = self.transact(OP_FTP_SETUP, &[], Some(&FTP_SETUP_DATA))?;
        Self::require_ok(OP_FTP_SETUP, code)?;

        let (code, _, _) = self.transact(OP_FTP_PROFILE_WRITE, &[0, 0], Some(&blob))?;
        Self::require_ok(OP_FTP_PROFILE_WRITE, code)?;
        Ok(())
    }
}

impl Drop for PtpUsbSession {
    fn drop(&mut self) {
        if self.session_open {
            let _ = self.transact(OP_CLOSE_SESSION, &[], None);
        }
        let _ = self.handle.release_interface(self.interface);
    }
}

/// Open a session, read one vendor property via `0x943B`, close it.
/// Convenience wrapper for one-off reads (e.g. the CLI).
pub fn read_vendor_property(serial: Option<&str>, propcode: u16) -> Result<Vec<u8>, PtpUsbError> {
    let mut session = PtpUsbSession::open(serial)?;
    session.read_vendor_property(propcode)
}

/// Open a session, write an FTP profile via `0x90EE`, close it. Convenience
/// wrapper for one-off writes (e.g. the CLI). See
/// [`PtpUsbSession::write_ftp_profile`] for `force_unconfirmed_model`.
pub fn write_ftp_profile(
    serial: Option<&str>,
    profile: &FtpProfile,
    force_unconfirmed_model: bool,
) -> Result<(), PtpUsbError> {
    let mut session = PtpUsbSession::open(serial)?;
    session.write_ftp_profile(profile, force_unconfirmed_model)
}

/// Format `data` as lowercase space-separated hex byte pairs, e.g.
/// `"de ad be ef"`. Shared by the CLI and the egui GUI (previously defined
/// identically in both `src/main.rs` and `gui/src/main.rs` despite both
/// already depending on this crate) so there's one definition, not two that
/// could silently diverge.
pub fn hex_bytes(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// Parse a vendor property code as `"0xD053"`/`"0XD053"` (hex) or a plain
/// decimal string. Shared by the CLI and both GUIs so the accepted input
/// format can't drift between them.
pub fn parse_propcode(s: &str) -> Result<u16, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16).map_err(|e| format!("invalid hex property code {s:?}: {e}"))
    } else {
        s.parse::<u16>().map_err(|e| format!("invalid property code {s:?}: {e}"))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── container framing ───────────────────────────────────────────────

    #[test]
    fn encode_command_no_params() {
        let buf = encode_command(0x1001, 0, &[]);
        assert_eq!(buf, vec![12, 0, 0, 0, 1, 0, 0x01, 0x10, 0, 0, 0, 0]);
    }

    #[test]
    fn encode_command_one_param() {
        // 0x943B with param 0x1_D053 (0x10000 | 0xD053), transaction id 1.
        let buf = encode_command(0x943B, 1, &[0x1D053]);
        assert_eq!(buf.len(), 16);
        assert_eq!(&buf[0..4], &16u32.to_le_bytes());
        assert_eq!(&buf[4..6], &CONTAINER_COMMAND.to_le_bytes());
        assert_eq!(&buf[6..8], &0x943Bu16.to_le_bytes());
        assert_eq!(&buf[8..12], &1u32.to_le_bytes());
        assert_eq!(&buf[12..16], &0x1D053u32.to_le_bytes());
    }

    #[test]
    fn encode_data_roundtrips_payload() {
        let buf = encode_data(0x9413, 2, &[0xAA, 0xBB, 0xCC]);
        assert_eq!(buf.len(), 15);
        assert_eq!(&buf[0..4], &15u32.to_le_bytes());
        assert_eq!(&buf[4..6], &CONTAINER_DATA.to_le_bytes());
        assert_eq!(&buf[12..], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn decode_container_rejects_short_input() {
        let err = decode_container(&[0, 0, 0]).unwrap_err();
        assert!(matches!(err, PtpUsbError::ShortContainer(3)));
    }

    #[test]
    fn decode_container_accepts_exactly_12_bytes() {
        // The `<` vs `<=` boundary on the raw.len() < 12 guard: exactly 12
        // bytes is a completely normal zero-payload PTP response (e.g. an
        // OpenSession/CloseSession ack) and must succeed, not be rejected.
        let mut raw = Vec::new();
        raw.extend_from_slice(&12u32.to_le_bytes());
        raw.extend_from_slice(&CONTAINER_RESPONSE.to_le_bytes());
        raw.extend_from_slice(&RESP_OK.to_le_bytes());
        raw.extend_from_slice(&0u32.to_le_bytes());

        let c = decode_container(&raw).unwrap();
        assert!(c.payload.is_empty());
    }

    #[test]
    fn decode_container_rejects_declared_length_below_header_size() {
        // A malformed/corrupt response could declare a length under 12 even
        // though the actual read is >= 12 bytes — must error, not panic via
        // an out-of-range slice (raw[12..end] with end < 12).
        let mut raw = Vec::new();
        raw.extend_from_slice(&5u32.to_le_bytes()); // declared length: 5, below the 12-byte header floor
        raw.extend_from_slice(&CONTAINER_RESPONSE.to_le_bytes());
        raw.extend_from_slice(&RESP_OK.to_le_bytes());
        raw.extend_from_slice(&0u32.to_le_bytes());

        let err = decode_container(&raw).unwrap_err();
        assert!(matches!(err, PtpUsbError::ShortContainer(5)));
    }

    #[test]
    fn decode_container_errors_instead_of_silently_truncating_short_buffer() {
        // Declared length exceeds what's actually in the buffer (e.g. a
        // >16KB response that needed more than one bulk read) — must error,
        // not silently hand back a truncated payload that would desync the
        // next container parse.
        let mut raw = Vec::new();
        raw.extend_from_slice(&100u32.to_le_bytes()); // declares 100 bytes total
        raw.extend_from_slice(&CONTAINER_RESPONSE.to_le_bytes());
        raw.extend_from_slice(&RESP_OK.to_le_bytes());
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw.extend_from_slice(&[0xAB; 10]); // only 10 payload bytes actually present, not 88

        let err = decode_container(&raw).unwrap_err();
        assert!(matches!(err, PtpUsbError::TruncatedContainer { declared: 100, got: 22 }));
    }

    #[test]
    fn decode_container_response_with_one_param() {
        // A minimal Response container: OK (0x2001), txn=1, one param (0x2A).
        let mut raw = Vec::new();
        raw.extend_from_slice(&16u32.to_le_bytes());
        raw.extend_from_slice(&CONTAINER_RESPONSE.to_le_bytes());
        raw.extend_from_slice(&RESP_OK.to_le_bytes());
        raw.extend_from_slice(&1u32.to_le_bytes());
        raw.extend_from_slice(&0x2Au32.to_le_bytes());

        let c = decode_container(&raw).unwrap();
        assert_eq!(c.kind, CONTAINER_RESPONSE);
        assert_eq!(c.code, RESP_OK);
        assert_eq!(c.transaction_id, 1);
        assert_eq!(decode_response_params(&c.payload).unwrap(), vec![0x2A]);
    }

    #[test]
    fn decode_container_truncates_to_declared_length() {
        // Declared length is 12 (no payload) but the buffer has trailing
        // garbage (e.g. leftover bytes from a larger read) — must not leak
        // into the payload. This is distinct from the "buffer is SHORTER
        // than declared" case above: here raw is longer than declared, which
        // is legitimate (a single bulk read can capture more than one
        // container's worth of bytes) and must still just cap at `length`.
        let mut raw = Vec::new();
        raw.extend_from_slice(&12u32.to_le_bytes());
        raw.extend_from_slice(&CONTAINER_RESPONSE.to_le_bytes());
        raw.extend_from_slice(&RESP_OK.to_le_bytes());
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw.extend_from_slice(&[0xFF; 8]); // garbage past the declared length

        let c = decode_container(&raw).unwrap();
        assert!(c.payload.is_empty());
    }

    #[test]
    fn decode_response_params_empty_payload() {
        assert_eq!(decode_response_params(&[]).unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn decode_response_params_rejects_misaligned_payload() {
        // 5 bytes: not a whole number of 4-byte u32 params. Silently
        // dropping the trailing byte via chunks_exact would lose data with
        // no signal — must error instead.
        let err = decode_response_params(&[0, 0, 0, 0, 0xFF]).unwrap_err();
        assert!(matches!(err, PtpUsbError::MisalignedResponseParams(5)));
    }

    #[test]
    fn decode_response_params_multiple() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());
        assert_eq!(decode_response_params(&payload).unwrap(), vec![1, 0xDEADBEEF]);
    }

    // ── 0x90EE FTP-profile encoder ──────────────────────────────────────
    //
    // The critical test: reproduce the exact 325-byte blob captured live
    // from a real NX Field session (session-20260709-204600-ftptest.pcap,
    // command channel port 53773) byte-for-byte from just the field values.
    // If this passes, the encoder's understanding of the wire format is
    // provably correct for every real example we have, not just plausible.

    fn reference_ftp_blob_hex() -> &'static str {
        // Verbatim from session-20260709-204600-ftptest.pcap, command
        // channel port 53773, request index 1069's EndData payload (minus
        // its 4-byte PTP/IP transaction-id prefix, which is transport
        // framing, not part of the actual 0x90EE data).
        concat!(
            "f4010743004c0041005500440045000000103000300030003000300030003000300054003000",
            "3000300030003000300000001030003000300030003000300030003000540030003000300030",
            "003000300000000203035742a8c0180000000000000000030001000b0000006e696b6f6e2d73",
            "6e6f6f700404090000006e696b6f6e3567687a010d3100390032002e003100360038002e0036",
            "0036002e003100000000150005000000666c6565740800000031323334353637380500901f00",
            "0000000027000000303030303a303030303a303030303a303030303a303030303a303030303a",
            "303030303a303030304027000000303030303a303030303a303030303a303030303a30303030",
            "3a303030303a303030303a3030303027000000303030303a303030303a303030303a30303030",
            "3a303030303a303030303a303030303a3030303081",
        )
    }

    #[test]
    fn encode_ftp_profile_matches_real_capture_byte_for_byte() {
        let expected = hex::decode_hex(reference_ftp_blob_hex());

        let profile = FtpProfile {
            profile_name: "CLAUDE".into(),
            ssid_24ghz: "nikon-snoop".into(),
            ssid_5ghz: "nikon5ghz".into(),
            host: "192.168.66.1".into(),
            port: 8080,
            username: "fleet".into(),
            password: "12345678".into(),
        };
        let actual = encode_ftp_profile(&profile).unwrap();
        assert_eq!(actual.len(), expected.len(), "length mismatch");
        if actual != expected {
            let first_diff = actual.iter().zip(expected.iter()).position(|(a, e)| a != e).unwrap();
            panic!(
                "byte content mismatch at offset {first_diff}\n  actual:   {:02x?}\n  expected: {:02x?}",
                &actual[first_diff.saturating_sub(4)..(first_diff + 12).min(actual.len())],
                &expected[first_diff.saturating_sub(4)..(first_diff + 12).min(expected.len())],
            );
        }
    }

    #[test]
    fn encode_ftp_profile_third_capture_variant_byte_for_byte() {
        // port 54731's instance (317 bytes): same as the reference capture
        // above except profile_name "C2" instead of "CLAUDE" -- confirms the
        // profile-name field's length actually drives the overall blob
        // length correctly, not just in the one reference case.
        //
        // NOTE: port 56171's instance (331 bytes) is NOT reproducible by
        // this encoder -- it has an extra optional UTF-16 field (content
        // "Claude", likely a device/hostname field) between host and
        // username that doesn't appear in this or the reference capture.
        // Not modeled here since it isn't needed for any field this API
        // exposes; see docs/nx-field-session-2026-07-09.md.
        let hex = concat!(
            "f401034300320000001030003000300030003000300030003000540030003000300030003000",
            "3000000010300030003000300030003000300030005400300030003000300030003000000002",
            "03035742a8c0180000000000000000030001000b0000006e696b6f6e2d736e6f6f7004040900",
            "00006e696b6f6e3567687a010d3100390032002e003100360038002e00360036002e00310000",
            "0000150005000000666c6565740800000031323334353637380500901f000000000027000000",
            "303030303a303030303a303030303a303030303a303030303a303030303a303030303a303030",
            "304027000000303030303a303030303a303030303a303030303a303030303a303030303a3030",
            "30303a3030303027000000303030303a303030303a303030303a303030303a303030303a3030",
            "30303a303030303a3030303081",
        );
        let expected = hex::decode_hex(hex);

        let profile = FtpProfile {
            profile_name: "C2".into(),
            ssid_24ghz: "nikon-snoop".into(),
            ssid_5ghz: "nikon5ghz".into(),
            host: "192.168.66.1".into(),
            port: 8080,
            username: "fleet".into(),
            password: "12345678".into(),
        };
        let actual = encode_ftp_profile(&profile).unwrap();
        assert_eq!(actual.len(), expected.len(), "length mismatch");
        assert_eq!(actual, expected, "byte content mismatch");
    }

    #[test]
    fn encode_ftp_profile_rejects_non_ascii_username() {
        let mut profile = valid_profile();
        profile.username = "flëet".into();
        assert!(matches!(
            encode_ftp_profile(&profile),
            Err(PtpUsbError::FtpFieldNotAscii { field: "username" })
        ));
    }

    #[test]
    fn encode_ftp_profile_rejects_oversized_password() {
        let mut profile = valid_profile();
        profile.password = "x".repeat(FTP_ASCII_FIELD_MAX + 1);
        assert!(matches!(
            encode_ftp_profile(&profile),
            Err(PtpUsbError::FtpFieldTooLong { field: "password", .. })
        ));
    }

    #[test]
    fn encode_ftp_profile_accepts_password_at_exactly_the_max() {
        // Kills the `s.len() > max` vs `>= max` mutant on ascii_bytes: only
        // the above-max rejection was previously tested, not the
        // exactly-at-max success case.
        let mut profile = valid_profile();
        profile.password = "x".repeat(FTP_ASCII_FIELD_MAX);
        assert!(encode_ftp_profile(&profile).is_ok());
    }

    #[test]
    fn encode_ftp_profile_port_is_little_endian() {
        let profile = valid_profile();
        let blob = encode_ftp_profile(&profile).unwrap();
        // Port section starts right after password's content in this profile.
        let port_tag_pos = blob.windows(2).rposition(|w| w == FTP_TAG_PORT_SECTION).unwrap();
        let port_bytes = &blob[port_tag_pos + 2..port_tag_pos + 4];
        assert_eq!(u16::from_le_bytes(port_bytes.try_into().unwrap()), profile.port);
    }

    // ── with_password_redacted ──────────────────────────────────────────

    #[test]
    fn with_password_redacted_hides_password_content() {
        let profile = valid_profile();
        let redacted = profile.with_password_redacted();
        assert_ne!(redacted.password, profile.password);
        assert!(!redacted.password.contains("pass"));
    }

    #[test]
    fn with_password_redacted_preserves_length_and_other_fields() {
        let profile = valid_profile();
        let redacted = profile.with_password_redacted();
        assert_eq!(redacted.password.len(), profile.password.len());
        assert_eq!(redacted.username, profile.username);
        assert_eq!(redacted.host, profile.host);
    }

    #[test]
    fn with_password_redacted_empty_password_stays_empty() {
        let mut profile = valid_profile();
        profile.password = String::new();
        assert_eq!(profile.with_password_redacted().password, "");
    }

    #[test]
    fn encode_ftp_profile_with_redacted_password_has_same_length_as_real() {
        // The whole point of redacting-then-encoding for a preview: the
        // previewed blob's size/layout must still match what a real write
        // would send, or the preview is misleading about more than the
        // password.
        let profile = valid_profile();
        let real_blob = encode_ftp_profile(&profile).unwrap();
        let redacted_blob = encode_ftp_profile(&profile.with_password_redacted()).unwrap();
        assert_eq!(real_blob.len(), redacted_blob.len());
    }

    fn valid_profile() -> FtpProfile {
        FtpProfile {
            profile_name: "TEST".into(),
            ssid_24ghz: "test-ssid".into(),
            ssid_5ghz: "test-ssid-5g".into(),
            host: "192.168.1.1".into(),
            port: 21,
            username: "user".into(),
            password: "pass".into(),
        }
    }

    // Minimal inline hex decoder so the reference blob above can be written
    // as a plain string literal without pulling in a crate dependency just
    // for one test fixture.
    mod hex {
        pub fn decode_hex(s: &str) -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        }
    }

    // ── decode_ftp_profile_fields (test-only, verification not production) ──
    //
    // The encoder's only correctness protection was 2 fixed golden captures
    // — nothing verified field combinations at OTHER lengths, where an
    // off-by-one in a length-prefix computation could silently corrupt the
    // blob without either golden test noticing. This decoder inverts just
    // the variable-length value fields (skipping the opaque structural
    // bytes, already covered by the golden-capture tests) using the same
    // tag constants the encoder does, so `decode(encode(profile)) ==
    // profile` can be checked across many synthesized lengths, not just the
    // two captured ones. It shares the encoder's understanding of the wire
    // format rather than being independently reverse-engineered — so it
    // catches internal-consistency bugs (a length prefix that doesn't match
    // what was actually written), not "the whole format model is wrong"
    // bugs, which only a real capture can catch.

    struct FieldReader<'a> {
        buf: &'a [u8],
        pos: usize,
    }

    impl<'a> FieldReader<'a> {
        fn new(buf: &'a [u8]) -> Self {
            FieldReader { buf, pos: 0 }
        }
        fn take(&mut self, n: usize) -> &'a [u8] {
            let s = &self.buf[self.pos..self.pos + n];
            self.pos += n;
            s
        }
        fn skip(&mut self, n: usize) {
            self.pos += n;
        }
        fn u8(&mut self) -> u8 {
            self.take(1)[0]
        }
        fn u16_le(&mut self) -> u16 {
            u16::from_le_bytes(self.take(2).try_into().unwrap())
        }
        fn u32_le(&mut self) -> u32 {
            u32::from_le_bytes(self.take(4).try_into().unwrap())
        }
        /// `[u32 len][ascii content]` — the SSID/username/password convention.
        fn ascii_field(&mut self) -> String {
            let len = self.u32_le() as usize;
            String::from_utf8(self.take(len).to_vec()).unwrap()
        }
        /// `[u8 len incl. null][utf16le content + null]` — profile_name/host.
        fn utf16_field_1byte_len(&mut self) -> String {
            let len_incl_null = self.u8() as usize;
            let content_units = len_incl_null - 1; // exclude the null terminator
            let bytes = self.take(content_units * 2);
            let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            self.skip(2); // null terminator
            String::from_utf16(&units).unwrap()
        }
    }

    /// Decode just the fields `FtpProfile` exposes, back out of an encoded
    /// blob — see the module comment above for what this does and doesn't
    /// verify.
    fn decode_ftp_profile_fields(blob: &[u8]) -> FtpProfile {
        let mut r = FieldReader::new(blob);
        r.skip(FTP_BLOB_HEADER.len());
        let profile_name = r.utf16_field_1byte_len();
        r.skip(FTP_BLOB_MYSTERY_BLOCK.len());
        r.skip(FTP_TAG_SSID_24GHZ.len());
        let ssid_24ghz = r.ascii_field();
        r.skip(FTP_TAG_SSID_5GHZ.len());
        let ssid_5ghz = r.ascii_field();
        r.skip(1); // FTP_TAG_HOST
        let host = r.utf16_field_1byte_len();
        r.skip(FTP_TAG_USERNAME.len());
        let username = r.ascii_field();
        let password = r.ascii_field(); // no tag — immediately follows username
        r.skip(FTP_TAG_PORT_SECTION.len());
        let port = r.u16_le();
        // Trailing IPv6/pad/trailer bytes deliberately not decoded — none of
        // them are FtpProfile fields, and the golden-capture tests already
        // cover their fixed content.
        FtpProfile { profile_name, ssid_24ghz, ssid_5ghz, host, port, username, password }
    }

    #[test]
    fn ftp_profile_round_trips_through_encode_decode_at_golden_lengths() {
        let profile = valid_profile();
        let blob = encode_ftp_profile(&profile).unwrap();
        assert_eq!(decode_ftp_profile_fields(&blob), profile.clone());
    }

    #[test]
    fn ftp_profile_round_trips_at_varied_field_lengths() {
        // Exactly what the encoder's golden-capture tests DON'T cover: field
        // lengths other than the two real captures happened to use. A bug in
        // a length-prefix computation would corrupt the blob at some lengths
        // but not others — this sweeps several to catch that class of bug.
        let cases: &[(&str, &str, &str, &str, u16, &str, &str)] = &[
            ("A", "s", "s", "1.2.3.4", 1, "u", "p"),
            ("", "", "", "", 0, "", ""),
            ("Profile Name With Spaces", "a-longer-ssid-name", "another-5ghz-ssid", "ftp.example.com", 65535, "a-longer-username", "a-longer-password-value"),
            ("X".repeat(50).leak(), "y".repeat(60).leak(), "z".repeat(30).leak(), "192.168.100.200", 21, "u".repeat(40).leak(), "p".repeat(55).leak()),
        ];
        for (profile_name, ssid_24ghz, ssid_5ghz, host, port, username, password) in cases {
            let profile = FtpProfile {
                profile_name: profile_name.to_string(),
                ssid_24ghz: ssid_24ghz.to_string(),
                ssid_5ghz: ssid_5ghz.to_string(),
                host: host.to_string(),
                port: *port,
                username: username.to_string(),
                password: password.to_string(),
            };
            let blob = encode_ftp_profile(&profile).unwrap();
            assert_eq!(decode_ftp_profile_fields(&blob), profile, "round-trip mismatch for {profile:?}");
        }
    }

    #[test]
    fn ftp_profile_round_trips_at_every_length_from_zero_to_max() {
        // Sweep every length for the ASCII max-64 fields specifically —
        // the boundary this file's own ascii_bytes()'s `>` vs `>=` mutant
        // (test review finding) makes worth covering exhaustively, not just
        // spot-checked.
        for len in 0..=FTP_ASCII_FIELD_MAX {
            let profile = FtpProfile {
                profile_name: "N".to_string(),
                ssid_24ghz: "a".repeat(len),
                ssid_5ghz: "test".to_string(),
                host: "1.1.1.1".to_string(),
                port: 21,
                username: "u".to_string(),
                password: "p".to_string(),
            };
            let blob = encode_ftp_profile(&profile).unwrap();
            assert_eq!(decode_ftp_profile_fields(&blob).ssid_24ghz, profile.ssid_24ghz, "mismatch at len={len}");
        }
    }

    // ── parse_propcode ───────────────────────────────────────────────────

    #[test]
    fn parse_propcode_lowercase_hex() {
        assert_eq!(parse_propcode("0xd053"), Ok(0xD053));
    }

    #[test]
    fn parse_propcode_uppercase_hex() {
        assert_eq!(parse_propcode("0XD053"), Ok(0xD053));
    }

    #[test]
    fn parse_propcode_decimal() {
        assert_eq!(parse_propcode("53331"), Ok(53331));
    }

    #[test]
    fn parse_propcode_trims_whitespace() {
        assert_eq!(parse_propcode("  0xD053  "), Ok(0xD053));
    }

    #[test]
    fn parse_propcode_invalid_hex_errors() {
        assert!(parse_propcode("0xZZZZ").is_err());
    }

    #[test]
    fn parse_propcode_invalid_decimal_errors() {
        assert!(parse_propcode("not-a-number").is_err());
    }

    #[test]
    fn parse_propcode_out_of_range_errors() {
        assert!(parse_propcode("0x10000").is_err());
    }

    // Regression tests for two confirmed cross-language divergences from
    // gui/fleet_lib.py's independent Python re-implementation (which relied
    // on Python's int(), tolerant of both forms below) — pinned here on the
    // Rust side too so a future edit can't silently reopen either gap.

    #[test]
    fn parse_propcode_rejects_internal_whitespace_after_hex_prefix() {
        assert!(parse_propcode("0x 10").is_err());
    }

    #[test]
    fn parse_propcode_rejects_underscore_digit_separators() {
        assert!(parse_propcode("0xD0_53").is_err());
        assert!(parse_propcode("53_331").is_err());
    }

    #[test]
    fn parse_propcode_accepts_leading_plus() {
        assert_eq!(parse_propcode("+53331"), Ok(53331));
        assert_eq!(parse_propcode("0x+D053"), Ok(0xD053));
    }

    // ── classify_open_session_response ──────────────────────────────────

    #[test]
    fn open_session_ok_classified_as_ok() {
        assert_eq!(classify_open_session_response(RESP_OK), OpenSessionResponse::Ok);
    }

    #[test]
    fn open_session_already_open_classified_as_retry_once() {
        assert_eq!(classify_open_session_response(RESP_SESSION_ALREADY_OPEN), OpenSessionResponse::RetryOnce);
    }

    #[test]
    fn open_session_other_code_classified_as_failed() {
        assert_eq!(classify_open_session_response(0x2002 /* GeneralError */), OpenSessionResponse::Failed);
    }

    // ── VENDOR_OPS ───────────────────────────────────────────────────────

    #[test]
    fn every_wired_up_op_const_has_a_vendor_ops_entry() {
        // Catches the table drifting out of sync with the actual OP_*
        // consts as this module grows (its own stated expectation — more
        // ops are planned per the module doc's open items).
        for code in [OP_GET_DEVICE_INFO, OP_OPEN_SESSION, OP_CLOSE_SESSION, OP_VENDOR_PROP_READ, OP_FTP_SETUP, OP_FTP_PROFILE_WRITE] {
            let info = vendor_op_info(code).unwrap_or_else(|| panic!("0x{code:04x} has no VENDOR_OPS entry"));
            assert!(info.wired_up, "0x{code:04x} is a real OP_* const but VENDOR_OPS says wired_up=false");
        }
    }

    #[test]
    fn vendor_op_info_unknown_code_returns_none() {
        assert!(vendor_op_info(0xFFFF).is_none());
    }

    // ── hex_bytes ─────────────────────────────────────────────────────────

    #[test]
    fn hex_bytes_formats_lowercase_space_separated() {
        assert_eq!(hex_bytes(&[0xDE, 0xAD, 0xBE, 0xEF]), "de ad be ef");
    }

    #[test]
    fn hex_bytes_empty_input() {
        assert_eq!(hex_bytes(&[]), "");
    }

    // ── model gate ───────────────────────────────────────────────────────

    #[test]
    fn model_confirmed_recognizes_confirmed_models() {
        assert!(model_confirmed("Z 9"));
        assert!(model_confirmed("Z6_3"));
    }

    #[test]
    fn model_confirmed_rejects_unconfirmed_models() {
        assert!(!model_confirmed("Z 5"));
        assert!(!model_confirmed(""));
        // Not a fuzzy/prefix match — trailing whitespace is a different string.
        assert!(!model_confirmed("Z6_3 "));
    }

    // ── PtpResponseCode ──────────────────────────────────────────────────

    #[test]
    fn response_code_known_names() {
        assert_eq!(PtpResponseCode(0x2001).to_string(), "OK (0x2001)");
        assert_eq!(PtpResponseCode(0x200A).to_string(), "DevicePropNotSupported (0x200a)");
        assert_eq!(PtpResponseCode(0x201E).to_string(), "SessionAlreadyOpen (0x201e)");
    }

    #[test]
    fn response_code_unknown_falls_back_to_hex() {
        assert_eq!(PtpResponseCode(0x1234).to_string(), "0x1234");
    }

    // ── select_candidate (find_device's actual selection logic) ────────────
    // (candidate_devices() itself needs real USB hardware; select_candidate
    // is the pure, hardware-independent part find_device delegates to, so
    // these tests call the SAME function find_device runs — not a
    // re-implementation of it that could silently drift.)

    #[test]
    fn select_candidate_by_serial_match() {
        let candidates = vec![("AAA".to_string(), 1), ("BBB".to_string(), 2)];
        assert!(matches!(select_candidate(candidates, Some("BBB")), Ok(2)));
    }

    #[test]
    fn select_candidate_by_serial_no_match_errors() {
        let candidates = vec![("AAA".to_string(), 1)];
        assert!(matches!(select_candidate(candidates, Some("ZZZ")), Err(PtpUsbError::SerialNotFound(_))));
    }

    #[test]
    fn select_candidate_no_serial_single_candidate() {
        let candidates = vec![("AAA".to_string(), 1)];
        assert!(matches!(select_candidate(candidates, None), Ok(1)));
    }

    #[test]
    fn select_candidate_no_serial_multiple_candidates_is_ambiguous() {
        let candidates = vec![("AAA".to_string(), 1), ("BBB".to_string(), 2)];
        assert!(matches!(select_candidate(candidates, None), Err(PtpUsbError::AmbiguousDevice(2))));
    }

    #[test]
    fn select_candidate_no_candidates_errors() {
        let candidates: Vec<(String, u32)> = vec![];
        assert!(matches!(select_candidate(candidates, None), Err(PtpUsbError::NoDevice)));
    }

    // ── fallback_serial (USB-topology fallback for a serial-less device) ──
    // (candidate_serial's "prefer non-empty real_serial" branch needs no
    // test of its own — it's a plain non-empty check with no rusb::Device
    // access, and select_candidate's tests above already exercise serial
    // matching against arbitrary string identifiers. This is the one part
    // of candidate_serial that's real logic and doesn't need a live
    // rusb::Device, which nothing in this crate can construct outside
    // hardware — see candidate_devices()'s own doc for that constraint.)

    #[test]
    fn fallback_serial_format() {
        assert_eq!(fallback_serial(20, 5), "usb-20:5");
    }

    #[test]
    fn fallback_serial_distinguishes_different_addresses_on_same_bus() {
        // The whole point of this fallback: two serial-less cameras on the
        // same bus must not collide on one shared identifier.
        assert_ne!(fallback_serial(20, 3), fallback_serial(20, 4));
    }
}

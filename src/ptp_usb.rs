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
//! its data-blob layout was mapped by diffing three real captured
//! instances, and the encoder is verified byte-for-byte against two of them
//! in tests; the third, a 331-byte variant with an extra optional field, is
//! not modelled). `0x9413` (IPTC profile write) is not — its data-blob
//! preamble couldn't be decoded from existing captures (only two instances
//! exist anywhere and they're byte-identical), so there's no way to construct
//! arbitrary IPTC content safely yet. See the doc's open items.
//!
//! ## macOS's own PTP claim
//!
//! macOS's Image Capture subsystem (`ptpcamerad`, `icdd`) claims a camera's
//! PTP interface as soon as it's touched, and SIP blocks disabling those
//! LaunchAgents permanently — so claiming the interface here means racing
//! them: kill, then claim immediately, retrying if they win the race. That
//! churn can disturb another nikon-fleet session talking to the same body
//! (a reset corrupts a capture), so [`PtpUsbSession::open`] takes the
//! cross-process [`CameraLock`] before it touches the bus and keeps it for
//! the life of the session.
//!
//! ## When a transaction fails
//!
//! A failed transaction usually leaves the camera's reply (or a stalled
//! bulk pipe) in an unknown state. The session is then *poisoned*: the bulk
//! endpoints get a `clear_halt`, buffered bytes are dropped, every further
//! operation fails with [`PtpUsbError::SessionPoisoned`], and teardown skips
//! the CloseSession it would otherwise push into a possibly stalled pipe.
//! Open a new session to continue.

use std::time::{Duration, Instant};

use rusb::{Direction, GlobalContext, TransferType};
use thiserror::Error;

use crate::camera_lock::{CameraLock, LockError};
use crate::ops::cameras::{KNOWN_MODELS, sdk_model_name};
use crate::sdk::{model_from_product_string, nikon_usb_devices};

// USB Still Image class (PIMA 15740): class 0x06, subclass 0x01, protocol 0x01.
const STILL_IMAGE_CLASS: u8 = 0x06;
const STILL_IMAGE_SUBCLASS: u8 = 0x01;
const STILL_IMAGE_PROTOCOL: u8 = 0x01;

const CONTAINER_COMMAND: u16 = 1;
const CONTAINER_DATA: u16 = 2;
const CONTAINER_RESPONSE: u16 = 3;
const CONTAINER_EVENT: u16 = 4;

/// Bytes in a container header: length, kind, code, transaction id.
const HEADER_LEN: usize = 12;
/// PTP commands carry at most five 32-bit parameters.
const MAX_COMMAND_PARAMS: usize = 5;

/// `0x1001` — GetDeviceInfo (ISO 15740): no params, the DeviceInfo dataset
/// as the data phase. Standard and read-only; it needs no open session, but
/// this module sends it inside one like every other op.
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
/// instance. Whether the first is a profile slot is **unconfirmed**: no
/// capture or USB test ever varied it, so every write here lands wherever
/// `[0, 0]` points and this module does not offer a way to choose another
/// slot. See `docs/nx-field-session-2026-07-09.md`'s field table for the
/// data blob.
const OP_FTP_PROFILE_WRITE: u16 = 0x90EE;
const FTP_PROFILE_WRITE_PARAMS: [u32; 2] = [0, 0];

/// `0x90EB` — FTP profile info. No params, no data-out; the data-in phase is
/// 57–59 bytes (see [`FtpProfileInfo`]). Always the second step of NX Field's
/// `0x90E8` → `0x90EB` → `0x90ED` sequence, and the only place the slot for
/// `0x90ED` comes from.
const OP_FTP_PROFILE_INFO: u16 = 0x90EB;

/// `0x90ED` — FTP profile **read**. One param, the profile slot taken from
/// the `0x90EB` reply; the data-in phase is the profile blob in the same
/// layout `0x90EE` writes. Confirmed offline from the 2026-07-09 Z 9 captures
/// (a 325-byte read, then a byte-identical 325-byte write; 317-byte reads
/// byte-identical to the 317-byte write before them); **not yet run over USB
/// on any camera**. A bare `0x90ED` with no param stalls the pipe.
const OP_FTP_PROFILE_READ: u16 = 0x90ED;

/// `0x944C` — NX Field sends `0x944C(0x90ED, 0)` before opening the FTP screen
/// and gets 32 bytes back, mostly zeros starting `01 01 01` (handoff doc, the
/// offline walk of the 2026-07-09 Z 9 captures; the exact bytes are in the
/// pcaps, not in this repo). **That it asks "does this camera implement
/// operation X" is an inference**, not a finding: the first parameter looks
/// like an operation code and the reply like a table of flags, nothing more.
/// The Z 9 lists `0x944C` in its DeviceInfo; the Z 5 does not.
/// This module sends it only through [`PtpUsbSession::query_vendor_op`], with
/// the second parameter fixed at its captured value, so it cannot be used to
/// fire another operation.
const OP_VENDOR_OP_QUERY: u16 = 0x944C;
const VENDOR_OP_QUERY_PARAM2: u32 = 0;

/// The slot `0x90EB` reports when the camera has no *active* FTP profile.
/// **Not** "no profile stored": on 2026-11-05 a Z9 that the owner says holds
/// FTP profiles reported this too, so 0xff is probably "none selected". NX Field
/// still sent `0x90ED(0xff)` in that state and got `0x201F`
/// (TransactionCancelled) with no data, five times in the captures; this
/// module does not send it, since the `0x90EB` reply already says so.
const FTP_NO_ACTIVE_SLOT: u32 = 0xff;

const RESP_OK: u16 = 0x2001;
const RESP_SESSION_ALREADY_OPEN: u16 = 0x201E;

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

/// Camera models the `0x90EE` FTP-profile write has been confirmed against
/// (see the module doc for the reverse-engineering history). Model strings
/// match `firmware::model_from_usb_product`'s output exactly (e.g. `"Z 9"`,
/// `"Z6_3"` for Z6III — space vs. underscore is not a typo, it's this
/// codebase's existing inconsistent USB product-string convention).
///
/// This is a deliberate *subset* of the fleet's surveyed models
/// ([`KNOWN_MODELS`], the one registry): the Z 5 is surveyed and its vendor
/// reads work, but no FTP write was ever captured on it. A test fails if an
/// entry here is not a surveyed model. There is no firmware dimension: the
/// captures were firmware-specific (Z6_3 fw 2.00, Z 9 fw 5.31 per
/// [`TxidCheck`]'s notes) but which firmware each *write* was confirmed on
/// was never recorded, so this gate cannot say more than "this model".
/// Sending an unconfirmed model a vendor write risks writing a malformed
/// blob to hardware whose exact protocol implementation was never checked.
pub const FTP_WRITE_CONFIRMED_MODELS: &[&str] = &["Z 9", "Z6_3"];

fn ftp_write_confirmed(model: &str) -> bool {
    FTP_WRITE_CONFIRMED_MODELS.contains(&model)
}

const CLAIM_RETRY_ATTEMPTS: u32 = 40;
const CLAIM_RETRY_DELAY: Duration = Duration::from_millis(50);
const BULK_TIMEOUT: Duration = Duration::from_secs(5);
/// libusb treats a zero timeout as "wait forever" (and `rusb` truncates to
/// whole milliseconds), so no bulk read is ever given less than this.
const MIN_BULK_TIMEOUT: Duration = Duration::from_millis(1);
const SESSION_ID: u32 = 1;

/// Bytes requested per bulk-IN read.
const READ_CHUNK: usize = 16384;
/// How many consecutive zero-length reads (zero-length packets) are skipped
/// between containers before the stream is called broken.
const MAX_EMPTY_READS: u32 = 8;

/// Aggregate ceiling on one `transact()` call, enforced by every read inside
/// it (see `fill_rx`): each individual `read_bulk` is capped at
/// `BULK_TIMEOUT`, but a device that keeps trickling bytes — or a stream of
/// unrecognised containers — would otherwise never finish. Generous relative
/// to real transactions (a handful of `BULK_TIMEOUT`-capped reads at most)
/// so it only ever fires on a genuinely stuck transaction.
const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling for the CloseSession sent on teardown, so dropping a session never
/// blocks as long as a full transaction could.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Ceiling on one `transact()` call's total accumulated Data-phase payload,
/// checked against each container's *declared* length before any of it is
/// buffered. Nothing in the PTP container format itself bounds how many
/// Data containers a misbehaving/malicious device could send before ever
/// sending a Response, or how long one container claims to be. Generous
/// relative to anything this module's own vendor ops actually transfer (the
/// FTP profile blob is a few hundred bytes; even an unusually large
/// vendor-property read is expected to be well under this).
const MAX_TRANSACTION_DATA_BYTES: usize = 16 * 1024 * 1024;

/// The transaction id that follows `id`. PTP reserves `0xFFFFFFFF` and
/// session-less id `0`, so the counter wraps from `0xFFFFFFFE` to `1`.
fn next_transaction_id(id: u32) -> u32 {
    if id >= 0xFFFF_FFFE { 1 } else { id + 1 }
}

/// Time left before `deadline`, or `None` once it has been reached.
fn remaining_until(deadline: Instant, now: Instant) -> Option<Duration> {
    deadline.checked_duration_since(now).filter(|d| !d.is_zero())
}

/// The timeout to hand `read_bulk` when `remaining` of the transaction is left.
fn bulk_read_timeout(remaining: Duration) -> Duration {
    remaining.clamp(MIN_BULK_TIMEOUT, BULK_TIMEOUT)
}

/// Whether a container of `len` bytes needs a zero-length packet after it:
/// PIMA 15740's bulk transport ends a transfer with a short packet, so one
/// whose length is an exact multiple of the endpoint's max packet size is
/// followed by an empty one. An unknown packet size (0) is a divisor of no
/// container, so no packet is guessed. (Not yet exercised on hardware: the
/// FTP blob's length varies with its fields and 512 and 1024 are reachable.)
fn needs_zero_length_packet(len: usize, max_packet: usize) -> bool {
    len.is_multiple_of(max_packet)
}

// ─────────────────────────────────────────────────────────────────────────
// Transport seam
// ─────────────────────────────────────────────────────────────────────────

/// The USB operations [`PtpUsbSession`] performs on its device handle.
///
/// This exists so the session's *sequencing* — OpenSession's
/// already-open retry, transaction-ID bookkeeping, data/response/event
/// container handling, the setup-then-write order of the FTP op, teardown
/// — can be exercised in tests against a scripted byte-level transport
/// instead of only against live hardware (finding
/// nikon-fleet-20260909-7b2e#18). The only real implementation is
/// `rusb::DeviceHandle`; it is `pub` solely so `PtpUsbSession<T>` is
/// nameable, not as a supported extension point.
pub trait BulkTransport {
    fn write_bulk(&self, endpoint: u8, data: &[u8], timeout: Duration) -> rusb::Result<usize>;
    fn read_bulk(&self, endpoint: u8, buf: &mut [u8], timeout: Duration) -> rusb::Result<usize>;
    fn clear_halt(&self, endpoint: u8) -> rusb::Result<()>;
    fn release_interface(&self, interface: u8) -> rusb::Result<()>;
}

impl BulkTransport for rusb::DeviceHandle<GlobalContext> {
    fn write_bulk(&self, endpoint: u8, data: &[u8], timeout: Duration) -> rusb::Result<usize> {
        rusb::DeviceHandle::write_bulk(self, endpoint, data, timeout)
    }
    fn read_bulk(&self, endpoint: u8, buf: &mut [u8], timeout: Duration) -> rusb::Result<usize> {
        rusb::DeviceHandle::read_bulk(self, endpoint, buf, timeout)
    }
    fn clear_halt(&self, endpoint: u8) -> rusb::Result<()> {
        rusb::DeviceHandle::clear_halt(self, endpoint)
    }
    fn release_interface(&self, interface: u8) -> rusb::Result<()> {
        rusb::DeviceHandle::release_interface(self, interface)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PtpUsbError {
    #[error("no Nikon USB camera found")]
    NoDevice,
    #[error("multiple Nikon USB cameras found ({0}); pass a serial to disambiguate")]
    AmbiguousDevice(usize),
    #[error("{}", serial_not_found_text(.requested, .unreadable))]
    SerialNotFound {
        requested: String,
        /// Attached bodies whose USB serial could not be read, each as `<fallback serial> (<what failed>)`.
        /// Their real serial is unknown, so the one requested may be among them; empty when every
        /// attached body was identified.
        unreadable: Vec<String>,
    },
    #[error("{count} Nikon USB cameras report serial {requested:?}; refusing to pick one")]
    DuplicateSerial { requested: String, count: usize },
    #[error(
        "{} Nikon USB device(s) could not be opened to read their serial ({}); a body that cannot be \
         identified may be the one you mean, so none is picked. Name the body with --serial, or release \
         it (macOS's ptpcamerad/icdd, NX Studio, another nikon-fleet session) and retry",
        .skipped.len(),
        .skipped.join("; ")
    )]
    BodiesSkipped { skipped: Vec<String> },
    #[error("USB error: {0}")]
    Usb(#[from] rusb::Error),
    #[error("{0}")]
    CameraLock(#[from] LockError),
    #[error("could not find a Still Image (PTP) class interface on this device")]
    NoPtpInterface,
    #[error(
        "could not claim the PTP USB interface after {attempts} attempts (macOS's ptpcamerad/icdd may be \
         holding it); last error: {last_error}"
    )]
    ClaimFailed { attempts: u32, last_error: String },
    #[error("PTP container too short ({0} bytes)")]
    ShortContainer(usize),
    #[error("PTP container declared {declared} bytes but only {got} were received")]
    TruncatedContainer { declared: usize, got: usize },
    #[error("USB wrote {written} of {expected} bytes of a PTP container")]
    ShortWrite { written: usize, expected: usize },
    #[error("a PTP container with a {payload_len}-byte payload does not fit the 32-bit length field")]
    ContainerTooLarge { payload_len: usize },
    #[error("a PTP command takes at most {max} parameters, got {given}")]
    TooManyCommandParams { given: usize, max: usize },
    #[error("PTP response params ({0} bytes) are not a whole number of 4-byte words")]
    MisalignedResponseParams(usize),
    #[error("PTP transaction did not complete within {0:?}")]
    TransactionTimedOut(Duration),
    #[error("PTP transaction's accumulated data ({got} bytes) exceeds the {limit}-byte limit")]
    TransactionDataTooLarge { got: usize, limit: usize },
    #[error("PTP session is unusable after an earlier failure ({cause}); open a new session")]
    SessionPoisoned { cause: String },
    #[error("OpenSession failed: {0}")]
    OpenSessionFailed(PtpResponseCode),
    #[error("PTP operation 0x{op:04x} failed: {code}")]
    OperationFailed { op: u16, code: PtpResponseCode },
    #[error(
        "PTP operation 0x{op:04x}: camera answered with transaction id {got}, expected {expected} \
         (a stale or out-of-order reply)"
    )]
    TransactionIdMismatch { op: u16, expected: u32, got: u32 },
    #[error("PTP operation 0x{op:04x}: camera sent a data container for operation 0x{got:04x}")]
    UnexpectedDataOp { op: u16, got: u16 },
    #[error("PTP operation 0x{op:04x} answered OK but sent no data container at all")]
    NoDataPhase { op: u16 },
    #[error("PTP DeviceInfo dataset is malformed at its {field} (byte offset {offset}): {fault}")]
    DeviceInfoMalformed { field: &'static str, offset: usize, fault: DeviceInfoFault },
    #[error("FTP profile data is not in a layout this decoder knows: {what} at byte offset {offset}: {detail}")]
    FtpBlobMalformed { what: &'static str, offset: usize, detail: String },
    #[error(
        "FTP profile slot {slot} is outside the {count} stored profile(s) (slots 0..{count}); a read past the end \
         stalls the USB pipe, so it was not sent"
    )]
    FtpSlotOutOfRange { slot: u32, count: u32 },
    #[error("FTP profile field {field} is {len} {unit}, exceeds the {max}-{unit} limit")]
    FtpFieldTooLong { field: &'static str, len: usize, max: usize, unit: &'static str },
    #[error("FTP profile field {field} contains non-ASCII characters, which this encoding does not support")]
    FtpFieldNotAscii { field: &'static str },
    #[error("FTP profile field {field} contains a control character (NUL, newline, tab, ...)")]
    FtpFieldControlChar { field: &'static str },
    #[error(
        "FTP profile write may or may not have taken effect: the 0x90EE data had already been sent when \
         this failed ({cause}). Check it in the camera's menu (or with `vendor-read-ftp`, not yet tried on a \
         camera); repeating the write is idempotent"
    )]
    FtpWriteIndeterminate { cause: Box<PtpUsbError> },
    #[error("camera model {0:?} is not in this module's confirmed-safe list for vendor writes; wire format may not match")]
    UnconfirmedModel(String),
}

impl PtpUsbError {
    /// Whether this failure, raised inside a transaction, leaves the bulk
    /// pipes in a known state. Only errors raised before any byte was sent
    /// (an unencodable command) or after the Response container was fully
    /// read (its params malformed) do; anything else may have left an unread
    /// reply or a stalled pipe behind, and poisons the session.
    fn leaves_stream_in_sync(&self) -> bool {
        matches!(
            self,
            PtpUsbError::MisalignedResponseParams(_)
                | PtpUsbError::ContainerTooLarge { .. }
                | PtpUsbError::TooManyCommandParams { .. }
        )
    }
}

/// A PTP response code, with `Display` naming the standard ones (PIMA 15740
/// table of response codes; vendor codes print as hex).
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
            0x2008 => "InvalidStorageID",
            0x2009 => "InvalidObjectHandle",
            0x200A => "DevicePropNotSupported",
            0x200B => "InvalidObjectFormatCode",
            0x200C => "StoreFull",
            0x200D => "ObjectWriteProtected",
            0x200E => "StoreReadOnly",
            0x200F => "AccessDenied",
            0x2010 => "NoThumbnailPresent",
            0x2011 => "SelfTestFailed",
            0x2012 => "PartialDeletion",
            0x2013 => "StoreNotAvailable",
            0x2014 => "SpecificationByFormatUnsupported",
            0x2015 => "NoValidObjectInfo",
            0x2016 => "InvalidCodeFormat",
            0x2017 => "UnknownVendorCode",
            0x2018 => "CaptureAlreadyTerminated",
            0x2019 => "DeviceBusy",
            0x201A => "InvalidParentObject",
            0x201B => "InvalidDevicePropFormat",
            0x201C => "InvalidDevicePropValue",
            0x201D => "InvalidParameter",
            0x201E => "SessionAlreadyOpen",
            0x201F => "TransactionCancelled",
            0x2020 => "SpecificationOfDestinationUnsupported",
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
        // effort and deliberately unchecked: pkill exits non-zero when
        // nothing matches, the common case after the first kill, so the
        // status carries no usable success/failure signal. Fixed argv, no
        // shell. (Behaviour unchanged by the 2026-10-28 review: how often it
        // spawns, and whether SIGKILL is needed, are hardware questions.)
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

/// The [`PtpUsbError::SerialNotFound`] text: that no attached body has the serial, and, when some
/// bodies could not be read, that the serial may be on one of them (their descriptor read failed,
/// so each stands under its `usb-<bus>:<addr>` fallback and a real serial cannot match it).
fn serial_not_found_text(requested: &str, unreadable: &[String]) -> String {
    let base = format!("no Nikon USB camera with serial {requested:?}");
    if unreadable.is_empty() {
        return base;
    }
    format!(
        "{base}; {} attached body(ies) could not be read: {}; the serial may be on one of them",
        unreadable.len(),
        unreadable.join("; ")
    )
}

/// A discovered Nikon USB device, already opened (identifying it needs an
/// open handle to read its string descriptors) — carried forward so
/// `PtpUsbSession::open` can reuse this same handle instead of opening the
/// device a second time once one candidate is selected.
struct Candidate {
    device: rusb::Device<GlobalContext>,
    handle: rusb::DeviceHandle<GlobalContext>,
    serial: String,
    /// The `iSerialNumber` descriptor as read; empty when the body has none or the read failed
    /// (`warnings` says which), unlike `serial`, which then holds the topology fallback.
    usb_serial: String,
    model: String,
    /// Descriptor reads that failed while identifying this body (its serial
    /// or model then fell back to the topology serial / `""`).
    warnings: Vec<String>,
    /// Whether `serial` is the `usb-<bus>:<addr>` fallback because the descriptor gave no serial.
    serial_from_topology: bool,
}

impl Candidate {
    fn unreadable(&self) -> Option<String> {
        describe_unreadable(&self.serial, self.serial_from_topology, &self.warnings)
    }
}

/// A body whose serial stands in for one that could not be read: the descriptor read failed
/// (`warnings`) and the serial is the topology fallback. A refusal to match a requested serial
/// must say such a body is there, since its real serial may be the one asked for. A body whose
/// reads all worked, or one that simply has no serial string, is not unreadable; nor is one that
/// only failed to read its product string (its serial is the real one).
fn describe_unreadable(serial: &str, serial_from_topology: bool, warnings: &[String]) -> Option<String> {
    (serial_from_topology && !warnings.is_empty()).then(|| format!("{serial} ({})", warnings.join(", ")))
}

/// Everything one pass over the bus found. `skipped` describes each Nikon
/// USB device that was seen but couldn't be opened (e.g. macOS's
/// `ptpcamerad` still holding the interface) and so couldn't be identified:
/// a body that is not in `candidates` is *not* a body that isn't there.
struct Enumeration {
    candidates: Vec<Candidate>,
    skipped: Vec<String>,
}

/// Read the serial and product strings with one language lookup (3 control
/// transfers instead of 4), reporting what failed instead of collapsing it
/// into an empty string. An index of 0 means the device has no such string,
/// which is not a failure.
fn read_identity(handle: &rusb::DeviceHandle<GlobalContext>, desc: &rusb::DeviceDescriptor) -> (String, String, Vec<String>) {
    let timeout = Duration::from_secs(1);
    let serial_idx = desc.serial_number_string_index().unwrap_or(0);
    let product_idx = desc.product_string_index().unwrap_or(0);
    let mut warnings = Vec::new();
    if serial_idx == 0 && product_idx == 0 {
        return (String::new(), String::new(), warnings);
    }
    let lang = match handle.read_languages(timeout) {
        Ok(langs) => langs.into_iter().next(),
        Err(e) => {
            warnings.push(format!("reading the USB language list failed: {e}"));
            None
        }
    };
    let mut read = |idx: u8, what: &str| -> String {
        if idx == 0 {
            return String::new();
        }
        let Some(lang) = lang else { return String::new() };
        handle.read_string_descriptor(lang, idx, timeout).unwrap_or_else(|e| {
            warnings.push(format!("reading the USB {what} string failed: {e}"));
            String::new()
        })
    };
    let serial = read(serial_idx, "serial");
    let product = read(product_idx, "product");
    (serial, product, warnings)
}

fn candidate_devices() -> Enumeration {
    let mut candidates = Vec::new();
    let mut skipped = Vec::new();
    for device in nikon_usb_devices() {
        let place = format!("bus {} address {}", device.bus_number(), device.address());
        let desc = match device.device_descriptor() {
            Ok(d) => d,
            Err(e) => {
                skipped.push(format!("{place}: device descriptor: {e}"));
                continue;
            }
        };
        let handle = match device.open() {
            Ok(h) => h,
            Err(e) => {
                skipped.push(format!("{place}: open: {e}"));
                continue;
            }
        };
        let (real_serial, product, warnings) = read_identity(&handle, &desc);
        let serial = candidate_serial(&device, &real_serial);
        let model = model_from_product_string(&product);
        let serial_from_topology = real_serial.is_empty();
        candidates.push(Candidate { device, handle, serial, usb_serial: real_serial, model, warnings, serial_from_topology });
    }
    Enumeration { candidates, skipped }
}

/// Pick one item from a `(serial, item)` candidate list, by serial (exact
/// match) or — if none given — the sole candidate. `skipped` describes the
/// Nikon bodies that were seen but could not be identified.
///
/// Extracted as a pure, hardware-independent function so tests exercise this
/// exact selection logic directly instead of a hand-copied re-implementation
/// that could silently drift from what `find_device` actually runs.
///
/// - Without a serial, any skipped body is a refusal: the one that opened is
///   not "the only camera", it is the only one that could be *looked at*.
/// - With a serial, a match is used; no match while a body was skipped is
///   reported as skipped (the missing one may be the body you named), not as
///   "no such serial". No match while a body was seen but its serial could not
///   be read (`unreadable`) is "no such serial" that says those bodies are
///   there, with them carried on the error.
/// - Two candidates with the same serial are an error, never first-wins (the
///   same contract as `ops::target::select_by_usb_serial`).
fn select_candidate<T>(
    candidates: Vec<(String, T)>,
    skipped: &[String],
    unreadable: &[String],
    serial: Option<&str>,
) -> Result<T, PtpUsbError> {
    let refuse = || PtpUsbError::BodiesSkipped { skipped: skipped.to_vec() };
    match serial {
        Some(s) => {
            let mut hits: Vec<T> = candidates.into_iter().filter(|(ser, _)| ser == s).map(|(_, item)| item).collect();
            match hits.len() {
                0 if !skipped.is_empty() => Err(refuse()),
                0 => Err(PtpUsbError::SerialNotFound { requested: s.to_string(), unreadable: unreadable.to_vec() }),
                1 => Ok(hits.remove(0)),
                count => Err(PtpUsbError::DuplicateSerial { requested: s.to_string(), count }),
            }
        }
        None => {
            if !skipped.is_empty() {
                return Err(refuse());
            }
            match candidates.len() {
                0 => Err(PtpUsbError::NoDevice),
                1 => Ok(candidates.into_iter().next().unwrap().1),
                n => Err(PtpUsbError::AmbiguousDevice(n)),
            }
        }
    }
}

/// Resolves a device by serial (or the sole candidate), returning it
/// together with its model string so callers can gate on confirmed-safe
/// models before sending reverse-engineered wire formats. Carries forward
/// the handle `candidate_devices` already opened to identify it, so the
/// caller doesn't need to open the device a second time.
fn find_device(serial: Option<&str>) -> Result<Candidate, PtpUsbError> {
    let Enumeration { candidates, skipped } = candidate_devices();
    for c in &candidates {
        for w in &c.warnings {
            eprintln!("warning: Nikon USB device {:?}: {w}", c.serial);
        }
    }
    let unreadable: Vec<String> = candidates.iter().filter_map(Candidate::unreadable).collect();
    let items = candidates.into_iter().map(|c| (c.serial.clone(), c)).collect();
    select_candidate(items, &skipped, &unreadable, serial)
}

/// The PTP (Still Image class) interface number, its bulk IN/OUT endpoint
/// addresses, and the OUT endpoint's max packet size, found by walking the
/// active config's interface descriptors.
struct PtpEndpoints {
    interface: u8,
    ep_out: u8,
    ep_in: u8,
    ep_out_max_packet: usize,
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
            let mut ep_out_max_packet = 0usize;
            for ep in desc.endpoint_descriptors() {
                if ep.transfer_type() != TransferType::Bulk {
                    continue;
                }
                match ep.direction() {
                    Direction::Out if ep_out.is_none() => {
                        ep_out = Some(ep.address());
                        ep_out_max_packet = usize::from(ep.max_packet_size());
                    }
                    Direction::In if ep_in.is_none() => ep_in = Some(ep.address()),
                    _ => {}
                }
            }
            if let (Some(ep_out), Some(ep_in)) = (ep_out, ep_in) {
                return Ok(PtpEndpoints { interface: desc.interface_number(), ep_out, ep_in, ep_out_max_packet });
            }
        }
    }
    Err(PtpUsbError::NoPtpInterface)
}

/// The `last_error` text of a [`PtpUsbError::ClaimFailed`]: the claim error,
/// plus the last `set_active_configuration` error if there was one (which
/// the retry loop otherwise swallows).
fn claim_failure_detail(claim_error: Option<&str>, config_error: Option<&str>) -> String {
    let claim = claim_error.unwrap_or("no attempt recorded");
    match config_error {
        Some(cfg) => format!("{claim}; last set_active_configuration(1) error: {cfg}"),
        None => claim.to_string(),
    }
}

/// Claim the PTP interface, retrying against macOS's own PTP claimants.
///
/// Killing `ptpcamerad`/`icdd` once is not enough — they re-claim the
/// interface as soon as it's touched, and SIP blocks disabling their
/// LaunchAgents outright. So this kills them and attempts the claim in a
/// tight loop, which reliably wins the race in practice (confirmed
/// interactively on a Z6III 2026-09-06). The caller holds the
/// [`CameraLock`] for the duration.
fn claim_with_retry(
    handle: &rusb::DeviceHandle<GlobalContext>,
    interface: u8,
) -> Result<(), PtpUsbError> {
    let mut last_err = None;
    let mut last_config_err = None;
    for _ in 0..CLAIM_RETRY_ATTEMPTS {
        kill_macos_ptp_claimants();
        // Re-asserted every attempt, not just once up front: macOS's PTP
        // claimants reset config state when they re-grab the device, so a
        // one-time set_active_configuration before the loop isn't enough —
        // confirmed against the reference Python prototype, which also
        // re-issues this every iteration. Its failure is expected when the
        // device is already in configuration 1 (and the number is
        // hard-coded: every surveyed body has one configuration), so it does
        // not stop the loop; only the last one is kept, for the failure report.
        last_config_err = handle.set_active_configuration(1).err().map(|e| e.to_string());
        match handle.claim_interface(interface) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e.to_string());
                std::thread::sleep(CLAIM_RETRY_DELAY);
            }
        }
    }
    Err(PtpUsbError::ClaimFailed {
        attempts: CLAIM_RETRY_ATTEMPTS,
        last_error: claim_failure_detail(last_err.as_deref(), last_config_err.as_deref()),
    })
}

// ─────────────────────────────────────────────────────────────────────────
// Container framing
// ─────────────────────────────────────────────────────────────────────────

/// The value of a container's length field, or an error when `payload_len`
/// plus the header does not fit in 32 bits.
fn container_length(payload_len: usize) -> Result<u32, PtpUsbError> {
    payload_len
        .checked_add(HEADER_LEN)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(PtpUsbError::ContainerTooLarge { payload_len })
}

/// The 12-byte container header (length, kind, code, transaction id)
/// shared by every container this module encodes, followed by `payload`.
/// `encode_command`/`encode_data` differ only in which `kind` they use and
/// how their payload gets built (params-as-le-u32 vs. raw bytes) — this is
/// the one place the header-assembly bytes are written.
fn encode_container(kind: u16, code: u16, transaction_id: u32, payload: &[u8]) -> Result<Vec<u8>, PtpUsbError> {
    let length = container_length(payload.len())?;
    let mut buf = Vec::with_capacity(length as usize);
    buf.extend_from_slice(&length.to_le_bytes());
    buf.extend_from_slice(&kind.to_le_bytes());
    buf.extend_from_slice(&code.to_le_bytes());
    buf.extend_from_slice(&transaction_id.to_le_bytes());
    buf.extend_from_slice(payload);
    Ok(buf)
}

fn encode_command(code: u16, transaction_id: u32, params: &[u32]) -> Result<Vec<u8>, PtpUsbError> {
    if params.len() > MAX_COMMAND_PARAMS {
        return Err(PtpUsbError::TooManyCommandParams { given: params.len(), max: MAX_COMMAND_PARAMS });
    }
    let payload: Vec<u8> = params.iter().flat_map(|p| p.to_le_bytes()).collect();
    encode_container(CONTAINER_COMMAND, code, transaction_id, &payload)
}

fn encode_data(code: u16, transaction_id: u32, data: &[u8]) -> Result<Vec<u8>, PtpUsbError> {
    encode_container(CONTAINER_DATA, code, transaction_id, data)
}

/// Whether [`PtpUsbSession::transact`] requires the camera's data and
/// response containers to carry the transaction id (and, for data, the
/// operation code) of the command.
///
/// Enforcement was added after live captures on a Z6_3 (fw 2.00), Z 5
/// (fw 1.43) and Z 9 (fw 5.31) showed every reply echoing its command's id
/// for OpenSession, CloseSession and `0x943B` vendor reads (including the
/// already-open retry and the Z 5's OperationNotSupported reply).
///
/// The FTP write ops (`0x90E8` setup and `0x90EE` profile write) are
/// deliberately [`TxidCheck::Unchecked`]: no capture of them exists on any
/// body, and a write has already changed camera state by the time its reply
/// arrives, so a wrongly strict check there would report failure for a
/// write that took effect. A deviation is not discarded, though: it is
/// recorded on the [`Reply`] and reported in [`FtpWriteReceipt`]. Move them
/// to `Enforce` once their replies have been captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxidCheck {
    Enforce,
    Unchecked,
}

/// A parsed container header + whatever payload followed it (params for
/// Command/Response, raw bytes for Data).
#[derive(Debug)]
struct Container {
    kind: u16,
    code: u16,
    transaction_id: u32,
    payload: Vec<u8>,
}

/// Parse the container at the start of `raw`, returning it and how many
/// bytes it occupied. `raw` may legitimately be longer than the container:
/// a single bulk read can capture the next container too (a Response
/// coalesced after its Data), and those bytes belong to the *next* parse,
/// not to the garbage bin — the caller drops `consumed` bytes and keeps the
/// rest. `raw` being *shorter* than declared means the caller didn't
/// actually receive the full container, which is a hard error (silently
/// truncating would desync every subsequent read on this connection).
fn parse_container(raw: &[u8]) -> Result<(Container, usize), PtpUsbError> {
    if raw.len() < HEADER_LEN {
        return Err(PtpUsbError::ShortContainer(raw.len()));
    }
    let length = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as usize;
    // The declared length is wire-controlled data (a malformed/corrupt
    // response is a real input on this reverse-engineered, live-hardware
    // path, not theoretical) — floor it at the header size before using it
    // to index, or a declared length under 12 panics the slice below.
    if length < HEADER_LEN {
        return Err(PtpUsbError::ShortContainer(length));
    }
    if raw.len() < length {
        return Err(PtpUsbError::TruncatedContainer { declared: length, got: raw.len() });
    }
    let kind = u16::from_le_bytes(raw[4..6].try_into().unwrap());
    let code = u16::from_le_bytes(raw[6..8].try_into().unwrap());
    let transaction_id = u32::from_le_bytes(raw[8..12].try_into().unwrap());
    let payload = raw[HEADER_LEN..length].to_vec();
    Ok((Container { kind, code, transaction_id, payload }, length))
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
// bytes (a header, a per-field "tag", a trailing byte) have no known
// semantic meaning — they're copied verbatim from a real captured template
// rather than reconstructed, since the camera presumably cares about their
// exact value and we don't need to know why.
//
// None of the field limits below was ever probed on a camera: the SSID cap
// is the 802.11 limit (32 octets), the others are guesses chosen to sit
// comfortably under the length prefixes' ranges. Settling them needs a
// write at the limit (and one over it) with the result read off the
// camera's own menu.
// ─────────────────────────────────────────────────────────────────────────

const FTP_BLOB_HEADER: [u8; 2] = [0xf4, 0x01];

/// The 82 bytes between the profile-name field and the SSID section, which
/// every real capture carried byte-identically, decoded (the field map in
/// the doc calls it "unresolved"):
///
/// - two UTF-16 strings, `"00000000T000000"` each (a date-time shape,
///   all zeros), each `[u8 length 16][16 UTF-16LE units]`: 66 bytes;
/// - then 16 bytes: tag `02 03`, `03`, **`57 42 a8 c0`**, `18`, and eight
///   zero bytes. `57 42 a8 c0` read as a little-endian `u32` is
///   `c0.a8.42.57` = 192.168.66.87, the Z 9's own address on the capture rig
///   (the doc's header names it), and `0x18` is 24: an IPv4 address and a
///   /24 prefix length.
///
/// So the part that varied nowhere in three captures only because all three
/// came from one rig is the address of that rig's camera. [`FtpCameraNetwork`]
/// is that part as a parameter. What the camera does with it (does it set a
/// static address? is it ignored?) is **not known**; the default stays the
/// captured value because writing zeros or another address was never tried.
const FTP_TEMPLATE_TIMESTAMP_PLACEHOLDER: &str = "00000000T000000";
const FTP_TEMPLATE_NETWORK_TAG: [u8; 3] = [0x02, 0x03, 0x03];
const FTP_TEMPLATE_NETWORK_PAD: [u8; 8] = [0; 8];
/// Length in bytes of the template block [`encode_template_block`] writes.
#[cfg(test)]
const FTP_TEMPLATE_LEN: usize = 82;

/// The address-shaped part of the 82-byte template block (see
/// [`FTP_TEMPLATE_TIMESTAMP_PLACEHOLDER`]'s notes): an IPv4 address and prefix
/// length. [`FtpCameraNetwork::CAPTURED`] is what all three captures carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FtpCameraNetwork {
    /// Dotted-quad octets in reading order, e.g. `[192, 168, 66, 87]`. (On
    /// the wire they are reversed.)
    pub ipv4: [u8; 4],
    pub prefix_len: u8,
}

impl FtpCameraNetwork {
    /// 192.168.66.87/24 — the capture rig's Z 9. Written to every camera by
    /// [`encode_ftp_profile`] and [`PtpUsbSession::write_ftp_profile`] until
    /// someone tests a different value on hardware.
    pub const CAPTURED: FtpCameraNetwork = FtpCameraNetwork { ipv4: [192, 168, 66, 87], prefix_len: 24 };
}

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

const FTP_PROFILE_NAME_MAX_UNITS: usize = 253; // + null terminator = 254, the most a 1-byte length prefix allows here
const FTP_SSID_MAX_BYTES: usize = 32; // 802.11
const FTP_HOST_MAX_UNITS: usize = 199; // + null terminator = 200; a guess, the prefix would allow 254
const FTP_CREDENTIAL_MAX_BYTES: usize = 64; // generous; real usernames/passwords are far shorter

/// How a field's value is laid out on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpEncoding {
    /// `[u32 length in bytes, no null][ascii content]`.
    AsciiU32Len,
    /// `[u8 length in UTF-16 units incl. null][utf16le content + null]`.
    Utf16U8Len,
}

/// The unit a field's limit is expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpLenUnit {
    Bytes,
    Utf16Units,
}

impl FtpLenUnit {
    pub fn label(self) -> &'static str {
        match self {
            FtpLenUnit::Bytes => "bytes",
            FtpLenUnit::Utf16Units => "UTF-16 units",
        }
    }
}

/// The six text fields of an FTP profile, in wire order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpField {
    ProfileName,
    Ssid24Ghz,
    Ssid5Ghz,
    Host,
    Username,
    Password,
}

impl FtpField {
    pub const ALL: [FtpField; 6] = [
        FtpField::ProfileName,
        FtpField::Ssid24Ghz,
        FtpField::Ssid5Ghz,
        FtpField::Host,
        FtpField::Username,
        FtpField::Password,
    ];

    pub fn spec(self) -> &'static FtpFieldSpec {
        &FTP_PROFILE_FIELDS[self as usize]
    }
}

/// Everything the encoder, the required-field check and the GUIs need to
/// know about one FTP-profile text field — the single source for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FtpFieldSpec {
    pub field: FtpField,
    /// Machine name, used in errors (`FtpFieldTooLong { field, .. }`).
    pub key: &'static str,
    /// Name for a "missing field(s)" message.
    pub display_name: &'static str,
    pub encoding: FtpEncoding,
    /// Longest value accepted, in [`FtpFieldSpec::unit`]s of *content* (the
    /// null terminator of a UTF-16 field is not counted).
    pub max_len: usize,
    /// Whether surrounding whitespace is ignored when checking the field is
    /// non-empty. The password is checked as typed: a leading or trailing
    /// space could be a real password character.
    pub trimmed_for_required_check: bool,
}

impl FtpFieldSpec {
    pub fn unit(&self) -> FtpLenUnit {
        match self.encoding {
            FtpEncoding::AsciiU32Len => FtpLenUnit::Bytes,
            FtpEncoding::Utf16U8Len => FtpLenUnit::Utf16Units,
        }
    }
}

/// The FTP-profile text fields with their limits and display names, in the
/// order of [`FtpField::ALL`].
pub const FTP_PROFILE_FIELDS: [FtpFieldSpec; 6] = [
    FtpFieldSpec {
        field: FtpField::ProfileName,
        key: "profile_name",
        display_name: "profile name",
        encoding: FtpEncoding::Utf16U8Len,
        max_len: FTP_PROFILE_NAME_MAX_UNITS,
        trimmed_for_required_check: true,
    },
    FtpFieldSpec {
        field: FtpField::Ssid24Ghz,
        key: "ssid_24ghz",
        display_name: "SSID (2.4GHz)",
        encoding: FtpEncoding::AsciiU32Len,
        max_len: FTP_SSID_MAX_BYTES,
        trimmed_for_required_check: true,
    },
    FtpFieldSpec {
        field: FtpField::Ssid5Ghz,
        key: "ssid_5ghz",
        display_name: "SSID (5GHz)",
        encoding: FtpEncoding::AsciiU32Len,
        max_len: FTP_SSID_MAX_BYTES,
        trimmed_for_required_check: true,
    },
    FtpFieldSpec {
        field: FtpField::Host,
        key: "host",
        display_name: "host",
        encoding: FtpEncoding::Utf16U8Len,
        max_len: FTP_HOST_MAX_UNITS,
        trimmed_for_required_check: true,
    },
    FtpFieldSpec {
        field: FtpField::Username,
        key: "username",
        display_name: "username",
        encoding: FtpEncoding::AsciiU32Len,
        max_len: FTP_CREDENTIAL_MAX_BYTES,
        trimmed_for_required_check: true,
    },
    FtpFieldSpec {
        field: FtpField::Password,
        key: "password",
        display_name: "password",
        encoding: FtpEncoding::AsciiU32Len,
        max_len: FTP_CREDENTIAL_MAX_BYTES,
        trimmed_for_required_check: false,
    },
];

/// Fields for an `0x90EE` FTP profile write. All required; see the module
/// docs for which structural bytes are opaque-but-fixed instead, and
/// [`FTP_PROFILE_FIELDS`] for each field's limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpProfile {
    /// Human-readable profile name shown in camera menus, e.g. `"CLAUDE"`.
    pub profile_name: String,
    pub ssid_24ghz: String,
    pub ssid_5ghz: String,
    /// Hostname or IP address of the FTP server.
    pub host: String,
    /// **Probably not the FTP server port.** This is written to the u16
    /// after the password, which holds 8080 in every blob read from a real
    /// camera (the usual default proxy port); the FTP port (21 or 22) is the
    /// u16 before the username, which the encoder hard-codes as 21. See
    /// [`DecodedFtpProfile::ftp_port`] and the handoff doc, 2026-11-05.
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl FtpProfile {
    /// A copy of this profile with the password replaced by asterisks — for
    /// previewing the encoded blob (dry-run output) without the plaintext
    /// password becoming readable in the hex dump, which would defeat the
    /// password-masking on any UI field the value came from.
    ///
    /// The copy encodes exactly when the real profile does: every ordinary
    /// ASCII character becomes one `*` (so the previewed blob's size and byte
    /// layout match what an actual write would send, and a password at the
    /// byte limit stays at it), while a character the encoder rejects
    /// becomes a fixed stand-in that the encoder rejects with the same
    /// error (`é` for any non-ASCII character, U+0001 for an ASCII control
    /// character). A preview that passes where the real write fails is the
    /// bug this guards against; nothing of a rejected character survives
    /// but its class.
    pub fn with_password_redacted(&self) -> Self {
        let password = self
            .password
            .chars()
            .map(|c| match c {
                c if !c.is_ascii() => '\u{e9}',
                c if c.is_control() => '\u{1}',
                _ => '*',
            })
            .collect();
        FtpProfile { password, ..self.clone() }
    }
}

/// Check `value` against `spec`: ASCII-only where the wire format is ASCII,
/// no control characters, and within the limit (in the spec's own unit).
fn validate_field(spec: &FtpFieldSpec, value: &str) -> Result<(), PtpUsbError> {
    if spec.encoding == FtpEncoding::AsciiU32Len && !value.is_ascii() {
        return Err(PtpUsbError::FtpFieldNotAscii { field: spec.key });
    }
    if value.chars().any(char::is_control) {
        return Err(PtpUsbError::FtpFieldControlChar { field: spec.key });
    }
    let len = match spec.encoding {
        FtpEncoding::AsciiU32Len => value.len(),
        FtpEncoding::Utf16U8Len => value.encode_utf16().count(),
    };
    if len > spec.max_len {
        return Err(PtpUsbError::FtpFieldTooLong {
            field: spec.key,
            len,
            max: spec.max_len,
            unit: spec.unit().label(),
        });
    }
    Ok(())
}

/// `[u8 length in UTF-16 units incl. null][utf16le content + null]`, no
/// validation: the profile-name and host fields' layout, and the template
/// block's two placeholder strings.
fn utf16_u8_len_bytes(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + (units.len() + 1) * 2);
    out.push((units.len() + 1) as u8);
    for u in units {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes()); // null terminator
    out
}

/// Validate `value` against `spec` and lay it out as `spec.encoding` says.
/// The `u8` length prefix is range-checked here as well as by `max_len`, so a
/// spec whose limit exceeded 254 units could not silently truncate it.
fn encode_field_with(spec: &FtpFieldSpec, value: &str) -> Result<Vec<u8>, PtpUsbError> {
    validate_field(spec, value)?;
    match spec.encoding {
        FtpEncoding::AsciiU32Len => {
            let bytes = value.as_bytes();
            let mut out = Vec::with_capacity(4 + bytes.len());
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
            Ok(out)
        }
        FtpEncoding::Utf16U8Len => {
            let units: Vec<u16> = value.encode_utf16().collect();
            if u8::try_from(units.len() + 1).is_err() {
                return Err(PtpUsbError::FtpFieldTooLong {
                    field: spec.key,
                    len: units.len(),
                    max: usize::from(u8::MAX) - 1,
                    unit: spec.unit().label(),
                });
            }
            Ok(utf16_u8_len_bytes(&units))
        }
    }
}

fn encode_field(field: FtpField, value: &str) -> Result<Vec<u8>, PtpUsbError> {
    encode_field_with(field.spec(), value)
}

fn encode_ipv6_placeholder() -> Vec<u8> {
    let content = FTP_IPV6_PLACEHOLDER.as_bytes();
    let mut out = Vec::with_capacity(4 + content.len());
    out.extend_from_slice(&(content.len() as u32).to_le_bytes());
    out.extend_from_slice(content);
    out
}

/// The 82-byte block between the profile name and the SSID section (see
/// [`FTP_TEMPLATE_TIMESTAMP_PLACEHOLDER`]).
fn encode_template_block(network: &FtpCameraNetwork) -> Vec<u8> {
    let placeholder: Vec<u16> = FTP_TEMPLATE_TIMESTAMP_PLACEHOLDER.encode_utf16().collect();
    let mut out = Vec::new();
    out.extend_from_slice(&utf16_u8_len_bytes(&placeholder));
    out.extend_from_slice(&utf16_u8_len_bytes(&placeholder));
    out.extend_from_slice(&FTP_TEMPLATE_NETWORK_TAG);
    let [a, b, c, d] = network.ipv4;
    out.extend_from_slice(&[d, c, b, a]); // little-endian u32 of the address
    out.push(network.prefix_len);
    out.extend_from_slice(&FTP_TEMPLATE_NETWORK_PAD);
    out
}

/// Build the `0x90EE` data-phase blob for `profile`, with the captured
/// [`FtpCameraNetwork`]. Reproduces the real captured wire format
/// byte-for-byte (verified in tests against two of the three 2026-07-09
/// captures; the 331-byte variant is not modelled) rather than a best guess.
pub fn encode_ftp_profile(profile: &FtpProfile) -> Result<Vec<u8>, PtpUsbError> {
    encode_ftp_profile_with(profile, &FtpCameraNetwork::CAPTURED)
}

/// [`encode_ftp_profile`] with an explicit [`FtpCameraNetwork`].
pub fn encode_ftp_profile_with(profile: &FtpProfile, network: &FtpCameraNetwork) -> Result<Vec<u8>, PtpUsbError> {
    let mut out = Vec::new();
    out.extend_from_slice(&FTP_BLOB_HEADER);
    out.extend_from_slice(&encode_field(FtpField::ProfileName, &profile.profile_name)?);
    out.extend_from_slice(&encode_template_block(network));
    out.extend_from_slice(&FTP_TAG_SSID_24GHZ);
    out.extend_from_slice(&encode_field(FtpField::Ssid24Ghz, &profile.ssid_24ghz)?);
    out.extend_from_slice(&FTP_TAG_SSID_5GHZ);
    out.extend_from_slice(&encode_field(FtpField::Ssid5Ghz, &profile.ssid_5ghz)?);
    out.push(FTP_TAG_HOST);
    out.extend_from_slice(&encode_field(FtpField::Host, &profile.host)?);
    out.extend_from_slice(&FTP_TAG_USERNAME);
    out.extend_from_slice(&encode_field(FtpField::Username, &profile.username)?);
    // password has no tag bytes of its own — it immediately follows username's content.
    out.extend_from_slice(&encode_field(FtpField::Password, &profile.password)?);
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
// 0x90ED FTP-profile read: decoding the blob and the 0x90EB info reply
//
// Worked out offline from the 2026-07-09 Z 9 captures (three distinct blobs:
// 325 B, 331 B, 317 B). The layout is the one `encode_ftp_profile_with`
// writes, plus one field the encoder does not model: a PTP string between
// host and username (see `DecodedFtpProfile::folder`). The decoder is
// strict: any structural byte it does not recognise is an error carrying
// the offset, because a silently wrong decode of a profile is worse than
// none. The raw bytes are always available to the caller.
// ─────────────────────────────────────────────────────────────────────────

/// What `0x90EB` returns: the camera's identity and where its FTP profile
/// is. Layout (57–59 bytes; **inferred from five replies in three body
/// states, not documented**): `u16 0x00c8`, `u32 count`, `u32 0x14`,
/// `u32 slot`, 16-byte GUID, PTP string (the body's `<model>_<serial>`),
/// `u16 0x01f4`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpProfileInfo {
    /// `0x00c8` (200) in every reply seen; possibly a limit.
    pub header: u16,
    /// 6 then 7 on a Z 9 (before / after it gained an FTP profile), 3 on a
    /// Z6III with none. Meaning unknown; could count saved network profiles
    /// rather than FTP ones.
    pub count: u32,
    /// `0x14` (20) in every reply seen; possibly a limit.
    pub limit: u32,
    /// The slot to pass to `0x90ED`: `0` when the camera holds a profile,
    /// [`FTP_NO_ACTIVE_SLOT`] (`0xff`) when none is active (stored profiles may
    /// still exist; see that constant).
    pub slot: u32,
    pub guid: [u8; 16],
    /// Contains the body's serial number.
    pub device_name: PtpString,
    /// `0x01f4` (500) in every reply seen; the same value as the profile
    /// blob's two-byte header.
    pub trailer: u16,
}

impl FtpProfileInfo {
    pub fn has_active_profile(&self) -> bool {
        self.slot != FTP_NO_ACTIVE_SLOT
    }
}

/// A bounds-checked cursor over an FTP-profile blob or `0x90EB` reply. Every
/// read names what it is for, and failures carry the offset.
struct FtpBlobReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> FtpBlobReader<'a> {
    fn bad(&self, what: &'static str, detail: impl Into<String>) -> PtpUsbError {
        PtpUsbError::FtpBlobMalformed { what, offset: self.pos, detail: detail.into() }
    }

    fn take(&mut self, what: &'static str, n: usize) -> Result<&'a [u8], PtpUsbError> {
        let remaining = self.data.len() - self.pos;
        if n > remaining {
            return Err(self.bad(what, format!("needs {n} byte(s) but only {remaining} remain")));
        }
        let bytes = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(bytes)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, PtpUsbError> {
        Ok(self.take(what, 1)?[0])
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, PtpUsbError> {
        Ok(u16::from_le_bytes(self.take(what, 2)?.try_into().expect("take(2) returns 2 bytes")))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, PtpUsbError> {
        Ok(u32::from_le_bytes(self.take(what, 4)?.try_into().expect("take(4) returns 4 bytes")))
    }

    /// A literal tag: the bytes must be exactly `expected`.
    fn expect(&mut self, what: &'static str, expected: &[u8]) -> Result<(), PtpUsbError> {
        let at = self.pos;
        let got = self.take(what, expected.len())?;
        if got != expected {
            self.pos = at;
            return Err(self.bad(what, format!("expected {}, found {}", hex_bytes(expected), hex_bytes(got))));
        }
        Ok(())
    }

    /// `[u8 count incl. null][utf16le]`; count 0 is the empty string with no
    /// further bytes. A non-empty string must end in the null.
    fn ptp_string(&mut self, what: &'static str) -> Result<PtpString, PtpUsbError> {
        let at = self.pos;
        let chars = usize::from(self.u8(what)?);
        let bytes = self.take(what, chars * 2)?;
        let mut units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let mut anomalies = Vec::new();
        if !units.is_empty() {
            if units.last() == Some(&0) {
                units.pop();
            } else {
                anomalies.push(StringAnomaly::MissingTerminator);
            }
        }
        if units.contains(&0) {
            anomalies.push(StringAnomaly::EmbeddedNul);
        }
        let mut text = String::with_capacity(units.len());
        for decoded in char::decode_utf16(units) {
            match decoded {
                Ok(c) => text.push(c),
                Err(_) => {
                    text.push(char::REPLACEMENT_CHARACTER);
                    if !anomalies.contains(&StringAnomaly::UnpairedSurrogate) {
                        anomalies.push(StringAnomaly::UnpairedSurrogate);
                    }
                }
            }
        }
        if !anomalies.is_empty() {
            let tags: Vec<&str> = anomalies.iter().map(|a| a.tag()).collect();
            self.pos = at;
            return Err(self.bad(what, format!("string is not well-formed ({})", tags.join(", "))));
        }
        Ok(PtpString { text, anomalies })
    }

    /// `[u32 length in bytes][ascii]`, no null. Non-ASCII is an error.
    fn ascii_u32_len(&mut self, what: &'static str) -> Result<String, PtpUsbError> {
        let at = self.pos;
        let len = self.u32(what)? as usize;
        if len > self.data.len() - self.pos {
            self.pos = at;
            return Err(self.bad(what, format!("declares {len} byte(s) but only {} remain", self.data.len() - at - 4)));
        }
        let bytes = self.take(what, len)?;
        if !bytes.is_ascii() {
            self.pos = at;
            return Err(self.bad(what, "contains non-ASCII bytes"));
        }
        Ok(String::from_utf8(bytes.to_vec()).expect("checked ASCII"))
    }
}

/// Decode a `0x90EB` reply. Strict on length and the PTP string; the other
/// fields are returned as read (see [`FtpProfileInfo`] for what is known).
pub fn parse_ftp_profile_info(data: &[u8]) -> Result<FtpProfileInfo, PtpUsbError> {
    let mut r = FtpBlobReader { data, pos: 0 };
    let header = r.u16("info header")?;
    let count = r.u32("info count")?;
    let limit = r.u32("info limit")?;
    let slot = r.u32("info slot")?;
    let guid: [u8; 16] = r.take("info GUID", 16)?.try_into().expect("take(16) returns 16 bytes");
    let device_name = r.ptp_string("info device name")?;
    let trailer = r.u16("info trailer")?;
    if r.pos != data.len() {
        return Err(r.bad("info end", format!("{} unexpected byte(s) after the trailer", data.len() - r.pos)));
    }
    Ok(FtpProfileInfo { header, count, limit, slot, guid, device_name, trailer })
}

/// An FTP profile as read back from the camera by `0x90ED`. Every byte of
/// the blob is accounted for: the fields the encoder writes, the one it does
/// not model ([`Self::folder`]), and the structural bytes the decoder
/// verified against literals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFtpProfile {
    pub profile_name: String,
    /// The two UTF-16 strings after the name. All-zero date-time shape
    /// (`"00000000T000000"`) in every capture; probably timestamps that were
    /// never set. Kept as read.
    pub timestamps: [String; 2],
    /// The camera's own address and prefix length as stored in the profile
    /// (192.168.66.87/24 in every capture; the capture rig's Z 9).
    pub camera_network: FtpCameraNetwork,
    pub ssid_24ghz: String,
    /// The third byte of the four-byte tag before [`Self::ssid_24ghz`]
    /// (`03 00 xx 00`): `01` in most profiles, `0x30` in one with no stored
    /// camera address. Meaning unknown. The encoder writes `01`.
    pub ssid24_kind: u8,
    /// The string the encoder calls the 5 GHz SSID. **Probably the Wi-Fi
    /// password, not an SSID**: in the July blob it is `nikon5ghz`, the
    /// passphrase of the capture rig's access point, and in the profiles read
    /// on 2026-11-05 it looked like a passphrase too. Treated as a secret by
    /// [`Self::with_secrets_masked`] and the CLI. (Unconfirmed against the
    /// camera's menu.)
    pub ssid_5ghz: String,
    /// The first byte of the two-byte tag before [`Self::ssid_5ghz`]: `04` in
    /// the July blobs and a NETSOL profile, `06` in a phone-hotspot profile
    /// (the second byte is `04` in all). Meaning unknown; possibly a security
    /// type. The encoder writes `04`.
    pub ssid5_kind: u8,
    pub host: String,
    /// A PTP string between host and username: the **destination folder**
    /// (confirmed 2026-11-05: a Z6III profile carried a real directory path
    /// here). Empty when none is set. The encoder cannot write it.
    pub folder: String,
    pub username: String,
    /// In plain text, as the camera returns it. Callers should not log it.
    pub password: String,
    /// The u16 between the folder string and the username: 21 in the Z 9
    /// blobs from July, 22 in a profile read over USB on 2026-11-05. Almost
    /// certainly the **FTP/SFTP server port** (the July body profile was
    /// typed in with port 21). The encoder hard-codes it as 21.
    pub ftp_port: u16,
    /// The u16 after the password: option bits. Seen: `0x0001`, `0x0005` (what
    /// the encoder writes), `0x0007`, `0x0009`, `0x000d`. `0x0008` is **SFTP**
    /// (confirmed on one profile whose menu says SFTP; set on both port-22
    /// profiles seen). `0x0002` is set only on profiles with an empty login
    /// (probably anonymous). `0x0004` is unexplained (maybe passive mode).
    /// `0x0001` was set on all but one profile.
    pub option_flags: u16,
    /// The u16 after the option flags: 8080 in most blobs seen, 0 in one
    /// (a hotspot profile with no stored camera address). 8080 is the usual
    /// default proxy port, so this is probably the proxy-server port.
    /// **The encoder's `FtpProfile.port` is written here**, not into
    /// [`Self::ftp_port`].
    pub second_port: u16,
    /// The three IPv6-shaped fields at the end, as read
    /// (`"0000:…:0000"` in every capture).
    pub ipv6_fields: [String; 3],
}

impl DecodedFtpProfile {
    /// The [`FtpProfile`] the encoder would need to write this profile back,
    /// when it can: not possible if [`Self::folder`] is non-empty, or
    /// [`Self::option_flags`] is not the encoder's `0x0005`, or
    /// [`Self::ftp_port`] is not the 21 it hard-codes, or [`Self::ssid5_kind`]
    /// is not `04`, or [`Self::ssid24_kind`] is not `01`, since the encoder has
    /// no way to carry any of them. `FtpProfile.port` receives
    /// [`Self::second_port`] (where the encoder writes it).
    /// The SFTP option bit (`0x0008`). Confirmed against the camera's menu on
    /// one profile; see [`Self::option_flags`].
    pub fn is_sftp(&self) -> bool {
        self.option_flags & 0x0008 != 0
    }

    pub fn as_encodable(&self) -> Option<FtpProfile> {
        (self.folder.is_empty() && self.option_flags == 0x0005 && self.ftp_port == 21 && self.ssid5_kind == FTP_TAG_SSID_5GHZ[0] && self.ssid24_kind == FTP_TAG_SSID_24GHZ[2])
            .then(|| FtpProfile {
            profile_name: self.profile_name.clone(),
            ssid_24ghz: self.ssid_24ghz.clone(),
            ssid_5ghz: self.ssid_5ghz.clone(),
            host: self.host.clone(),
            port: self.second_port,
            username: self.username.clone(),
            password: self.password.clone(),
        })
    }

    /// This profile with the password and the string the encoder calls the
    /// 5 GHz SSID (probably a Wi-Fi password) replaced by asterisks of the
    /// same length, for display.
    pub fn with_secrets_masked(&self) -> Self {
        DecodedFtpProfile {
            password: "*".repeat(self.password.len()),
            ssid_5ghz: "*".repeat(self.ssid_5ghz.len()),
            ..self.clone()
        }
    }
}

/// Decode a `0x90ED` reply (or any `0x90EE` data-out blob) into its fields.
/// Strict: an unrecognised structural byte, a truncated field or trailing
/// bytes is [`PtpUsbError::FtpBlobMalformed`] with the offset.
pub fn decode_ftp_profile(blob: &[u8]) -> Result<DecodedFtpProfile, PtpUsbError> {
    let mut r = FtpBlobReader { data: blob, pos: 0 };
    r.expect("blob header", &FTP_BLOB_HEADER)?;
    let profile_name = r.ptp_string("profile name")?.text;
    let timestamps = [r.ptp_string("timestamp 1")?.text, r.ptp_string("timestamp 2")?.text];
    r.expect("network tag", &FTP_TEMPLATE_NETWORK_TAG)?;
    let addr: [u8; 4] = r.take("camera address", 4)?.try_into().expect("take(4) returns 4 bytes");
    let prefix_len = r.u8("camera prefix length")?;
    r.expect("network padding", &FTP_TEMPLATE_NETWORK_PAD)?;
    let camera_network = FtpCameraNetwork { ipv4: [addr[3], addr[2], addr[1], addr[0]], prefix_len };
    let tag_at = r.pos;
    let tag24 = r.take("2.4 GHz SSID tag", 4)?;
    if tag24[0] != FTP_TAG_SSID_24GHZ[0] || tag24[1] != FTP_TAG_SSID_24GHZ[1] || tag24[3] != FTP_TAG_SSID_24GHZ[3] {
        r.pos = tag_at;
        return Err(r.bad("2.4 GHz SSID tag", format!("expected 03 00 xx 00, found {}", hex_bytes(tag24))));
    }
    let ssid24_kind = tag24[2];
    let ssid_24ghz = r.ascii_u32_len("2.4 GHz SSID")?;
    let tag_at = r.pos;
    let tag5 = r.take("5 GHz SSID tag", 2)?;
    if tag5[1] != FTP_TAG_SSID_5GHZ[1] {
        r.pos = tag_at;
        return Err(r.bad("5 GHz SSID tag", format!("expected xx {:02x}, found {}", FTP_TAG_SSID_5GHZ[1], hex_bytes(tag5))));
    }
    let ssid5_kind = tag5[0];
    let ssid_5ghz = r.ascii_u32_len("5 GHz SSID")?;
    r.expect("host tag", &[FTP_TAG_HOST])?;
    let host = r.ptp_string("host")?.text;
    let folder = r.ptp_string("folder")?.text;
    // The encoder's FTP_TAG_USERNAME is [00 15 00]: the first byte is the
    // empty `folder` string (count 0) and `15 00` is the FTP port, 21. It is
    // a u16 here because a profile read on 2026-11-05 carried `16 00` (22).
    let ftp_port = r.u16("FTP port")?;
    let username = r.ascii_u32_len("username")?;
    let password = r.ascii_u32_len("password")?;
    let option_flags = r.u16("option flags")?;
    let second_port = r.u16("second port")?;
    r.expect("port-section padding", &FTP_PORT_SECTION_PAD)?;
    let ipv6_a = r.ascii_u32_len("IPv6 field 1")?;
    r.expect("IPv6 separator", &[FTP_IPV6_FIELD1_SEPARATOR])?;
    let ipv6_b = r.ascii_u32_len("IPv6 field 2")?;
    let ipv6_c = r.ascii_u32_len("IPv6 field 3")?;
    r.expect("blob trailer", &[FTP_BLOB_TRAILER])?;
    if r.pos != blob.len() {
        return Err(r.bad("blob end", format!("{} unexpected byte(s) after the trailer", blob.len() - r.pos)));
    }
    Ok(DecodedFtpProfile {
        profile_name,
        timestamps,
        camera_network,
        ssid_24ghz,
        ssid24_kind,
        ssid_5ghz,
        ssid5_kind,
        host,
        folder,
        username,
        password,
        ftp_port,
        option_flags,
        second_port,
        ipv6_fields: [ipv6_a, ipv6_b, ipv6_c],
    })
}

// ─────────────────────────────────────────────────────────────────────────
// Session
// ─────────────────────────────────────────────────────────────────────────

/// Something a reply did that [`TxidCheck::Unchecked`] let through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyDeviation {
    TransactionId { got: u32 },
    DataOp { got: u16 },
}

/// What one transaction collected.
#[derive(Debug)]
struct Reply {
    code: u16,
    params: Vec<u32>,
    data: Vec<u8>,
    /// How many Data containers arrived — 0 is "no data phase", which is not
    /// the same as a data phase of zero bytes.
    data_containers: usize,
    /// Mismatches an `Unchecked` transaction tolerated.
    deviations: Vec<ReplyDeviation>,
    /// Containers of an unknown kind that were skipped.
    unrecognized_containers: u32,
}

/// What a successful FTP-profile write tells you — and what it does not.
///
/// `RESP_OK` is the only acknowledgement there is: the camera answered OK to
/// the `0x90EE` data. Nothing reads the profile back (no read op is known),
/// so this is "the camera accepted the blob", not "the profile now holds
/// these values".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpWriteReceipt {
    /// The parameters of the `0x90EE` response (none seen in any capture).
    pub response_params: Vec<u32>,
    /// False when a reply to the setup or write carried a different
    /// transaction id (or a data container a different op code) than the
    /// command — tolerated because those ops have no capture to hold them to.
    pub reply_matched_command: bool,
}

/// The profile bytes of an FTP-profile read, with the decode attempted on
/// them. The raw bytes are kept whether or not the decode worked.
#[derive(Debug)]
pub struct FtpProfileBlob {
    pub raw: Vec<u8>,
    pub decoded: Result<DecodedFtpProfile, PtpUsbError>,
}

/// What [`PtpUsbSession::read_ftp_profile`] found.
#[derive(Debug)]
pub struct FtpProfileReadout {
    pub info: FtpProfileInfo,
    /// `None` when `0x90EB` reported no active profile (slot `0xff`); `0x90ED` was
    /// not sent then.
    pub profile: Option<FtpProfileBlob>,
    /// Things worth telling the user that are not failures.
    pub warnings: Vec<String>,
    /// False when a reply carried a different transaction id or operation
    /// code than its command (tolerated; these ops have no USB capture).
    pub reply_matched_command: bool,
}

/// What a vendor-property read returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorRead {
    pub data: Vec<u8>,
    /// Things worth telling the user that are not failures (an unsurveyed
    /// model). [`PtpUsbSession::read_vendor_property`] prints them to stderr.
    pub warnings: Vec<String>,
}

/// What one `0x944C` query answered. Unlike the other reads here, a non-OK
/// response code is **data, not an error**: for a query whose meaning is
/// being worked out, the code the camera gives for an operation it does not
/// know is as much the finding as the bytes it gives for one it does. So
/// nothing is called "supported" here; read [`Self::response_code`] and
/// [`Self::data`] yourself. Transport faults (a timeout, a stalled pipe) are
/// still `Err`, and poison the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorOpQuery {
    /// The operation code asked about (the first `0x944C` parameter).
    pub target_op: u16,
    /// The camera's response code to the `0x944C` command itself.
    pub response_code: u16,
    /// The data-in phase, concatenated (empty when there was none).
    pub data: Vec<u8>,
    /// How many data containers arrived: 0 is "no data phase", which is not
    /// the same as a data phase of zero bytes.
    pub data_containers: usize,
    /// False when the reply carried a different transaction id or operation
    /// code than the command (tolerated: no USB capture exists to hold a
    /// `0x944C` reply to).
    pub reply_matched_command: bool,
    /// Things worth telling the user that are not failures.
    pub warnings: Vec<String>,
}

impl VendorOpQuery {
    /// The camera answered the command with `OK`. Says nothing about whether
    /// `target_op` is implemented; that reading is the open question.
    pub fn is_ok(&self) -> bool {
        self.response_code == RESP_OK
    }
}

/// Why a DeviceInfo dataset could not be parsed (the `fault` of
/// [`PtpUsbError::DeviceInfoMalformed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DeviceInfoFault {
    #[error("needs {needed} more byte(s) but only {available} remain")]
    Truncated { needed: usize, available: usize },
    #[error("declares {count} element(s) ({needed} byte(s)) but only {available} byte(s) remain")]
    CountExceedsData { count: u32, needed: u64, available: usize },
}

/// Something odd about a decoded [`PtpString`]. A closed set: the text is still
/// returned, and these say how far to trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringAnomaly {
    /// An unpaired UTF-16 surrogate, decoded as U+FFFD.
    UnpairedSurrogate,
    /// The string's last code unit is not the NUL the format requires.
    MissingTerminator,
    /// A NUL before the last code unit; the text keeps it.
    EmbeddedNul,
}

impl StringAnomaly {
    pub fn tag(self) -> &'static str {
        match self {
            StringAnomaly::UnpairedSurrogate => "unpaired_surrogate",
            StringAnomaly::MissingTerminator => "missing_terminator",
            StringAnomaly::EmbeddedNul => "embedded_nul",
        }
    }
}

/// A PTP string (ISO 15740 §3.2.2): a character count `N` (terminating NUL
/// included; 0 means an empty string with no further bytes), then `N`
/// UTF-16LE code units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtpString {
    /// Decoded without the terminating NUL; lossy where `anomalies` says so.
    pub text: String,
    pub anomalies: Vec<StringAnomaly>,
}

/// The ISO 15740 DeviceInfo dataset (response data of operation `0x1001`),
/// every field of it: none is dropped. Bytes after the last field are kept in
/// `trailing` rather than ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfoDataset {
    pub standard_version: u16,
    pub vendor_extension_id: u32,
    pub vendor_extension_version: u16,
    pub vendor_extension_desc: PtpString,
    pub functional_mode: u16,
    pub operations_supported: Vec<u16>,
    pub events_supported: Vec<u16>,
    pub device_properties_supported: Vec<u16>,
    pub capture_formats: Vec<u16>,
    pub playback_formats: Vec<u16>,
    pub manufacturer: PtpString,
    pub model: PtpString,
    pub device_version: PtpString,
    pub serial_number: PtpString,
    pub trailing: Vec<u8>,
}

/// A bounds-checked cursor over a DeviceInfo dataset. Every read names the
/// field it is for, so a failure says where the dataset went wrong.
struct DeviceInfoReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> DeviceInfoReader<'a> {
    fn malformed(&self, field: &'static str, fault: DeviceInfoFault) -> PtpUsbError {
        PtpUsbError::DeviceInfoMalformed { field, offset: self.pos, fault }
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn take(&mut self, field: &'static str, n: usize) -> Result<&'a [u8], PtpUsbError> {
        if n > self.remaining() {
            return Err(self.malformed(field, DeviceInfoFault::Truncated { needed: n, available: self.remaining() }));
        }
        let bytes = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(bytes)
    }

    fn u8(&mut self, field: &'static str) -> Result<u8, PtpUsbError> {
        Ok(self.take(field, 1)?[0])
    }

    fn u16(&mut self, field: &'static str) -> Result<u16, PtpUsbError> {
        Ok(u16::from_le_bytes(self.take(field, 2)?.try_into().expect("take(2) returns 2 bytes")))
    }

    fn u32(&mut self, field: &'static str) -> Result<u32, PtpUsbError> {
        Ok(u32::from_le_bytes(self.take(field, 4)?.try_into().expect("take(4) returns 4 bytes")))
    }

    /// A u32 count and that many u16 codes. The count is checked against the
    /// bytes left before anything is allocated for it.
    fn u16_array(&mut self, field: &'static str) -> Result<Vec<u16>, PtpUsbError> {
        let count = self.u32(field)?;
        let needed = u64::from(count) * 2;
        if needed > self.remaining() as u64 {
            return Err(self.malformed(field, DeviceInfoFault::CountExceedsData { count, needed, available: self.remaining() }));
        }
        let bytes = self.take(field, needed as usize)?;
        Ok(bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect())
    }

    fn string(&mut self, field: &'static str) -> Result<PtpString, PtpUsbError> {
        let chars = usize::from(self.u8(field)?);
        let bytes = self.take(field, chars * 2)?;
        let mut units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let mut anomalies = Vec::new();
        if !units.is_empty() {
            if units.last() == Some(&0) {
                units.pop();
            } else {
                anomalies.push(StringAnomaly::MissingTerminator);
            }
        }
        if units.contains(&0) {
            anomalies.push(StringAnomaly::EmbeddedNul);
        }
        let mut text = String::with_capacity(units.len());
        let mut lossy = false;
        for decoded in char::decode_utf16(units) {
            match decoded {
                Ok(c) => text.push(c),
                Err(_) => {
                    lossy = true;
                    text.push(char::REPLACEMENT_CHARACTER);
                }
            }
        }
        if lossy {
            anomalies.push(StringAnomaly::UnpairedSurrogate);
        }
        Ok(PtpString { text, anomalies })
    }
}

/// Parse a DeviceInfo dataset (ISO 15740 §5.5.1.1). Total: a truncated
/// dataset, or an array count larger than the bytes left, is a typed error;
/// bytes after the SerialNumber are returned in `trailing`.
pub fn parse_device_info(data: &[u8]) -> Result<DeviceInfoDataset, PtpUsbError> {
    let mut r = DeviceInfoReader { data, pos: 0 };
    let standard_version = r.u16("StandardVersion")?;
    let vendor_extension_id = r.u32("VendorExtensionID")?;
    let vendor_extension_version = r.u16("VendorExtensionVersion")?;
    let vendor_extension_desc = r.string("VendorExtensionDesc")?;
    let functional_mode = r.u16("FunctionalMode")?;
    let operations_supported = r.u16_array("OperationsSupported")?;
    let events_supported = r.u16_array("EventsSupported")?;
    let device_properties_supported = r.u16_array("DevicePropertiesSupported")?;
    let capture_formats = r.u16_array("CaptureFormats")?;
    let playback_formats = r.u16_array("PlaybackFormats")?;
    let manufacturer = r.string("Manufacturer")?;
    let model = r.string("Model")?;
    let device_version = r.string("DeviceVersion")?;
    let serial_number = r.string("SerialNumber")?;
    let trailing = r.data[r.pos..].to_vec();
    Ok(DeviceInfoDataset {
        standard_version,
        vendor_extension_id,
        vendor_extension_version,
        vendor_extension_desc,
        functional_mode,
        operations_supported,
        events_supported,
        device_properties_supported,
        capture_formats,
        playback_formats,
        manufacturer,
        model,
        device_version,
        serial_number,
        trailing,
    })
}

/// How the PTP DeviceInfo SerialNumber compares with the USB `iSerialNumber`
/// of the same body. A closed set; [`Self::tag`] is its wire form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialVerdict {
    /// Both non-empty and byte-for-byte equal.
    Match,
    /// Both non-empty and different.
    Differ,
    /// The PTP serial is empty, the USB one is not.
    PtpEmpty,
    /// The USB serial is empty, the PTP one is not: the case DeviceInfo could identify a body the
    /// descriptor cannot.
    UsbEmpty,
    BothEmpty,
}

impl SerialVerdict {
    /// Exact comparison, no trimming or case folding: a near-miss is `Differ`, and both values are
    /// printed beside the verdict.
    pub fn compare(ptp: &str, usb: &str) -> Self {
        match (ptp.is_empty(), usb.is_empty()) {
            (true, true) => SerialVerdict::BothEmpty,
            (true, false) => SerialVerdict::PtpEmpty,
            (false, true) => SerialVerdict::UsbEmpty,
            (false, false) if ptp == usb => SerialVerdict::Match,
            (false, false) => SerialVerdict::Differ,
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            SerialVerdict::Match => "match",
            SerialVerdict::Differ => "differ",
            SerialVerdict::PtpEmpty => "ptp_empty",
            SerialVerdict::UsbEmpty => "usb_empty",
            SerialVerdict::BothEmpty => "both_empty",
        }
    }
}

/// What a DeviceInfo read returned, with the identity the USB lookup had already read for the
/// same body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfoReport {
    pub dataset: DeviceInfoDataset,
    /// The USB product string's model (`"Z 9"`); empty when it could not be read.
    pub usb_model: String,
    /// The USB `iSerialNumber` descriptor; empty when the body has none or the read failed
    /// (`usb_warnings` says which).
    pub usb_serial: String,
    /// Descriptor reads that failed while identifying the body.
    pub usb_warnings: Vec<String>,
}

impl DeviceInfoReport {
    pub fn serial_verdict(&self) -> SerialVerdict {
        SerialVerdict::compare(&self.dataset.serial_number.text, &self.usb_serial)
    }
}

/// An open raw-PTP-over-USB session. Closes the PTP session and releases the
/// USB interface on drop (best-effort — errors are ignored, matching
/// `sdk.rs`'s `Device::drop` policy of not panicking on teardown), unless a
/// failure poisoned it (see the module doc).
pub struct PtpUsbSession<T: BulkTransport = rusb::DeviceHandle<GlobalContext>> {
    handle: T,
    interface: u8,
    ep_out: u8,
    ep_in: u8,
    /// The bulk-OUT endpoint's max packet size (0 when unknown): the unit a
    /// container's length is checked against for a trailing zero-length packet.
    max_packet_out: usize,
    transaction_id: u32,
    session_open: bool,
    model: String,
    /// Reused scratch buffer for `fill_rx`'s bulk-IN reads, so a session
    /// doing many reads (every PTP transaction needs at least one, often
    /// more for a Data phase) doesn't allocate+zero a fresh buffer on every
    /// single one.
    read_scratch: Vec<u8>,
    /// Bytes received but not yet parsed into a container: the start of the
    /// next container when a read captured more than one.
    rx: Vec<u8>,
    transaction_timeout: Duration,
    /// Why the session is unusable, once a failure left the pipes in an
    /// unknown state.
    poisoned: Option<String>,
    /// The body's USB `iSerialNumber` descriptor as the device lookup read it (empty: none, or the
    /// read failed — see `usb_warnings`); the test constructor leaves it empty.
    usb_serial: String,
    /// Descriptor reads that failed while the lookup identified this body.
    usb_warnings: Vec<String>,
    /// Whether the most recent transaction got as far as handing its
    /// data-out container to the pipe (so the camera may have acted on it).
    data_phase_started: bool,
    /// Containers of an unknown kind skipped over the session's life.
    unrecognized_containers: u32,
    /// Held for the session's life (dropped after the interface is released);
    /// `None` only for the test constructor.
    _camera_lock: Option<CameraLock>,
}

impl PtpUsbSession {
    /// Find the Nikon camera (by serial if given, else the sole one
    /// attached), claim its PTP interface (racing macOS's own claimants),
    /// and open a PTP session. Takes the [`CameraLock`] first, so it fails
    /// with [`PtpUsbError::CameraLock`] (`camera_busy`) instead of killing
    /// `ptpcamerad` and re-asserting the USB configuration underneath another
    /// nikon-fleet session that is using the body.
    pub fn open(serial: Option<&str>) -> Result<Self, PtpUsbError> {
        Self::open_with(CameraLock::acquire, find_device, serial)
    }

    /// [`Self::open`] for a caller that already holds the [`CameraLock`] (it had to look at the bus
    /// first, e.g. to resolve an `id-N` serial): the lock is taken over, not taken again, so it
    /// cannot refuse itself, and is released with the session.
    pub fn open_locked(lock: CameraLock, serial: Option<&str>) -> Result<Self, PtpUsbError> {
        Self::open_locked_with(lock, find_device, serial)
    }

    /// [`Self::open`] with the lock and the device lookup injected, so the
    /// order (lock first, bus second) is testable without hardware.
    fn open_with(
        acquire_lock: impl FnOnce() -> Result<CameraLock, LockError>,
        find: impl FnOnce(Option<&str>) -> Result<Candidate, PtpUsbError>,
        serial: Option<&str>,
    ) -> Result<Self, PtpUsbError> {
        Self::open_locked_with(acquire_lock()?, find, serial)
    }

    fn open_locked_with(
        lock: CameraLock,
        find: impl FnOnce(Option<&str>) -> Result<Candidate, PtpUsbError>,
        serial: Option<&str>,
    ) -> Result<Self, PtpUsbError> {
        let Candidate { device, handle, model, usb_serial, warnings, .. } = find(serial)?;
        let endpoints = find_ptp_endpoints(&device)?;
        claim_with_retry(&handle, endpoints.interface)?;

        let mut session = PtpUsbSession::new(
            handle,
            endpoints.interface,
            endpoints.ep_out,
            endpoints.ep_in,
            endpoints.ep_out_max_packet,
            model,
            Some(lock),
        );
        session.usb_serial = usb_serial;
        session.usb_warnings = warnings;
        session.open_ptp_session()?;
        Ok(session)
    }
}

impl<T: BulkTransport> PtpUsbSession<T> {
    fn new(
        handle: T,
        interface: u8,
        ep_out: u8,
        ep_in: u8,
        max_packet_out: usize,
        model: String,
        camera_lock: Option<CameraLock>,
    ) -> Self {
        PtpUsbSession {
            handle,
            interface,
            ep_out,
            ep_in,
            max_packet_out,
            transaction_id: 0,
            session_open: false,
            model,
            read_scratch: vec![0u8; READ_CHUNK],
            rx: Vec::new(),
            transaction_timeout: TRANSACTION_TIMEOUT,
            poisoned: None,
            usb_serial: String::new(),
            usb_warnings: Vec::new(),
            data_phase_started: false,
            unrecognized_containers: 0,
            _camera_lock: camera_lock,
        }
    }

    /// Test-only constructor: a session over `handle` with fixed endpoints,
    /// no PTP session opened yet (call [`Self::open_ptp_session`]).
    #[cfg(test)]
    fn with_transport(handle: T, model: &str) -> Self {
        Self::new(handle, 0, 0x02, 0x81, 512, model.to_string(), None)
    }

    /// The connected camera's model string (e.g. `"Z 9"`, `"Z6_3"`), as read
    /// from the USB product-string descriptor.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// How many containers of an unknown kind this session has skipped.
    pub fn unrecognized_container_count(&self) -> u32 {
        self.unrecognized_containers
    }

    /// Write one whole container, checking that the pipe took all of it, and
    /// end the transfer with a zero-length packet when its length is a
    /// multiple of the endpoint's max packet size.
    fn send_container(&mut self, buf: &[u8]) -> Result<(), PtpUsbError> {
        let written = self.handle.write_bulk(self.ep_out, buf, BULK_TIMEOUT)?;
        if written != buf.len() {
            return Err(PtpUsbError::ShortWrite { written, expected: buf.len() });
        }
        if needs_zero_length_packet(buf.len(), self.max_packet_out) {
            // Nothing to count: a write of no bytes can only transfer none.
            self.handle.write_bulk(self.ep_out, &[], BULK_TIMEOUT)?;
        }
        Ok(())
    }

    /// One bulk-IN read appended to `rx`; returns the byte count (0 is a
    /// zero-length packet). Checks the transaction deadline first and never
    /// lets a read outlast it.
    fn fill_rx(&mut self, deadline: Instant, timeout: Duration) -> Result<usize, PtpUsbError> {
        let Some(remaining) = remaining_until(deadline, Instant::now()) else {
            return Err(PtpUsbError::TransactionTimedOut(timeout));
        };
        match self.handle.read_bulk(self.ep_in, &mut self.read_scratch, bulk_read_timeout(remaining)) {
            Ok(n) => {
                self.rx.extend_from_slice(&self.read_scratch[..n]);
                Ok(n)
            }
            Err(rusb::Error::Timeout) if Instant::now() >= deadline => Err(PtpUsbError::TransactionTimedOut(timeout)),
            Err(e) => Err(e.into()),
        }
    }

    /// Read the next whole container off the bulk-IN pipe.
    ///
    /// The pipe is a byte stream with USB's framing quirks, not one
    /// container per read: a read can return the tail of a container and the
    /// head of the next (kept in `rx`), a header can arrive in pieces, a
    /// container can span several reads (this camera's own protocol family
    /// has vendor ops documented in docs/nx-field-session-2026-07-09.md
    /// needing more than one chunk), and a container whose length is a
    /// multiple of the packet size is followed by a zero-length packet,
    /// which is skipped between containers (never inside one: that is a
    /// truncation). `data_so_far` is the Data payload this transaction has
    /// already accumulated, so the 16 MiB ceiling is checked against the
    /// declared length *before* any of a huge container is buffered, and
    /// every read honours `deadline`.
    fn read_container(&mut self, deadline: Instant, timeout: Duration, data_so_far: usize) -> Result<Container, PtpUsbError> {
        let mut empty_reads = 0u32;
        while self.rx.len() < HEADER_LEN {
            if self.fill_rx(deadline, timeout)? == 0 {
                if !self.rx.is_empty() {
                    return Err(PtpUsbError::ShortContainer(self.rx.len()));
                }
                empty_reads += 1;
                if empty_reads > MAX_EMPTY_READS {
                    return Err(PtpUsbError::ShortContainer(0));
                }
            }
        }
        let declared = u32::from_le_bytes(self.rx[0..4].try_into().unwrap()) as usize;
        if declared < HEADER_LEN {
            return Err(PtpUsbError::ShortContainer(declared));
        }
        let payload_len = declared - HEADER_LEN;
        if payload_len > MAX_TRANSACTION_DATA_BYTES.saturating_sub(data_so_far) {
            return Err(PtpUsbError::TransactionDataTooLarge {
                got: data_so_far.saturating_add(payload_len),
                limit: MAX_TRANSACTION_DATA_BYTES,
            });
        }
        while self.rx.len() < declared {
            if self.fill_rx(deadline, timeout)? == 0 {
                return Err(PtpUsbError::TruncatedContainer { declared, got: self.rx.len() });
            }
        }
        let (container, consumed) = parse_container(&self.rx)?;
        self.rx.drain(..consumed);
        Ok(container)
    }

    /// Send a command (with an optional data-out phase), collect any
    /// data-in, and return the [`Reply`].
    ///
    /// The command's transaction id is consumed as soon as it is sent, not
    /// when the reply arrives: after any failure the next command must not
    /// reuse an id the camera may already have seen (and the `Unchecked`
    /// write ops would take a stale reply for the ack). The counter wraps
    /// per [`next_transaction_id`].
    ///
    /// With [`TxidCheck::Enforce`], a data or response container whose id
    /// differs from the command's fails with
    /// [`PtpUsbError::TransactionIdMismatch`], and a data container for
    /// another operation with [`PtpUsbError::UnexpectedDataOp`]. With
    /// `Unchecked` both are recorded on the reply instead. Event containers
    /// and unrecognized kinds are ignored before the check.
    ///
    /// A failure that may have left an unread reply or a stalled pipe
    /// poisons the session (see [`PtpUsbError::leaves_stream_in_sync`]).
    fn transact(
        &mut self,
        op: u16,
        params: &[u32],
        data_out: Option<&[u8]>,
        check: TxidCheck,
    ) -> Result<Reply, PtpUsbError> {
        self.transact_within(op, params, data_out, check, self.transaction_timeout)
    }

    fn transact_within(
        &mut self,
        op: u16,
        params: &[u32],
        data_out: Option<&[u8]>,
        check: TxidCheck,
        timeout: Duration,
    ) -> Result<Reply, PtpUsbError> {
        self.data_phase_started = false;
        if let Some(cause) = &self.poisoned {
            return Err(PtpUsbError::SessionPoisoned { cause: cause.clone() });
        }
        let result = self.run_transaction(op, params, data_out, check, timeout);
        if let Err(e) = &result
            && !e.leaves_stream_in_sync()
        {
            self.poison(e);
        }
        result
    }

    /// Mark the session unusable: clear the halt on both bulk endpoints (the
    /// NX field notes record that a hung op stalls the pipe and `clear_halt`
    /// is what frees it), drop buffered bytes, and remember why. A failing
    /// `clear_halt` is part of the recorded cause rather than an error of its
    /// own, since there is nothing further to do about it.
    fn poison(&mut self, cause: &PtpUsbError) {
        let mut note = cause.to_string();
        for ep in [self.ep_in, self.ep_out] {
            if let Err(e) = self.handle.clear_halt(ep) {
                note.push_str(&format!("; clear_halt(0x{ep:02x}) failed: {e}"));
            }
        }
        self.rx.clear();
        self.poisoned = Some(note);
    }

    fn run_transaction(
        &mut self,
        op: u16,
        params: &[u32],
        data_out: Option<&[u8]>,
        check: TxidCheck,
        timeout: Duration,
    ) -> Result<Reply, PtpUsbError> {
        let txid = self.transaction_id;
        let command = encode_command(op, txid, params)?;
        let data = data_out.map(|d| encode_data(op, txid, d)).transpose()?;
        self.transaction_id = next_transaction_id(txid);
        let deadline = Instant::now() + timeout;
        self.send_container(&command)?;
        if let Some(data) = data {
            self.data_phase_started = true;
            self.send_container(&data)?;
        }
        let mut reply = Reply {
            code: 0,
            params: Vec::new(),
            data: Vec::new(),
            data_containers: 0,
            deviations: Vec::new(),
            unrecognized_containers: 0,
        };
        loop {
            let container = self.read_container(deadline, timeout, reply.data.len())?;
            if matches!(container.kind, CONTAINER_DATA | CONTAINER_RESPONSE) && container.transaction_id != txid {
                match check {
                    TxidCheck::Enforce => {
                        return Err(PtpUsbError::TransactionIdMismatch { op, expected: txid, got: container.transaction_id });
                    }
                    TxidCheck::Unchecked => {
                        reply.deviations.push(ReplyDeviation::TransactionId { got: container.transaction_id });
                    }
                }
            }
            if container.kind == CONTAINER_DATA && container.code != op {
                match check {
                    TxidCheck::Enforce => return Err(PtpUsbError::UnexpectedDataOp { op, got: container.code }),
                    TxidCheck::Unchecked => reply.deviations.push(ReplyDeviation::DataOp { got: container.code }),
                }
            }
            match container.kind {
                CONTAINER_DATA => {
                    reply.data_containers += 1;
                    reply.data.extend(container.payload);
                }
                CONTAINER_RESPONSE => {
                    reply.params = decode_response_params(&container.payload)?;
                    reply.code = container.code;
                    return Ok(reply);
                }
                CONTAINER_EVENT => continue, // shouldn't arrive on the bulk pipe (the interrupt endpoint is never opened); ignore defensively
                other => {
                    // An unrecognized container kind is a protocol violation
                    // (or a misparse — see read_container's own truncation/
                    // desync guards). Counted, visible, and bounded by the
                    // deadline checked in every read.
                    reply.unrecognized_containers += 1;
                    self.unrecognized_containers += 1;
                    eprintln!("warning: PTP transaction received unrecognized container kind 0x{other:04x}, ignoring");
                }
            }
        }
    }

    /// Close the PTP session, checking the camera's answer. Teardown and the
    /// already-open retry ignore the result, but it is checked here so a
    /// reply with a different id or a non-OK code is a typed error rather
    /// than a silent discard.
    fn close_session(&mut self, timeout: Duration) -> Result<(), PtpUsbError> {
        let reply = self.transact_within(OP_CLOSE_SESSION, &[], None, TxidCheck::Enforce, timeout)?;
        Self::require_ok(OP_CLOSE_SESSION, reply.code)?;
        self.session_open = false;
        Ok(())
    }

    /// Open the PTP session, retrying once if a prior claimant (typically
    /// macOS's own `ptpcamerad`) left one open.
    ///
    /// The OpenSession-response decision is [`classify_open_session_response`];
    /// the surrounding sequencing (send, read, CloseSession, reset the
    /// transaction ID, resend) is covered against a scripted
    /// [`BulkTransport`] in this module's tests — the real-hardware
    /// behavior of that sequence on a Z 9 / Z6_3 is what the
    /// 2026-09-06 interactive session confirmed, but is not re-checked by
    /// the test suite.
    fn open_ptp_session(&mut self) -> Result<(), PtpUsbError> {
        let reply = self.transact(OP_OPEN_SESSION, &[SESSION_ID], None, TxidCheck::Enforce)?;
        match classify_open_session_response(reply.code) {
            OpenSessionResponse::Ok => {}
            OpenSessionResponse::RetryOnce => {
                // Leftover session from a prior claimant (typically macOS's
                // own ptpcamerad, which we just evicted) — close it and
                // retry once.
                self.transaction_id = 0;
                let timeout = self.transaction_timeout;
                // Best-effort: whether the close worked is judged by the
                // re-open below. If it failed, `transact` has already
                // cleared the halt on both endpoints and poisoned the
                // session; the re-open deliberately starts from that clean
                // state, as it did before poisoning existed.
                let _ = self.close_session(timeout);
                self.poisoned = None;
                self.transaction_id = 0;
                let reply = self.transact(OP_OPEN_SESSION, &[SESSION_ID], None, TxidCheck::Enforce)?;
                // Deliberately not re-matching classify_open_session_response
                // here: a second RESP_SESSION_ALREADY_OPEN on the retry
                // itself is treated as fatal, not looped again.
                if reply.code != RESP_OK {
                    return Err(PtpUsbError::OpenSessionFailed(PtpResponseCode(reply.code)));
                }
            }
            OpenSessionResponse::Failed => {
                return Err(PtpUsbError::OpenSessionFailed(PtpResponseCode(reply.code)));
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

    /// Read a vendor property via the `0x943B` wrapper. `propcode` is the
    /// raw `0xD0xx`/`0x5xxx` property code (without the `0x10000` flag —
    /// this adds it). Returns the property's raw bytes on success; any
    /// warning is printed to stderr (use [`Self::read_vendor_property_report`]
    /// to get it as data).
    pub fn read_vendor_property(&mut self, propcode: u16) -> Result<Vec<u8>, PtpUsbError> {
        let read = self.read_vendor_property_report(propcode)?;
        for w in &read.warnings {
            eprintln!("warning: {w}");
        }
        Ok(read.data)
    }

    /// [`Self::read_vendor_property`], returning the warnings instead of
    /// printing them. An OK answer with no data container at all is an error
    /// ([`PtpUsbError::NoDataPhase`]), distinct from a property whose data
    /// is empty.
    pub fn read_vendor_property_report(&mut self, propcode: u16) -> Result<VendorRead, PtpUsbError> {
        let mut warnings = Vec::new();
        if sdk_model_name(self.model()).is_none() {
            warnings.push(format!(
                "camera model {:?} is not one of the surveyed models ({:?}) for vendor-property reads; wire format may not match",
                self.model(),
                KNOWN_MODELS
            ));
        }
        let param = 0x10000 | propcode as u32;
        let reply = self.transact(OP_VENDOR_PROP_READ, &[param], None, TxidCheck::Enforce)?;
        Self::require_ok(OP_VENDOR_PROP_READ, reply.code)?;
        if reply.data_containers == 0 {
            return Err(PtpUsbError::NoDataPhase { op: OP_VENDOR_PROP_READ });
        }
        Ok(VendorRead { data: reply.data, warnings })
    }

    /// Send `0x944C(target_op, 0)` and report the answer as it came; see
    /// [`OP_VENDOR_OP_QUERY`] for what is and is not known about it. **Not yet
    /// run over USB on any camera**: this module has never sent `0x944C`, so
    /// the first runs are the test, and an unfamiliar `target_op` is a guess
    /// at the parameter (a stalled pipe is the failure seen so far from a
    /// wrong parameter on other ops, and the session recovers from it). It
    /// sends nothing but that one command: no data-out phase, no setup step.
    pub fn query_vendor_op(&mut self, target_op: u16) -> Result<VendorOpQuery, PtpUsbError> {
        let mut warnings = Vec::new();
        if sdk_model_name(self.model()).is_none() {
            warnings.push(format!(
                "camera model {:?} is not one of the surveyed models ({:?}); 0x944C was only captured on a Z 9",
                self.model(),
                KNOWN_MODELS
            ));
        }
        let reply = self.transact(
            OP_VENDOR_OP_QUERY,
            &[u32::from(target_op), VENDOR_OP_QUERY_PARAM2],
            None,
            TxidCheck::Unchecked,
        )?;
        Ok(VendorOpQuery {
            target_op,
            response_code: reply.code,
            data: reply.data,
            data_containers: reply.data_containers,
            reply_matched_command: reply.deviations.is_empty(),
            warnings,
        })
    }

    /// Read the DeviceInfo dataset (`0x1001`, standard and read-only). Same
    /// checks as a vendor read: a non-OK answer is [`PtpUsbError::OperationFailed`], an OK
    /// answer with no data container at all is [`PtpUsbError::NoDataPhase`], and the dataset is
    /// parsed strictly ([`parse_device_info`]).
    pub fn get_device_info(&mut self) -> Result<DeviceInfoDataset, PtpUsbError> {
        let reply = self.transact(OP_GET_DEVICE_INFO, &[], None, TxidCheck::Enforce)?;
        Self::require_ok(OP_GET_DEVICE_INFO, reply.code)?;
        if reply.data_containers == 0 {
            return Err(PtpUsbError::NoDataPhase { op: OP_GET_DEVICE_INFO });
        }
        parse_device_info(&reply.data)
    }

    /// [`Self::get_device_info`] with the identity the USB lookup read for this body.
    pub fn read_device_info_report(&mut self) -> Result<DeviceInfoReport, PtpUsbError> {
        let dataset = self.get_device_info()?;
        Ok(DeviceInfoReport {
            dataset,
            usb_model: self.model.clone(),
            usb_serial: self.usb_serial.clone(),
            usb_warnings: self.usb_warnings.clone(),
        })
    }

    /// Write an FTP profile via `0x90EE`, with the captured
    /// [`FtpCameraNetwork`]. See [`Self::write_ftp_profile_with`].
    pub fn write_ftp_profile(
        &mut self,
        profile: &FtpProfile,
        force_unconfirmed_model: bool,
    ) -> Result<FtpWriteReceipt, PtpUsbError> {
        self.write_ftp_profile_with(profile, &FtpCameraNetwork::CAPTURED, force_unconfirmed_model)
    }

    /// Write an FTP profile via `0x90EE`. Always sends the `0x90E8` setup
    /// call immediately before it — the first write in a fresh session
    /// doesn't strictly need this, but a second one hangs without it
    /// (confirmed 2026-09-06), and sending it unconditionally is simpler and
    /// safer than tracking write count.
    ///
    /// Refuses to write to a camera model outside [`FTP_WRITE_CONFIRMED_MODELS`]
    /// unless `force_unconfirmed_model` is set — see the module doc and
    /// [`PtpUsbError::UnconfirmedModel`].
    ///
    /// A failure after the `0x90EE` data went out (the reply never came, or
    /// was garbled) is [`PtpUsbError::FtpWriteIndeterminate`], not the bare
    /// cause: the camera may have taken the profile, and it cannot be read
    /// back. A camera that *answers* with a non-OK code is a definite
    /// refusal ([`PtpUsbError::OperationFailed`]). `Ok` carries a
    /// [`FtpWriteReceipt`] — see its doc for how little "OK" proves.
    pub fn write_ftp_profile_with(
        &mut self,
        profile: &FtpProfile,
        network: &FtpCameraNetwork,
        force_unconfirmed_model: bool,
    ) -> Result<FtpWriteReceipt, PtpUsbError> {
        if !force_unconfirmed_model && !ftp_write_confirmed(&self.model) {
            return Err(PtpUsbError::UnconfirmedModel(self.model.clone()));
        }
        let blob = encode_ftp_profile_with(profile, network)?;
        self.send_ftp_blob(FTP_PROFILE_WRITE_PARAMS, &blob)
    }

    /// `0x90E8` setup, then `0x90EE(params)` with `blob` as its data-out
    /// phase. Shared by the encoder-built write and the raw-blob write.
    fn send_ftp_blob(&mut self, params: [u32; 2], blob: &[u8]) -> Result<FtpWriteReceipt, PtpUsbError> {
        let setup = self.transact(OP_FTP_SETUP, &[], Some(&FTP_SETUP_DATA), TxidCheck::Unchecked)?;
        Self::require_ok(OP_FTP_SETUP, setup.code)?;

        let reply = match self.transact(OP_FTP_PROFILE_WRITE, &params, Some(blob), TxidCheck::Unchecked) {
            Ok(reply) => reply,
            Err(e) if self.data_phase_started => return Err(PtpUsbError::FtpWriteIndeterminate { cause: Box::new(e) }),
            Err(e) => return Err(e),
        };
        Self::require_ok(OP_FTP_PROFILE_WRITE, reply.code)?;
        let reply_matched_command = setup.deviations.is_empty() && reply.deviations.is_empty();
        Ok(FtpWriteReceipt { response_params: reply.params, reply_matched_command })
    }

    /// Write `blob` (a profile in the `0x90ED`/`0x90EE` layout, for instance
    /// one read from another camera) with `0x90EE(slot, param2)`, unchanged.
    /// The blob must decode ([`decode_ftp_profile`]), so a garbled one never
    /// reaches the camera. **What the two parameters mean is partly unknown**:
    /// every capture used `[0, 0]`, and on a Z 9 `[0, 0]` replaced the only
    /// stored profile (slot 0) instead of adding one. Same model gate as
    /// [`Self::write_ftp_profile_with`].
    pub fn write_ftp_profile_blob(
        &mut self,
        slot: u32,
        param2: u32,
        blob: &[u8],
        force_unconfirmed_model: bool,
    ) -> Result<FtpWriteReceipt, PtpUsbError> {
        if !force_unconfirmed_model && !ftp_write_confirmed(&self.model) {
            return Err(PtpUsbError::UnconfirmedModel(self.model.clone()));
        }
        decode_ftp_profile(blob)?;
        self.send_ftp_blob([slot, param2], blob)
    }

    /// Read the camera's FTP profile: `0x90E8` (the same fixed 3-byte setup
    /// the write sends) → `0x90EB` (info; says whether a profile exists and
    /// in which slot) → `0x90ED(slot)` (the profile blob). That is NX
    /// Field's own sequence, as captured; **it has not yet been run over USB
    /// on a camera**, so the first run is the test of this method. Nothing
    /// in it writes a profile.
    ///
    /// When `0x90EB` reports no active profile (slot `0xff`), `0x90ED` is not sent
    /// and the readout's `profile` is `None`. A blob that the decoder does
    /// not recognise is still returned (raw bytes plus the decode error),
    /// since the bytes are the finding.
    pub fn read_ftp_profile(&mut self) -> Result<FtpProfileReadout, PtpUsbError> {
        self.read_ftp_profile_slot(None)
    }

    /// [`Self::read_ftp_profile`] reading `slot` instead of the active one
    /// (`None` = the active one). `0x90ED(n)` is a read: the camera answers
    /// `0x201F` (TransactionCancelled) for a slot it holds nothing in, which
    /// comes back as [`PtpUsbError::OperationFailed`] (not seen over USB). A slot at or above the
    /// stored `count` `0x90EB` reports is refused before `0x90ED` is sent: on
    /// a Z 9 with count 3, `0x90ED(3)` stalled the USB pipe (clear_halt
    /// recovered it) instead of answering `0x201F`. Slots `0..count` are the
    /// stored profiles, in an order that is not the menu's active one (the
    /// slot `0x90EB` reports stayed `0` across two different active profiles).
    pub fn read_ftp_profile_slot(&mut self, slot: Option<u32>) -> Result<FtpProfileReadout, PtpUsbError> {
        let mut warnings = Vec::new();
        if sdk_model_name(self.model()).is_none() {
            warnings.push(format!(
                "camera model {:?} is not one of the surveyed models ({:?}); the FTP-profile read was only captured on a Z 9",
                self.model(),
                KNOWN_MODELS
            ));
        }
        let setup = self.transact(OP_FTP_SETUP, &[], Some(&FTP_SETUP_DATA), TxidCheck::Unchecked)?;
        Self::require_ok(OP_FTP_SETUP, setup.code)?;

        let info_reply = self.transact(OP_FTP_PROFILE_INFO, &[], None, TxidCheck::Unchecked)?;
        Self::require_ok(OP_FTP_PROFILE_INFO, info_reply.code)?;
        if info_reply.data_containers == 0 {
            return Err(PtpUsbError::NoDataPhase { op: OP_FTP_PROFILE_INFO });
        }
        let info = parse_ftp_profile_info(&info_reply.data)?;
        let mut matched = setup.deviations.is_empty() && info_reply.deviations.is_empty();

        let wanted = match slot {
            Some(n) if n >= info.count => return Err(PtpUsbError::FtpSlotOutOfRange { slot: n, count: info.count }),
            Some(n) => Some(n),
            None => info.has_active_profile().then_some(info.slot),
        };
        let profile = if let Some(wanted) = wanted {
            let reply = self.transact(OP_FTP_PROFILE_READ, &[wanted], None, TxidCheck::Unchecked)?;
            Self::require_ok(OP_FTP_PROFILE_READ, reply.code)?;
            if reply.data_containers == 0 {
                return Err(PtpUsbError::NoDataPhase { op: OP_FTP_PROFILE_READ });
            }
            matched &= reply.deviations.is_empty();
            let decoded = decode_ftp_profile(&reply.data);
            Some(FtpProfileBlob { raw: reply.data, decoded })
        } else {
            None
        };
        Ok(FtpProfileReadout { info, profile, warnings, reply_matched_command: matched })
    }
}

impl<T: BulkTransport> Drop for PtpUsbSession<T> {
    fn drop(&mut self) {
        // A poisoned session sends no CloseSession (`transact` refuses it
        // before touching the pipe, which may be stalled and would block for
        // the whole timeout). The next process's OpenSession sees "already
        // open" and takes the retry path. Best-effort teardown: the close's
        // result is checked inside `close_session` and has no caller left to
        // report to.
        if self.session_open {
            let _ = self.close_session(CLOSE_TIMEOUT);
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

/// [`read_vendor_property`], returning the warnings instead of printing them.
pub fn read_vendor_property_report(serial: Option<&str>, propcode: u16) -> Result<VendorRead, PtpUsbError> {
    let mut session = PtpUsbSession::open(serial)?;
    session.read_vendor_property_report(propcode)
}

/// [`read_vendor_property_report`] for a caller already holding the [`CameraLock`]; see
/// [`PtpUsbSession::open_locked`].
pub fn read_vendor_property_report_locked(
    lock: CameraLock,
    serial: Option<&str>,
    propcode: u16,
) -> Result<VendorRead, PtpUsbError> {
    let mut session = PtpUsbSession::open_locked(lock, serial)?;
    session.read_vendor_property_report(propcode)
}

/// Open a session, read the DeviceInfo dataset (`0x1001`), close it. Takes the [`CameraLock`]
/// first, like every session. Read-only.
pub fn read_device_info(serial: Option<&str>) -> Result<DeviceInfoReport, PtpUsbError> {
    let mut session = PtpUsbSession::open(serial)?;
    session.read_device_info_report()
}

/// [`read_device_info`] for a caller already holding the [`CameraLock`]; see
/// [`PtpUsbSession::open_locked`].
pub fn read_device_info_locked(lock: CameraLock, serial: Option<&str>) -> Result<DeviceInfoReport, PtpUsbError> {
    let mut session = PtpUsbSession::open_locked(lock, serial)?;
    session.read_device_info_report()
}

/// Open a session, write an FTP profile via `0x90EE`, close it. Convenience
/// wrapper for one-off writes (e.g. the CLI). See
/// [`PtpUsbSession::write_ftp_profile`] for `force_unconfirmed_model` and
/// [`FtpWriteReceipt`] for what success means.
pub fn write_ftp_profile(
    serial: Option<&str>,
    profile: &FtpProfile,
    force_unconfirmed_model: bool,
) -> Result<FtpWriteReceipt, PtpUsbError> {
    let mut session = PtpUsbSession::open(serial)?;
    session.write_ftp_profile(profile, force_unconfirmed_model)
}

/// [`write_ftp_profile`] for a caller already holding the [`CameraLock`]; see
/// [`PtpUsbSession::open_locked`].
pub fn write_ftp_profile_locked(
    lock: CameraLock,
    serial: Option<&str>,
    profile: &FtpProfile,
    force_unconfirmed_model: bool,
) -> Result<FtpWriteReceipt, PtpUsbError> {
    let mut session = PtpUsbSession::open_locked(lock, serial)?;
    session.write_ftp_profile(profile, force_unconfirmed_model)
}

/// [`PtpUsbSession::write_ftp_profile_blob`] for a caller already holding the
/// [`CameraLock`].
pub fn write_ftp_profile_blob_locked(
    lock: CameraLock,
    serial: Option<&str>,
    slot: u32,
    param2: u32,
    blob: &[u8],
    force_unconfirmed_model: bool,
) -> Result<FtpWriteReceipt, PtpUsbError> {
    let mut session = PtpUsbSession::open_locked(lock, serial)?;
    session.write_ftp_profile_blob(slot, param2, blob, force_unconfirmed_model)
}

/// [`PtpUsbSession::read_ftp_profile`] for a caller already holding the
/// [`CameraLock`]; see [`PtpUsbSession::open_locked`]. Read-only.
pub fn read_ftp_profile_locked(
    lock: CameraLock,
    serial: Option<&str>,
    slot: Option<u32>,
) -> Result<FtpProfileReadout, PtpUsbError> {
    let mut session = PtpUsbSession::open_locked(lock, serial)?;
    session.read_ftp_profile_slot(slot)
}

/// [`PtpUsbSession::query_vendor_op`] for a caller already holding the
/// [`CameraLock`]; see [`PtpUsbSession::open_locked`]. One query per session.
pub fn query_vendor_op_locked(
    lock: CameraLock,
    serial: Option<&str>,
    target_op: u16,
) -> Result<VendorOpQuery, PtpUsbError> {
    let mut session = PtpUsbSession::open_locked(lock, serial)?;
    session.query_vendor_op(target_op)
}

/// Format `data` as lowercase space-separated hex byte pairs, e.g.
/// `"de ad be ef"`. Shared by the CLI and the egui GUI (previously defined
/// identically in both `src/main.rs` and `gui/src/main.rs` despite both
/// already depending on this crate) so there's one definition, not two that
/// could silently diverge.
pub fn hex_bytes(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// Which required FTP-profile fields are empty, as display names for a
/// "missing field(s)" message — checked before a caller GUI has enough to
/// construct an actual [`FtpProfile`]. Which fields are trimmed first, and
/// what each is called, come from [`FTP_PROFILE_FIELDS`] (the password is
/// checked as typed). Centralizes this list so the egui GUI doesn't
/// hand-copy it.
pub fn ftp_profile_missing_fields<'a>(
    profile_name: &str,
    ssid_24ghz: &str,
    ssid_5ghz: &str,
    host: &str,
    username: &str,
    password: &str,
) -> Vec<&'a str> {
    let values = [profile_name, ssid_24ghz, ssid_5ghz, host, username, password];
    FtpField::ALL
        .iter()
        .zip(values)
        .filter(|(field, value)| {
            let spec = field.spec();
            if spec.trimmed_for_required_check { value.trim().is_empty() } else { value.is_empty() }
        })
        .map(|(field, _)| field.spec().display_name)
        .collect()
}

/// Parse a vendor property code as `"0xD053"`/`"0XD053"` (hex) or a plain
/// decimal string. Shared by the CLI and both GUIs so the accepted input
/// format can't drift between them. Note `"5010"` is decimal 5010
/// (`0x1392`), not `0x5010`: a bare number is always decimal, hex needs the
/// prefix (and the CLI prints the resolved `0x…` code back).
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

    /// Test shorthand: an encoded container, which cannot fail for the small
    /// payloads used here.
    fn container(kind: u16, code: u16, transaction_id: u32, payload: &[u8]) -> Vec<u8> {
        encode_container(kind, code, transaction_id, payload).unwrap()
    }

    /// A header declaring `declared` bytes with kind Response, code OK, id 0.
    fn header(declared: u32) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.extend_from_slice(&declared.to_le_bytes());
        raw.extend_from_slice(&CONTAINER_RESPONSE.to_le_bytes());
        raw.extend_from_slice(&RESP_OK.to_le_bytes());
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw
    }

    // ── container framing ───────────────────────────────────────────────

    #[test]
    fn encode_command_no_params() {
        let buf = encode_command(0x1001, 0, &[]).unwrap();
        assert_eq!(buf, vec![12, 0, 0, 0, 1, 0, 0x01, 0x10, 0, 0, 0, 0]);
    }

    #[test]
    fn encode_command_one_param() {
        // 0x943B with param 0x1_D053 (0x10000 | 0xD053), transaction id 1.
        let buf = encode_command(0x943B, 1, &[0x1D053]).unwrap();
        assert_eq!(buf.len(), 16);
        assert_eq!(&buf[0..4], &16u32.to_le_bytes());
        assert_eq!(&buf[4..6], &CONTAINER_COMMAND.to_le_bytes());
        assert_eq!(&buf[6..8], &0x943Bu16.to_le_bytes());
        assert_eq!(&buf[8..12], &1u32.to_le_bytes());
        assert_eq!(&buf[12..16], &0x1D053u32.to_le_bytes());
    }

    #[test]
    fn encode_command_param_count_at_below_above_the_limit() {
        assert_eq!(MAX_COMMAND_PARAMS, 5);
        assert_eq!(encode_command(0x1001, 0, &[1, 2, 3, 4]).unwrap().len(), 12 + 16);
        assert_eq!(encode_command(0x1001, 0, &[1, 2, 3, 4, 5]).unwrap().len(), 12 + 20);
        let err = encode_command(0x1001, 0, &[1, 2, 3, 4, 5, 6]).unwrap_err();
        assert!(matches!(err, PtpUsbError::TooManyCommandParams { given: 6, max: 5 }), "{err:?}");
    }

    #[test]
    fn encode_data_roundtrips_payload() {
        let buf = encode_data(0x9413, 2, &[0xAA, 0xBB, 0xCC]).unwrap();
        assert_eq!(buf.len(), 15);
        assert_eq!(&buf[0..4], &15u32.to_le_bytes());
        assert_eq!(&buf[4..6], &CONTAINER_DATA.to_le_bytes());
        assert_eq!(&buf[12..], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn container_length_at_below_above_the_32_bit_limit() {
        // The length field is a u32 holding header + payload.
        let max_payload = u32::MAX as usize - HEADER_LEN;
        assert_eq!(container_length(max_payload).unwrap(), u32::MAX);
        assert_eq!(container_length(max_payload - 1).unwrap(), u32::MAX - 1);
        let err = container_length(max_payload + 1).unwrap_err();
        assert!(matches!(err, PtpUsbError::ContainerTooLarge { payload_len } if payload_len == max_payload + 1), "{err:?}");
        assert_eq!(container_length(0).unwrap(), 12);
    }

    #[test]
    fn parse_container_rejects_short_input() {
        let err = parse_container(&[0, 0, 0]).unwrap_err();
        assert!(matches!(err, PtpUsbError::ShortContainer(3)));
        // One byte under the header is still short; exactly the header is not.
        let eleven = &header(12)[..11];
        assert!(matches!(parse_container(eleven).unwrap_err(), PtpUsbError::ShortContainer(11)));
    }

    #[test]
    fn parse_container_declared_length_at_below_above_the_header_size() {
        // Declared 11: below the header, must error rather than panic on the slice.
        let err = parse_container(&header(11)).unwrap_err();
        assert!(matches!(err, PtpUsbError::ShortContainer(11)), "{err:?}");
        // Declared 12 with exactly 12 bytes: a normal zero-payload response.
        let (c, consumed) = parse_container(&header(12)).unwrap();
        assert!(c.payload.is_empty());
        assert_eq!(consumed, 12);
        // Declared 13 but only the 12 header bytes present: truncated, not ok.
        let err = parse_container(&header(13)).unwrap_err();
        assert!(matches!(err, PtpUsbError::TruncatedContainer { declared: 13, got: 12 }), "{err:?}");
        // Declared 0 (and 5): below the floor too.
        assert!(matches!(parse_container(&header(0)).unwrap_err(), PtpUsbError::ShortContainer(0)));
        assert!(matches!(parse_container(&header(5)).unwrap_err(), PtpUsbError::ShortContainer(5)));
    }

    #[test]
    fn parse_container_errors_instead_of_silently_truncating_short_buffer() {
        // Declared length exceeds what's actually in the buffer (e.g. a
        // >16KB response that needed more than one bulk read) — must error,
        // not silently hand back a truncated payload that would desync the
        // next container parse.
        let mut raw = header(100);
        raw.extend_from_slice(&[0xAB; 10]); // only 10 payload bytes actually present, not 88
        let err = parse_container(&raw).unwrap_err();
        assert!(matches!(err, PtpUsbError::TruncatedContainer { declared: 100, got: 22 }));
    }

    #[test]
    fn parse_container_response_with_one_param() {
        // A minimal Response container: OK (0x2001), txn=1, one param (0x2A).
        let raw = container(CONTAINER_RESPONSE, RESP_OK, 1, &0x2Au32.to_le_bytes());
        let (c, consumed) = parse_container(&raw).unwrap();
        assert_eq!(consumed, 16);
        assert_eq!(c.kind, CONTAINER_RESPONSE);
        assert_eq!(c.code, RESP_OK);
        assert_eq!(c.transaction_id, 1);
        assert_eq!(decode_response_params(&c.payload).unwrap(), vec![0x2A]);
    }

    #[test]
    fn parse_container_reports_the_declared_length_and_leaves_trailing_bytes() {
        // Bytes past the declared length are the NEXT container's (a single
        // bulk read can capture more than one): the payload stops at the
        // declared length and `consumed` tells the caller what to keep.
        let mut raw = header(12);
        raw.extend_from_slice(&[0xFF; 8]);
        let (c, consumed) = parse_container(&raw).unwrap();
        assert!(c.payload.is_empty());
        assert_eq!(consumed, 12);
        assert_eq!(raw.len() - consumed, 8);
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

    // ── transaction ids, deadlines, zero-length packets ─────────────────

    #[test]
    fn next_transaction_id_counts_up_and_wraps_past_the_reserved_ids() {
        assert_eq!(next_transaction_id(0), 1);
        assert_eq!(next_transaction_id(5), 6);
        assert_eq!(next_transaction_id(0xFFFF_FFFD), 0xFFFF_FFFE);
        // 0xFFFFFFFF and 0 are reserved: the last usable id wraps to 1.
        assert_eq!(next_transaction_id(0xFFFF_FFFE), 1);
        assert_eq!(next_transaction_id(0xFFFF_FFFF), 1);
    }

    #[test]
    fn remaining_until_at_below_above_the_deadline() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(100);
        assert_eq!(remaining_until(deadline, now), Some(Duration::from_millis(100)));
        assert_eq!(remaining_until(deadline, deadline - Duration::from_nanos(1)), Some(Duration::from_nanos(1)));
        assert_eq!(remaining_until(deadline, deadline), None, "reaching the deadline is expiry");
        assert_eq!(remaining_until(deadline, deadline + Duration::from_nanos(1)), None);
    }

    #[test]
    fn bulk_read_timeout_is_clamped_to_one_millisecond_and_the_bulk_ceiling() {
        // Below 1 ms rusb would pass 0 to libusb, which means "wait forever".
        assert_eq!(bulk_read_timeout(Duration::from_micros(100)), Duration::from_millis(1));
        assert_eq!(bulk_read_timeout(Duration::from_millis(1)), Duration::from_millis(1));
        assert_eq!(bulk_read_timeout(Duration::from_millis(2)), Duration::from_millis(2));
        assert_eq!(bulk_read_timeout(Duration::from_millis(4999)), Duration::from_millis(4999));
        assert_eq!(bulk_read_timeout(Duration::from_secs(5)), Duration::from_secs(5));
        assert_eq!(bulk_read_timeout(Duration::from_millis(5001)), Duration::from_secs(5));
        assert_eq!(bulk_read_timeout(Duration::from_secs(30)), Duration::from_secs(5));
    }

    #[test]
    fn zero_length_packet_needed_exactly_at_multiples_of_the_packet_size() {
        assert!(!needs_zero_length_packet(511, 512));
        assert!(needs_zero_length_packet(512, 512));
        assert!(!needs_zero_length_packet(513, 512));
        assert!(needs_zero_length_packet(1024, 512));
        assert!(!needs_zero_length_packet(1536 + 1, 1024));
        assert!(needs_zero_length_packet(1024, 1024));
        assert!(!needs_zero_length_packet(512, 1024), "512 is a short packet on a 1024-byte endpoint");
        // Unknown packet size: never guess one.
        assert!(!needs_zero_length_packet(512, 0));
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
        // this encoder -- it has an extra PTP string (content "Claude",
        // probably the destination folder; the decoder calls it `folder`)
        // between host and username, and flag byte 01 instead of 05 before
        // the port. `decode_331_blob_finds_the_folder_the_encoder_cannot_write`
        // pins that down. So "byte for byte" holds for two of the three captures.
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
    fn encode_ftp_profile_port_is_little_endian() {
        let profile = valid_profile();
        let blob = encode_ftp_profile(&profile).unwrap();
        // Port section starts right after password's content in this profile.
        let port_tag_pos = blob.windows(2).rposition(|w| w == FTP_TAG_PORT_SECTION).unwrap();
        let port_bytes = &blob[port_tag_pos + 2..port_tag_pos + 4];
        assert_eq!(u16::from_le_bytes(port_bytes.try_into().unwrap()), profile.port);
    }

    // The 82-byte template block (finding #04): what every capture carried,
    // decoded, and what changes when the address part is a parameter.

    /// The 82 bytes of the reference capture between the profile name
    /// (offsets 2..17) and the SSID section, straight from the hex above.
    fn captured_template_block() -> Vec<u8> {
        hex::decode_hex(reference_ftp_blob_hex())[17..17 + FTP_TEMPLATE_LEN].to_vec()
    }

    #[test]
    fn template_block_is_82_bytes_and_equals_the_captured_bytes() {
        let block = encode_template_block(&FtpCameraNetwork::CAPTURED);
        assert_eq!(block.len(), FTP_TEMPLATE_LEN);
        assert_eq!(block, captured_template_block());
    }

    #[test]
    fn captured_template_block_decodes_to_the_capture_rigs_address_and_prefix() {
        // The pin on finding #04: the bytes shipped to every camera are
        // 192.168.66.87/24 — the Z 9's address on the original test rig
        // (docs/nx-field-session-2026-07-09.md line 8) — not an opaque constant.
        let block = captured_template_block();
        // Two 33-byte placeholder strings, then tag 02 03 03, address, prefix, 8 zero bytes.
        assert_eq!(block[0], 16, "length prefix of the first placeholder string");
        assert_eq!(block[33], 16, "length prefix of the second placeholder string");
        assert_eq!(&block[66..69], &[0x02, 0x03, 0x03]);
        assert_eq!(&block[69..73], &[0x57, 0x42, 0xa8, 0xc0]);
        assert_eq!(u32::from_le_bytes(block[69..73].try_into().unwrap()), 0xC0A8_4257);
        assert_eq!(std::net::Ipv4Addr::from(0xC0A8_4257u32).octets(), [192, 168, 66, 87]);
        assert_eq!(block[73], 24);
        assert_eq!(&block[74..82], &[0u8; 8]);
        assert_eq!(FtpCameraNetwork::CAPTURED, FtpCameraNetwork { ipv4: [192, 168, 66, 87], prefix_len: 24 });
    }

    #[test]
    fn template_block_placeholders_decode_to_the_zero_timestamp_string() {
        let block = captured_template_block();
        for start in [0usize, 33] {
            let units: Vec<u16> = block[start + 1..start + 31].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            assert_eq!(String::from_utf16(&units).unwrap(), "00000000T000000");
            assert_eq!(&block[start + 31..start + 33], &[0, 0], "null terminator");
        }
    }

    #[test]
    fn a_different_camera_network_changes_only_the_address_and_prefix_bytes() {
        let default = encode_ftp_profile(&valid_profile()).unwrap();
        let other = FtpCameraNetwork { ipv4: [10, 1, 2, 3], prefix_len: 16 };
        let custom = encode_ftp_profile_with(&valid_profile(), &other).unwrap();
        assert_eq!(default.len(), custom.len());
        // Template block starts after the header (2) and the profile name ("TEST": 1 + 5 * 2 bytes).
        let block_at = 2 + 1 + 5 * 2;
        let diff: Vec<usize> = (0..default.len()).filter(|&i| default[i] != custom[i]).collect();
        assert_eq!(diff, vec![block_at + 69, block_at + 70, block_at + 71, block_at + 72, block_at + 73]);
        assert_eq!(&custom[block_at + 69..block_at + 73], &[3, 2, 1, 10], "address bytes are little-endian");
        assert_eq!(custom[block_at + 73], 16);
    }

    #[test]
    fn encode_ftp_profile_is_encode_with_the_captured_network() {
        let p = valid_profile();
        assert_eq!(encode_ftp_profile(&p).unwrap(), encode_ftp_profile_with(&p, &FtpCameraNetwork::CAPTURED).unwrap());
    }

    // ── per-field specs and limits ──────────────────────────────────────

    #[test]
    fn field_specs_are_in_wire_order_with_literal_names_and_limits() {
        // Pinned by literal values, not by the constants the code uses: a
        // retuned limit must be a deliberate edit of this test.
        let table: Vec<(&str, &str, usize, FtpLenUnit, bool)> = FTP_PROFILE_FIELDS
            .iter()
            .map(|s| (s.key, s.display_name, s.max_len, s.unit(), s.trimmed_for_required_check))
            .collect();
        assert_eq!(
            table,
            vec![
                ("profile_name", "profile name", 253, FtpLenUnit::Utf16Units, true),
                ("ssid_24ghz", "SSID (2.4GHz)", 32, FtpLenUnit::Bytes, true),
                ("ssid_5ghz", "SSID (5GHz)", 32, FtpLenUnit::Bytes, true),
                ("host", "host", 199, FtpLenUnit::Utf16Units, true),
                ("username", "username", 64, FtpLenUnit::Bytes, true),
                ("password", "password", 64, FtpLenUnit::Bytes, false),
            ]
        );
        for (i, field) in FtpField::ALL.iter().enumerate() {
            assert_eq!(FTP_PROFILE_FIELDS[i].field, *field, "FtpField::spec indexes by discriminant");
            assert_eq!(field.spec().field, *field);
        }
        // A UTF-16 field's length prefix is one byte holding content + null.
        for spec in &FTP_PROFILE_FIELDS {
            if spec.encoding == FtpEncoding::Utf16U8Len {
                assert!(spec.max_len < usize::from(u8::MAX), "{}", spec.key);
            }
        }
    }

    fn profile_with(field: FtpField, value: &str) -> FtpProfile {
        let mut p = valid_profile();
        match field {
            FtpField::ProfileName => p.profile_name = value.into(),
            FtpField::Ssid24Ghz => p.ssid_24ghz = value.into(),
            FtpField::Ssid5Ghz => p.ssid_5ghz = value.into(),
            FtpField::Host => p.host = value.into(),
            FtpField::Username => p.username = value.into(),
            FtpField::Password => p.password = value.into(),
        }
        p
    }

    #[test]
    fn every_field_accepts_its_limit_and_rejects_one_over_with_its_own_unit() {
        // (field, literal limit, literal unit label): at, below and above.
        let cases = [
            (FtpField::ProfileName, 253usize, "profile_name", "UTF-16 units"),
            (FtpField::Ssid24Ghz, 32, "ssid_24ghz", "bytes"),
            (FtpField::Ssid5Ghz, 32, "ssid_5ghz", "bytes"),
            (FtpField::Host, 199, "host", "UTF-16 units"),
            (FtpField::Username, 64, "username", "bytes"),
            (FtpField::Password, 64, "password", "bytes"),
        ];
        for (field, limit, key, unit) in cases {
            for ok_len in [0, 1, limit - 1, limit] {
                assert!(encode_ftp_profile(&profile_with(field, &"a".repeat(ok_len))).is_ok(), "{key} at {ok_len}");
            }
            let err = encode_ftp_profile(&profile_with(field, &"a".repeat(limit + 1))).unwrap_err();
            assert!(
                matches!(&err, PtpUsbError::FtpFieldTooLong { field: f, len, max, unit: u }
                    if *f == key && *len == limit + 1 && *max == limit && *u == unit),
                "{key}: {err:?}"
            );
            let err = encode_ftp_profile(&profile_with(field, &"a".repeat(limit + 100))).unwrap_err();
            assert!(matches!(&err, PtpUsbError::FtpFieldTooLong { field: f, .. } if *f == key), "{key}: {err:?}");
        }
    }

    #[test]
    fn too_long_message_names_the_unit_the_limit_is_in() {
        let err = encode_ftp_profile(&profile_with(FtpField::Host, &"h".repeat(200))).unwrap_err();
        assert_eq!(err.to_string(), "FTP profile field host is 200 UTF-16 units, exceeds the 199-UTF-16 units limit");
        let err = encode_ftp_profile(&profile_with(FtpField::Ssid5Ghz, &"s".repeat(33))).unwrap_err();
        assert_eq!(err.to_string(), "FTP profile field ssid_5ghz is 33 bytes, exceeds the 32-bytes limit");
    }

    #[test]
    fn host_limit_counts_utf16_units_not_bytes_or_chars() {
        // 'é' is 2 UTF-8 bytes and 1 UTF-16 unit; U+1F600 is 4 bytes and 2 units.
        assert!(encode_ftp_profile(&profile_with(FtpField::Host, &"é".repeat(199))).is_ok());
        assert!(encode_ftp_profile(&profile_with(FtpField::Host, &"é".repeat(200))).is_err());
        // 99 emoji = 198 units: fine; plus one 'a' = 199: at the limit; plus another: over.
        let at = format!("{}a", "\u{1F600}".repeat(99));
        assert!(encode_ftp_profile(&profile_with(FtpField::Host, &at)).is_ok());
        let over = format!("{at}a");
        let err = encode_ftp_profile(&profile_with(FtpField::Host, &over)).unwrap_err();
        assert!(matches!(err, PtpUsbError::FtpFieldTooLong { len: 200, max: 199, .. }), "{err:?}");
    }

    #[test]
    fn utf16_fields_accept_non_ascii_but_ascii_fields_never_do() {
        // profile name and host go out as UTF-16: a non-ASCII character is encodable.
        assert!(encode_ftp_profile(&profile_with(FtpField::ProfileName, "Kamera é")).is_ok());
        assert!(encode_ftp_profile(&profile_with(FtpField::Host, "ünï.example")).is_ok());
        for (field, key) in [
            (FtpField::Ssid24Ghz, "ssid_24ghz"),
            (FtpField::Ssid5Ghz, "ssid_5ghz"),
            (FtpField::Username, "username"),
            (FtpField::Password, "password"),
        ] {
            for bad in ["flëet", "é", "日本語", "ok\u{1F600}"] {
                let err = encode_ftp_profile(&profile_with(field, bad)).unwrap_err();
                assert!(matches!(&err, PtpUsbError::FtpFieldNotAscii { field: f } if *f == key), "{key} {bad:?}: {err:?}");
            }
        }
    }

    #[test]
    fn every_field_rejects_control_characters() {
        for field in FtpField::ALL {
            let key = field.spec().key;
            for bad in ["a\0b", "\0", "a\nb", "tab\there", "x\r", "del\u{7f}", "esc\u{1b}"] {
                let err = encode_ftp_profile(&profile_with(field, bad)).unwrap_err();
                assert!(matches!(&err, PtpUsbError::FtpFieldControlChar { field: f } if *f == key), "{key} {bad:?}: {err:?}");
            }
        }
        // C1 controls on the UTF-16 fields; a plain space is fine everywhere.
        assert!(matches!(
            encode_ftp_profile(&profile_with(FtpField::Host, "a\u{85}b")),
            Err(PtpUsbError::FtpFieldControlChar { field: "host" })
        ));
        for field in FtpField::ALL {
            assert!(encode_ftp_profile(&profile_with(field, "a b")).is_ok(), "{:?}", field);
        }
    }

    #[test]
    fn a_non_ascii_character_wins_over_the_length_error_and_control_over_length() {
        // Order is non-ASCII, then control, then length: 40 'é' in an SSID is a NotAscii, not TooLong.
        let err = encode_ftp_profile(&profile_with(FtpField::Ssid24Ghz, &"é".repeat(40))).unwrap_err();
        assert!(matches!(err, PtpUsbError::FtpFieldNotAscii { field: "ssid_24ghz" }), "{err:?}");
        let err = encode_ftp_profile(&profile_with(FtpField::Ssid24Ghz, &format!("\0{}", "a".repeat(40)))).unwrap_err();
        assert!(matches!(err, PtpUsbError::FtpFieldControlChar { field: "ssid_24ghz" }), "{err:?}");
    }

    #[test]
    fn the_u8_length_prefix_guard_fires_when_a_spec_allows_more_than_it_can_hold() {
        // No spec in the table can reach this (max 253 content units), so it
        // needs a hand-made one: content 254 units = prefix 255 = u8::MAX
        // fits; 255 units = prefix 256 does not and must not truncate.
        let spec = FtpFieldSpec {
            field: FtpField::Host,
            key: "test",
            display_name: "test",
            encoding: FtpEncoding::Utf16U8Len,
            max_len: 1000,
            trimmed_for_required_check: true,
        };
        let ok = encode_field_with(&spec, &"a".repeat(254)).unwrap();
        assert_eq!(ok[0], 255);
        assert_eq!(ok.len(), 1 + 255 * 2);
        let err = encode_field_with(&spec, &"a".repeat(255)).unwrap_err();
        assert!(matches!(err, PtpUsbError::FtpFieldTooLong { field: "test", len: 255, max: 254, .. }), "{err:?}");
    }

    #[test]
    fn empty_fields_still_encode() {
        // Required-ness is ftp_profile_missing_fields' job, not the encoder's.
        let p = FtpProfile {
            profile_name: String::new(),
            ssid_24ghz: String::new(),
            ssid_5ghz: String::new(),
            host: String::new(),
            port: 0,
            username: String::new(),
            password: String::new(),
        };
        assert!(encode_ftp_profile(&p).is_ok());
    }

    // ── ftp_profile_missing_fields ──────────────────────────────────────

    #[test]
    fn ftp_profile_missing_fields_all_present_is_empty() {
        assert!(ftp_profile_missing_fields("N", "s24", "s5", "h", "u", "p").is_empty());
    }

    #[test]
    fn ftp_profile_missing_fields_names_each_of_the_six_fields_when_it_alone_is_empty() {
        let full = ["N", "s24", "s5", "h", "u", "p"];
        let names = ["profile name", "SSID (2.4GHz)", "SSID (5GHz)", "host", "username", "password"];
        for i in 0..6 {
            let mut v = full;
            v[i] = "";
            assert_eq!(ftp_profile_missing_fields(v[0], v[1], v[2], v[3], v[4], v[5]), vec![names[i]], "field {i}");
        }
    }

    #[test]
    fn ftp_profile_missing_fields_reports_every_empty_field_in_order() {
        assert_eq!(ftp_profile_missing_fields("", "", "s5", "h", "u", "p"), vec!["profile name", "SSID (2.4GHz)"]);
        assert_eq!(
            ftp_profile_missing_fields("", "", "", "", "", ""),
            vec!["profile name", "SSID (2.4GHz)", "SSID (5GHz)", "host", "username", "password"]
        );
    }

    #[test]
    fn ftp_profile_missing_fields_whitespace_only_counts_as_empty_except_the_password() {
        // Matches the .trim().is_empty() convention used for every field
        // except password, whose spaces could be real characters.
        let full = ["N", "s24", "s5", "h", "u", "p"];
        let names = ["profile name", "SSID (2.4GHz)", "SSID (5GHz)", "host", "username"];
        for i in 0..5 {
            let mut v = full;
            v[i] = " \t ";
            assert_eq!(ftp_profile_missing_fields(v[0], v[1], v[2], v[3], v[4], v[5]), vec![names[i]], "field {i}");
        }
        assert!(ftp_profile_missing_fields("N", "s24", "s5", "h", "u", "   ").is_empty());
    }

    #[test]
    fn ftp_profile_missing_fields_names_come_from_the_spec_table() {
        let all_empty = ftp_profile_missing_fields("", "", "", "", "", "");
        let from_table: Vec<&str> = FTP_PROFILE_FIELDS.iter().map(|s| s.display_name).collect();
        assert_eq!(all_empty, from_table);
    }

    // ── with_password_redacted ──────────────────────────────────────────

    #[test]
    fn with_password_redacted_hides_password_content() {
        let profile = valid_profile();
        let redacted = profile.with_password_redacted();
        assert_ne!(redacted.password, profile.password);
        assert_eq!(redacted.password, "****");
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
    fn with_password_redacted_masks_spaces_and_punctuation_too() {
        let mut profile = valid_profile();
        profile.password = "pa ss--word!".into();
        assert_eq!(profile.with_password_redacted().password, "************");
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

    #[test]
    fn the_dry_run_preview_fails_exactly_where_the_real_write_fails() {
        // Finding #72: a preview that passes where the real encode fails.
        // Every password the real encoder rejects must be rejected by the
        // redacted copy with the same error; every one it accepts, accepted.
        let passwords = [
            "pass".to_string(),
            "x".repeat(64),
            "x".repeat(65),                // over the byte limit
            "é".repeat(40),                // 40 chars but 80 bytes, and non-ASCII
            "é".to_string(),               // non-ASCII, well under the limit
            "ok\u{1F600}".to_string(),     // a 4-byte character
            "tab\there".to_string(),       // control character
            format!("\0{}", "a".repeat(70)), // control and too long
            format!("é{}", "a".repeat(70)),  // non-ASCII and too long
        ];
        for pw in &passwords {
            let mut p = valid_profile();
            p.password = pw.clone();
            let real = encode_ftp_profile(&p);
            let preview = encode_ftp_profile(&p.with_password_redacted());
            match (real, preview) {
                (Ok(r), Ok(v)) => assert_eq!(r.len(), v.len(), "{pw:?}"),
                (Err(r), Err(v)) => assert_eq!(std::mem::discriminant(&r), std::mem::discriminant(&v), "{pw:?}: {r:?} vs {v:?}"),
                (r, v) => panic!("preview and real write disagree for {pw:?}: real {r:?}, preview {v:?}"),
            }
        }
    }

    #[test]
    fn a_redacted_non_ascii_password_leaks_nothing_but_its_class() {
        let mut p = valid_profile();
        p.password = "sécret".into();
        assert_eq!(p.with_password_redacted().password, "*\u{e9}****");
        p.password = "a\u{2}b".into();
        assert_eq!(p.with_password_redacted().password, "*\u{1}*");
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
        r.skip(FTP_TEMPLATE_LEN);
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
            ("X".repeat(50).leak(), "y".repeat(32).leak(), "z".repeat(30).leak(), "192.168.100.200", 21, "u".repeat(40).leak(), "p".repeat(55).leak()),
            ("é".repeat(100).leak(), "y", "z", "ünï.example", 21, "u", "p"),
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
    fn ftp_profile_round_trips_at_every_length_from_zero_to_each_fields_limit() {
        // Every length of every field, accept side: the boundary a `>` vs
        // `>=` mutant in the limit check would move.
        for field in FtpField::ALL {
            for len in 0..=field.spec().max_len {
                let mut profile = valid_profile();
                let value = "a".repeat(len);
                match field {
                    FtpField::ProfileName => profile.profile_name = value.clone(),
                    FtpField::Ssid24Ghz => profile.ssid_24ghz = value.clone(),
                    FtpField::Ssid5Ghz => profile.ssid_5ghz = value.clone(),
                    FtpField::Host => profile.host = value.clone(),
                    FtpField::Username => profile.username = value.clone(),
                    FtpField::Password => profile.password = value.clone(),
                }
                let blob = encode_ftp_profile(&profile).unwrap();
                assert_eq!(decode_ftp_profile_fields(&blob), profile, "{:?} len={len}", field);
            }
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
    fn parse_propcode_bare_digits_are_decimal_even_when_they_look_like_hex() {
        // Finding #85, kept as documented behaviour:
        // "5010" is 5010, i.e. 0x1392, never 0x5010.
        assert_eq!(parse_propcode("5010"), Ok(0x1392));
        assert_eq!(parse_propcode("0x5010"), Ok(0x5010));
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

    // Regression tests for two divergences found against a since-removed Python
    // re-implementation (which relied on Python's int(), tolerant of both forms
    // below) — pinned so a future edit can't silently reopen either gap.

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
    fn ftp_write_gate_is_exactly_the_two_confirmed_models() {
        assert_eq!(FTP_WRITE_CONFIRMED_MODELS, ["Z 9", "Z6_3"]);
        assert!(ftp_write_confirmed("Z 9"));
        assert!(ftp_write_confirmed("Z6_3"));
    }

    #[test]
    fn ftp_write_gate_rejects_other_models_exactly() {
        assert!(!ftp_write_confirmed("Z 5"), "surveyed for reads, never captured for the FTP write");
        assert!(!ftp_write_confirmed(""));
        // Not a fuzzy/prefix match — trailing whitespace is a different string.
        assert!(!ftp_write_confirmed("Z6_3 "));
        assert!(!ftp_write_confirmed("z 9"));
    }

    #[test]
    fn every_write_confirmed_model_is_a_surveyed_model() {
        // Drift guard against the one model registry (ops::cameras::KNOWN_MODELS):
        // a model cannot be write-confirmed without being surveyed. Renaming
        // or dropping one side makes this fail instead of silently diverging.
        for m in FTP_WRITE_CONFIRMED_MODELS {
            assert!(sdk_model_name(m).is_some(), "{m:?} is write-confirmed but not in KNOWN_MODELS");
            assert!(KNOWN_MODELS.contains(m), "{m:?}");
        }
    }

    // ── PtpResponseCode ──────────────────────────────────────────────────

    #[test]
    fn response_code_known_names() {
        assert_eq!(PtpResponseCode(0x2001).to_string(), "OK (0x2001)");
        assert_eq!(PtpResponseCode(0x200A).to_string(), "DevicePropNotSupported (0x200a)");
        assert_eq!(PtpResponseCode(0x201E).to_string(), "SessionAlreadyOpen (0x201e)");
    }

    #[test]
    fn response_code_names_every_standard_code_and_nothing_either_side() {
        let names = [
            "OK", "GeneralError", "SessionNotOpen", "InvalidTransactionID", "OperationNotSupported",
            "ParameterNotSupported", "IncompleteTransfer", "InvalidStorageID", "InvalidObjectHandle",
            "DevicePropNotSupported", "InvalidObjectFormatCode", "StoreFull", "ObjectWriteProtected",
            "StoreReadOnly", "AccessDenied", "NoThumbnailPresent", "SelfTestFailed", "PartialDeletion",
            "StoreNotAvailable", "SpecificationByFormatUnsupported", "NoValidObjectInfo", "InvalidCodeFormat",
            "UnknownVendorCode", "CaptureAlreadyTerminated", "DeviceBusy", "InvalidParentObject",
            "InvalidDevicePropFormat", "InvalidDevicePropValue", "InvalidParameter", "SessionAlreadyOpen",
            "TransactionCancelled", "SpecificationOfDestinationUnsupported",
        ];
        for (i, name) in names.iter().enumerate() {
            let code = 0x2001 + i as u16;
            assert_eq!(PtpResponseCode(code).to_string(), format!("{name} (0x{code:04x})"));
        }
        // Just outside the standard range: hex only.
        assert_eq!(PtpResponseCode(0x2000).to_string(), "0x2000");
        assert_eq!(PtpResponseCode(0x2021).to_string(), "0x2021");
    }

    #[test]
    fn response_code_unknown_falls_back_to_hex() {
        assert_eq!(PtpResponseCode(0x1234).to_string(), "0x1234");
    }

    // ── select_candidate (find_device's actual selection logic) ────────────
    // (candidate_devices() itself needs real USB hardware; select_candidate
    // is the pure, hardware-independent part find_device delegates to, so
    // these tests call the SAME function find_device runs — not a
    // re-implementation of it that could silently drift from it.)

    fn held(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("bus 20 address {}: open: Access denied (insufficient permissions)", i + 3)).collect()
    }

    #[test]
    fn select_candidate_by_serial_match() {
        let candidates = vec![("AAA".to_string(), 1), ("BBB".to_string(), 2)];
        assert!(matches!(select_candidate(candidates, &[], &[], Some("BBB")), Ok(2)));
    }

    #[test]
    fn select_candidate_by_serial_no_match_errors() {
        let candidates = vec![("AAA".to_string(), 1)];
        assert!(matches!(select_candidate(candidates, &[], &[], Some("ZZZ")), Err(PtpUsbError::SerialNotFound { .. })));
    }

    #[test]
    fn select_candidate_no_serial_single_candidate() {
        let candidates = vec![("AAA".to_string(), 1)];
        assert!(matches!(select_candidate(candidates, &[], &[], None), Ok(1)));
    }

    #[test]
    fn select_candidate_no_serial_multiple_candidates_is_ambiguous() {
        let candidates = vec![("AAA".to_string(), 1), ("BBB".to_string(), 2)];
        assert!(matches!(select_candidate(candidates, &[], &[], None), Err(PtpUsbError::AmbiguousDevice(2))));
    }

    #[test]
    fn select_candidate_no_candidates_errors() {
        let candidates: Vec<(String, u32)> = vec![];
        assert!(matches!(select_candidate(candidates, &[], &[], None), Err(PtpUsbError::NoDevice)));
    }

    #[test]
    fn select_candidate_refuses_the_one_openable_body_when_another_was_skipped() {
        // Finding #01/#26: one body opened, one was held by ptpcamerad. With
        // no serial the opened one is not "the only camera"; picking it would
        // let a write go to the wrong body.
        let candidates = vec![("AAA".to_string(), 1)];
        let err = select_candidate(candidates, &held(1), &[], None).unwrap_err();
        assert!(matches!(&err, PtpUsbError::BodiesSkipped { skipped } if skipped.len() == 1), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("1 Nikon USB device(s) could not be opened"), "{text}");
        assert!(text.contains("bus 20 address 3"), "names the skipped device: {text}");
        assert!(text.contains("--serial"), "says how to proceed: {text}");
    }

    #[test]
    fn select_candidate_with_only_skipped_bodies_reports_them_not_no_device() {
        let candidates: Vec<(String, u32)> = vec![];
        let err = select_candidate(candidates, &held(2), &[], None).unwrap_err();
        assert!(matches!(&err, PtpUsbError::BodiesSkipped { skipped } if skipped.len() == 2), "{err:?}");
    }

    #[test]
    fn select_candidate_with_a_serial_uses_the_match_even_when_another_body_was_skipped() {
        let candidates = vec![("AAA".to_string(), 1), ("BBB".to_string(), 2)];
        assert!(matches!(select_candidate(candidates, &held(1), &[], Some("BBB")), Ok(2)));
    }

    #[test]
    fn select_candidate_serial_not_found_while_a_body_was_skipped_names_the_skipped_body() {
        // The missing serial may well be the held body: "no such serial"
        // would send the user hunting for a typo.
        let candidates = vec![("AAA".to_string(), 1)];
        let err = select_candidate(candidates, &held(1), &[], Some("ZZZ")).unwrap_err();
        assert!(matches!(err, PtpUsbError::BodiesSkipped { .. }), "{err:?}");
    }

    #[test]
    fn select_candidate_duplicate_serial_is_an_error_not_first_wins() {
        // Finding #31: mirrors ops::target::select_by_usb_serial's DuplicateSerial.
        let candidates = vec![("AAA".to_string(), 1), ("AAA".to_string(), 2)];
        let err = select_candidate(candidates, &[], &[], Some("AAA")).unwrap_err();
        assert!(matches!(&err, PtpUsbError::DuplicateSerial { requested, count: 2 } if requested == "AAA"), "{err:?}");
        assert!(err.to_string().contains("refusing to pick one"));
        let three = vec![("AAA".to_string(), 1), ("BBB".to_string(), 9), ("AAA".to_string(), 2), ("AAA".to_string(), 3)];
        let err = select_candidate(three, &[], &[], Some("AAA")).unwrap_err();
        assert!(matches!(err, PtpUsbError::DuplicateSerial { count: 3, .. }), "{err:?}");
    }

    #[test]
    fn select_candidate_a_duplicate_of_another_serial_does_not_block_a_unique_match() {
        let candidates = vec![("AAA".to_string(), 1), ("AAA".to_string(), 2), ("BBB".to_string(), 3)];
        assert!(matches!(select_candidate(candidates, &[], &[], Some("BBB")), Ok(3)));
    }

    #[test]
    fn select_candidate_duplicates_without_a_serial_are_ambiguous() {
        let candidates = vec![("AAA".to_string(), 1), ("AAA".to_string(), 2)];
        assert!(matches!(select_candidate(candidates, &[], &[], None), Err(PtpUsbError::AmbiguousDevice(2))));
    }

    #[test]
    fn select_candidate_matches_serials_exactly() {
        let candidates = vec![("AAA".to_string(), 1)];
        assert!(matches!(select_candidate(candidates, &[], &[], Some("aaa")), Err(PtpUsbError::SerialNotFound { .. })));
        let candidates = vec![("AAA".to_string(), 1)];
        assert!(matches!(select_candidate(candidates, &[], &[], Some("")), Err(PtpUsbError::SerialNotFound { .. })));
    }

    // ── a requested serial that matched nothing while a body's serial could not be read ──

    fn unreadable(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("usb-20:{} (reading the USB serial string failed: pipe error)", i + 3)).collect()
    }

    #[test]
    fn serial_not_found_with_every_identity_read_says_only_that_no_body_has_it() {
        let candidates = vec![("AAA".to_string(), 1)];
        let err = select_candidate(candidates, &[], &[], Some("ZZZ")).unwrap_err();
        assert!(matches!(&err, PtpUsbError::SerialNotFound { requested, unreadable } if requested == "ZZZ" && unreadable.is_empty()), "{err:?}");
        assert_eq!(err.to_string(), "no Nikon USB camera with serial \"ZZZ\"");
    }

    #[test]
    fn serial_not_found_while_one_identity_fell_back_names_it_as_a_possible_match() {
        let candidates = vec![("AAA".to_string(), 1), ("usb-20:3".to_string(), 2)];
        let err = select_candidate(candidates, &[], &unreadable(1), Some("REAL")).unwrap_err();
        match &err {
            PtpUsbError::SerialNotFound { requested, unreadable } => {
                assert_eq!(requested, "REAL");
                assert_eq!(unreadable.len(), 1, "typed on the variant, not only in the text");
                assert!(unreadable[0].contains("usb-20:3") && unreadable[0].contains("pipe error"), "{unreadable:?}");
            }
            other => panic!("{other:?}"),
        }
        let text = err.to_string();
        assert!(text.contains("no Nikon USB camera with serial \"REAL\""), "{text}");
        assert!(text.contains("1 attached body(ies) could not be read: usb-20:3 (reading the USB serial string failed: pipe error)"), "{text}");
        assert!(text.ends_with("the serial may be on one of them"), "{text}");
    }

    #[test]
    fn serial_not_found_counts_and_lists_every_unreadable_body() {
        let candidates: Vec<(String, u32)> = vec![];
        let err = select_candidate(candidates, &[], &unreadable(2), Some("REAL")).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("2 attached body(ies) could not be read: usb-20:3"), "{text}");
        assert!(text.contains("; usb-20:4 ("), "each body is listed: {text}");
    }

    #[test]
    fn serial_not_found_with_no_body_attached_has_no_unreadable_note() {
        // Nothing attached: nothing to say may be hiding the serial.
        let candidates: Vec<(String, u32)> = vec![];
        let err = select_candidate(candidates, &[], &[], Some("REAL")).unwrap_err();
        assert!(matches!(&err, PtpUsbError::SerialNotFound { unreadable, .. } if unreadable.is_empty()), "{err:?}");
        assert!(!err.to_string().contains("could not be read"), "{err}");
    }

    #[test]
    fn an_unreadable_body_does_not_change_a_match_or_a_skipped_refusal() {
        // A match is still used; a skipped body is still reported as skipped, ahead of the note.
        let candidates = vec![("REAL".to_string(), 1), ("usb-20:3".to_string(), 2)];
        assert!(matches!(select_candidate(candidates, &[], &unreadable(1), Some("REAL")), Ok(1)));
        let candidates = vec![("usb-20:3".to_string(), 2)];
        let err = select_candidate(candidates, &held(1), &unreadable(1), Some("REAL")).unwrap_err();
        assert!(matches!(err, PtpUsbError::BodiesSkipped { .. }), "{err:?}");
    }

    #[test]
    fn only_a_failed_serial_read_makes_a_body_unreadable() {
        let warn = vec!["reading the USB serial string failed: pipe error".to_string()];
        let d = describe_unreadable("usb-20:3", true, &warn).expect("fell back after a failed read");
        assert_eq!(d, "usb-20:3 (reading the USB serial string failed: pipe error)");
        assert_eq!(describe_unreadable("usb-20:3", true, &[]), None, "a body with no serial string at all is identified");
        assert_eq!(describe_unreadable("REAL", false, &warn), None, "the serial was read; only the product string failed");
        assert_eq!(describe_unreadable("REAL", false, &[]), None);
        let two = vec!["a".to_string(), "b".to_string()];
        assert_eq!(describe_unreadable("usb-1:2", true, &two).unwrap(), "usb-1:2 (a, b)");
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

    // ── claim failure report ─────────────────────────────────────────────

    #[test]
    fn claim_failure_detail_includes_the_swallowed_configuration_error() {
        assert_eq!(claim_failure_detail(Some("Resource busy"), None), "Resource busy");
        assert_eq!(
            claim_failure_detail(Some("Resource busy"), Some("Entity not found")),
            "Resource busy; last set_active_configuration(1) error: Entity not found"
        );
        assert_eq!(claim_failure_detail(None, None), "no attempt recorded");
    }

    // ── session sequencing against a scripted transport ──────────────────
    //
    // Finding nikon-fleet-20260909-7b2e#18: `transact`, OpenSession's
    // already-open retry, the vendor ops and teardown previously had no
    // coverage short of live hardware. `ScriptedTransport` is a hand-rolled
    // byte-level replay (not a mocking framework): it records every
    // bulk-OUT write and serves a fixed queue of bulk-IN reads. An
    // unscripted read fails with `Timeout`, so a code path that reads more
    // than the test expects fails loudly instead of hanging or passing
    // vacuously. What this CANNOT show is that the camera really answers
    // this way — that remains the 2026-09-06 live session's job.
    //
    // It delivers what real USB does and the code relies on (finding #11):
    // replies stamped with the transaction id of the command just written
    // (as the camera does) unless a test names one; several containers in
    // one read; zero-length reads (zero-length packets); a header split
    // across reads; writes the pipe only partly takes; zero-length-packet
    // writes recorded apart from containers; per-read delay; a never-ending
    // stream of one chunk; and every `clear_halt`.

    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    /// One container a scripted reply carries: (kind, code, payload).
    type Part = (u16, u16, Vec<u8>);

    enum Step {
        /// Raw bytes, delivered as one read.
        Bytes(Vec<u8>),
        /// One or more containers delivered in a single read, each stamped
        /// with the id of the most recent command written.
        Stamped(Vec<Part>),
        Err(rusb::Error),
    }

    #[derive(Default)]
    struct Wire {
        writes: Vec<(u8, Vec<u8>)>,
        reads: VecDeque<Step>,
        released: Vec<u8>,
        halts: Vec<u8>,
        /// Per write call (front first): bytes the pipe accepts instead of all.
        write_caps: VecDeque<usize>,
        read_timeouts: Vec<Duration>,
        reads_served: usize,
        read_delay: Duration,
        /// When `reads` runs dry: this chunk, this many more times.
        repeat: Option<(Vec<u8>, usize)>,
        last_command_id: u32,
        fail_halt: bool,
    }

    #[derive(Clone, Default)]
    struct ScriptedTransport(Rc<RefCell<Wire>>);

    impl BulkTransport for ScriptedTransport {
        fn write_bulk(&self, endpoint: u8, data: &[u8], _timeout: Duration) -> rusb::Result<usize> {
            let mut w = self.0.borrow_mut();
            let taken = match w.write_caps.pop_front() {
                Some(cap) => cap.min(data.len()),
                None => data.len(),
            };
            w.writes.push((endpoint, data[..taken].to_vec()));
            if data.len() >= 12 && u16::from_le_bytes([data[4], data[5]]) == CONTAINER_COMMAND {
                w.last_command_id = u32::from_le_bytes(data[8..12].try_into().unwrap());
            }
            Ok(taken)
        }
        fn read_bulk(&self, _endpoint: u8, buf: &mut [u8], timeout: Duration) -> rusb::Result<usize> {
            let delay = self.0.borrow().read_delay;
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            let mut w = self.0.borrow_mut();
            w.read_timeouts.push(timeout);
            w.reads_served += 1;
            let bytes = match w.reads.pop_front() {
                Some(Step::Bytes(b)) => b,
                Some(Step::Stamped(parts)) => {
                    let id = w.last_command_id;
                    parts.iter().flat_map(|(kind, code, payload)| container(*kind, *code, id, payload)).collect()
                }
                Some(Step::Err(e)) => return Err(e),
                None => match &mut w.repeat {
                    Some((chunk, left)) if *left > 0 => {
                        *left -= 1;
                        chunk.clone()
                    }
                    _ => return Err(rusb::Error::Timeout),
                },
            };
            assert!(bytes.len() <= buf.len(), "scripted chunk larger than the read buffer");
            buf[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
        fn clear_halt(&self, endpoint: u8) -> rusb::Result<()> {
            let mut w = self.0.borrow_mut();
            w.halts.push(endpoint);
            if w.fail_halt { Err(rusb::Error::NoDevice) } else { Ok(()) }
        }
        fn release_interface(&self, interface: u8) -> rusb::Result<()> {
            self.0.borrow_mut().released.push(interface);
            Ok(())
        }
    }

    impl ScriptedTransport {
        fn script(steps: Vec<Step>) -> Self {
            let t = ScriptedTransport::default();
            t.0.borrow_mut().reads = steps.into();
            t
        }
        /// Every non-empty frame written so far, decoded: (kind, code, transaction id, payload).
        fn sent(&self) -> Vec<(u16, u16, u32, Vec<u8>)> {
            self.0
                .borrow()
                .writes
                .iter()
                .filter(|(_, raw)| !raw.is_empty())
                .map(|(_, raw)| {
                    let (c, consumed) = parse_container(raw).expect("session wrote a well-formed container");
                    assert_eq!(consumed, raw.len(), "one container per write");
                    (c.kind, c.code, c.transaction_id, c.payload)
                })
                .collect()
        }
        /// How many zero-length packets were written.
        fn zlps(&self) -> usize {
            self.0.borrow().writes.iter().filter(|(_, raw)| raw.is_empty()).count()
        }
        fn released(&self) -> Vec<u8> {
            self.0.borrow().released.clone()
        }
        fn halts(&self) -> Vec<u8> {
            self.0.borrow().halts.clone()
        }
        fn unread(&self) -> usize {
            self.0.borrow().reads.len()
        }
        fn reads_served(&self) -> usize {
            self.0.borrow().reads_served
        }
        fn read_timeouts(&self) -> Vec<Duration> {
            self.0.borrow().read_timeouts.clone()
        }
    }

    /// A Response with `code`, carrying the id of the command it answers.
    fn resp(code: u16) -> Step {
        Step::Stamped(vec![(CONTAINER_RESPONSE, code, vec![])])
    }
    /// A Response carrying an explicit id (a stale or out-of-order reply).
    fn resp_id(code: u16, id: u32) -> Step {
        Step::Bytes(container(CONTAINER_RESPONSE, code, id, &[]))
    }
    fn data_c(op: u16, payload: &[u8]) -> Step {
        Step::Stamped(vec![(CONTAINER_DATA, op, payload.to_vec())])
    }
    /// One logical container delivered as bulk-IN chunks of at most 16384 bytes.
    fn chunked(raw: Vec<u8>) -> Vec<Step> {
        raw.chunks(16384).map(|c| Step::Bytes(c.to_vec())).collect()
    }
    fn session(steps: Vec<Step>, model: &str) -> (PtpUsbSession<ScriptedTransport>, ScriptedTransport) {
        let t = ScriptedTransport::script(steps);
        (PtpUsbSession::with_transport(t.clone(), model), t)
    }
    fn le(v: u32) -> Vec<u8> {
        v.to_le_bytes().to_vec()
    }
    fn unchecked(s: &mut PtpUsbSession<ScriptedTransport>, op: u16) -> Result<Reply, PtpUsbError> {
        s.transact(op, &[], None, TxidCheck::Unchecked)
    }

    // ── open_ptp_session ─────────────────────────────────────────────────

    #[test]
    fn open_session_clean_sends_one_open_and_marks_open() {
        let (mut s, t) = session(vec![resp(RESP_OK)], "Z 9");
        s.open_ptp_session().unwrap();
        assert_eq!(t.sent(), vec![(CONTAINER_COMMAND, OP_OPEN_SESSION, 0, le(SESSION_ID))]);
        assert!(s.session_open);
        assert_eq!(s.transaction_id, 1);
    }

    #[test]
    fn open_session_already_open_closes_resets_ids_and_reopens() {
        let (mut s, t) = session(vec![resp(RESP_SESSION_ALREADY_OPEN), resp(RESP_OK), resp(RESP_OK)], "Z 9");
        s.open_ptp_session().unwrap();
        // Open, Close, Open — each at transaction id 0 (the id is reset
        // before the close and again before the re-open).
        assert_eq!(
            t.sent(),
            vec![
                (CONTAINER_COMMAND, OP_OPEN_SESSION, 0, le(SESSION_ID)),
                (CONTAINER_COMMAND, OP_CLOSE_SESSION, 0, vec![]),
                (CONTAINER_COMMAND, OP_OPEN_SESSION, 0, le(SESSION_ID)),
            ]
        );
        assert!(s.session_open);
        assert_eq!(s.transaction_id, 1);
        assert_eq!(t.unread(), 0);
    }

    #[test]
    fn open_session_second_already_open_is_fatal_not_looped() {
        let (mut s, t) = session(vec![resp(RESP_SESSION_ALREADY_OPEN), resp(RESP_OK), resp(RESP_SESSION_ALREADY_OPEN)], "Z 9");
        let err = s.open_ptp_session().unwrap_err();
        assert!(matches!(err, PtpUsbError::OpenSessionFailed(PtpResponseCode(RESP_SESSION_ALREADY_OPEN))), "{err:?}");
        assert_eq!(t.sent().len(), 3, "exactly one retry, no further loop");
        assert!(!s.session_open);
    }

    #[test]
    fn open_session_retry_failing_with_a_different_code_reports_that_code() {
        let (mut s, _t) = session(vec![resp(RESP_SESSION_ALREADY_OPEN), resp(RESP_OK), resp(0x2002)], "Z 9");
        let err = s.open_ptp_session().unwrap_err();
        assert!(matches!(err, PtpUsbError::OpenSessionFailed(PtpResponseCode(0x2002))), "{err:?}");
        assert!(!s.session_open);
    }

    #[test]
    fn open_session_retry_enforces_the_id_of_the_reopen_reply() {
        // Finding #23: with replies stamped by the transport, the retry's
        // OpenSession can be made to answer with a stale id, which only the
        // Enforce check catches (it would otherwise read as a clean OK).
        let (mut s, t) = session(vec![resp(RESP_SESSION_ALREADY_OPEN), resp(RESP_OK), resp_id(RESP_OK, 7)], "Z 9");
        let err = s.open_ptp_session().unwrap_err();
        assert!(
            matches!(err, PtpUsbError::TransactionIdMismatch { op: OP_OPEN_SESSION, expected: 0, got: 7 }),
            "{err:?}"
        );
        assert!(!s.session_open);
        assert_eq!(t.sent().len(), 3);
    }

    #[test]
    fn open_session_close_failure_during_retry_is_ignored_and_reopen_still_attempted() {
        // Pins the deliberate best-effort on the CloseSession: its I/O
        // failure must not stop the re-open — even though that failure
        // poisons the session, the retry starts over from the cleared pipes.
        let (mut s, t) = session(vec![resp(RESP_SESSION_ALREADY_OPEN), Step::Err(rusb::Error::Timeout), resp(RESP_OK)], "Z 9");
        s.open_ptp_session().unwrap();
        assert_eq!(t.sent().len(), 3);
        assert!(s.session_open);
        assert!(s.poisoned.is_none());
        assert_eq!(t.halts(), vec![0x81, 0x02], "the failed close cleared both endpoints before the re-open");
    }

    #[test]
    fn open_session_other_failure_code_does_not_retry() {
        let (mut s, t) = session(vec![resp(0x2002)], "Z 9");
        let err = s.open_ptp_session().unwrap_err();
        assert!(matches!(err, PtpUsbError::OpenSessionFailed(PtpResponseCode(0x2002))), "{err:?}");
        assert_eq!(t.sent().len(), 1);
        assert!(!s.session_open);
    }

    #[test]
    fn open_session_io_error_propagates_without_retry() {
        let (mut s, t) = session(vec![Step::Err(rusb::Error::Timeout)], "Z 9");
        let err = s.open_ptp_session().unwrap_err();
        assert!(matches!(err, PtpUsbError::Usb(rusb::Error::Timeout)), "{err:?}");
        assert_eq!(t.sent().len(), 1);
        assert!(!s.session_open);
    }

    // ── transact / read_container ────────────────────────────────────────

    #[test]
    fn transact_collects_data_phase_then_response() {
        let (mut s, _t) = session(vec![data_c(0x943B, &[1, 2, 3]), resp(RESP_OK)], "Z 9");
        let r = unchecked(&mut s, 0x943B).unwrap();
        assert_eq!((r.code, r.params, r.data, r.data_containers), (RESP_OK, vec![], vec![1, 2, 3], 1));
    }

    #[test]
    fn transact_concatenates_multiple_data_containers() {
        let (mut s, _t) = session(vec![data_c(0x943B, &[1, 2]), data_c(0x943B, &[3]), resp(RESP_OK)], "Z 9");
        let r = unchecked(&mut s, 0x943B).unwrap();
        assert_eq!((r.data, r.data_containers), (vec![1, 2, 3], 2));
    }

    #[test]
    fn transact_decodes_response_params() {
        let raw = container(CONTAINER_RESPONSE, RESP_OK, 0, &[7, 0, 0, 0, 9, 0, 0, 0]);
        let (mut s, _t) = session(vec![Step::Bytes(raw)], "Z 9");
        assert_eq!(unchecked(&mut s, 0x1001).unwrap().params, vec![7, 9]);
    }

    #[test]
    fn transact_consumes_the_transaction_id_even_when_it_fails() {
        // Finding #30: the id is spent when the command goes out. After a
        // failure the next command must not reuse it (the Unchecked write
        // ops would take a stale reply for the ack). The session is also
        // poisoned, so "the next command" is a new session — whose ids are
        // pinned by the success path below.
        let (mut s, t) = session(vec![Step::Err(rusb::Error::Timeout), resp(RESP_OK)], "Z 9");
        s.transaction_id = 5;
        assert!(unchecked(&mut s, 0x1001).is_err());
        assert_eq!(s.transaction_id, 6, "an I/O failure still consumes the id");
        assert_eq!(t.sent().iter().map(|f| f.2).collect::<Vec<_>>(), vec![5]);
    }

    #[test]
    fn transact_ids_advance_one_per_transaction_across_successes() {
        let (mut s, t) = session(vec![resp(RESP_OK), resp(RESP_OK), resp(RESP_OK)], "Z 9");
        s.transaction_id = 5;
        for _ in 0..3 {
            unchecked(&mut s, 0x1001).unwrap();
        }
        assert_eq!(s.transaction_id, 8);
        assert_eq!(t.sent().iter().map(|f| f.2).collect::<Vec<_>>(), vec![5, 6, 7]);
    }

    #[test]
    fn transact_wraps_the_transaction_id_at_the_top_of_the_range() {
        let (mut s, t) = session(vec![resp(RESP_OK), resp(RESP_OK)], "Z 9");
        s.transaction_id = 0xFFFF_FFFE;
        unchecked(&mut s, 0x1001).unwrap();
        assert_eq!(s.transaction_id, 1);
        unchecked(&mut s, 0x1001).unwrap();
        assert_eq!(t.sent().iter().map(|f| f.2).collect::<Vec<_>>(), vec![0xFFFF_FFFE, 1]);
    }

    #[test]
    fn transact_misaligned_response_params_is_an_error_but_not_a_poisoning_one() {
        // The Response container was read in full, so the pipe is in sync:
        // the id is spent and the session stays usable.
        let raw = container(CONTAINER_RESPONSE, RESP_OK, 4, &[1, 2, 3]);
        let (mut s, t) = session(vec![Step::Bytes(raw), resp(RESP_OK)], "Z 9");
        s.transaction_id = 4;
        let err = s.transact(0x1001, &[], None, TxidCheck::Enforce).unwrap_err();
        assert!(matches!(err, PtpUsbError::MisalignedResponseParams(3)), "{err:?}");
        assert_eq!(s.transaction_id, 5);
        assert!(s.poisoned.is_none());
        assert!(t.halts().is_empty(), "no clear_halt for an in-sync failure");
        s.transact(0x1001, &[], None, TxidCheck::Enforce).unwrap();
        assert_eq!(t.sent().iter().map(|f| f.2).collect::<Vec<_>>(), vec![4, 5]);
    }

    #[test]
    fn transact_ignores_event_and_unrecognized_containers_and_counts_the_latter() {
        let event = Step::Bytes(container(CONTAINER_EVENT, 0x4006, 0, &[]));
        let weird = Step::Bytes(container(9, 0, 0, &[]));
        let weird2 = Step::Bytes(container(CONTAINER_COMMAND, 0, 0, &[]));
        let (mut s, _t) = session(vec![event, weird, weird2, resp(RESP_OK)], "Z 9");
        let r = unchecked(&mut s, 0x1001).unwrap();
        assert_eq!(r.code, RESP_OK);
        assert_eq!(r.unrecognized_containers, 2, "the event is not counted, the two strays are");
        assert_eq!(s.unrecognized_container_count(), 2);
    }

    #[test]
    fn transact_sends_command_then_data_out_under_the_same_transaction_id() {
        let (mut s, t) = session(vec![resp(RESP_OK)], "Z 9");
        s.transact(0x90EE, &[7], Some(&[9, 9]), TxidCheck::Unchecked).unwrap();
        assert_eq!(
            t.sent(),
            vec![(CONTAINER_COMMAND, 0x90EE, 0, le(7)), (CONTAINER_DATA, 0x90EE, 0, vec![9, 9])]
        );
    }

    #[test]
    fn transact_rejects_too_many_params_before_sending_and_keeps_the_session_usable() {
        let (mut s, t) = session(vec![resp(RESP_OK)], "Z 9");
        let err = s.transact(0x1001, &[1, 2, 3, 4, 5, 6], None, TxidCheck::Unchecked).unwrap_err();
        assert!(matches!(err, PtpUsbError::TooManyCommandParams { given: 6, max: 5 }), "{err:?}");
        assert!(t.sent().is_empty());
        assert_eq!(s.transaction_id, 0, "nothing was sent, so no id was spent");
        assert!(s.poisoned.is_none());
        unchecked(&mut s, 0x1001).unwrap();
    }

    // ── what real USB delivers (finding #11) ─────────────────────────────

    #[test]
    fn a_response_coalesced_after_its_data_in_one_read_is_not_lost() {
        // Finding #29: one bulk read can carry the Data container and the
        // Response behind it. The old decode dropped everything past the
        // declared length and the transaction waited out its timeout.
        let both = Step::Stamped(vec![
            (CONTAINER_DATA, 0x943B, vec![0xAA, 0xBB]),
            (CONTAINER_RESPONSE, RESP_OK, vec![]),
        ]);
        let (mut s, t) = session(vec![both], "Z 9");
        let r = s.transact(0x943B, &[], None, TxidCheck::Enforce).unwrap();
        assert_eq!((r.code, r.data), (RESP_OK, vec![0xAA, 0xBB]));
        assert_eq!(t.reads_served(), 1, "answered from the single read, no second read (and no timeout)");
        assert!(s.rx.is_empty());
    }

    #[test]
    fn a_container_coalesced_behind_a_response_is_kept_for_the_next_read() {
        // Two Responses in one read: the second belongs to the next
        // transaction and must still be there for it.
        let two = Step::Stamped(vec![(CONTAINER_RESPONSE, RESP_OK, vec![]), (CONTAINER_RESPONSE, 0x2002, vec![])]);
        let (mut s, t) = session(vec![two], "Z 9");
        s.transaction_id = 0;
        let first = unchecked(&mut s, 0x1001).unwrap();
        assert_eq!(first.code, RESP_OK);
        assert_eq!(s.rx.len(), 12, "the second container waits in the buffer");
        let second = unchecked(&mut s, 0x1001).unwrap();
        assert_eq!(second.code, 0x2002);
        assert_eq!(t.reads_served(), 1);
    }

    #[test]
    fn a_zero_length_packet_after_a_container_that_fills_the_read_buffer_is_skipped() {
        // Finding #11: a container of exactly 16384 bytes fills one bulk
        // read, and its terminating zero-length packet then arrives as an
        // empty read ahead of the Response (previously ShortContainer(0)).
        let payload = vec![0x5A; READ_CHUNK - HEADER_LEN];
        let full = container(CONTAINER_DATA, 0x943B, 0, &payload);
        assert_eq!(full.len(), 16384);
        let (mut s, _t) = session(vec![Step::Bytes(full), Step::Bytes(vec![]), resp(RESP_OK)], "Z 9");
        let r = s.transact(0x943B, &[], None, TxidCheck::Enforce).unwrap();
        assert_eq!(r.data, payload);
    }

    #[test]
    fn a_container_one_byte_either_side_of_the_read_buffer_still_assembles() {
        for total in [READ_CHUNK - 1, READ_CHUNK + 1] {
            let payload = vec![0x11; total - HEADER_LEN];
            let mut steps = chunked(container(CONTAINER_DATA, 0x943B, 0, &payload));
            steps.push(resp(RESP_OK));
            let (mut s, _t) = session(steps, "Z 9");
            assert_eq!(s.transact(0x943B, &[], None, TxidCheck::Enforce).unwrap().data, payload, "total {total}");
        }
    }

    #[test]
    fn zero_length_packets_between_containers_are_skipped_up_to_a_limit() {
        // At / below / above the number of consecutive empty reads tolerated.
        assert_eq!(MAX_EMPTY_READS, 8);
        for (empties, ok) in [(7usize, true), (8, true), (9, false)] {
            let mut steps: Vec<Step> = (0..empties).map(|_| Step::Bytes(vec![])).collect();
            steps.push(resp(RESP_OK));
            let (mut s, _t) = session(steps, "Z 9");
            let r = unchecked(&mut s, 0x1001);
            if ok {
                assert_eq!(r.unwrap().code, RESP_OK, "{empties} empties");
            } else {
                assert!(matches!(r.unwrap_err(), PtpUsbError::ShortContainer(0)), "{empties} empties");
            }
        }
    }

    #[test]
    fn a_header_split_across_reads_is_reassembled_at_every_split_point() {
        let whole = container(CONTAINER_DATA, 0x943B, 0, &[1, 2, 3, 4]);
        for split in 1..HEADER_LEN + 2 {
            let steps = vec![Step::Bytes(whole[..split].to_vec()), Step::Bytes(whole[split..].to_vec()), resp(RESP_OK)];
            let (mut s, _t) = session(steps, "Z 9");
            assert_eq!(s.transact(0x943B, &[], None, TxidCheck::Enforce).unwrap().data, vec![1, 2, 3, 4], "split at {split}");
        }
    }

    #[test]
    fn a_first_read_of_exactly_the_header_is_not_a_short_container() {
        // Finding #67: 12 bytes is a complete header; the payload follows in
        // the next read.
        let whole = container(CONTAINER_DATA, 0x943B, 0, &[9, 8, 7, 6]);
        let steps = vec![Step::Bytes(whole[..12].to_vec()), Step::Bytes(whole[12..].to_vec()), resp(RESP_OK)];
        let (mut s, _t) = session(steps, "Z 9");
        assert_eq!(s.transact(0x943B, &[], None, TxidCheck::Enforce).unwrap().data, vec![9, 8, 7, 6]);
    }

    #[test]
    fn declared_length_eleven_twelve_thirteen_through_transact() {
        // Declared 11: below the floor even though 12 bytes arrived.
        let (mut s, _t) = session(vec![Step::Bytes(header(11))], "Z 9");
        let err = unchecked(&mut s, 0x1001).unwrap_err();
        assert!(matches!(err, PtpUsbError::ShortContainer(11)), "{err:?}");
        // Declared 12: a complete empty container.
        let (mut s, _t) = session(vec![Step::Bytes(header(12))], "Z 9");
        assert_eq!(unchecked(&mut s, 0x1001).unwrap().code, RESP_OK);
        // Declared 13 with 12 bytes in the first read: wait for the 13th.
        let mut kind_data = header(13);
        kind_data[4..6].copy_from_slice(&CONTAINER_DATA.to_le_bytes());
        kind_data[6..8].copy_from_slice(&0x1001u16.to_le_bytes());
        let steps = vec![Step::Bytes(kind_data), Step::Bytes(vec![0x42]), resp(RESP_OK)];
        let (mut s, _t) = session(steps, "Z 9");
        assert_eq!(unchecked(&mut s, 0x1001).unwrap().data, vec![0x42]);
        // Declared 13 and the device never sends the byte: truncated.
        let (mut s, _t) = session(vec![Step::Bytes(header(13)), Step::Bytes(vec![])], "Z 9");
        let err = unchecked(&mut s, 0x1001).unwrap_err();
        assert!(matches!(err, PtpUsbError::TruncatedContainer { declared: 13, got: 12 }), "{err:?}");
    }

    #[test]
    fn a_partial_header_followed_by_a_zero_length_packet_is_a_short_container() {
        let (mut s, _t) = session(vec![Step::Bytes(vec![1, 2, 3]), Step::Bytes(vec![])], "Z 9");
        let err = unchecked(&mut s, 0x1001).unwrap_err();
        assert!(matches!(err, PtpUsbError::ShortContainer(3)), "{err:?}");
        // 11 bytes is still a partial header; 12 is a whole one that declares more.
        let (mut s, _t) = session(vec![Step::Bytes(header(16)[..11].to_vec()), Step::Bytes(vec![])], "Z 9");
        let err = unchecked(&mut s, 0x1001).unwrap_err();
        assert!(matches!(err, PtpUsbError::ShortContainer(11)), "{err:?}");
        let (mut s, _t) = session(vec![Step::Bytes(header(16)), Step::Bytes(vec![])], "Z 9");
        let err = unchecked(&mut s, 0x1001).unwrap_err();
        assert!(matches!(err, PtpUsbError::TruncatedContainer { declared: 16, got: 12 }), "{err:?}");
    }

    #[test]
    fn a_partial_header_followed_by_silence_is_a_usb_timeout() {
        let (mut s, _t) = session(vec![Step::Bytes(vec![1, 2, 3])], "Z 9");
        let err = unchecked(&mut s, 0x1001).unwrap_err();
        assert!(matches!(err, PtpUsbError::Usb(rusb::Error::Timeout)), "{err:?}");
    }

    #[test]
    fn read_container_assembles_a_container_split_across_bulk_reads() {
        let payload: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let mut steps = chunked(container(CONTAINER_DATA, 0x943B, 0, &payload));
        assert_eq!(steps.len(), 2, "20 KB payload must span two 16 KB bulk reads");
        steps.push(resp(RESP_OK));
        let (mut s, _t) = session(steps, "Z 9");
        assert_eq!(unchecked(&mut s, 0x943B).unwrap().data, payload);
    }

    #[test]
    fn read_container_stops_on_a_zero_length_read_inside_a_container_and_reports_truncation() {
        let full = container(CONTAINER_DATA, 0x943B, 0, &[0u8; 88]); // declares 100 bytes
        assert_eq!(full.len(), 100);
        let (mut s, _t) = session(vec![Step::Bytes(full[..20].to_vec()), Step::Bytes(vec![])], "Z 9");
        let err = unchecked(&mut s, 0x943B).unwrap_err();
        assert!(matches!(err, PtpUsbError::TruncatedContainer { declared: 100, got: 20 }), "{err:?}");
    }

    // ── the data ceiling and the deadline, inside the read ───────────────

    #[test]
    fn policy_constants_are_pinned_by_literal_values() {
        assert_eq!(MAX_TRANSACTION_DATA_BYTES, 16_777_216);
        assert_eq!(TRANSACTION_TIMEOUT, Duration::from_secs(30));
        assert_eq!(CLOSE_TIMEOUT, Duration::from_secs(5));
        assert_eq!(BULK_TIMEOUT, Duration::from_secs(5));
        assert_eq!(MIN_BULK_TIMEOUT, Duration::from_millis(1));
        assert_eq!(READ_CHUNK, 16384);
        assert_eq!(HEADER_LEN, 12);
        assert_eq!(CLAIM_RETRY_ATTEMPTS, 40);
        assert_eq!(CLAIM_RETRY_DELAY, Duration::from_millis(50));
        assert_eq!(SESSION_ID, 1);
        let (s, _t) = session(vec![], "Z 9");
        assert_eq!(s.transaction_timeout, Duration::from_secs(30), "a fresh session uses the literal 30 s");
    }

    #[test]
    fn transact_data_phase_at_the_limit_is_accepted() {
        let mut steps = chunked(container(CONTAINER_DATA, 0x943B, 0, &vec![0u8; 16 * 1024 * 1024]));
        steps.push(resp(RESP_OK));
        let (mut s, _t) = session(steps, "Z 9");
        assert_eq!(unchecked(&mut s, 0x943B).unwrap().data.len(), 16 * 1024 * 1024);
    }

    #[test]
    fn transact_data_phase_one_byte_over_the_limit_is_rejected() {
        let mut steps = chunked(container(CONTAINER_DATA, 0x943B, 0, &vec![0u8; 16 * 1024 * 1024 + 1]));
        steps.push(resp(RESP_OK));
        let (mut s, _t) = session(steps, "Z 9");
        let err = unchecked(&mut s, 0x943B).unwrap_err();
        assert!(
            matches!(err, PtpUsbError::TransactionDataTooLarge { got, limit }
                if got == 16 * 1024 * 1024 + 1 && limit == 16 * 1024 * 1024),
            "{err:?}"
        );
    }

    #[test]
    fn a_huge_declared_length_is_refused_from_the_header_alone() {
        // Finding #28: the length is wire-controlled up to ~4 GiB. Only the
        // 12 header bytes are ever delivered; the cap must fire on those,
        // not after buffering what the header promised.
        let mut hdr = header(u32::MAX);
        hdr[4..6].copy_from_slice(&CONTAINER_DATA.to_le_bytes());
        let (mut s, t) = session(vec![Step::Bytes(hdr), resp(RESP_OK)], "Z 9");
        let err = unchecked(&mut s, 0x943B).unwrap_err();
        assert!(
            matches!(err, PtpUsbError::TransactionDataTooLarge { got, limit }
                if got == u32::MAX as usize - HEADER_LEN && limit == 16 * 1024 * 1024),
            "{err:?}"
        );
        assert_eq!(t.reads_served(), 1);
        assert_eq!(t.unread(), 1, "nothing after the header was read");
    }

    #[test]
    fn the_data_ceiling_is_cumulative_across_containers_at_below_above() {
        let limit = 16 * 1024 * 1024;
        for (second, ok) in [(9usize, true), (10, true), (11, false)] {
            // First container: limit - 10 bytes of payload; second: `second` bytes.
            let mut steps = chunked(container(CONTAINER_DATA, 0x943B, 0, &vec![0u8; limit - 10]));
            steps.extend(chunked(container(CONTAINER_DATA, 0x943B, 0, &vec![1u8; second])));
            steps.push(resp(RESP_OK));
            let (mut s, _t) = session(steps, "Z 9");
            let r = unchecked(&mut s, 0x943B);
            if ok {
                assert_eq!(r.unwrap().data.len(), limit - 10 + second, "second {second}");
            } else {
                assert!(
                    matches!(r.unwrap_err(), PtpUsbError::TransactionDataTooLarge { got, .. } if got == limit + 1),
                    "second {second}"
                );
            }
        }
    }

    #[test]
    fn a_trickling_container_is_bounded_by_the_transaction_deadline_inside_the_read() {
        // Finding #28: the deadline used to be checked once per container,
        // so a device sending one byte at a time of a container that claims
        // 1 MB could outlast it. Every read must check it.
        let (mut s, t) = session(vec![Step::Bytes(header(1_000_000))], "Z 9");
        {
            let mut w = t.0.borrow_mut();
            w.read_delay = Duration::from_millis(5);
            w.repeat = Some((vec![0xAA], 400));
        }
        s.transaction_timeout = Duration::from_millis(60);
        let err = unchecked(&mut s, 0x943B).unwrap_err();
        assert!(matches!(err, PtpUsbError::TransactionTimedOut(d) if d == Duration::from_millis(60)), "{err:?}");
        assert!(t.reads_served() < 40, "stopped near the deadline, not after draining the stream: {}", t.reads_served());
    }

    #[test]
    fn a_stream_of_unrecognized_containers_is_bounded_by_the_transaction_deadline() {
        // Finding #22: the unrecognised-container loop is bounded only by
        // TRANSACTION_TIMEOUT; this pins that it is bounded at all.
        let (mut s, t) = session(vec![], "Z 9");
        {
            let mut w = t.0.borrow_mut();
            w.read_delay = Duration::from_millis(3);
            w.repeat = Some((container(9, 0, 0, &[]), 1000));
        }
        s.transaction_timeout = Duration::from_millis(50);
        let err = unchecked(&mut s, 0x1001).unwrap_err();
        assert!(matches!(err, PtpUsbError::TransactionTimedOut(d) if d == Duration::from_millis(50)), "{err:?}");
        assert!(t.reads_served() < 100, "{}", t.reads_served());
        assert!(s.unrecognized_container_count() > 0, "the strays were counted before the deadline fired");
    }

    #[test]
    fn a_zero_transaction_timeout_expires_before_any_read() {
        let (mut s, t) = session(vec![resp(RESP_OK)], "Z 9");
        s.transaction_timeout = Duration::ZERO;
        let err = unchecked(&mut s, 0x1001).unwrap_err();
        assert!(matches!(err, PtpUsbError::TransactionTimedOut(d) if d.is_zero()), "{err:?}");
        assert_eq!(t.reads_served(), 0);
        assert_eq!(t.unread(), 1);
    }

    #[test]
    fn each_read_is_given_no_more_time_than_the_transaction_has_left() {
        let (mut s, t) = session(vec![resp(RESP_OK)], "Z 9");
        s.transaction_timeout = Duration::from_millis(2500);
        unchecked(&mut s, 0x1001).unwrap();
        let timeouts = t.read_timeouts();
        assert_eq!(timeouts.len(), 1);
        assert!(timeouts[0] <= Duration::from_millis(2500), "{:?}", timeouts[0]);
        assert!(timeouts[0] >= MIN_BULK_TIMEOUT);
        // With the default budget the per-read cap is BULK_TIMEOUT.
        let (mut s, t) = session(vec![resp(RESP_OK)], "Z 9");
        unchecked(&mut s, 0x1001).unwrap();
        assert_eq!(t.read_timeouts(), vec![BULK_TIMEOUT]);
    }

    #[test]
    fn a_read_timeout_after_the_deadline_is_reported_as_the_transaction_timing_out() {
        let (mut s, t) = session(vec![], "Z 9");
        t.0.borrow_mut().read_delay = Duration::from_millis(30);
        s.transaction_timeout = Duration::from_millis(10);
        let err = unchecked(&mut s, 0x1001).unwrap_err();
        assert!(matches!(err, PtpUsbError::TransactionTimedOut(_)), "{err:?}");
        // Before the deadline the same transport error stays a USB error.
        let (mut s, _t) = session(vec![], "Z 9");
        let err = unchecked(&mut s, 0x1001).unwrap_err();
        assert!(matches!(err, PtpUsbError::Usb(rusb::Error::Timeout)), "{err:?}");
    }

    // ── writes ───────────────────────────────────────────────────────────

    #[test]
    fn a_short_command_write_is_an_error_not_a_success() {
        // Finding #27: write_bulk's byte count used to be discarded.
        let (mut s, t) = session(vec![resp(RESP_OK)], "Z 9");
        t.0.borrow_mut().write_caps = VecDeque::from([11]);
        let err = s.transact(0x1001, &[], None, TxidCheck::Unchecked).unwrap_err();
        assert!(matches!(err, PtpUsbError::ShortWrite { written: 11, expected: 12 }), "{err:?}");
        assert_eq!(t.reads_served(), 0, "no reply was waited for after a failed send");
    }

    #[test]
    fn a_short_data_write_is_an_error_not_a_success() {
        let (mut s, t) = session(vec![resp(RESP_OK)], "Z 9");
        t.0.borrow_mut().write_caps = VecDeque::from([12, 13]);
        let err = s.transact(0x90EE, &[], Some(&[1, 2, 3, 4]), TxidCheck::Unchecked).unwrap_err();
        assert!(matches!(err, PtpUsbError::ShortWrite { written: 13, expected: 16 }), "{err:?}");
    }

    #[test]
    fn a_container_that_is_a_multiple_of_the_packet_size_is_followed_by_a_zero_length_packet() {
        // Finding #27: container lengths of 511 / 512 / 513 / 1024 against a
        // 512-byte endpoint, as a data-out container (header 12 + payload).
        for (payload, zlps) in [(499usize, 0usize), (500, 1), (501, 0), (1012, 1), (0, 0)] {
            let (mut s, t) = session(vec![resp(RESP_OK)], "Z 9");
            s.transact(0x90EE, &[], Some(&vec![0u8; payload]), TxidCheck::Unchecked).unwrap();
            assert_eq!(t.zlps(), zlps, "payload {payload} -> container {}", payload + 12);
        }
    }

    #[test]
    fn the_zero_length_packet_follows_the_container_it_terminates() {
        let (mut s, t) = session(vec![resp(RESP_OK)], "Z 9");
        s.transact(0x90EE, &[], Some(&vec![7u8; 500]), TxidCheck::Unchecked).unwrap();
        let writes = t.0.borrow().writes.clone();
        let lens: Vec<usize> = writes.iter().map(|(_, w)| w.len()).collect();
        assert_eq!(lens, vec![12, 512, 0], "command, 512-byte data container, then its zero-length packet");
        assert!(writes.iter().all(|(ep, _)| *ep == 0x02));
    }

    #[test]
    fn no_zero_length_packet_when_the_endpoint_packet_size_is_unknown() {
        let (mut s, t) = session(vec![resp(RESP_OK)], "Z 9");
        s.max_packet_out = 0;
        s.transact(0x90EE, &[], Some(&vec![7u8; 500]), TxidCheck::Unchecked).unwrap();
        assert_eq!(t.zlps(), 0);
    }

    #[test]
    fn a_short_write_of_a_packet_multiple_container_gets_no_zero_length_packet() {
        let (mut s, t) = session(vec![resp(RESP_OK)], "Z 9");
        // Command in full, then only 100 of the 512-byte data container.
        t.0.borrow_mut().write_caps = VecDeque::from([12, 100]);
        let err = s.transact(0x90EE, &[], Some(&vec![7u8; 500]), TxidCheck::Unchecked).unwrap_err();
        assert!(matches!(err, PtpUsbError::ShortWrite { written: 100, expected: 512 }), "{err:?}");
        assert_eq!(t.zlps(), 0, "no ZLP after a short container");
    }

    // ── response transaction-id enforcement (7b2e N1) ──────────────────────

    #[test]
    fn enforce_accepts_a_matching_response_id() {
        let (mut s, _t) = session(vec![resp(RESP_OK)], "Z 9");
        assert_eq!(s.transact(0x1001, &[], None, TxidCheck::Enforce).unwrap().code, RESP_OK);
    }

    #[test]
    fn enforce_accepts_matching_ids_after_the_first_transaction() {
        // The transport stamps what the camera would: with the id moving on,
        // every reply carries its own command's id.
        let (mut s, _t) = session(vec![resp(RESP_OK), resp(RESP_OK)], "Z 9");
        s.transact(0x1001, &[], None, TxidCheck::Enforce).unwrap();
        s.transact(0x1001, &[], None, TxidCheck::Enforce).unwrap();
    }

    #[test]
    fn enforce_rejects_a_response_with_a_different_id_both_directions() {
        // One above and one below the expected id (3): the contract is
        // equality, not ordering.
        for got in [4u32, 2u32] {
            let (mut s, _t) = session(vec![resp_id(RESP_OK, got)], "Z 9");
            s.transaction_id = 3;
            let err = s.transact(0x1001, &[], None, TxidCheck::Enforce).unwrap_err();
            assert!(
                matches!(err, PtpUsbError::TransactionIdMismatch { op: 0x1001, expected: 3, got: g } if g == got),
                "{err:?}"
            );
            assert_eq!(s.transaction_id, 4, "the id was spent when the command went out");
            assert!(s.poisoned.is_some(), "a stale reply means the stream is out of step");
        }
    }

    #[test]
    fn enforce_rejects_a_data_container_with_a_different_id() {
        let (mut s, _t) = session(vec![Step::Bytes(encode_data(0x943B, 9, &[1]).unwrap()), resp(RESP_OK)], "Z 9");
        let err = s.transact(0x943B, &[], None, TxidCheck::Enforce).unwrap_err();
        assert!(matches!(err, PtpUsbError::TransactionIdMismatch { expected: 0, got: 9, .. }), "{err:?}");
    }

    #[test]
    fn enforce_ignores_the_id_on_event_and_unrecognized_containers() {
        // Events carry their own ids (often 0xFFFFFFFF); they are skipped
        // before the check, as they were before enforcement existed.
        let event = Step::Bytes(container(CONTAINER_EVENT, 0x4006, 0xFFFF_FFFF, &[]));
        let weird = Step::Bytes(container(9, 0, 77, &[]));
        let (mut s, _t) = session(vec![event, weird, resp(RESP_OK)], "Z 9");
        assert_eq!(s.transact(0x1001, &[], None, TxidCheck::Enforce).unwrap().code, RESP_OK);
    }

    #[test]
    fn enforce_rejects_a_data_container_for_another_operation() {
        // Finding #56: the data container's op code was never compared.
        let (mut s, _t) = session(vec![data_c(0x9999, &[1]), resp(RESP_OK)], "Z 9");
        let err = s.transact(0x943B, &[], None, TxidCheck::Enforce).unwrap_err();
        assert!(matches!(err, PtpUsbError::UnexpectedDataOp { op: 0x943B, got: 0x9999 }), "{err:?}");
    }

    #[test]
    fn unchecked_accepts_a_mismatched_response_id_and_records_it() {
        let (mut s, _t) = session(vec![resp_id(RESP_OK, 999)], "Z 9");
        let r = unchecked(&mut s, 0x1001).unwrap();
        assert_eq!(r.code, RESP_OK);
        assert_eq!(r.deviations, vec![ReplyDeviation::TransactionId { got: 999 }]);
    }

    #[test]
    fn unchecked_records_a_data_container_for_another_operation() {
        let (mut s, _t) = session(vec![data_c(0x9999, &[1]), resp(RESP_OK)], "Z 9");
        let r = unchecked(&mut s, 0x943B).unwrap();
        assert_eq!(r.data, vec![1]);
        assert_eq!(r.deviations, vec![ReplyDeviation::DataOp { got: 0x9999 }]);
    }

    #[test]
    fn a_clean_unchecked_reply_records_no_deviation() {
        let (mut s, _t) = session(vec![resp(RESP_OK)], "Z 9");
        assert!(unchecked(&mut s, 0x1001).unwrap().deviations.is_empty());
    }

    #[test]
    fn read_path_enforces_the_id_end_to_end() {
        // OpenSession ok (id 0), then the vendor read's reply carries the
        // wrong id: the caller must see the mismatch, not a value.
        let steps = vec![
            resp(RESP_OK),
            Step::Bytes(encode_data(OP_VENDOR_PROP_READ, 7, &[1]).unwrap()),
            Step::Bytes(container(CONTAINER_RESPONSE, RESP_OK, 7, &[])),
        ];
        let (mut s, _t) = session(steps, "Z 9");
        s.open_ptp_session().unwrap();
        let err = s.read_vendor_property(0xD053).unwrap_err();
        assert!(matches!(err, PtpUsbError::TransactionIdMismatch { op: OP_VENDOR_PROP_READ, .. }), "{err:?}");
    }

    #[test]
    fn open_session_enforces_the_id() {
        let (mut s, _t) = session(vec![resp_id(RESP_OK, 5)], "Z 9");
        let err = s.open_ptp_session().unwrap_err();
        assert!(matches!(err, PtpUsbError::TransactionIdMismatch { op: OP_OPEN_SESSION, .. }), "{err:?}");
    }

    // ── poisoning ────────────────────────────────────────────────────────

    #[test]
    fn a_failed_transaction_clears_both_halts_drops_buffered_bytes_and_refuses_the_next_one() {
        // Finding #53.
        let (mut s, t) = session(vec![Step::Err(rusb::Error::Timeout), resp(RESP_OK)], "Z 9");
        s.rx.extend_from_slice(&[1, 2, 3]); // stale bytes from before
        assert!(unchecked(&mut s, 0x1001).is_err());
        assert_eq!(t.halts(), vec![0x81, 0x02], "bulk-IN then bulk-OUT");
        assert!(s.rx.is_empty());
        let before = t.sent().len();
        let err = unchecked(&mut s, 0x1001).unwrap_err();
        assert!(matches!(&err, PtpUsbError::SessionPoisoned { cause } if cause.contains("Operation timed out")), "{err:?}");
        assert_eq!(t.sent().len(), before, "the refused transaction sent nothing");
        assert_eq!(t.unread(), 1, "and read nothing");
    }

    #[test]
    fn a_failing_clear_halt_is_part_of_the_recorded_cause() {
        let (mut s, t) = session(vec![Step::Err(rusb::Error::Timeout)], "Z 9");
        t.0.borrow_mut().fail_halt = true;
        assert!(unchecked(&mut s, 0x1001).is_err());
        let cause = s.poisoned.clone().unwrap();
        assert!(cause.contains("clear_halt(0x81) failed"), "{cause}");
        assert!(cause.contains("clear_halt(0x02) failed"), "{cause}");
    }

    #[test]
    fn every_kind_of_stream_failure_poisons_and_only_in_sync_failures_do_not() {
        let cases: Vec<(Vec<Step>, bool)> = vec![
            (vec![Step::Err(rusb::Error::Pipe)], true),
            (vec![Step::Bytes(vec![1, 2, 3]), Step::Bytes(vec![])], true), // ShortContainer
            (vec![Step::Bytes(header(40)), Step::Bytes(vec![])], true),    // Truncated
            (vec![resp_id(RESP_OK, 99)], true),                            // id mismatch
            (vec![Step::Bytes(container(CONTAINER_RESPONSE, RESP_OK, 0, &[1]))], false), // misaligned params
            (vec![resp(RESP_OK)], false),
        ];
        for (i, (steps, poisons)) in cases.into_iter().enumerate() {
            let (mut s, _t) = session(steps, "Z 9");
            let _ = s.transact(0x1001, &[], None, TxidCheck::Enforce);
            assert_eq!(s.poisoned.is_some(), poisons, "case {i}");
        }
    }

    #[test]
    fn dropping_a_poisoned_open_session_skips_the_close_but_releases_the_interface() {
        let (mut s, t) = session(vec![resp(RESP_OK), Step::Err(rusb::Error::Timeout)], "Z 9");
        s.open_ptp_session().unwrap();
        assert!(unchecked(&mut s, 0x943B).is_err());
        let sent_before = t.sent().len();
        drop(s);
        assert_eq!(t.sent().len(), sent_before, "no CloseSession into a possibly stalled pipe");
        assert_eq!(t.released(), vec![0]);
    }

    // ── close ────────────────────────────────────────────────────────────

    #[test]
    fn close_session_checks_the_id_and_the_response_code() {
        // Finding #91/#75: the close was `let _`-swallowed; the helper
        // returns what the camera said, so Enforce on close is observable.
        let (mut s, _t) = session(vec![resp(RESP_OK)], "Z 9");
        s.session_open = true;
        s.close_session(CLOSE_TIMEOUT).unwrap();
        assert!(!s.session_open);

        let (mut s, _t) = session(vec![resp_id(RESP_OK, 9)], "Z 9");
        s.session_open = true;
        let err = s.close_session(CLOSE_TIMEOUT).unwrap_err();
        assert!(matches!(err, PtpUsbError::TransactionIdMismatch { op: OP_CLOSE_SESSION, expected: 0, got: 9 }), "{err:?}");

        let (mut s, _t) = session(vec![resp(0x2003)], "Z 9");
        s.session_open = true;
        let err = s.close_session(CLOSE_TIMEOUT).unwrap_err();
        assert!(
            matches!(err, PtpUsbError::OperationFailed { op: OP_CLOSE_SESSION, code: PtpResponseCode(0x2003) }),
            "{err:?}"
        );
        assert!(s.session_open, "a refused close leaves the session marked open");
    }

    #[test]
    fn close_session_honours_its_own_timeout() {
        let (mut s, _t) = session(vec![resp(RESP_OK)], "Z 9");
        s.session_open = true;
        let err = s.close_session(Duration::ZERO).unwrap_err();
        assert!(matches!(err, PtpUsbError::TransactionTimedOut(d) if d.is_zero()), "{err:?}");
    }

    // ── vendor ops ───────────────────────────────────────────────────────

    #[test]
    fn read_vendor_property_sends_flagged_propcode_and_returns_data() {
        let (mut s, t) = session(vec![data_c(OP_VENDOR_PROP_READ, &[0xAA, 0xBB]), resp(RESP_OK)], "Z 9");
        assert_eq!(s.read_vendor_property(0xD053).unwrap(), vec![0xAA, 0xBB]);
        assert_eq!(t.sent(), vec![(CONTAINER_COMMAND, OP_VENDOR_PROP_READ, 0, le(0x1D053))]);
    }

    // ── 0x944C query ─────────────────────────────────────────────────────
    // The reply bytes below are SYNTHETIC (the real one is in pcaps that are not in this repo); they pin
    // framing and reporting, not what a camera says.

    fn query_reply_bytes() -> Vec<u8> {
        let mut v = vec![0u8; 32];
        v[..3].copy_from_slice(&[1, 1, 1]);
        v
    }

    #[test]
    fn query_vendor_op_sends_one_command_with_the_target_and_a_zero_and_nothing_else() {
        let (mut s, t) = session(vec![data_c(OP_VENDOR_OP_QUERY, &query_reply_bytes()), resp(RESP_OK)], "Z 9");
        let q = s.query_vendor_op(0x90ED).unwrap();
        let mut params = le(0x90ED);
        params.extend(le(0));
        assert_eq!(
            t.sent(),
            vec![(CONTAINER_COMMAND, OP_VENDOR_OP_QUERY, 0, params)],
            "0x944C(target, 0): no setup step, no data-out"
        );
        assert!(q.is_ok() && q.reply_matched_command && q.warnings.is_empty());
        assert_eq!((q.target_op, q.data_containers, q.data), (0x90ED, 1, query_reply_bytes()));
    }

    #[test]
    fn query_vendor_op_target_is_sent_zero_extended_at_the_edges_of_the_u16() {
        // 0x8000 is the sign bit of an i16, 0xFFFF the top: neither may sign-extend into the u32 param.
        for target in [0x0000u16, 0x7FFF, 0x8000, 0x9FFF, 0xFFFF] {
            let (mut s, t) = session(vec![resp(RESP_OK)], "Z 9");
            s.query_vendor_op(target).unwrap();
            let mut params = le(u32::from(target));
            params.extend(le(0));
            assert_eq!(t.sent(), vec![(CONTAINER_COMMAND, OP_VENDOR_OP_QUERY, 0, params)], "target 0x{target:04x}");
        }
    }

    #[test]
    fn query_vendor_op_reports_a_non_ok_code_as_data_not_as_an_error() {
        let (mut s, _t) = session(vec![resp(0x2005)], "Z 9");
        let q = s.query_vendor_op(0x9FFF).unwrap();
        assert_eq!(q.response_code, 0x2005);
        assert!(!q.is_ok(), "a non-OK code must never read as OK");
        assert_eq!((q.data_containers, q.data.len()), (0, 0));
    }

    #[test]
    fn query_vendor_op_keeps_no_data_phase_apart_from_an_empty_one() {
        let (mut none, _t) = session(vec![resp(RESP_OK)], "Z 9");
        let a = none.query_vendor_op(0x90ED).unwrap();
        let (mut empty, _t) = session(vec![data_c(OP_VENDOR_OP_QUERY, &[]), resp(RESP_OK)], "Z 9");
        let b = empty.query_vendor_op(0x90ED).unwrap();
        assert_eq!((a.data_containers, b.data_containers), (0, 1));
        assert!(a.data.is_empty() && b.data.is_empty());
        assert_ne!(a, b, "OK-with-no-data and OK-with-empty-data are different answers");
    }

    #[test]
    fn query_vendor_op_tolerates_a_mismatched_transaction_id_and_says_so() {
        let (mut s, _t) = session(vec![resp_id(RESP_OK, 99)], "Z 9");
        let q = s.query_vendor_op(0x90ED).unwrap();
        assert!(q.is_ok() && !q.reply_matched_command);
    }

    #[test]
    fn query_vendor_op_warns_on_an_unsurveyed_model_but_still_asks() {
        let (mut s, t) = session(vec![resp(RESP_OK)], "Z 99");
        let q = s.query_vendor_op(0x90ED).unwrap();
        assert_eq!(q.warnings.len(), 1, "{:?}", q.warnings);
        assert_eq!(t.sent().len(), 1);
    }

    #[test]
    fn query_vendor_op_transport_failure_is_an_error_and_poisons_the_session() {
        let (mut s, _t) = session(vec![Step::Err(rusb::Error::Pipe)], "Z 9");
        assert!(matches!(s.query_vendor_op(0x90ED), Err(PtpUsbError::Usb(rusb::Error::Pipe))));
        assert!(matches!(s.query_vendor_op(0x90ED), Err(PtpUsbError::SessionPoisoned { .. })), "a second query must not touch the pipe");
    }

    #[test]
    fn read_vendor_property_non_ok_response_is_operation_failed() {
        let (mut s, _t) = session(vec![resp(0x2002)], "Z 9");
        let err = s.read_vendor_property(0xD053).unwrap_err();
        assert!(
            matches!(err, PtpUsbError::OperationFailed { op: OP_VENDOR_PROP_READ, code: PtpResponseCode(0x2002) }),
            "{err:?}"
        );
    }

    #[test]
    fn read_vendor_property_ok_without_any_data_container_is_an_error() {
        // Finding #89: this used to be Ok(empty), indistinguishable from a
        // property that is genuinely empty.
        let (mut s, _t) = session(vec![resp(RESP_OK)], "Z 9");
        let err = s.read_vendor_property(0xD053).unwrap_err();
        assert!(matches!(err, PtpUsbError::NoDataPhase { op: OP_VENDOR_PROP_READ }), "{err:?}");
    }

    #[test]
    fn read_vendor_property_with_an_empty_data_container_is_ok_and_empty() {
        let (mut s, _t) = session(vec![data_c(OP_VENDOR_PROP_READ, &[]), resp(RESP_OK)], "Z 9");
        assert_eq!(s.read_vendor_property(0xD053).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn read_vendor_property_on_an_unsurveyed_model_still_reads_and_reports_a_warning() {
        // Pins: unlike the write, the read only warns — and the warning is
        // data the caller can show, not just stderr.
        let (mut s, t) = session(vec![data_c(OP_VENDOR_PROP_READ, &[1]), resp(RESP_OK)], "Z 50");
        let read = s.read_vendor_property_report(0xD053).unwrap();
        assert_eq!(read.data, vec![1]);
        assert_eq!(read.warnings.len(), 1);
        assert!(read.warnings[0].contains("\"Z 50\""), "{}", read.warnings[0]);
        assert_eq!(t.sent().len(), 1);
    }

    #[test]
    fn read_vendor_property_warns_for_exactly_the_models_outside_the_registry() {
        for m in KNOWN_MODELS {
            let (mut s, _t) = session(vec![data_c(OP_VENDOR_PROP_READ, &[1]), resp(RESP_OK)], m);
            assert!(s.read_vendor_property_report(0xD053).unwrap().warnings.is_empty(), "{m}");
        }
        for m in ["", "Z 50", "Z6_3 ", "z 9"] {
            let (mut s, _t) = session(vec![data_c(OP_VENDOR_PROP_READ, &[1]), resp(RESP_OK)], m);
            assert_eq!(s.read_vendor_property_report(0xD053).unwrap().warnings.len(), 1, "{m:?}");
        }
    }

    // ── GetDeviceInfo (0x1001) ───────────────────────────────────────────
    //
    // The parser itself is pinned by tests/followup_device_info_parse.rs; these are the session's
    // half: what is sent, what is checked, and what a failure does to the session.

    fn pstr(s: &str) -> Vec<u8> {
        if s.is_empty() {
            return vec![0];
        }
        let units: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
        let mut out = vec![units.len() as u8];
        out.extend(units.iter().flat_map(|u| u.to_le_bytes()));
        out
    }

    fn u16_arr(codes: &[u16]) -> Vec<u8> {
        let mut out = (codes.len() as u32).to_le_bytes().to_vec();
        out.extend(codes.iter().flat_map(|c| c.to_le_bytes()));
        out
    }

    /// A DeviceInfo dataset laid out from ISO 15740: a Nikon-looking Z 9 with the given serial.
    fn device_info_bytes(serial: &str) -> Vec<u8> {
        [
            100u16.to_le_bytes().to_vec(),
            0x0au32.to_le_bytes().to_vec(),
            100u16.to_le_bytes().to_vec(),
            pstr("Nikon"),
            0u16.to_le_bytes().to_vec(),
            u16_arr(&[0x1001, 0x1002, 0x1003, 0x943B]),
            u16_arr(&[0x4006]),
            u16_arr(&[0x5001, 0xD053]),
            u16_arr(&[0x3801]),
            u16_arr(&[0x3000, 0x3801]),
            pstr("Nikon Corporation"),
            pstr("Z 9"),
            pstr("5.31"),
            pstr(serial),
        ]
        .concat()
    }

    #[test]
    fn get_device_info_sends_a_parameterless_1001_and_parses_the_data_phase() {
        let (mut s, t) = session(vec![data_c(OP_GET_DEVICE_INFO, &device_info_bytes("0000003023668")), resp(RESP_OK)], "Z 9");
        let d = s.get_device_info().unwrap();
        assert_eq!(t.sent(), vec![(CONTAINER_COMMAND, 0x1001, 0, vec![])]);
        assert_eq!(OP_GET_DEVICE_INFO, 0x1001);
        assert_eq!(d.serial_number.text, "0000003023668");
        assert_eq!(d.model.text, "Z 9");
        assert_eq!(d.operations_supported, [0x1001, 0x1002, 0x1003, 0x943B]);
        assert_eq!(t.unread(), 0);
    }

    #[test]
    fn get_device_info_reassembles_a_dataset_split_over_data_containers() {
        let raw = device_info_bytes("0000003023668");
        let (a, b) = raw.split_at(raw.len() / 2);
        let (mut s, _t) = session(vec![data_c(OP_GET_DEVICE_INFO, a), data_c(OP_GET_DEVICE_INFO, b), resp(RESP_OK)], "Z 9");
        assert_eq!(s.get_device_info().unwrap().serial_number.text, "0000003023668");
    }

    #[test]
    fn get_device_info_runs_after_open_session_with_the_next_transaction_id() {
        let (mut s, t) = session(vec![resp(RESP_OK), data_c(OP_GET_DEVICE_INFO, &device_info_bytes("")), resp(RESP_OK)], "Z 9");
        s.open_ptp_session().unwrap();
        assert_eq!(s.get_device_info().unwrap().serial_number.text, "");
        assert_eq!(
            t.sent(),
            vec![(CONTAINER_COMMAND, OP_OPEN_SESSION, 0, le(SESSION_ID)), (CONTAINER_COMMAND, 0x1001, 1, vec![])]
        );
    }

    #[test]
    fn get_device_info_non_ok_response_is_operation_failed_and_the_data_is_not_parsed() {
        // A garbage data phase followed by an error response: the response code wins, the
        // dataset is not looked at.
        let (mut s, _t) = session(vec![data_c(OP_GET_DEVICE_INFO, &[1, 2, 3]), resp(0x2005)], "Z 9");
        let err = s.get_device_info().unwrap_err();
        assert!(
            matches!(err, PtpUsbError::OperationFailed { op: OP_GET_DEVICE_INFO, code: PtpResponseCode(0x2005) }),
            "{err:?}"
        );
    }

    #[test]
    fn get_device_info_ok_without_any_data_container_is_an_error() {
        let (mut s, _t) = session(vec![resp(RESP_OK)], "Z 9");
        let err = s.get_device_info().unwrap_err();
        assert!(matches!(err, PtpUsbError::NoDataPhase { op: OP_GET_DEVICE_INFO }), "{err:?}");
    }

    #[test]
    fn get_device_info_with_an_empty_data_container_is_malformed_not_an_empty_dataset() {
        let (mut s, _t) = session(vec![data_c(OP_GET_DEVICE_INFO, &[]), resp(RESP_OK)], "Z 9");
        let err = s.get_device_info().unwrap_err();
        assert!(
            matches!(err, PtpUsbError::DeviceInfoMalformed { field: "StandardVersion", offset: 0, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_malformed_dataset_leaves_the_session_usable() {
        // The response was fully read, so the pipes are in sync: the next read works.
        let raw = device_info_bytes("0000003023668");
        let (mut s, t) = session(
            vec![
                data_c(OP_GET_DEVICE_INFO, &raw[..raw.len() - 1]),
                resp(RESP_OK),
                data_c(OP_GET_DEVICE_INFO, &raw),
                resp(RESP_OK),
            ],
            "Z 9",
        );
        let err = s.get_device_info().unwrap_err();
        assert!(matches!(err, PtpUsbError::DeviceInfoMalformed { field: "SerialNumber", .. }), "{err:?}");
        assert!(s.poisoned.is_none());
        assert_eq!(s.get_device_info().unwrap().serial_number.text, "0000003023668");
        assert_eq!(t.sent().iter().map(|c| c.2).collect::<Vec<_>>(), [0, 1], "each attempt consumed its own transaction id");
    }

    #[test]
    fn get_device_info_enforces_the_transaction_id_of_the_data_and_response() {
        let data = |id| Step::Bytes(encode_data(OP_GET_DEVICE_INFO, id, &device_info_bytes("X")).unwrap());
        let (mut s, _t) = session(vec![data(9), resp(RESP_OK)], "Z 9");
        let err = s.get_device_info().unwrap_err();
        assert!(matches!(err, PtpUsbError::TransactionIdMismatch { op: OP_GET_DEVICE_INFO, expected: 0, got: 9 }), "{err:?}");
        assert!(s.poisoned.is_some(), "a stale reply may still be queued: the session must not be reused");

        let (mut s, _t) = session(vec![data_c(OP_GET_DEVICE_INFO, &device_info_bytes("X")), resp_id(RESP_OK, 4)], "Z 9");
        let err = s.get_device_info().unwrap_err();
        assert!(matches!(err, PtpUsbError::TransactionIdMismatch { op: OP_GET_DEVICE_INFO, expected: 0, got: 4 }), "{err:?}");
    }

    #[test]
    fn get_device_info_refuses_a_data_container_for_another_operation() {
        let (mut s, _t) = session(vec![data_c(OP_VENDOR_PROP_READ, &device_info_bytes("X")), resp(RESP_OK)], "Z 9");
        let err = s.get_device_info().unwrap_err();
        assert!(matches!(err, PtpUsbError::UnexpectedDataOp { op: OP_GET_DEVICE_INFO, got: OP_VENDOR_PROP_READ }), "{err:?}");
    }

    #[test]
    fn a_failed_device_info_read_poisons_the_session_so_the_next_call_sends_nothing() {
        let (mut s, t) = session(vec![Step::Err(rusb::Error::Timeout)], "Z 9");
        assert!(matches!(s.get_device_info().unwrap_err(), PtpUsbError::Usb(rusb::Error::Timeout)));
        let sent = t.sent().len();
        assert!(matches!(s.get_device_info().unwrap_err(), PtpUsbError::SessionPoisoned { .. }));
        assert_eq!(t.sent().len(), sent, "a poisoned session writes nothing");
    }

    #[test]
    fn the_report_carries_the_usb_identity_beside_the_ptp_serial_and_its_verdict() {
        for (usb_serial, ptp_serial, verdict) in [
            ("0000003023668", "0000003023668", "match"),
            ("0000003023669", "0000003023668", "differ"),
            ("", "0000003023668", "usb_empty"),
            ("0000003023668", "", "ptp_empty"),
            ("", "", "both_empty"),
        ] {
            let (mut s, _t) = session(vec![data_c(OP_GET_DEVICE_INFO, &device_info_bytes(ptp_serial)), resp(RESP_OK)], "Z 9");
            s.usb_serial = usb_serial.to_string();
            s.usb_warnings = vec!["reading the USB serial string failed: timeout".to_string()];
            let report = s.read_device_info_report().unwrap();
            assert_eq!(report.usb_serial, usb_serial);
            assert_eq!(report.usb_model, "Z 9");
            assert_eq!(report.usb_warnings, ["reading the USB serial string failed: timeout"]);
            assert_eq!(report.dataset.serial_number.text, ptp_serial);
            assert_eq!(report.serial_verdict().tag(), verdict, "USB {usb_serial:?} PTP {ptp_serial:?}");
        }
    }

    #[test]
    fn a_device_info_read_failure_yields_no_report() {
        let (mut s, _t) = session(vec![resp(0x2002)], "Z 9");
        assert!(matches!(s.read_device_info_report().unwrap_err(), PtpUsbError::OperationFailed { .. }));
    }

    #[test]
    fn write_ftp_profile_sends_setup_then_profile_with_incrementing_ids() {
        let profile = valid_profile();
        let (mut s, t) = session(vec![resp(RESP_OK), resp(RESP_OK)], "Z 9");
        let receipt = s.write_ftp_profile(&profile, false).unwrap();
        let blob = encode_ftp_profile(&profile).unwrap();
        assert_eq!(
            t.sent(),
            vec![
                (CONTAINER_COMMAND, OP_FTP_SETUP, 0, vec![]),
                (CONTAINER_DATA, OP_FTP_SETUP, 0, FTP_SETUP_DATA.to_vec()),
                (CONTAINER_COMMAND, OP_FTP_PROFILE_WRITE, 1, [le(0), le(0)].concat()),
                (CONTAINER_DATA, OP_FTP_PROFILE_WRITE, 1, blob),
            ]
        );
        assert_eq!(receipt, FtpWriteReceipt { response_params: vec![], reply_matched_command: true });
    }

    #[test]
    fn write_ftp_profile_receipt_carries_the_response_params() {
        let reply = Step::Stamped(vec![(CONTAINER_RESPONSE, RESP_OK, le(0xAB))]);
        let (mut s, _t) = session(vec![resp(RESP_OK), reply], "Z 9");
        assert_eq!(s.write_ftp_profile(&valid_profile(), false).unwrap().response_params, vec![0xAB]);
    }

    #[test]
    fn write_ftp_profile_with_an_explicit_network_sends_that_network() {
        let net = FtpCameraNetwork { ipv4: [10, 0, 0, 9], prefix_len: 8 };
        let (mut s, t) = session(vec![resp(RESP_OK), resp(RESP_OK)], "Z 9");
        s.write_ftp_profile_with(&valid_profile(), &net, false).unwrap();
        let sent = t.sent();
        assert_eq!(sent[3].3, encode_ftp_profile_with(&valid_profile(), &net).unwrap());
        assert_ne!(sent[3].3, encode_ftp_profile(&valid_profile()).unwrap());
    }

    #[test]
    fn write_ftp_profile_setup_failure_stops_before_the_profile_write() {
        let (mut s, t) = session(vec![resp(0x2002)], "Z 9");
        let err = s.write_ftp_profile(&valid_profile(), false).unwrap_err();
        assert!(
            matches!(err, PtpUsbError::OperationFailed { op: OP_FTP_SETUP, code: PtpResponseCode(0x2002) }),
            "{err:?}"
        );
        assert_eq!(t.sent().len(), 2, "only the setup command+data; the 0x90EE profile blob is never sent");
    }

    #[test]
    fn write_ftp_profile_write_failure_is_reported_against_the_profile_op() {
        // The camera ANSWERED with a refusal: a definite failure, not an
        // indeterminate one.
        let (mut s, _t) = session(vec![resp(RESP_OK), resp(0x2002)], "Z 9");
        let err = s.write_ftp_profile(&valid_profile(), false).unwrap_err();
        assert!(
            matches!(err, PtpUsbError::OperationFailed { op: OP_FTP_PROFILE_WRITE, code: PtpResponseCode(0x2002) }),
            "{err:?}"
        );
    }

    #[test]
    fn a_write_that_fails_after_the_data_went_out_is_indeterminate() {
        // Finding #54: the 0x90EE data was sent and the reply never came.
        let (mut s, t) = session(vec![resp(RESP_OK), Step::Err(rusb::Error::Timeout)], "Z 9");
        let err = s.write_ftp_profile(&valid_profile(), false).unwrap_err();
        match &err {
            PtpUsbError::FtpWriteIndeterminate { cause } => {
                assert!(matches!(**cause, PtpUsbError::Usb(rusb::Error::Timeout)), "{cause:?}");
            }
            other => panic!("expected FtpWriteIndeterminate, got {other:?}"),
        }
        assert_eq!(t.sent().len(), 4, "the blob did go out");
        let text = err.to_string();
        assert!(text.contains("may or may not have taken effect"), "{text}");
        assert!(text.contains("camera's menu"), "{text}");
    }

    #[test]
    fn a_garbled_reply_after_the_data_is_indeterminate_too() {
        let bad = Step::Bytes(container(CONTAINER_RESPONSE, RESP_OK, 1, &[1, 2, 3]));
        let (mut s, _t) = session(vec![resp(RESP_OK), bad], "Z 9");
        let err = s.write_ftp_profile(&valid_profile(), false).unwrap_err();
        assert!(
            matches!(&err, PtpUsbError::FtpWriteIndeterminate { cause } if matches!(**cause, PtpUsbError::MisalignedResponseParams(3))),
            "{err:?}"
        );
    }

    #[test]
    fn a_write_that_fails_sending_the_data_container_is_indeterminate() {
        // 0x90E8 setup: command (12) + data (15), then the 0x90EE command (20) in full and a short data write.
        let (mut s, _t) = session(vec![resp(RESP_OK)], "Z 9");
        s.handle.0.borrow_mut().write_caps = VecDeque::from([12, 15, 20, 40]);
        let err = s.write_ftp_profile(&valid_profile(), false).unwrap_err();
        assert!(
            matches!(&err, PtpUsbError::FtpWriteIndeterminate { cause } if matches!(**cause, PtpUsbError::ShortWrite { written: 40, .. })),
            "{err:?}"
        );
    }

    #[test]
    fn a_write_that_never_got_its_data_out_is_not_indeterminate() {
        // The 0x90EE *command* fails to send: nothing of the profile left the host.
        let (mut s, _t) = session(vec![resp(RESP_OK)], "Z 9");
        s.handle.0.borrow_mut().write_caps = VecDeque::from([12, 15, 5]);
        let err = s.write_ftp_profile(&valid_profile(), false).unwrap_err();
        assert!(matches!(err, PtpUsbError::ShortWrite { written: 5, expected: 20 }), "{err:?}");
    }

    #[test]
    fn a_setup_failure_is_not_indeterminate_even_though_it_sent_data() {
        // The setup's own data container is not the profile.
        let (mut s, _t) = session(vec![Step::Err(rusb::Error::Timeout)], "Z 9");
        let err = s.write_ftp_profile(&valid_profile(), false).unwrap_err();
        assert!(matches!(err, PtpUsbError::Usb(rusb::Error::Timeout)), "{err:?}");
    }

    #[test]
    fn a_poisoned_session_refuses_a_write_as_poisoned_not_indeterminate() {
        // data_phase_started must not carry over from the failed transaction.
        let (mut s, t) = session(vec![resp(RESP_OK), Step::Err(rusb::Error::Timeout)], "Z 9");
        assert!(s.write_ftp_profile(&valid_profile(), false).is_err());
        let before = t.sent().len();
        let err = s.write_ftp_profile(&valid_profile(), false).unwrap_err();
        assert!(matches!(err, PtpUsbError::SessionPoisoned { .. }), "{err:?}");
        assert_eq!(t.sent().len(), before);
    }

    #[test]
    fn ftp_write_path_stays_unchecked_with_mismatched_ids_but_says_so() {
        // Deliberate (see TxidCheck): no live capture of the write ops
        // exists, so their replies are not id-validated. Pinned so that
        // tightening it is a conscious change made alongside a capture —
        // and the mismatch is no longer silent: the receipt carries it.
        let steps = vec![resp_id(RESP_OK, 41), resp_id(RESP_OK, 42)];
        let (mut s, _t) = session(steps, "Z 9");
        let receipt = s.write_ftp_profile(&valid_profile(), false).unwrap();
        assert!(!receipt.reply_matched_command);
    }

    #[test]
    fn a_mismatch_on_the_setup_reply_alone_is_enough_to_clear_the_matched_flag() {
        let (mut s, _t) = session(vec![resp_id(RESP_OK, 41), resp(RESP_OK)], "Z 9");
        assert!(!s.write_ftp_profile(&valid_profile(), false).unwrap().reply_matched_command);
        let (mut s, _t) = session(vec![resp(RESP_OK), resp_id(RESP_OK, 42)], "Z 9");
        assert!(!s.write_ftp_profile(&valid_profile(), false).unwrap().reply_matched_command);
    }

    #[test]
    fn write_ftp_profile_refuses_an_unconfirmed_model_before_any_io() {
        let (mut s, t) = session(vec![resp(RESP_OK), resp(RESP_OK)], "Z 50");
        let err = s.write_ftp_profile(&valid_profile(), false).unwrap_err();
        assert!(matches!(&err, PtpUsbError::UnconfirmedModel(m) if m == "Z 50"), "{err:?}");
        assert!(t.sent().is_empty());
    }

    #[test]
    fn write_ftp_profile_refuses_a_surveyed_but_write_unconfirmed_model() {
        let (mut s, t) = session(vec![resp(RESP_OK), resp(RESP_OK)], "Z 5");
        let err = s.write_ftp_profile(&valid_profile(), false).unwrap_err();
        assert!(matches!(&err, PtpUsbError::UnconfirmedModel(m) if m == "Z 5"), "{err:?}");
        assert!(t.sent().is_empty());
    }

    #[test]
    fn write_ftp_profile_force_flag_overrides_the_model_gate() {
        let (mut s, t) = session(vec![resp(RESP_OK), resp(RESP_OK)], "Z 50");
        s.write_ftp_profile(&valid_profile(), true).unwrap();
        assert_eq!(t.sent().len(), 4);
    }

    #[test]
    fn write_ftp_profile_encoding_error_happens_before_any_io() {
        let mut p = valid_profile();
        p.ssid_24ghz = "café".to_string();
        let (mut s, t) = session(vec![resp(RESP_OK), resp(RESP_OK)], "Z 9");
        let err = s.write_ftp_profile(&p, false).unwrap_err();
        assert!(matches!(err, PtpUsbError::FtpFieldNotAscii { .. }), "{err:?}");
        assert!(t.sent().is_empty(), "a bad field must not leave a half-done setup call on the camera");
    }

    #[test]
    fn write_ftp_profile_over_length_ssid_happens_before_any_io() {
        let mut p = valid_profile();
        p.ssid_5ghz = "s".repeat(33);
        let (mut s, t) = session(vec![resp(RESP_OK), resp(RESP_OK)], "Z 9");
        let err = s.write_ftp_profile(&p, false).unwrap_err();
        assert!(matches!(err, PtpUsbError::FtpFieldTooLong { field: "ssid_5ghz", .. }), "{err:?}");
        assert!(t.sent().is_empty());
    }

    // ── teardown ─────────────────────────────────────────────────────────

    #[test]
    fn dropping_an_open_session_sends_close_then_releases_the_interface() {
        // Finding #91/#15: the reply carries the close's own id, so the
        // Enforce check on the close is genuinely satisfied rather than
        // mismatching and being swallowed.
        let (mut s, t) = session(vec![resp(RESP_OK), resp(RESP_OK)], "Z 9");
        s.open_ptp_session().unwrap();
        drop(s);
        let sent = t.sent();
        assert_eq!(sent.last().map(|f| f.1), Some(OP_CLOSE_SESSION));
        assert_eq!(sent.last().map(|f| f.2), Some(1), "the close is the second transaction");
        assert_eq!(sent.len(), 2);
        assert_eq!(t.released(), vec![0]);
        assert_eq!(t.unread(), 0, "the close's reply was consumed");
    }

    #[test]
    fn dropping_a_never_opened_session_releases_without_sending_close() {
        let (s, t) = session(vec![], "Z 9");
        drop(s);
        assert!(t.sent().is_empty());
        assert_eq!(t.released(), vec![0]);
    }

    #[test]
    fn drop_after_failed_open_does_not_send_close() {
        let (mut s, t) = session(vec![resp(0x2002)], "Z 9");
        assert!(s.open_ptp_session().is_err());
        drop(s);
        assert_eq!(t.sent().len(), 1, "only the failed OpenSession; no CloseSession for a session that never opened");
        assert_eq!(t.released(), vec![0]);
    }

    // ── camera lock ──────────────────────────────────────────────────────

    #[test]
    fn open_takes_the_camera_lock_before_it_looks_at_the_bus() {
        // Finding #02: the raw-USB path kills ptpcamerad/icdd and re-asserts
        // the configuration, which must not happen underneath another
        // nikon-fleet session. With the lock refused, the finder (which
        // enumerates and opens devices) must never run.
        let finder_ran = std::cell::Cell::new(false);
        let busy = || Err(LockError::Busy { path: std::path::PathBuf::from("/tmp/camera.lock") });
        let find = |_: Option<&str>| {
            finder_ran.set(true);
            Err(PtpUsbError::NoDevice)
        };
        let err = PtpUsbSession::open_with(busy, find, None).err().expect("open must fail");
        assert!(matches!(err, PtpUsbError::CameraLock(LockError::Busy { .. })), "{err:?}");
        assert!(err.to_string().contains("camera busy"), "{err}");
        assert!(!finder_ran.get(), "the bus was touched before the lock was taken");
    }

    #[test]
    fn open_reaches_the_finder_once_the_lock_is_held() {
        // The converse: with the lock granted, the finder runs and its error
        // is what comes back (a real lock on a temp path, no USB involved).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("camera.lock");
        let finder_ran = std::cell::Cell::new(false);
        let find = |_: Option<&str>| {
            finder_ran.set(true);
            Err(PtpUsbError::NoDevice)
        };
        let err = PtpUsbSession::open_with(|| CameraLock::acquire_at(&path), find, None).err().expect("open must fail");
        assert!(matches!(err, PtpUsbError::NoDevice), "{err:?}");
        assert!(finder_ran.get());
    }

    #[test]
    fn a_lock_taken_by_the_caller_is_handed_to_the_open_without_being_taken_again() {
        // open_locked: the caller took the lock first (to resolve an id-N against the bus). It must
        // still be held while the finder looks at the bus, and the open must not try to take it a
        // second time (flock refuses a second descriptor in the same process: it would be Busy).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("camera.lock");
        let lock = CameraLock::acquire_at(&path).unwrap();
        let held_during_find = std::cell::Cell::new(None);
        let find = |_: Option<&str>| {
            held_during_find.set(Some(matches!(CameraLock::acquire_at(&path), Err(LockError::Busy { .. }))));
            Err(PtpUsbError::NoDevice)
        };
        let err = PtpUsbSession::open_locked_with(lock, find, None).err().expect("open must fail");
        assert!(matches!(err, PtpUsbError::NoDevice), "the finder's error, not a self-inflicted camera_busy: {err:?}");
        assert_eq!(held_during_find.get(), Some(true), "the lock was not held while the bus was read");
        assert!(CameraLock::acquire_at(&path).is_ok(), "the failed open released the lock it was handed");
    }

    // ── FTP profile read (0x90E8 → 0x90EB → 0x90ED) ──────────────────────
    //
    // Fixtures are the real blobs from session-20260709-204600-ftptest.pcap
    // (Z 9, 2026-07-09). The 0x90EB replies are rebuilt from their decoded
    // fields with a dummy GUID and serial, so no body identity is committed.

    /// The 317-byte blob: profile "C2", empty folder. Seen as a `0x90EE`
    /// write, then read back three times byte-identically.
    fn ftp_blob_317_hex() -> &'static str {
        concat!(
            "f401034300320000001030003000300030003000300030003000540030003000300030003000",
            "3000000010300030003000300030003000300030005400300030003000300030003000000002",
            "03035742a8c0180000000000000000030001000b0000006e696b6f6e2d736e6f6f7004040900",
            "00006e696b6f6e3567687a010d3100390032002e003100360038002e00360036002e00310000",
            "0000150005000000666c6565740800000031323334353637380500901f000000000027000000",
            "303030303a303030303a303030303a303030303a303030303a303030303a303030303a303030",
            "304027000000303030303a303030303a303030303a303030303a303030303a303030303a3030",
            "30303a3030303027000000303030303a303030303a303030303a303030303a303030303a3030",
            "30303a303030303a3030303081",
        )
    }

    /// The 331-byte blob: profile "C2" read before a rewrite, with a non-empty
    /// string between host and username and `01` instead of `05` before the port.
    fn ftp_blob_331_hex() -> &'static str {
        concat!(
            "f40103430032000000103000300030003000300030003000300054003000300030003000300030",
            "000000103000300030003000300030003000300054003000300030003000300030000000020303",
            "5742a8c0180000000000000000030001000b0000006e696b6f6e2d736e6f6f700404090000006e",
            "696b6f6e3567687a010d3100390032002e003100360038002e00360036002e0031000000074300",
            "6c0061007500640065000000150005000000666c6565740800000031323334353637380100901f",
            "000000000027000000303030303a303030303a303030303a303030303a303030303a303030303a",
            "303030303a303030304027000000303030303a303030303a303030303a303030303a303030303a",
            "303030303a303030303a3030303027000000303030303a303030303a303030303a303030303a30",
            "3030303a303030303a303030303a3030303081",
        )
    }

    /// A `0x90EB` reply built from its decoded fields (dummy GUID).
    fn ftp_info_bytes(count: u32, slot: u32, name: &str) -> Vec<u8> {
        let mut v = vec![0xc8, 0x00];
        v.extend_from_slice(&le(count));
        v.extend_from_slice(&le(0x14));
        v.extend_from_slice(&le(slot));
        v.extend_from_slice(&[0xAB; 16]);
        v.extend_from_slice(&utf16_u8_len_bytes(&name.encode_utf16().collect::<Vec<_>>()));
        v.extend_from_slice(&[0xf4, 0x01]);
        v
    }

    #[test]
    fn decode_reference_blob_gives_every_field_and_reencodes_byte_for_byte() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let d = decode_ftp_profile(&blob).unwrap();
        assert_eq!(d.profile_name, "CLAUDE");
        assert_eq!(d.timestamps, ["00000000T000000", "00000000T000000"]);
        assert_eq!(d.camera_network, FtpCameraNetwork::CAPTURED);
        assert_eq!((d.ssid_24ghz.as_str(), d.ssid_5ghz.as_str()), ("nikon-snoop", "nikon5ghz"));
        assert_eq!((d.host.as_str(), d.folder.as_str()), ("192.168.66.1", ""));
        assert_eq!((d.username.as_str(), d.password.as_str()), ("fleet", "12345678"));
        assert_eq!((d.ftp_port, d.option_flags, d.second_port), (21, 0x0005, 8080));
        assert!(d.ipv6_fields.iter().all(|f| f == FTP_IPV6_PLACEHOLDER));
        let profile = d.as_encodable().expect("empty folder and the encoder's flag byte");
        assert_eq!(encode_ftp_profile(&profile).unwrap(), blob);
    }

    #[test]
    fn decode_317_blob_reencodes_byte_for_byte() {
        let blob = hex::decode_hex(ftp_blob_317_hex());
        let d = decode_ftp_profile(&blob).unwrap();
        assert_eq!(d.profile_name, "C2");
        assert_eq!(encode_ftp_profile(&d.as_encodable().unwrap()).unwrap(), blob);
    }

    #[test]
    fn decode_331_blob_finds_the_folder_the_encoder_cannot_write() {
        let blob = hex::decode_hex(ftp_blob_331_hex());
        assert_eq!(blob.len(), 331);
        let d = decode_ftp_profile(&blob).unwrap();
        assert_eq!(d.profile_name, "C2");
        assert_eq!(d.folder, "Claude");
        assert_eq!((d.ftp_port, d.option_flags, d.second_port), (21, 0x0001, 8080));
        assert_eq!((d.host.as_str(), d.username.as_str()), ("192.168.66.1", "fleet"));
        assert!(d.as_encodable().is_none(), "the encoder cannot carry a folder or flag byte 01");
    }

    /// A profile read over USB from a Z 9 on 2026-11-05 (354 B), rebuilt with
    /// placeholder names, host and credentials: same layout, SFTP-style port 22,
    /// option flags 0x0d, a /22 camera network. Its real counterpart is what
    /// showed that the u16 before the username is a port, not a constant.
    fn ftp_blob_port22_hex() -> &'static str {
        concat!(
            "f4010a500052004f00460049004c0045002d004200000010300030003000300030003000300030",
            "005400300030003000300030003000000010300030003000300030003000300030005400300030",
            "00300030003000300000000203035344a8c0160000000000000000030001000600000073736964",
            "2d61040406000000737369642d6201106600740070002e006500780061006d0070006c0065002e",
            "0063006f006d00000000160004000000757365720800000070617373313233340d00901f000000",
            "000027000000303030303a303030303a303030303a303030303a303030303a303030303a303030",
            "303a303030304027000000303030303a303030303a303030303a303030303a303030303a303030",
            "303a303030303a3030303027000000303030303a303030303a303030303a303030303a30303030",
            "3a303030303a303030303a3030303081",
        )
    }

    /// A phone-hotspot style profile (rebuilt with placeholders): 5 GHz-field
    /// tag `06 04`, empty username and password (anonymous login?), FTP port
    /// 2221, option flags 0x07, a 10.x /24 camera network.
    fn ftp_blob_anonymous_hex() -> &'static str {
        concat!(
            "f4010a500052004f00460049004c0045002d004300000010300030003000300030003000300030",
            "005400300030003000300030003000000010300030003000300030003000300030005400300030",
            "0030003000300030000000020303fb00000a180000000000000000030001000400000061702d61",
            "06040400000070772d62010b310030002e0030002e0030002e00310030003700000000ad080000",
            "0000000000000700901f000000000027000000303030303a303030303a303030303a303030303a",
            "303030303a303030303a303030303a303030304027000000303030303a303030303a303030303a",
            "303030303a303030303a303030303a303030303a3030303027000000303030303a303030303a30",
            "3030303a303030303a303030303a303030303a303030303a3030303081",
        )
    }

    /// A hotspot profile with no stored camera address (network block all
    /// zero): 2.4 GHz tag `03 00 30 00`, 5 GHz tag `06 04`, empty login,
    /// second port 0. Rebuilt with placeholders from a Z 9 profile read on
    /// 2026-11-05.
    fn ftp_blob_no_address_hex() -> &'static str {
        concat!(
            "f4010a500052004f00460049004c0045002d004400000010300030003000300030003000300030",
            "005400300030003000300030003000000010300030003000300030003000300030005400300030",
            "003000300030003000000002030300000000000000000000000000030030000400000061702d64",
            "06040400000070772d64010c3100390032002e0030002e0032002e00320034003300000000ad08",
            "000000000000000007000000000000000027000000303030303a303030303a303030303a303030",
            "303a303030303a303030303a303030303a303030304027000000303030303a303030303a303030",
            "303a303030303a303030303a303030303a303030303a3030303027000000303030303a30303030",
            "3a303030303a303030303a303030303a303030303a303030303a3030303081",
        )
    }

    #[test]
    fn decode_a_profile_with_no_camera_address_and_a_different_2_4ghz_tag() {
        let d = decode_ftp_profile(&hex::decode_hex(ftp_blob_no_address_hex())).unwrap();
        assert_eq!(d.profile_name, "PROFILE-D");
        assert_eq!(d.camera_network, FtpCameraNetwork { ipv4: [0, 0, 0, 0], prefix_len: 0 });
        assert_eq!((d.ssid24_kind, d.ssid5_kind), (0x30, 0x06));
        assert_eq!((d.ftp_port, d.option_flags, d.second_port), (2221, 0x0007, 0));
        assert!(d.as_encodable().is_none());
    }

    #[test]
    fn decode_a_profile_with_a_different_5ghz_tag_and_an_empty_login() {
        let d = decode_ftp_profile(&hex::decode_hex(ftp_blob_anonymous_hex())).unwrap();
        assert_eq!(d.profile_name, "PROFILE-C");
        assert_eq!(d.camera_network, FtpCameraNetwork { ipv4: [10, 0, 0, 251], prefix_len: 24 });
        assert_eq!((d.ssid5_kind, d.ssid_5ghz.as_str()), (0x06, "pw-b"));
        assert_eq!((d.username.as_str(), d.password.as_str()), ("", ""));
        assert_eq!((d.ftp_port, d.option_flags, d.second_port), (2221, 0x0007, 8080));
        assert!(d.as_encodable().is_none());
    }

    #[test]
    fn the_sftp_bit_is_set_on_the_port_22_profile_and_not_on_the_ftp_ones() {
        let sftp = decode_ftp_profile(&hex::decode_hex(ftp_blob_port22_hex())).unwrap();
        assert!(sftp.is_sftp());
        for hex in [reference_ftp_blob_hex(), ftp_blob_331_hex(), ftp_blob_anonymous_hex()] {
            assert!(!decode_ftp_profile(&hex::decode_hex(hex)).unwrap().is_sftp());
        }
    }

    #[test]
    fn decode_a_profile_with_a_non_default_ftp_port_and_option_flags() {
        let blob = hex::decode_hex(ftp_blob_port22_hex());
        let d = decode_ftp_profile(&blob).unwrap();
        assert_eq!(d.profile_name, "PROFILE-B");
        assert_eq!(d.camera_network, FtpCameraNetwork { ipv4: [192, 168, 68, 83], prefix_len: 22 });
        assert_eq!((d.host.as_str(), d.folder.as_str()), ("ftp.example.com", ""));
        assert_eq!((d.username.as_str(), d.password.as_str()), ("user", "pass1234"));
        assert_eq!((d.ftp_port, d.option_flags, d.second_port), (22, 0x000d, 8080));
        assert!(d.as_encodable().is_none(), "the encoder hard-codes port 21 and flags 0x0005");
    }

    #[test]
    fn the_331_blob_is_the_317_blob_plus_the_folder_string_and_flag_byte() {
        // The capture's own evidence: the 317-byte write that followed the
        // 331-byte read is that read with the 15-byte string
        // `07 "Claude" 00 00` (offset 153) replaced by the single empty-string
        // byte `00`, and the flag byte 01 → 05 (offset 191 in the read).
        let read = hex::decode_hex(ftp_blob_331_hex());
        let written = hex::decode_hex(ftp_blob_317_hex());
        let mut edited = read;
        edited[191] = 0x05;
        edited.splice(153..168, [0x00]);
        assert_eq!(edited, written);
    }

    #[test]
    fn decode_never_panics_on_a_truncated_blob_and_always_says_it_failed() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        for cut in 0..blob.len() {
            assert!(
                matches!(decode_ftp_profile(&blob[..cut]), Err(PtpUsbError::FtpBlobMalformed { .. })),
                "a blob cut to {cut} byte(s) must be rejected"
            );
        }
    }

    #[test]
    fn decode_rejects_trailing_bytes_and_unknown_tags_with_their_offset() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let mut longer = blob.clone();
        longer.push(0);
        assert!(matches!(
            decode_ftp_profile(&longer),
            Err(PtpUsbError::FtpBlobMalformed { what: "blob end", offset, .. }) if offset == blob.len()
        ));
        let mut bad_tag = blob.clone();
        bad_tag[99] ^= 0xff; // first byte of the 2.4 GHz SSID tag
        assert!(matches!(
            decode_ftp_profile(&bad_tag),
            Err(PtpUsbError::FtpBlobMalformed { what: "2.4 GHz SSID tag", offset: 99, .. })
        ));
        let mut bad_header = blob;
        bad_header[0] = 0;
        assert!(matches!(
            decode_ftp_profile(&bad_header),
            Err(PtpUsbError::FtpBlobMalformed { what: "blob header", offset: 0, .. })
        ));
    }

    #[test]
    fn secret_masking_keeps_the_lengths_and_hides_the_text() {
        let d = decode_ftp_profile(&hex::decode_hex(reference_ftp_blob_hex())).unwrap();
        let m = d.with_secrets_masked();
        assert_eq!((m.password.as_str(), m.ssid_5ghz.as_str()), ("********", "*********"));
        assert_eq!((d.password.as_str(), d.ssid_5ghz.as_str()), ("12345678", "nikon5ghz"), "masking returns a copy");
    }

    #[test]
    fn info_decodes_a_z9_reply_with_a_profile_and_one_without() {
        let with = parse_ftp_profile_info(&ftp_info_bytes(7, 0, "Z_9_0000000")).unwrap();
        assert_eq!((with.header, with.count, with.limit, with.slot, with.trailer), (0xc8, 7, 0x14, 0, 0x1f4));
        assert_eq!(with.device_name.text, "Z_9_0000000");
        assert!(with.has_active_profile());
        let raw = ftp_info_bytes(3, 0xff, "Z6_3_0000000");
        assert_eq!(raw.len(), 59);
        let without = parse_ftp_profile_info(&raw).unwrap();
        assert!(!without.has_active_profile());
        assert_eq!(ftp_info_bytes(6, 0xff, "Z_9_0000000").len(), 57);
    }

    #[test]
    fn info_rejects_truncation_and_trailing_bytes() {
        let ok = ftp_info_bytes(7, 0, "Z_9_0000000");
        for cut in 0..ok.len() {
            assert!(parse_ftp_profile_info(&ok[..cut]).is_err(), "cut to {cut}");
        }
        let mut longer = ok;
        longer.push(0);
        assert!(matches!(parse_ftp_profile_info(&longer), Err(PtpUsbError::FtpBlobMalformed { what: "info end", .. })));
    }

    #[test]
    fn read_ftp_profile_runs_the_captured_sequence_and_decodes_the_blob() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let info = ftp_info_bytes(7, 0, "Z_9_0000000");
        let (mut s, t) = session(
            vec![
                resp(RESP_OK),
                data_c(OP_FTP_PROFILE_INFO, &info),
                resp(RESP_OK),
                data_c(OP_FTP_PROFILE_READ, &blob),
                resp(RESP_OK),
            ],
            "Z 9",
        );
        let out = s.read_ftp_profile().unwrap();
        assert_eq!(
            t.sent(),
            vec![
                (CONTAINER_COMMAND, OP_FTP_SETUP, 0, vec![]),
                (CONTAINER_DATA, OP_FTP_SETUP, 0, FTP_SETUP_DATA.to_vec()),
                (CONTAINER_COMMAND, OP_FTP_PROFILE_INFO, 1, vec![]),
                (CONTAINER_COMMAND, OP_FTP_PROFILE_READ, 2, le(0)),
            ],
            "0x90E8 with its data, 0x90EB with no params, 0x90ED with the slot 0x90EB reported; nothing else"
        );
        assert!(out.reply_matched_command && out.warnings.is_empty());
        let p = out.profile.expect("a profile was reported");
        assert_eq!(p.raw, blob);
        assert_eq!(p.decoded.unwrap().profile_name, "CLAUDE");
    }

    #[test]
    fn read_ftp_profile_passes_the_slot_from_the_info_reply_to_0x90ed() {
        let blob = hex::decode_hex(ftp_blob_317_hex());
        let (mut s, t) = session(
            vec![
                resp(RESP_OK),
                data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(7, 4, "Z_9_0000000")),
                resp(RESP_OK),
                data_c(OP_FTP_PROFILE_READ, &blob),
                resp(RESP_OK),
            ],
            "Z 9",
        );
        s.read_ftp_profile().unwrap();
        assert_eq!(t.sent().last().unwrap(), &(CONTAINER_COMMAND, OP_FTP_PROFILE_READ, 2, le(4)));
    }

    #[test]
    fn write_ftp_profile_blob_sends_the_bytes_unchanged_under_the_given_params() {
        let blob = hex::decode_hex(ftp_blob_port22_hex());
        let (mut s, t) = session(vec![resp(RESP_OK), resp(RESP_OK)], "Z 9");
        s.write_ftp_profile_blob(1, 0, &blob, false).unwrap();
        assert_eq!(
            t.sent(),
            vec![
                (CONTAINER_COMMAND, OP_FTP_SETUP, 0, vec![]),
                (CONTAINER_DATA, OP_FTP_SETUP, 0, FTP_SETUP_DATA.to_vec()),
                (CONTAINER_COMMAND, OP_FTP_PROFILE_WRITE, 1, [le(1), le(0)].concat()),
                (CONTAINER_DATA, OP_FTP_PROFILE_WRITE, 1, blob),
            ]
        );
    }

    #[test]
    fn write_ftp_profile_blob_refuses_an_undecodable_blob_and_an_unconfirmed_model_before_any_io() {
        let mut bad = hex::decode_hex(ftp_blob_port22_hex());
        bad[0] = 0;
        let (mut s, t) = session(vec![], "Z 9");
        assert!(matches!(s.write_ftp_profile_blob(0, 0, &bad, false), Err(PtpUsbError::FtpBlobMalformed { .. })));
        let good = hex::decode_hex(ftp_blob_port22_hex());
        let (mut s2, t2) = session(vec![], "Z 5");
        assert!(matches!(s2.write_ftp_profile_blob(0, 0, &good, false), Err(PtpUsbError::UnconfirmedModel(_))));
        assert!(t.sent().is_empty() && t2.sent().is_empty());
    }

    #[test]
    fn read_ftp_profile_slot_reads_the_asked_slot_even_when_none_is_active() {
        let blob = hex::decode_hex(ftp_blob_317_hex());
        let (mut s, t) = session(
            vec![
                resp(RESP_OK),
                data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(3, 0xff, "Z_9_0000000")),
                resp(RESP_OK),
                data_c(OP_FTP_PROFILE_READ, &blob),
                resp(RESP_OK),
            ],
            "Z 9",
        );
        let out = s.read_ftp_profile_slot(Some(2)).unwrap();
        assert!(out.profile.is_some() && !out.info.has_active_profile());
        assert_eq!(t.sent().last().unwrap(), &(CONTAINER_COMMAND, OP_FTP_PROFILE_READ, 2, le(2)));
    }

    #[test]
    fn read_ftp_profile_slot_refuses_a_slot_at_or_above_the_stored_count_before_sending_it() {
        let (mut s, t) = session(
            vec![resp(RESP_OK), data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(3, 0, "Z_9_0000000")), resp(RESP_OK)],
            "Z 9",
        );
        assert!(matches!(
            s.read_ftp_profile_slot(Some(3)).unwrap_err(),
            PtpUsbError::FtpSlotOutOfRange { slot: 3, count: 3 }
        ));
        assert!(t.sent().iter().all(|(_, op, _, _)| *op != OP_FTP_PROFILE_READ));
    }

    #[test]
    fn read_ftp_profile_with_no_profile_never_sends_0x90ed() {
        let (mut s, t) = session(
            vec![resp(RESP_OK), data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(3, 0xff, "Z6_3_0000000")), resp(RESP_OK)],
            "Z6_3",
        );
        let out = s.read_ftp_profile().unwrap();
        assert!(out.profile.is_none() && !out.info.has_active_profile());
        assert!(t.sent().iter().all(|(_, op, _, _)| *op != OP_FTP_PROFILE_READ));
    }

    #[test]
    fn read_ftp_profile_keeps_the_raw_bytes_when_the_blob_is_not_recognised() {
        let mut odd = hex::decode_hex(reference_ftp_blob_hex());
        odd[0] = 0x00;
        let (mut s, _t) = session(
            vec![
                resp(RESP_OK),
                data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(7, 0, "Z_9_0000000")),
                resp(RESP_OK),
                data_c(OP_FTP_PROFILE_READ, &odd),
                resp(RESP_OK),
            ],
            "Z 9",
        );
        let p = s.read_ftp_profile().unwrap().profile.unwrap();
        assert_eq!(p.raw, odd);
        assert!(matches!(p.decoded, Err(PtpUsbError::FtpBlobMalformed { what: "blob header", .. })));
    }

    #[test]
    fn read_ftp_profile_reports_a_refused_0x90ed_as_that_op_failing() {
        let (mut s, _t) = session(
            vec![
                resp(RESP_OK),
                data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(7, 0, "Z_9_0000000")),
                resp(RESP_OK),
                resp(0x201F),
            ],
            "Z 9",
        );
        assert!(matches!(
            s.read_ftp_profile().unwrap_err(),
            PtpUsbError::OperationFailed { op: OP_FTP_PROFILE_READ, code: PtpResponseCode(0x201F) }
        ));
    }

    #[test]
    fn read_ftp_profile_info_ok_without_data_is_no_data_phase_and_stops() {
        let (mut s, t) = session(vec![resp(RESP_OK), resp(RESP_OK)], "Z 9");
        assert!(matches!(s.read_ftp_profile().unwrap_err(), PtpUsbError::NoDataPhase { op: OP_FTP_PROFILE_INFO }));
        assert!(t.sent().iter().all(|(_, op, _, _)| *op != OP_FTP_PROFILE_READ));
    }

    #[test]
    fn read_ftp_profile_setup_failure_stops_before_the_info_op() {
        let (mut s, t) = session(vec![resp(0x2002)], "Z 9");
        assert!(matches!(s.read_ftp_profile().unwrap_err(), PtpUsbError::OperationFailed { op: OP_FTP_SETUP, .. }));
        assert!(t.sent().iter().all(|(_, op, _, _)| *op == OP_FTP_SETUP));
    }

    #[test]
    fn read_ftp_profile_on_an_unsurveyed_model_still_reads_and_warns() {
        let (mut s, _t) = session(
            vec![resp(RESP_OK), data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(3, 0xff, "Z_8_0000000")), resp(RESP_OK)],
            "Z 8",
        );
        let out = s.read_ftp_profile().unwrap();
        assert_eq!(out.warnings.len(), 1);
    }
}

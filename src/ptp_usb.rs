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
//! in tests; the third, a 331-byte variant with a folder string, is decoded
//! but not written: the blob layout is one table, [`ftp_layout::FTP_LAYOUT`],
//! from which the encoder, the decoder and the write gate are all driven, so
//! enabling that form is an edit of the table). `0x9413` (IPTC profile write)
//! is not — its data-blob preamble couldn't be decoded from existing captures (only two instances
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

/// The stages a transaction records as its `phase` field while it is in flight: the command is out, or
/// the data-out is out too (after which the camera may have acted). A hang shows the last one set.
const TXN_PHASE_COMMAND_SENT: &str = "command_sent";
const TXN_PHASE_DATA_SENT: &str = "data_sent";
/// Field names for the first parameters of a transaction; a count of all of them is recorded too.
const TXN_PARAM_KEYS: [&str; 5] = ["p0", "p1", "p2", "p3", "p4"];

/// How a transaction ended, as the diagnostics vocabulary. A camera that answers with a non-OK code
/// completed the transaction but refused it (`Failed`); an error after the data-out went out means
/// the effect is unknown (`Indeterminate`); a timeout and a stalled pipe are their own outcomes.
fn txn_outcome(result: &Result<Reply, PtpUsbError>, data_sent: bool) -> crate::diag::Outcome {
    use crate::diag::Outcome;
    match result {
        Ok(reply) if reply.code == RESP_OK => Outcome::Ok,
        Ok(_) => Outcome::Failed,
        Err(_) if data_sent => Outcome::Indeterminate,
        Err(PtpUsbError::Usb(rusb::Error::Timeout) | PtpUsbError::TransactionTimedOut(_)) => Outcome::Timeout,
        Err(PtpUsbError::Usb(rusb::Error::Pipe)) => Outcome::Stalled,
        Err(_) => Outcome::Failed,
    }
}

/// Standard PTP `GetDevicePropDesc` (ISO 15740 §10.5.3): one param, the property code; the data
/// phase is the property's descriptor (type, get/set flag, default, current value, form).
const OP_GET_DEVICE_PROP_DESC: u16 = 0x1014;
/// Standard PTP `GetDevicePropValue`: one param, the property code; the data phase is the value
/// alone, encoded per the property's data type (a string is `[u8 count incl. NUL][UTF-16LE]`).
const OP_GET_DEVICE_PROP_VALUE: u16 = 0x1015;
/// Standard PTP `SetDevicePropValue`: one param (the property code) and a data-out phase holding the
/// value. Used here only for the three string properties in [`STRING_PROPS`].
const OP_SET_DEVICE_PROP_VALUE: u16 = 0x1016;
/// `0x9412` — IPTC profile read, the pair of `0x9413` (write). One param, the slot (NX Field sent `1`, and `0` when
/// the IPTC screen opened); the data phase is the profile. Never sent over USB before 2026-11-06.
const OP_IPTC_READ: u16 = 0x9412;
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

/// `0x90EE` — FTP profile write. Two params; NX Field only ever sent `[0, 0]`. Experiments since
/// (`docs/nx-field-ftp-profile-session-2026-07-10.md`, section 5): `(n, 0)` replaces stored profile `n`
/// and moves it to the front, `(count, 1)` adds a new profile at the front, anything else did nothing,
/// stalled the pipe or (slot `0xFFFFFFFF`) hung the camera. [`classify_ftp_write`] allows only those two
/// forms. The encoder path ([`PtpUsbSession::write_ftp_profile_with`]) still sends the fixed
/// `[0, 0]`. See `docs/nx-field-session-2026-07-09.md`'s field table for the data blob.
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
/// byte-identical to the 317-byte write before them) and run over USB on two Z 9s and a Z6III
/// (2026-11-05). A bare `0x90ED` with no param stalls the pipe, and so does a slot at or past the stored count.
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

/// The string properties [`PtpUsbSession::set_string_property`] will write: Artist, Copyright and
/// Image comment. The names are libgphoto2's; the codes were read as read-write strings on a Z 9
/// (2026-11-05). A closed list: any other code is refused, never passed through.
pub const STRING_PROPS: [(u16, &str); 3] = [(0xD072, "Artist"), (0xD073, "Copyright"), (0xD090, "Image comment")];

/// Models [`PtpUsbSession::set_string_property`] accepts without `force_unconfirmed_model`. A USB write
/// of the image comment (`0xD090`) was confirmed on all three, 2026-11-05, and of Artist (`0xD072`) and
/// Copyright (`0xD073`) on all three, 2026-11-06: written, read back (in a separate session for the
/// image comment), restored byte for byte, with a value one character over the limit (36 / 54 / 36)
/// refused with `InvalidDevicePropFormat` and the stored value unchanged. The Z 8 and other bodies are not listed.
pub const STRING_PROP_WRITE_MODELS: &[&str] = &["Z 9", "Z6_3", "Z 5"];

/// The properties of [`STRING_PROPS`] whose USB write was actually run on the models in
/// [`STRING_PROP_WRITE_MODELS`]: all three now (image comment 2026-11-05; Artist and Copyright on the
/// Z 9, Z6III and Z 5 2026-11-06). A property in [`STRING_PROPS`] but not in this list would need
/// `force_unconfirmed_model`, like an unconfirmed model does: the gate is the pair (property, model),
/// kept as [`check_string_write_gate`] so the mechanism stays tested for the next property or body. A test keeps
/// this list inside [`STRING_PROPS`].
pub const STRING_PROP_WRITE_CONFIRMED_PROPS: &[u16] = &[0xD072, 0xD073, 0xD090];

/// The slot `0x90EB` reports when the camera has no *active* FTP profile.
/// **Not** "no profile stored": on 2026-11-05 a Z9 that the owner says holds
/// FTP profiles reported this too, so 0xff is probably "none selected". NX Field
/// still sent `0x90ED(0xff)` in that state and got `0x201F`
/// (TransactionCancelled) with no data, five times in the captures; this
/// module does not send it, since the `0x90EB` reply already says so.
pub const FTP_NO_ACTIVE_SLOT: u32 = 0xff;

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

/// The one warning for a camera outside [`KNOWN_MODELS`] (`None` for a surveyed model). `what` names the
/// operation(s) whose behaviour on this body is unverified. The condition is membership of the registry,
/// not "is a Z 9", so the text claims no more than that: each operation was captured on some of the
/// surveyed bodies, and nothing is known about this one.
fn unsurveyed_model_warning(model: &str, what: &str) -> Option<String> {
    sdk_model_name(model).is_none().then(|| {
        format!(
            "camera model {model:?} is not one of the surveyed models ({KNOWN_MODELS:?}); the wire format and behaviour \
             of {what} on it are unverified"
        )
    })
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
    #[error("{}", ftp_info_malformed_text(.raw, .cause))]
    FtpInfoMalformed {
        /// The whole `0x90EB` reply data, kept because a reply the decoder cannot read is the finding. The
        /// decoder reads no password from it (count, limit, slot, GUID, device name, trailer), so it is
        /// printed to the user; diagnostics record its length only.
        raw: Vec<u8>,
        /// The decoder's complaint: always a [`PtpUsbError::FtpBlobMalformed`].
        cause: Box<PtpUsbError>,
    },
    #[error(
        "FTP profile slot {slot} is outside the {count} stored profile(s) (slots 0..{count}); a read past the end \
         stalls the USB pipe, so it was not sent"
    )]
    FtpSlotOutOfRange { slot: u32, count: u32 },
    #[error("operation 0x{op:04x} is not on the FTP-probe list ({}); it was not sent", ftp_probe_ops_text())]
    FtpProbeOpNotAllowed { op: u16 },
    #[error("the FTP probe's data-out is limited to {} bytes; {len} was not sent", FTP_PROBE_MAX_DATA)]
    FtpProbeDataTooLong { len: usize },
    #[error(
        "the FTP probe takes at most {} parameters and refuses 0x{:X} (an out-of-range slot value that \
         hung a camera's network stack); {params:?} was not sent",
        FTP_PROBE_MAX_PARAMS,
        FTP_PROBE_REFUSED_PARAM
    )]
    FtpProbeBadParams { params: Vec<u32> },
    #[error(
        "operation 0x{op:04x} {} and the probe sends it only when asked for by name: pass \
         --allow-destructive-op 0x{op:04x} (and, off a confirmed model, --force-unconfirmed-model); it was not sent",
        .risk.describe()
    )]
    FtpProbeDestructiveNotConfirmed { op: u16, risk: FtpProbeRisk },
    #[error("FTP profile field {field} is {len} {unit}, exceeds the {max}-{unit} limit")]
    FtpFieldTooLong { field: &'static str, len: usize, max: usize, unit: &'static str },
    #[error("FTP profile field {field} contains non-ASCII characters, which this encoding does not support")]
    FtpFieldNotAscii { field: &'static str },
    #[error("FTP profile field {field} contains a control character (NUL, newline, tab, ...)")]
    FtpFieldControlChar { field: &'static str },
    #[error(
        "FTP profile write may or may not have taken effect: the 0x90EE data had already been sent when \
         this failed ({cause}). Read the list back before doing anything else (`vendor-read-ftp --slot 0`, \
         from a new session, or the camera's menu): a write that took effect put its profile at the FRONT. \
         Do not just repeat it: a repeated add stores a duplicate, and a repeated replace of a slot above 0 \
         hits whichever profile now sits there"
    )]
    FtpWriteIndeterminate { cause: Box<PtpUsbError> },
    #[error("FTP profile write 0x90EE({slot}, {param2}) refused before anything was written: {why}")]
    FtpWriteRefused { slot: u32, param2: u32, why: FtpWriteRefusal },
    #[error("camera model {0:?} is not in this module's confirmed-safe list for vendor writes; wire format may not match")]
    UnconfirmedModel(String),
    #[error("property {propcode:#06x} is not one of the string properties this writes ({allowed}); nothing was sent")]
    PropWriteUnsupported { propcode: u16, allowed: String },
    #[error("{}", prop_write_unconfirmed_text(*.propcode, .model))]
    PropWriteUnconfirmed { propcode: u16, model: String },
    #[error(
        "property {propcode:#06x} is read-only on this camera right now (descriptor get/set flag {get_set:#04x}); \
         the camera makes some properties writable only in some states. Nothing was written"
    )]
    PropNotWritable { propcode: u16, get_set: u8 },
    #[error("property {propcode:#06x}: {why}; nothing was written")]
    PropValueBad { propcode: u16, why: String },
    #[error(
        "property {propcode:#06x} write may or may not have taken effect: the value had already been sent when \
         this failed ({cause}). Read it back with `vendor-get-prop`; repeating the write is idempotent"
    )]
    PropWriteIndeterminate { propcode: u16, cause: Box<PtpUsbError> },
}

/// The eviction class of a transaction's diagnostics span. A transaction with a data phase out sends
/// something to the camera (a property write, an FTP profile write, a vendor probe with data): a
/// `Class::Top` span, so the ring keeps a successful one longer than the routine reads and a failed one
/// longer than anything else. Every other transaction (a read, a session open or close) is one of
/// hundreds and a `Class::Leaf`, which the ring evicts first and the report collapses in runs.
fn txn_span_class(data_out: Option<&[u8]>) -> crate::diag::Class {
    if data_out.is_some() { crate::diag::Class::Top } else { crate::diag::Class::Leaf }
}

/// Whether the data phase of `op` carries an FTP profile (so a password). Diagnostics record the length
/// of such a payload and nothing else about it: not the bytes, and not a hash either.
fn op_payload_holds_a_secret(op: u16) -> bool {
    matches!(op, OP_FTP_PROFILE_WRITE | OP_FTP_PROFILE_READ)
}

/// The name [`STRING_PROPS`] gives `propcode`, or its hex code when it is not one of them.
fn string_prop_name(propcode: u16) -> String {
    STRING_PROPS
        .iter()
        .find(|(c, _)| *c == propcode)
        .map_or_else(|| format!("{propcode:#06x}"), |(_, n)| (*n).to_string())
}

fn prop_write_unconfirmed_text(propcode: u16, model: &str) -> String {
    format!(
        "writing {} ({propcode:#06x}) has not been confirmed on a camera (confirmed: {}, on {:?}); \
         pass --force-unconfirmed-model to try it on this {model:?}",
        string_prop_name(propcode),
        STRING_PROP_WRITE_CONFIRMED_PROPS.iter().map(|c| string_prop_name(*c)).collect::<Vec<_>>().join(", "),
        STRING_PROP_WRITE_MODELS,
    )
}

/// The write gate of `vendor-set-string`: the model must be in `models` (checked first, so the error names
/// the real cause), then the property in `confirmed`. Both lists are parameters so the mechanism is tested
/// with lists that differ; the session passes [`STRING_PROP_WRITE_MODELS`] and [`STRING_PROP_WRITE_CONFIRMED_PROPS`].
fn check_string_write_gate(model: &str, propcode: u16, models: &[&str], confirmed: &[u16]) -> Result<(), PtpUsbError> {
    if !models.contains(&model) {
        return Err(PtpUsbError::UnconfirmedModel(model.to_string()));
    }
    if !confirmed.contains(&propcode) {
        return Err(PtpUsbError::PropWriteUnconfirmed { propcode, model: model.to_string() });
    }
    Ok(())
}

impl PtpUsbError {
    /// Raised by a transaction before it put a single byte on the pipe: the session was already unusable,
    /// or the command could not be encoded. Anything else raised by `transact` may follow a command the
    /// camera received.
    fn nothing_was_sent(&self) -> bool {
        matches!(
            self,
            PtpUsbError::SessionPoisoned { .. } | PtpUsbError::TooManyCommandParams { .. } | PtpUsbError::ContainerTooLarge { .. }
        )
    }

    /// Closed machine-readable tag, one per variant. Exhaustive match: a new variant must be given a tag
    /// before this compiles.
    pub fn tag(&self) -> &'static str {
        match self {
            PtpUsbError::NoDevice => "no_device",
            PtpUsbError::AmbiguousDevice(_) => "ambiguous_device",
            PtpUsbError::SerialNotFound { .. } => "serial_not_found",
            PtpUsbError::DuplicateSerial { .. } => "duplicate_serial",
            PtpUsbError::BodiesSkipped { .. } => "bodies_skipped",
            PtpUsbError::Usb(_) => "usb",
            PtpUsbError::CameraLock(_) => "camera_lock",
            PtpUsbError::NoPtpInterface => "no_ptp_interface",
            PtpUsbError::ClaimFailed { .. } => "claim_failed",
            PtpUsbError::ShortContainer(_) => "short_container",
            PtpUsbError::TruncatedContainer { .. } => "truncated_container",
            PtpUsbError::ShortWrite { .. } => "short_write",
            PtpUsbError::ContainerTooLarge { .. } => "container_too_large",
            PtpUsbError::TooManyCommandParams { .. } => "too_many_command_params",
            PtpUsbError::MisalignedResponseParams(_) => "misaligned_response_params",
            PtpUsbError::TransactionTimedOut(_) => "transaction_timed_out",
            PtpUsbError::TransactionDataTooLarge { .. } => "transaction_data_too_large",
            PtpUsbError::SessionPoisoned { .. } => "session_poisoned",
            PtpUsbError::OpenSessionFailed(_) => "open_session_failed",
            PtpUsbError::OperationFailed { .. } => "operation_failed",
            PtpUsbError::TransactionIdMismatch { .. } => "transaction_id_mismatch",
            PtpUsbError::UnexpectedDataOp { .. } => "unexpected_data_op",
            PtpUsbError::NoDataPhase { .. } => "no_data_phase",
            PtpUsbError::DeviceInfoMalformed { .. } => "device_info_malformed",
            PtpUsbError::FtpBlobMalformed { .. } => "ftp_blob_malformed",
            PtpUsbError::FtpInfoMalformed { .. } => "ftp_info_malformed",
            PtpUsbError::FtpSlotOutOfRange { .. } => "ftp_slot_out_of_range",
            PtpUsbError::FtpProbeOpNotAllowed { .. } => "ftp_probe_op_not_allowed",
            PtpUsbError::FtpProbeDataTooLong { .. } => "ftp_probe_data_too_long",
            PtpUsbError::FtpProbeBadParams { .. } => "ftp_probe_bad_params",
            PtpUsbError::FtpProbeDestructiveNotConfirmed { .. } => "ftp_probe_destructive_not_confirmed",
            PtpUsbError::FtpFieldTooLong { .. } => "ftp_field_too_long",
            PtpUsbError::FtpFieldNotAscii { .. } => "ftp_field_not_ascii",
            PtpUsbError::FtpFieldControlChar { .. } => "ftp_field_control_char",
            PtpUsbError::FtpWriteIndeterminate { .. } => "ftp_write_indeterminate",
            PtpUsbError::FtpWriteRefused { .. } => "ftp_write_refused",
            PtpUsbError::UnconfirmedModel(_) => "unconfirmed_model",
            PtpUsbError::PropWriteUnsupported { .. } => "prop_write_unsupported",
            PtpUsbError::PropWriteUnconfirmed { .. } => "prop_write_unconfirmed",
            PtpUsbError::PropNotWritable { .. } => "prop_not_writable",
            PtpUsbError::PropValueBad { .. } => "prop_value_bad",
            PtpUsbError::PropWriteIndeterminate { .. } => "prop_write_indeterminate",
        }
    }

    /// What a transaction's diagnostics span records for this error: typed fields, never the `Display`
    /// text. The tag is always there as `error_kind`; a transport error adds the closed `rusb` error name
    /// (`usb_error`), a refused answer its operation and response code (`op`, `code`), the other
    /// protocol faults their numbers. Nothing else is recorded, so a `Display` that grew a payload (a
    /// string from the camera, a profile field) could not reach the record through here.
    fn diag_fields(&self) -> Vec<(&'static str, crate::diag::Val)> {
        use crate::diag::Val;
        let mut f = vec![("error_kind", Val::text(self.tag()))];
        match self {
            PtpUsbError::Usb(e) => f.push(("usb_error", Val::text(&format!("{e:?}")))),
            PtpUsbError::OperationFailed { op, code } => {
                f.push(("op", Val::Hex(u64::from(*op))));
                f.push(("code", Val::Hex(u64::from(code.0))));
            }
            PtpUsbError::NoDataPhase { op } => f.push(("op", Val::Hex(u64::from(*op)))),
            PtpUsbError::TransactionIdMismatch { op, expected, got } => {
                f.push(("op", Val::Hex(u64::from(*op))));
                f.push(("expected", Val::U64(u64::from(*expected))));
                f.push(("got", Val::U64(u64::from(*got))));
            }
            PtpUsbError::UnexpectedDataOp { op, got } => {
                f.push(("op", Val::Hex(u64::from(*op))));
                f.push(("got", Val::Hex(u64::from(*got))));
            }
            PtpUsbError::TransactionTimedOut(d) => f.push(("timeout_ms", Val::U64(u64::try_from(d.as_millis()).unwrap_or(u64::MAX)))),
            PtpUsbError::TruncatedContainer { declared, got } => {
                f.push(("declared", Val::U64(*declared as u64)));
                f.push(("got", Val::U64(*got as u64)));
            }
            PtpUsbError::ShortWrite { written, expected } => {
                f.push(("written", Val::U64(*written as u64)));
                f.push(("expected", Val::U64(*expected as u64)));
            }
            PtpUsbError::TransactionDataTooLarge { got, limit } => {
                f.push(("got", Val::U64(*got as u64)));
                f.push(("limit", Val::U64(*limit as u64)));
            }
            PtpUsbError::ShortContainer(n) | PtpUsbError::MisalignedResponseParams(n) => f.push(("len", Val::U64(*n as u64))),
            PtpUsbError::ContainerTooLarge { payload_len } => f.push(("len", Val::U64(*payload_len as u64))),
            PtpUsbError::FtpWriteIndeterminate { cause } | PtpUsbError::PropWriteIndeterminate { cause, .. } => {
                f.push(("cause_kind", Val::text(cause.tag())));
            }
            // Length only: a reply that did not decode as the info layout has no known shape, so no hash or bytes.
            PtpUsbError::FtpInfoMalformed { raw, cause } => {
                f.push(("len", Val::U64(raw.len() as u64)));
                f.push(("cause_kind", Val::text(cause.tag())));
            }
            // Tag only: the remaining variants carry paths, serials, camera strings or no data that adds to the tag.
            _ => {}
        }
        f
    }

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

/// The most bytes of a malformed `0x90EB` reply that [`PtpUsbError::FtpInfoMalformed`] prints. A real reply is
/// 57 to 59 bytes (a Z 9, a Z6III); the whole reply stays in the error's `raw` whatever this is.
pub const FTP_INFO_SHOWN_MAX: usize = 128;

fn ftp_info_malformed_text(raw: &[u8], cause: &PtpUsbError) -> String {
    let shown = &raw[..raw.len().min(FTP_INFO_SHOWN_MAX)];
    let cut = if shown.len() < raw.len() {
        format!(", first {} shown", shown.len())
    } else {
        String::new()
    };
    format!("the 0x90EB info reply ({} byte(s){cut}: {}) is not in a layout this decoder knows: {cause}", raw.len(), hex_bytes(shown))
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

#[cfg(test)]
mod ftp_golden;

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

pub mod ftp_layout;
pub use ftp_layout::{FtpSecretSpan, FtpSecretSpans};

/// The address-shaped part of the 82-byte template block (see the notes on
/// [`ftp_layout::FTP_LAYOUT`]): an IPv4 address and prefix length.
/// [`FtpCameraNetwork::CAPTURED`] is what all three captures carried.
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

const FTP_PROFILE_NAME_MAX_UNITS: usize = 253; // + null terminator = 254, the most a 1-byte length prefix allows here
const FTP_SSID_MAX_BYTES: usize = 32; // 802.11
const FTP_HOST_MAX_UNITS: usize = 199; // + null terminator = 200; a guess, the prefix would allow 254
const FTP_CREDENTIAL_MAX_BYTES: usize = 64; // generous; real usernames/passwords are far shorter

/// How a text value is laid out on the wire. Stated once, per value, in [`ftp_layout::FTP_LAYOUT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpEncoding {
    /// `[u32 length in bytes, no null][content]`. The encoder writes ASCII only; the decoder reads
    /// UTF-8 (see `FtpBlobReader::utf8_u32_len`). The name is historical.
    AsciiU32Len,
    /// `[u8 length in UTF-16 units incl. null][utf16le content + null]`.
    Utf16U8Len,
    /// [`Self::Utf16U8Len`], except that the encoder writes the empty string as the single count byte
    /// `00` (no null terminator), as the 317- and 325-byte captures carry the folder. The decoder reads
    /// both forms of an empty string.
    Utf16U8LenOptional,
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
    /// Longest value accepted, in [`FtpFieldSpec::unit`]s of *content* (the
    /// null terminator of a UTF-16 field is not counted). The unit is that of
    /// [`Self::encoding`], which the layout states.
    pub max_len: usize,
    /// Whether surrounding whitespace is ignored when checking the field is
    /// non-empty. The password is checked as typed: a leading or trailing
    /// space could be a real password character.
    pub trimmed_for_required_check: bool,
}

impl FtpFieldSpec {
    /// The wire form of this field, as [`ftp_layout::FTP_LAYOUT`] states it (the one place it is).
    pub fn encoding(&self) -> FtpEncoding {
        ftp_layout::encoding_of(ftp_layout::FTP_LAYOUT, self.field).expect("FTP_LAYOUT places every FtpField (pinned by a test)")
    }

    pub fn unit(&self) -> FtpLenUnit {
        self.encoding().unit()
    }
}

/// The FTP-profile text fields with their limits and display names, in the
/// order of [`FtpField::ALL`].
pub const FTP_PROFILE_FIELDS: [FtpFieldSpec; 6] = [
    FtpFieldSpec {
        field: FtpField::ProfileName,
        key: "profile_name",
        display_name: "profile name",
        max_len: FTP_PROFILE_NAME_MAX_UNITS,
        trimmed_for_required_check: true,
    },
    FtpFieldSpec {
        field: FtpField::Ssid24Ghz,
        key: "ssid_24ghz",
        display_name: "SSID (2.4GHz)",
        max_len: FTP_SSID_MAX_BYTES,
        trimmed_for_required_check: true,
    },
    FtpFieldSpec {
        field: FtpField::Ssid5Ghz,
        key: "ssid_5ghz",
        display_name: "SSID (5GHz)",
        max_len: FTP_SSID_MAX_BYTES,
        trimmed_for_required_check: true,
    },
    FtpFieldSpec {
        field: FtpField::Host,
        key: "host",
        display_name: "host",
        max_len: FTP_HOST_MAX_UNITS,
        trimmed_for_required_check: true,
    },
    FtpFieldSpec {
        field: FtpField::Username,
        key: "username",
        display_name: "username",
        max_len: FTP_CREDENTIAL_MAX_BYTES,
        trimmed_for_required_check: true,
    },
    FtpFieldSpec {
        field: FtpField::Password,
        key: "password",
        display_name: "password",
        max_len: FTP_CREDENTIAL_MAX_BYTES,
        trimmed_for_required_check: false,
    },
];

/// Fields for an `0x90EE` FTP profile write. All required; see the module
/// docs for which structural bytes are opaque-but-fixed instead, and
/// [`FTP_PROFILE_FIELDS`] for each field's limit. `Debug` hides the password and the
/// 5 GHz field (probably a Wi-Fi password): it shows their lengths only.
#[derive(Clone, PartialEq, Eq)]
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

impl std::fmt::Debug for FtpProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FtpProfile")
            .field("profile_name", &self.profile_name)
            .field("ssid_24ghz", &self.ssid_24ghz)
            .field("ssid_5ghz", &format_args!("<{} byte(s) hidden>", self.ssid_5ghz.len()))
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &format_args!("<{} byte(s) hidden>", self.password.len()))
            .finish()
    }
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

/// `[u8 length in UTF-16 units incl. null][utf16le content + null]`, no
/// validation: the layout of [`FtpEncoding::Utf16U8Len`].
fn utf16_u8_len_bytes(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + (units.len() + 1) * 2);
    out.push((units.len() + 1) as u8);
    for u in units {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes()); // null terminator
    out
}

/// Build the `0x90EE` data-phase blob for `profile`, with the captured
/// [`FtpCameraNetwork`]. Reproduces the real captured wire format
/// byte-for-byte (verified in tests against two of the three 2026-07-09
/// captures; the 331-byte variant has a folder string the encoder does not
/// write) rather than a best guess. The layout is [`ftp_layout::FTP_LAYOUT`].
pub fn encode_ftp_profile(profile: &FtpProfile) -> Result<Vec<u8>, PtpUsbError> {
    encode_ftp_profile_with(profile, &FtpCameraNetwork::CAPTURED)
}

/// [`encode_ftp_profile`] with an explicit [`FtpCameraNetwork`].
pub fn encode_ftp_profile_with(profile: &FtpProfile, network: &FtpCameraNetwork) -> Result<Vec<u8>, PtpUsbError> {
    ftp_layout::encode_with(ftp_layout::FTP_LAYOUT, profile, network)
}

// ─────────────────────────────────────────────────────────────────────────
// 0x90ED FTP-profile read: decoding the blob and the 0x90EB info reply
//
// Worked out offline from the 2026-07-09 Z 9 captures (three distinct blobs:
// 325 B, 331 B, 317 B). The layout is the one `encode_ftp_profile_with`
// writes (both are driven by `ftp_layout::FTP_LAYOUT`), including a PTP
// string between host and username that the encoder only ever writes empty
// (see `DecodedFtpProfile::folder`). The decoder is strict: any structural byte it does not recognise is an error carrying
// the offset, because a silently wrong decode of a profile is worse than
// none. The raw bytes are always available to the caller.
// ─────────────────────────────────────────────────────────────────────────

/// What `0x90EE(slot, param2)` does, for the two forms a camera has accepted (Z 9, 2026-07-10/11,
/// `docs/nx-field-ftp-profile-session-2026-07-10.md` section 5): a second parameter of `0` replaces the
/// profile in `slot` and moves it to the front of the list; `1` with `slot` equal to the stored count
/// adds a new profile at the front. Nothing else is known to work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpWriteKind {
    Replace,
    Add,
}

/// Why [`classify_ftp_write`] refused a `0x90EE(slot, param2)`: each is a form that, on a Z 9, either
/// did nothing (`0x201F`), stalled the USB pipe, or hung the camera's network stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FtpWriteRefusal {
    #[error(
        "the second parameter is {param2:#x}; only 0 (replace the slot) and 1 (add at the end) were ever accepted by a \
         camera, other values were answered 0x201F"
    )]
    UnknownParam2 { param2: u32 },
    #[error("slot 0xFFFFFFFF hung a camera's network stack until it was power-cycled")]
    SlotHangsCamera,
    #[error(
        "replacing slot {slot} needs a stored profile there, but the camera holds {count} (slots 0..{count}); a write \
         one past the end stalled the USB pipe. To add a profile use slot {count} with second parameter 1"
    )]
    ReplaceSlotNotStored { slot: u32, count: u32 },
    #[error(
        "adding a profile is 0x90EE({count}, 1), slot = the stored count; slot {slot} with second parameter 1 was \
         never tried (slot {count} adds, a slot below it with 0 replaces)"
    )]
    AddNotAtCount { slot: u32, count: u32 },
}

/// Decide whether `0x90EE(slot, param2)` is one of the two forms known to work. With `count` (the
/// stored profile count `0x90EB` reports) every rule is applied; with `None` only the rules that need
/// no camera state, so a caller can refuse before any I/O. A count-independent refusal wins over
/// a count-dependent one, and an unknown second parameter over the hang slot.
pub fn classify_ftp_write(slot: u32, param2: u32, count: Option<u32>) -> Result<FtpWriteKind, FtpWriteRefusal> {
    let kind = match param2 {
        0 => FtpWriteKind::Replace,
        1 => FtpWriteKind::Add,
        other => return Err(FtpWriteRefusal::UnknownParam2 { param2: other }),
    };
    if slot == u32::MAX {
        return Err(FtpWriteRefusal::SlotHangsCamera);
    }
    if let Some(count) = count {
        match kind {
            FtpWriteKind::Replace if slot >= count => return Err(FtpWriteRefusal::ReplaceSlotNotStored { slot, count }),
            FtpWriteKind::Add if slot != count => return Err(FtpWriteRefusal::AddNotAtCount { slot, count }),
            _ => {}
        }
    }
    Ok(kind)
}

/// What `0x90EB` returns: the camera's identity and where its FTP profile
/// is. Layout (57–59 bytes; **inferred from five replies in three body
/// states, not documented**): `u16 0x00c8`, `u32 count`, `u32 0x14`,
/// `u32 slot`, 16-byte GUID, PTP string (the body's `<model>_<serial>`),
/// `u16 0x01f4`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpProfileInfo {
    /// `0x00c8` (200) in every reply seen; possibly a limit.
    pub header: u16,
    /// The number of stored FTP profiles: slots `0..count` can be read, and a replace needs a slot
    /// below it while an add needs a slot equal to it ([`classify_ftp_write`]). It moves with a
    /// delete, an add and a delete-all (0), as in the 2026-11-05 Z 9 experiments. (Earlier
    /// captures showed 6 then 7 on a Z 9 and 3 on a Z6III that held none; those were read before
    /// the meaning was known and are not explained.)
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

    /// A PTP string through [`decode_ptp_string`], strictly: any anomaly is an error at the string's
    /// first byte, so a garbled string never reaches a decoded profile.
    fn ptp_string(&mut self, what: &'static str) -> Result<PtpString, PtpUsbError> {
        match decode_ptp_string(&self.data[self.pos..]) {
            Err(StringFrameFault::NoCountByte) => Err(self.bad(what, "needs 1 byte(s) but only 0 remain")),
            Err(StringFrameFault::Truncated { needed, available }) => {
                self.pos += 1; // the count byte was read
                Err(self.bad(what, format!("needs {needed} byte(s) but only {available} remain")))
            }
            Ok((string, used)) => {
                if !string.anomalies.is_empty() {
                    let tags: Vec<&str> = string.anomalies.iter().map(|a| a.tag()).collect();
                    return Err(self.bad(what, format!("string is not well-formed ({})", tags.join(", "))));
                }
                self.pos += used;
                Ok(string)
            }
        }
    }

    /// `[u32 length in bytes][bytes]`, no null, read as UTF-8. The length is in bytes, so the framing
    /// carries any octets; no capture has non-ASCII content, and UTF-8 is assumed by analogy with the
    /// IPTC fields of the same shape. A field that is not valid UTF-8 is an error naming the field
    /// (the raw bytes stay available to the caller), not a lossy guess.
    fn utf8_u32_len(&mut self, what: &'static str) -> Result<String, PtpUsbError> {
        let at = self.pos;
        let len = self.u32(what)? as usize;
        if len > self.data.len() - self.pos {
            self.pos = at;
            return Err(self.bad(what, format!("declares {len} byte(s) but only {} remain", self.data.len() - at - 4)));
        }
        let bytes = self.take(what, len)?;
        match std::str::from_utf8(bytes) {
            Ok(text) => Ok(text.to_string()),
            Err(e) => {
                self.pos = at + 4 + e.valid_up_to();
                Err(self.bad(what, "is not valid UTF-8"))
            }
        }
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
/// verified against literals. `Debug` hides the password and the 5 GHz field
/// (lengths only), so a `{:?}` of this or of anything holding it cannot leak them.
#[derive(Clone, PartialEq, Eq)]
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
    /// The first byte of that same tag: `03` in the captured and encoded
    /// profiles, `00` in some profiles made in the camera's own menu. Meaning
    /// unknown (a guess, unverified: how the 2.4 GHz network was configured).
    /// The encoder writes `03`, so [`Self::as_encodable`] refuses `00`.
    pub ssid24_lead: u8,
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

impl std::fmt::Debug for DecodedFtpProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedFtpProfile")
            .field("profile_name", &self.profile_name)
            .field("timestamps", &self.timestamps)
            .field("camera_network", &self.camera_network)
            .field("ssid_24ghz", &self.ssid_24ghz)
            .field("ssid24_kind", &self.ssid24_kind)
            .field("ssid24_lead", &self.ssid24_lead)
            .field("ssid_5ghz", &format_args!("<{} byte(s) hidden>", self.ssid_5ghz.len()))
            .field("ssid5_kind", &self.ssid5_kind)
            .field("host", &self.host)
            .field("folder", &self.folder)
            .field("username", &self.username)
            .field("password", &format_args!("<{} byte(s) hidden>", self.password.len()))
            .field("ftp_port", &self.ftp_port)
            .field("option_flags", &self.option_flags)
            .field("second_port", &self.second_port)
            .field("ipv6_fields", &self.ipv6_fields)
            .finish()
    }
}

impl DecodedFtpProfile {
    /// The SFTP option bit (`0x0008`). Confirmed against the camera's menu on
    /// one profile; see [`Self::option_flags`].
    pub fn is_sftp(&self) -> bool {
        self.option_flags & 0x0008 != 0
    }

    /// The [`FtpProfile`] the encoder would need to write this profile back,
    /// when it can. [`ftp_layout::FTP_LAYOUT`] says what the encoder fixes; it is not possible if
    /// [`Self::folder`] is non-empty, or [`Self::option_flags`] is not the encoder's `0x0005`, or
    /// [`Self::ftp_port`] is not the 21 it hard-codes, or [`Self::ssid5_kind`]
    /// is not `04`, or [`Self::ssid24_kind`] is not `01`, or [`Self::ssid24_lead`]
    /// is not `03`, or the camera address, the two timestamps or the three IPv6
    /// fields are not the encoder's captured placeholders, or one of the four
    /// text fields the encoder writes as ASCII is not ASCII, since the encoder has
    /// no way to carry any of them and a re-encode would silently change the
    /// profile. `FtpProfile.port` receives [`Self::second_port`] (where the
    /// encoder writes it).
    pub fn as_encodable(&self) -> Option<FtpProfile> {
        ftp_layout::encodable_with(ftp_layout::FTP_LAYOUT, self)
    }

    // `with_secrets_masked` and `secret_texts`, which read the layout's secret flags, are in `ftp_layout`.
}

/// Decode a `0x90ED` reply (or any `0x90EE` data-out blob) into its fields.
/// Strict: an unrecognised structural byte, a truncated field or trailing
/// bytes is [`PtpUsbError::FtpBlobMalformed`] with the offset.
pub fn decode_ftp_profile(blob: &[u8]) -> Result<DecodedFtpProfile, PtpUsbError> {
    decode_ftp_profile_spans(blob).map(|(decoded, _)| decoded)
}

/// [`decode_ftp_profile`], also returning where the secret strings are in `blob`
/// ([`FtpSecretSpans`]).
pub fn decode_ftp_profile_spans(blob: &[u8]) -> Result<(DecodedFtpProfile, FtpSecretSpans), PtpUsbError> {
    ftp_layout::decode_with(ftp_layout::FTP_LAYOUT, blob)
}

// ─────────────────────────────────────────────────────────────────────────
// Session
// ─────────────────────────────────────────────────────────────────────────

/// Something a reply did that [`TxidCheck::Unchecked`] let through: what the camera sent instead of
/// what the command called for, kept as data so an experiment can read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyDeviation {
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
/// `RESP_OK` is the only acknowledgement this receipt carries: the camera answered OK to
/// the `0x90EE` data. It is "the camera accepted the blob", not "the profile now holds
/// these values". The profile can be read back (`0x90ED`);
/// [`PtpUsbSession::write_ftp_profile_blob_verified`] does it and returns [`FtpWriteVerification`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpWriteReceipt {
    /// The parameters of the `0x90EE` response (none seen in any capture).
    pub response_params: Vec<u32>,
    /// False when a reply to the setup or write carried a different
    /// transaction id (or a data container a different op code) than the
    /// command — tolerated because those ops have no capture to hold them to.
    pub reply_matched_command: bool,
}

/// The result of reading a just-written FTP profile back. **Never run on hardware as one step**: the
/// pieces are (a write then `vendor-read-ftp --slot 0` in a separate process returned what was sent, Z 9,
/// 2026-11-05) but a read inside the same session as the write is not. It relies on two facts seen
/// on a Z 9 only: a successful `0x90EE` moves the written profile to slot 0 (replace and add alike), and
/// the count rises by one on an add and stays on a replace.
///
/// The comparison is of decoded profiles, not bytes: the camera re-serialises an empty folder
/// (`00` comes back `01 00 00`) when option bit `0x04` is clear, so the lengths can differ while the
/// profile is the same. A [`Mismatch`](Self::Mismatch) does not say which side is wrong: it can also mean a
/// camera that does not move the profile to the front.
#[derive(Debug)]
pub enum FtpWriteVerification {
    /// Slot 0 decodes to exactly the profile that was sent, and the count is what the write should leave.
    Verified { count_after: u32 },
    /// The read-back worked but is not what the write should have left. `differing_fields` names the
    /// fields of slot 0 that differ (never their values: they include the password); empty when only
    /// the count is off.
    Mismatch { count_after: u32, count_expected: u32, differing_fields: Vec<&'static str> },
    /// Slot 0 was read but its bytes do not decode in a layout this decoder knows.
    ReadBackUndecodable { error: PtpUsbError },
    /// The read-back itself failed (a refused or timed-out operation, a stalled pipe); the write is
    /// neither confirmed nor contradicted. The session is poisoned if the transport failed.
    ReadBackFailed { error: PtpUsbError },
}

impl FtpWriteVerification {
    pub fn is_verified(&self) -> bool {
        matches!(self, FtpWriteVerification::Verified { .. })
    }
}

/// A guarded FTP-profile write with its read-back; see [`PtpUsbSession::write_ftp_profile_blob_verified`].
#[derive(Debug)]
pub struct FtpWriteOutcome {
    pub receipt: FtpWriteReceipt,
    pub kind: FtpWriteKind,
    /// The stored profile count `0x90EB` reported in the write's own session, before the write.
    pub count_before: u32,
    pub verification: FtpWriteVerification,
}

/// What a guarded FTP-profile write would do on this camera right now, from
/// [`PtpUsbSession::plan_ftp_write`]: nothing has been written.
#[derive(Debug)]
pub struct FtpWritePlan {
    /// The USB model the model gate was applied to.
    pub model: String,
    pub kind: FtpWriteKind,
    /// The stored profile count `0x90EB` reported.
    pub count: u32,
    /// The blob, decoded.
    pub decoded: DecodedFtpProfile,
    /// False when the `0x90E8` or `0x90EB` reply did not match its command (tolerated, see [`TxidCheck`]).
    pub info_replies_matched: bool,
}

/// The fields of `want` that differ in `got`, by name. Both sides are destructured without `..`, so
/// adding a field to [`DecodedFtpProfile`] without deciding how to compare it does not compile.
fn ftp_profile_differences(want: &DecodedFtpProfile, got: &DecodedFtpProfile) -> Vec<&'static str> {
    let DecodedFtpProfile {
        profile_name,
        timestamps,
        camera_network,
        ssid_24ghz,
        ssid24_kind,
        ssid24_lead,
        ssid_5ghz,
        ssid5_kind,
        host,
        folder,
        username,
        password,
        ftp_port,
        option_flags,
        second_port,
        ipv6_fields,
    } = want;
    let mut differing = Vec::new();
    macro_rules! cmp {
        ($($f:ident),*) => { $( if *$f != got.$f { differing.push(stringify!($f)); } )* };
    }
    cmp!(
        profile_name, timestamps, camera_network, ssid_24ghz, ssid24_kind, ssid24_lead, ssid_5ghz, ssid5_kind, host,
        folder, username, password, ftp_port, option_flags, second_port, ipv6_fields
    );
    differing
}

/// The profile bytes of an FTP-profile read, with the decode attempted on
/// them. The raw bytes are kept whether or not the decode worked. `Debug` shows the raw
/// length, not the bytes (they hold the password).
pub struct FtpProfileBlob {
    pub raw: Vec<u8>,
    pub decoded: Result<DecodedFtpProfile, PtpUsbError>,
}

impl std::fmt::Debug for FtpProfileBlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FtpProfileBlob")
            .field("raw", &format_args!("<{} byte(s) hidden>", self.raw.len()))
            .field("decoded", &self.decoded)
            .finish()
    }
}

/// What one reply of an FTP-profile read carried besides its data: kept as data, because a reply that
/// departs from what the command called for is a finding. None of it is a profile field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpReplyNotes {
    /// The operation the reply answered (`0x90E8`, `0x90EB` or `0x90ED`).
    pub op: u16,
    /// The parameters of the response container (none seen in any capture).
    pub response_params: Vec<u32>,
    /// Exactly how the reply departed from its command: the transaction id or operation code it carried instead.
    pub deviations: Vec<ReplyDeviation>,
    /// Containers of a kind this module does not know that arrived and were skipped.
    pub unrecognized_containers: u32,
}

impl FtpReplyNotes {
    fn of(op: u16, reply: &Reply) -> Self {
        FtpReplyNotes {
            op,
            response_params: reply.params.clone(),
            deviations: reply.deviations.clone(),
            unrecognized_containers: reply.unrecognized_containers,
        }
    }

    /// Nothing worth telling: no response parameters, no deviation, no skipped container.
    pub fn is_empty(&self) -> bool {
        self.response_params.is_empty() && self.deviations.is_empty() && self.unrecognized_containers == 0
    }
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
    /// True exactly when no entry of [`Self::replies`] has a deviation.
    pub reply_matched_command: bool,
    /// What each reply carried besides its data, in the order sent: `0x90E8`, `0x90EB` and, when it was
    /// sent, `0x90ED`.
    pub replies: Vec<FtpReplyNotes>,
}

/// `propcode` is one of the [`STRING_PROPS`] this module writes; otherwise [`PtpUsbError::PropWriteUnsupported`].
/// No I/O: a caller can refuse before opening a camera.
fn check_string_prop_known(propcode: u16) -> Result<(), PtpUsbError> {
    if STRING_PROPS.iter().any(|(c, _)| *c == propcode) {
        return Ok(());
    }
    let allowed = STRING_PROPS.iter().map(|(c, n)| format!("{c:#06x} {n}")).collect::<Vec<_>>().join(", ");
    Err(PtpUsbError::PropWriteUnsupported { propcode, allowed })
}

/// What a string-property write would do on this camera right now, from
/// [`PtpUsbSession::plan_string_property`]: nothing has been written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringPropPlan {
    pub propcode: u16,
    /// The value the camera holds now: what to write back to undo the write.
    pub previous: String,
    pub warnings: Vec<String>,
    /// The encoded value that would be sent.
    pub data: Vec<u8>,
}

/// What [`PtpUsbSession::set_string_property`] did. `Ok` means the camera answered OK; whether the
/// value took is `verified`, from reading it back with `GetDevicePropValue`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropWriteReceipt {
    pub propcode: u16,
    /// The value before the write, as the camera reported it: what to write back to undo this.
    pub previous: String,
    /// What was sent.
    pub written: String,
    /// The value read back afterwards, or why it could not be read or decoded.
    pub read_back: Result<String, String>,
    /// `read_back == Ok(written)`. False with an `Ok` read-back means the camera stored something
    /// else (truncated, normalised); that is a finding, not a failure to hide.
    pub verified: bool,
    pub warnings: Vec<String>,
}

/// What a vendor-property read returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorRead {
    pub data: Vec<u8>,
    /// The parameters of the response container (none expected for a read; kept because a reply that
    /// carries some is a finding).
    pub response_params: Vec<u32>,
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
    /// The parameters of the response container. For a query whose answer is the finding these are part
    /// of it.
    pub response_params: Vec<u32>,
    /// Exactly how the reply departed from the command (the detail behind
    /// [`Self::reply_matched_command`]): the transaction id or operation code it carried instead.
    pub deviations: Vec<ReplyDeviation>,
    /// Containers of a kind this module does not know that arrived and were skipped.
    pub unrecognized_containers: u32,
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

    /// What is wrong, as a sentence fragment for the strict decoders' errors.
    fn describe(self) -> &'static str {
        match self {
            StringAnomaly::UnpairedSurrogate => "an unpaired UTF-16 surrogate",
            StringAnomaly::MissingTerminator => "a non-empty string must end in a NUL unit",
            StringAnomaly::EmbeddedNul => "an embedded NUL unit before the end",
        }
    }
}

/// A PTP string (ISO 15740 §3.2.2): a character count `N` (terminating NUL
/// included; 0 means an empty string with no further bytes), then `N`
/// UTF-16LE code units.
///
/// `anomalies` is non-empty only where the caller chose to be lenient:
/// DeviceInfo, which must still describe a body whose strings are odd. The strict
/// sites (the FTP blob, the `0x90EB` reply, property values) reject any anomaly,
/// so a `PtpString` they return always has `anomalies` empty. All of them decode
/// through [`decode_ptp_string`]; they differ only in what they do with its verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtpString {
    /// Decoded without the terminating NUL; lossy where `anomalies` says so.
    pub text: String,
    pub anomalies: Vec<StringAnomaly>,
}

/// Why [`decode_ptp_string`] could not frame a string: the bytes ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StringFrameFault {
    /// Not even the count byte.
    NoCountByte,
    /// The count byte promises `needed` bytes of UTF-16 units; `available` follow it.
    Truncated { needed: usize, available: usize },
}

/// THE decoder of the PTP string wire form (`[u8 count incl. NUL][count x UTF-16LE]`): the one place
/// the framing, the terminator, the embedded-NUL and the surrogate rules live. Returns the string
/// with every anomaly it has (in the fixed order missing terminator, embedded NUL, unpaired
/// surrogate) and the number of bytes consumed. It never decides whether an anomaly is fatal: the
/// lenient site (DeviceInfo) keeps them on the value, the strict ones (the FTP blob, property
/// values) refuse any. Count 0 is the empty string with no further bytes; a count of 1 is the NUL alone.
fn decode_ptp_string(data: &[u8]) -> Result<(PtpString, usize), StringFrameFault> {
    let Some((&count, rest)) = data.split_first() else { return Err(StringFrameFault::NoCountByte) };
    let needed = usize::from(count) * 2;
    if needed > rest.len() {
        return Err(StringFrameFault::Truncated { needed, available: rest.len() });
    }
    let mut units: Vec<u16> = rest[..needed].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
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
    Ok((PtpString { text, anomalies }, 1 + needed))
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

    /// Lenient: the anomalies stay on the returned value (see [`decode_ptp_string`]).
    fn string(&mut self, field: &'static str) -> Result<PtpString, PtpUsbError> {
        match decode_ptp_string(&self.data[self.pos..]) {
            Ok((string, used)) => {
                self.pos += used;
                Ok(string)
            }
            Err(StringFrameFault::NoCountByte) => {
                Err(self.malformed(field, DeviceInfoFault::Truncated { needed: 1, available: 0 }))
            }
            Err(StringFrameFault::Truncated { needed, available }) => {
                self.pos += 1; // the count byte was read
                Err(self.malformed(field, DeviceInfoFault::Truncated { needed, available }))
            }
        }
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
    /// Event containers that arrived on the bulk pipe and were skipped (events belong on the
    /// interrupt endpoint, which is never opened, so any here is unexpected).
    event_containers: u32,
    /// Held for the session's life (dropped after the interface is released);
    /// `None` only for the test constructor.
    _camera_lock: Option<CameraLock>,
    /// Where this session's transactions are recorded: the process-wide recorder when the program
    /// started one, else nothing. A field and not a global lookup per call so a test can give a
    /// session a recorder of its own.
    diag: Option<&'static crate::diag::Diag>,
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
            event_containers: 0,
            _camera_lock: camera_lock,
            diag: crate::diag::global(),
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

    /// How many Event containers this session has skipped on the bulk pipe.
    pub fn event_container_count(&self) -> u32 {
        self.event_containers
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
        let span = self.begin_txn_span(op, params, data_out);
        let result = self.run_transaction(&span, op, params, data_out, check, timeout);
        let outcome = txn_outcome(&result, self.data_phase_started);
        match &result {
            Ok(reply) => {
                let mut extra = vec![
                    ("response", crate::diag::Val::Hex(u64::from(reply.code))),
                    ("reply_len", crate::diag::Val::len(reply.data.len())),
                ];
                // A hash of nothing identifies nothing: only when the camera sent data. And none for a
                // profile read: its blob holds the password, and a short hash of a low-entropy secret
                // inside a mostly known structure lets whoever holds the file test guesses offline.
                if !reply.data.is_empty() && !op_payload_holds_a_secret(op) {
                    extra.push(("reply_hash", crate::diag::Val::hash(&reply.data)));
                }
                span.finish_with(outcome, &extra);
            }
            Err(e) => span.finish_with(outcome, &e.diag_fields()),
        }
        if let Err(e) = &result
            && !e.leaves_stream_in_sync()
        {
            self.poison(e);
        }
        result
    }

    /// Open the diagnostics span for one transaction. What it records about the payload is its length
    /// and a hash, never the bytes (an FTP profile carries a password). The operation and parameters
    /// are what the camera was asked; they are codes and slot numbers.
    fn begin_txn_span(&self, op: u16, params: &[u32], data_out: Option<&[u8]>) -> crate::diag::Span<'static> {
        use crate::diag::{Subsystem, Val};
        let Some(d) = self.diag else { return crate::diag::Span::none() };
        let mut fields: Vec<(&'static str, Val)> = vec![
            ("op", Val::Hex(u64::from(op))),
            ("txid", Val::U64(u64::from(self.transaction_id))),
            ("params", Val::U64(params.len() as u64)),
        ];
        for (key, p) in TXN_PARAM_KEYS.iter().zip(params) {
            fields.push((key, Val::Hex(u64::from(*p))));
        }
        if let Some(data) = data_out {
            fields.push(("data_len", Val::len(data.len())));
            if !op_payload_holds_a_secret(op) {
                fields.push(("data_hash", Val::hash(data)));
            }
        }
        // The operation code is part of the name so the report only collapses runs of the *same*
        // operation, never a close, an open and a read into one line.
        d.span(Subsystem::Usb, &format!("ptp_{op:04x}"), txn_span_class(data_out), &fields)
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
        span: &crate::diag::Span<'_>,
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
        span.set("phase", crate::diag::Val::text(TXN_PHASE_COMMAND_SENT));
        if let Some(data) = data {
            self.data_phase_started = true;
            self.send_container(&data)?;
            span.set("phase", crate::diag::Val::text(TXN_PHASE_DATA_SENT));
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
                CONTAINER_EVENT => {
                    // Shouldn't arrive on the bulk pipe (the interrupt endpoint is never opened):
                    // skipped, but counted.
                    self.event_containers += 1;
                }
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

    /// [`Self::transact`], then [`Self::require_ok`] on the answer: the pair every read and setup step
    /// below needs. A camera that answers non-OK is [`PtpUsbError::OperationFailed`].
    fn transact_ok(&mut self, op: u16, params: &[u32], data_out: Option<&[u8]>, check: TxidCheck) -> Result<Reply, PtpUsbError> {
        let reply = self.transact(op, params, data_out, check)?;
        Self::require_ok(op, reply.code)?;
        Ok(reply)
    }

    /// [`Self::transact_ok`] for an operation whose answer must carry a data container: an OK answer with
    /// none at all is [`PtpUsbError::NoDataPhase`], distinct from a data phase of zero bytes.
    fn transact_ok_with_data(
        &mut self,
        op: u16,
        params: &[u32],
        data_out: Option<&[u8]>,
        check: TxidCheck,
    ) -> Result<Reply, PtpUsbError> {
        let reply = self.transact_ok(op, params, data_out, check)?;
        if reply.data_containers == 0 {
            return Err(PtpUsbError::NoDataPhase { op });
        }
        Ok(reply)
    }

    /// The `0x90E8` setup transaction (fixed 3-byte data-out) that the FTP-profile ops are preceded by.
    fn ftp_setup_step(&mut self) -> Result<Reply, PtpUsbError> {
        self.transact_ok(OP_FTP_SETUP, &[], Some(&FTP_SETUP_DATA), TxidCheck::Unchecked)
    }

    /// `0x90ED(slot)`: the stored profile's raw blob. The caller has checked `slot` against the stored
    /// count (a read past the end stalls the pipe).
    fn read_ftp_profile_reply(&mut self, slot: u32) -> Result<Reply, PtpUsbError> {
        self.transact_ok_with_data(OP_FTP_PROFILE_READ, &[slot], None, TxidCheck::Unchecked)
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
        let warnings: Vec<String> = unsurveyed_model_warning(self.model(), "vendor-property reads").into_iter().collect();
        let param = 0x10000 | propcode as u32;
        let reply = self.transact_ok_with_data(OP_VENDOR_PROP_READ, &[param], None, TxidCheck::Enforce)?;
        Ok(VendorRead { data: reply.data, response_params: reply.params, warnings })
    }

    /// `0x9412(slot)`: the IPTC profile in `slot`, undecoded. EXPERIMENTAL and read-only; the wire form is
    /// inferred from an NX Field capture, never sent over USB before. An OK answer with no data container is
    /// [`PtpUsbError::NoDataPhase`].
    pub fn read_iptc_slot(&mut self, slot: u32) -> Result<VendorRead, PtpUsbError> {
        let warnings: Vec<String> = unsurveyed_model_warning(self.model(), "the IPTC read").into_iter().collect();
        let reply = self.transact_ok_with_data(OP_IPTC_READ, &[slot], None, TxidCheck::Unchecked)?;
        Ok(VendorRead { data: reply.data, response_params: reply.params, warnings })
    }

    /// Standard `GetDevicePropValue` (`0x1015`): the property's value bytes, undecoded. Read-only.
    /// This is the generic PTP path, distinct from the `0x943B` wrapper ([`Self::read_vendor_property`]):
    /// on a Z 9 `0x943B` answers `DevicePropNotSupported` for the artist and image-comment strings, so
    /// which of the two reaches which property is a per-property fact to be found out.
    /// An OK answer with no data container is [`PtpUsbError::NoDataPhase`], distinct from empty data.
    pub fn get_device_prop_value(&mut self, propcode: u16) -> Result<VendorRead, PtpUsbError> {
        self.get_device_prop(OP_GET_DEVICE_PROP_VALUE, propcode)
    }

    /// Standard `GetDevicePropDesc` (`0x1014`): the property's descriptor bytes, undecoded. Read-only.
    /// Its get/set flag says whether the camera will accept a write *now* (it is state-dependent).
    pub fn get_device_prop_desc(&mut self, propcode: u16) -> Result<VendorRead, PtpUsbError> {
        self.get_device_prop(OP_GET_DEVICE_PROP_DESC, propcode)
    }

    fn get_device_prop(&mut self, op: u16, propcode: u16) -> Result<VendorRead, PtpUsbError> {
        let warnings: Vec<String> = unsurveyed_model_warning(self.model(), "standard property reads").into_iter().collect();
        let reply = self.transact_ok_with_data(op, &[u32::from(propcode)], None, TxidCheck::Enforce)?;
        Ok(VendorRead { data: reply.data, response_params: reply.params, warnings })
    }

    /// Everything [`Self::set_string_property`] checks before it sends the value, and nothing more: the
    /// property is one of [`STRING_PROPS`], the model and property gates, the text (Latin-1, no control
    /// characters), the descriptor (a read-write string right now) and the current value. A dry run uses
    /// this alone, so it refuses exactly what the real write would refuse.
    pub fn plan_string_property(
        &mut self,
        propcode: u16,
        text: &str,
        force_unconfirmed_model: bool,
    ) -> Result<StringPropPlan, PtpUsbError> {
        check_string_prop_known(propcode)?;
        if !force_unconfirmed_model {
            check_string_write_gate(self.model(), propcode, STRING_PROP_WRITE_MODELS, STRING_PROP_WRITE_CONFIRMED_PROPS)?;
        }
        let bad = |why: String| PtpUsbError::PropValueBad { propcode, why };
        // Observed on a Z 9 (2026-11-05): the camera keeps only the LOW BYTE of each UTF-16 unit of
        // these strings, so Latin-1 (U+0000..U+00FF) survives and anything above is stored as
        // something else ("日本" came back as "å,", an emoji as "=÷"). Refuse it rather than send
        // text the camera will quietly corrupt.
        if let Some(c) = text.chars().find(|c| u32::from(*c) > 0xFF) {
            return Err(bad(format!(
                "{c:?} (U+{:04X}) is outside Latin-1; the camera stores only the low byte of each character in this \
                 property, so it would hold something else",
                u32::from(c)
            )));
        }
        let data = encode_ptp_string_value(text).map_err(&bad)?;

        let desc = self.get_device_prop_desc(propcode)?;
        // PropCode u16, DataType u16 (0xFFFF = string), GetSet u8: 5 bytes before anything variable.
        if desc.data.len() < 5 {
            return Err(bad(format!("descriptor is {} byte(s), too short to hold the get/set flag", desc.data.len())));
        }
        let echoed = u16::from_le_bytes([desc.data[0], desc.data[1]]);
        if echoed != propcode {
            return Err(bad(format!("the descriptor is for property {echoed:#06x}, not the one asked for")));
        }
        let data_type = u16::from_le_bytes([desc.data[2], desc.data[3]]);
        if data_type != 0xFFFF {
            return Err(bad(format!("descriptor data type is {data_type:#06x}, not the string type 0xffff")));
        }
        if desc.data[4] != 0x01 {
            return Err(PtpUsbError::PropNotWritable { propcode, get_set: desc.data[4] });
        }
        let before = self.get_device_prop_value(propcode)?;
        let previous = decode_ptp_string_value(&before.data).map_err(|e| bad(format!("the current value does not decode ({e})")))?;
        Ok(StringPropPlan { propcode, previous, data, warnings: desc.warnings })
    }

    /// Write one of the [`STRING_PROPS`] with `SetDevicePropValue` (`0x1016`), in one session:
    /// descriptor (`0x1014`, must be a read-write string), current value (`0x1015`, kept as
    /// [`PropWriteReceipt::previous`]), the write, then a read-back of the value. A refusal before the
    /// write leaves the camera untouched. A failure after the value went out is
    /// [`PtpUsbError::PropWriteIndeterminate`]; a camera that *answers* non-OK is a definite refusal.
    /// The camera's length limit for each property is not known here: nothing is truncated by this
    /// code, and a camera that truncates shows up as `verified == false` with the read-back text.
    /// Models outside [`STRING_PROP_WRITE_MODELS`], and properties outside
    /// [`STRING_PROP_WRITE_CONFIRMED_PROPS`] (all three today), are
    /// refused unless `force_unconfirmed_model`. Text is refused before anything is sent if it has a
    /// character above U+00FF (the camera keeps one byte per character) or any `char::is_control`
    /// character, which includes U+007F and the C1 range U+0080..=U+009F even though those are
    /// inside Latin-1.
    pub fn set_string_property(
        &mut self,
        propcode: u16,
        text: &str,
        force_unconfirmed_model: bool,
    ) -> Result<PropWriteReceipt, PtpUsbError> {
        let StringPropPlan { previous, data, mut warnings, .. } =
            self.plan_string_property(propcode, text, force_unconfirmed_model)?;

        let reply = match self.transact(OP_SET_DEVICE_PROP_VALUE, &[u32::from(propcode)], Some(&data), TxidCheck::Enforce) {
            Ok(reply) => reply,
            Err(e) if self.data_phase_started => {
                return Err(PtpUsbError::PropWriteIndeterminate { propcode, cause: Box::new(e) })
            }
            Err(e) => return Err(e),
        };
        Self::require_ok(OP_SET_DEVICE_PROP_VALUE, reply.code)?;

        let read_back = match self.get_device_prop_value(propcode) {
            Ok(r) => decode_ptp_string_value(&r.data).map_err(|e| format!("the value read back does not decode ({e})")),
            Err(e) => Err(format!("reading it back failed: {e}")),
        };
        if let Err(why) = &read_back {
            warnings.push(format!("the write was accepted but could not be confirmed: {why}"));
        }
        let verified = read_back.as_deref().ok() == Some(text);
        Ok(PropWriteReceipt { propcode, previous, written: text.to_string(), read_back, verified, warnings })
    }

    /// Send `0x944C(target_op, 0)` and report the answer as it came; see
    /// [`OP_VENDOR_OP_QUERY`] for what is and is not known about it. **Not yet
    /// run over USB on any camera**: this module has never sent `0x944C`, so
    /// the first runs are the test, and an unfamiliar `target_op` is a guess
    /// at the parameter (a stalled pipe is the failure seen so far from a
    /// wrong parameter on other ops, and the session recovers from it). It
    /// sends nothing but that one command: no data-out phase, no setup step.
    pub fn query_vendor_op(&mut self, target_op: u16) -> Result<VendorOpQuery, PtpUsbError> {
        let warnings: Vec<String> = unsurveyed_model_warning(self.model(), "0x944C").into_iter().collect();
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
            response_params: reply.params,
            deviations: reply.deviations,
            unrecognized_containers: reply.unrecognized_containers,
            warnings,
        })
    }

    /// Read the DeviceInfo dataset (`0x1001`, standard and read-only). Same
    /// checks as a vendor read: a non-OK answer is [`PtpUsbError::OperationFailed`], an OK
    /// answer with no data container at all is [`PtpUsbError::NoDataPhase`], and the dataset is
    /// parsed strictly ([`parse_device_info`]).
    pub fn get_device_info(&mut self) -> Result<DeviceInfoDataset, PtpUsbError> {
        let reply = self.transact_ok_with_data(OP_GET_DEVICE_INFO, &[], None, TxidCheck::Enforce)?;
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
    /// cause: the camera may have taken the profile; read slot 0 back to see (a
    /// written profile moves to the front). A camera that *answers* with a non-OK code is a definite
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

    /// `0x90E8` setup then `0x90EB`: the stored count and the selected slot, read-only. The notes are
    /// those of the two replies (tolerated departures, see [`TxidCheck`]). A `0x90EB` reply the decoder
    /// cannot read is [`PtpUsbError::FtpInfoMalformed`], which keeps its bytes.
    fn read_ftp_info_step(&mut self) -> Result<(FtpProfileInfo, Vec<FtpReplyNotes>), PtpUsbError> {
        let setup = self.ftp_setup_step()?;
        let info_reply = self.transact_ok_with_data(OP_FTP_PROFILE_INFO, &[], None, TxidCheck::Unchecked)?;
        let info = parse_ftp_profile_info(&info_reply.data)
            .map_err(|cause| PtpUsbError::FtpInfoMalformed { raw: info_reply.data.clone(), cause: Box::new(cause) })?;
        Ok((info, vec![FtpReplyNotes::of(OP_FTP_SETUP, &setup), FtpReplyNotes::of(OP_FTP_PROFILE_INFO, &info_reply)]))
    }

    /// The part of a guarded FTP write that sends nothing: the model gate, the blob must decode,
    /// `(slot, param2)` must be a form [`classify_ftp_write`] knows (count-independent rules before any
    /// I/O), then the stored count is read **in this session** (`0x90E8`, `0x90EB`) and the rules that
    /// need it are applied. A dry run uses this alone, so it fails exactly where the real write would.
    pub fn plan_ftp_write(
        &mut self,
        slot: u32,
        param2: u32,
        blob: &[u8],
        force_unconfirmed_model: bool,
    ) -> Result<FtpWritePlan, PtpUsbError> {
        if !force_unconfirmed_model && !ftp_write_confirmed(&self.model) {
            return Err(PtpUsbError::UnconfirmedModel(self.model.clone()));
        }
        let decoded = decode_ftp_profile(blob)?;
        let refuse = |why| PtpUsbError::FtpWriteRefused { slot, param2, why };
        classify_ftp_write(slot, param2, None).map_err(refuse)?;
        let (info, info_notes) = self.read_ftp_info_step()?;
        let info_replies_matched = info_notes.iter().all(|n| n.deviations.is_empty());
        let kind = classify_ftp_write(slot, param2, Some(info.count)).map_err(refuse)?;
        Ok(FtpWritePlan { model: self.model.clone(), kind, count: info.count, decoded, info_replies_matched })
    }

    /// The write every caller that chooses its own slot goes through: [`Self::plan_ftp_write`], then
    /// the write, so the count cannot change between the check and the write. Returns what was
    /// decoded from the blob so the caller can read it back.
    fn write_ftp_guarded(
        &mut self,
        slot: u32,
        param2: u32,
        blob: &[u8],
        force_unconfirmed_model: bool,
    ) -> Result<(FtpWriteReceipt, FtpWriteKind, u32, DecodedFtpProfile), PtpUsbError> {
        let plan = self.plan_ftp_write(slot, param2, blob, force_unconfirmed_model)?;
        let mut receipt = self.send_ftp_blob([slot, param2], blob)?;
        receipt.reply_matched_command &= plan.info_replies_matched;
        Ok((receipt, plan.kind, plan.count, plan.decoded))
    }

    /// Read slot 0 back after a successful write and compare; see [`FtpWriteVerification`].
    fn verify_ftp_write(&mut self, expected: &DecodedFtpProfile, kind: FtpWriteKind, count_before: u32) -> FtpWriteVerification {
        let count_expected = match kind {
            FtpWriteKind::Replace => count_before,
            FtpWriteKind::Add => count_before.saturating_add(1),
        };
        let readout = match self.read_ftp_profile_slot(Some(0)) {
            Ok(r) => r,
            // Slot 0 does not exist: the camera now holds nothing, which is a finding, not a failed read.
            Err(PtpUsbError::FtpSlotOutOfRange { count, .. }) => {
                return FtpWriteVerification::Mismatch { count_after: count, count_expected, differing_fields: Vec::new() };
            }
            Err(error) => return FtpWriteVerification::ReadBackFailed { error },
        };
        let count_after = readout.info.count;
        let Some(blob) = readout.profile else {
            return FtpWriteVerification::ReadBackFailed { error: PtpUsbError::NoDataPhase { op: OP_FTP_PROFILE_READ } };
        };
        match blob.decoded {
            Err(error) => FtpWriteVerification::ReadBackUndecodable { error },
            Ok(got) => {
                let differing_fields = ftp_profile_differences(expected, &got);
                if differing_fields.is_empty() && count_after == count_expected {
                    FtpWriteVerification::Verified { count_after }
                } else {
                    FtpWriteVerification::Mismatch { count_after, count_expected, differing_fields }
                }
            }
        }
    }

    /// `0x90E8` setup, then `0x90EE(params)` with `blob` as its data-out phase. Shared by the
    /// encoder-built write and the raw-blob write.
    fn send_ftp_blob(&mut self, params: [u32; 2], blob: &[u8]) -> Result<FtpWriteReceipt, PtpUsbError> {
        let setup = self.ftp_setup_step()?;

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
    /// reaches the camera. **What the two parameters mean is partly unknown**, so only
    /// the two forms a camera has accepted are sent ([`classify_ftp_write`]): `(n, 0)` replaces the
    /// stored profile `n` and `(count, 1)` adds one; anything else is
    /// [`PtpUsbError::FtpWriteRefused`] with a typed [`FtpWriteRefusal`]. The stored count is read
    /// in this same session first (`0x90E8`, `0x90EB`, then the write's own `0x90E8`, `0x90EE`; the
    /// extra read-only pair has not been run ahead of a write on hardware). Same model gate as
    /// [`Self::write_ftp_profile_with`]. `Ok` is the camera's OK and nothing more; for a read-back use
    /// [`Self::write_ftp_profile_blob_verified`].
    pub fn write_ftp_profile_blob(
        &mut self,
        slot: u32,
        param2: u32,
        blob: &[u8],
        force_unconfirmed_model: bool,
    ) -> Result<FtpWriteReceipt, PtpUsbError> {
        self.write_ftp_guarded(slot, param2, blob, force_unconfirmed_model).map(|(receipt, ..)| receipt)
    }

    /// [`Self::write_ftp_profile_blob`], then a read-back in the same session: `0x90E8`, `0x90EB`,
    /// `0x90ED(0)` and a comparison of the decoded profile with the one sent (see
    /// [`FtpWriteVerification`] for exactly what is and is not established, and that this
    /// read-back has not been run on hardware inside the write's session). A write that fails
    /// is the same `Err` as for [`Self::write_ftp_profile_blob`]; the read-back's own failure is
    /// reported in the outcome, not as an `Err`, because the write did succeed.
    pub fn write_ftp_profile_blob_verified(
        &mut self,
        slot: u32,
        param2: u32,
        blob: &[u8],
        force_unconfirmed_model: bool,
    ) -> Result<FtpWriteOutcome, PtpUsbError> {
        let (receipt, kind, count_before, expected) = self.write_ftp_guarded(slot, param2, blob, force_unconfirmed_model)?;
        let verification = self.verify_ftp_write(&expected, kind, count_before);
        Ok(FtpWriteOutcome { receipt, kind, count_before, verification })
    }

    /// [`Self::write_ftp_profile_with`]'s profile, written as a guarded replace of slot 0
    /// (`0x90EE(0, 0)`, what the encoder path has always sent) with the read-back of
    /// [`Self::write_ftp_profile_blob_verified`]. A camera holding no profile refuses the replace
    /// ([`FtpWriteRefusal::ReplaceSlotNotStored`]) instead of stalling.
    pub fn write_ftp_profile_verified(
        &mut self,
        profile: &FtpProfile,
        network: &FtpCameraNetwork,
        force_unconfirmed_model: bool,
    ) -> Result<FtpWriteOutcome, PtpUsbError> {
        let blob = encode_ftp_profile_with(profile, network)?;
        let [slot, param2] = FTP_PROFILE_WRITE_PARAMS;
        self.write_ftp_profile_blob_verified(slot, param2, &blob, force_unconfirmed_model)
    }

    /// Read the camera's FTP profile: `0x90E8` (the same fixed 3-byte setup
    /// the write sends) → `0x90EB` (info; says whether a profile exists and
    /// in which slot) → `0x90ED(slot)` (the profile blob). That is NX
    /// Field's own sequence, as captured; it has been run over USB on a Z 9
    /// and a Z6III (2026-11-05). Nothing in it writes a profile.
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
        let mut warnings: Vec<String> = unsurveyed_model_warning(self.model(), "the FTP-profile read").into_iter().collect();
        let (info, mut replies) = self.read_ftp_info_step()?;

        let wanted = match slot {
            Some(n) if n >= info.count => return Err(PtpUsbError::FtpSlotOutOfRange { slot: n, count: info.count }),
            Some(n) => Some(n),
            // The active slot gets the same bound as an explicit one: `0x90ED(count)` stalls the pipe,
            // and a camera holding nothing (count 0) can still report slot 0.
            None if info.has_active_profile() && info.slot >= info.count => {
                warnings.push(format!(
                    "0x90EB reports active slot {} but only {} profile(s) are stored; 0x90ED was not sent (it stalls the pipe past the end)",
                    info.slot, info.count
                ));
                None
            }
            None => info.has_active_profile().then_some(info.slot),
        };
        let profile = if let Some(wanted) = wanted {
            let reply = self.read_ftp_profile_reply(wanted)?;
            replies.push(FtpReplyNotes::of(OP_FTP_PROFILE_READ, &reply));
            let decoded = decode_ftp_profile(&reply.data);
            Some(FtpProfileBlob { raw: reply.data, decoded })
        } else {
            None
        };
        let reply_matched_command = replies.iter().all(|n| n.deviations.is_empty());
        Ok(FtpProfileReadout { info, profile, warnings, reply_matched_command, replies })
    }
}

/// What is known about the damage a probe op can do. One closed classification, assigned in
/// [`FTP_PROBE_OP_TABLE`] and nowhere else: the gates in [`check_ftp_probe_request`] and
/// [`PtpUsbSession::probe_ftp_op`] read it, so no site re-encodes which ops delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpProbeRisk {
    /// `0x9FFF`: an op the camera does not have (`0x2005` OperationNotSupported at once, list unchanged).
    /// The control; changes nothing.
    Control,
    /// `0x90E9`, `0x90EC`: they exist and their purpose is unknown. Sent with data, `0x90EC` blanked the
    /// stored `0x90EB` device-name string on a Z 9 and nothing restored it; `0x90E9` sent bare wedged
    /// the camera until a power cycle. Treated as writes.
    Unexplained,
    /// `0x90EA`: **deletes every stored FTP profile** (Z 9, 2026-07-10: count 4 to 0, no confirmation).
    DeleteAll,
    /// `0x90EF(n)`: **deletes stored profile `n`** (Z 9, 2026-07-10).
    DeleteOne,
}

impl FtpProbeRisk {
    /// The op deletes stored profiles; sending it needs the specific opt-in
    /// ([`FtpProbeConsent::destructive_op`]) on top of the model gate.
    pub fn is_destructive(self) -> bool {
        matches!(self, FtpProbeRisk::DeleteAll | FtpProbeRisk::DeleteOne)
    }

    /// The op can change what the camera stores, so it passes the same model gate as the FTP writes
    /// ([`FTP_WRITE_CONFIRMED_MODELS`] or `force_unconfirmed_model`). Only the control cannot.
    pub fn may_change_camera(self) -> bool {
        !matches!(self, FtpProbeRisk::Control)
    }

    /// What the op is known to do, for refusal text.
    pub fn describe(self) -> &'static str {
        match self {
            FtpProbeRisk::Control => "does not exist on the camera (a control)",
            FtpProbeRisk::Unexplained => "has an unknown purpose and has changed stored camera state",
            FtpProbeRisk::DeleteAll => "deletes ALL stored FTP profiles",
            FtpProbeRisk::DeleteOne => "deletes the stored FTP profile it is given",
        }
    }
}

/// The only operations [`PtpUsbSession::probe_ftp_op`] will send, each with what is known about it: the
/// unused numbers in the FTP range plus `0x9FFF`, which should not exist, as a control for "what does an
/// op the camera does not have look like". The one source of the allow-list ([`FTP_PROBE_OPS`] is derived
/// from it) and of which entries are destructive.
pub const FTP_PROBE_OP_TABLE: [(u16, FtpProbeRisk); 5] = [
    (0x90E9, FtpProbeRisk::Unexplained),
    (0x90EA, FtpProbeRisk::DeleteAll),
    (0x90EC, FtpProbeRisk::Unexplained),
    (0x90EF, FtpProbeRisk::DeleteOne),
    (0x9FFF, FtpProbeRisk::Control),
];

/// The op codes of [`FTP_PROBE_OP_TABLE`].
pub const FTP_PROBE_OPS: [u16; FTP_PROBE_OP_TABLE.len()] = {
    let mut ops = [0u16; FTP_PROBE_OP_TABLE.len()];
    let mut i = 0;
    while i < ops.len() {
        ops[i] = FTP_PROBE_OP_TABLE[i].0;
        i += 1;
    }
    ops
};

/// A probe sends at most this many parameters.
pub const FTP_PROBE_MAX_PARAMS: usize = 2;
/// A probe's data-out is at most this many bytes.
pub const FTP_PROBE_MAX_DATA: usize = 32;
/// The parameter value a probe refuses: as a slot number it hung a camera's network stack.
pub const FTP_PROBE_REFUSED_PARAM: u32 = u32::MAX;
/// At most this many stored profiles are read for a before/after list. The camera reports a limit of 20;
/// a count above this is a camera-supplied number this code does not trust, and the list says so
/// ([`FtpListState::truncated`]).
pub const FTP_LIST_MAX_READ: u32 = 32;

/// The risk class of `op`, or `None` when it is not on [`FTP_PROBE_OP_TABLE`].
pub fn ftp_probe_risk(op: u16) -> Option<FtpProbeRisk> {
    FTP_PROBE_OP_TABLE.iter().find(|(o, _)| *o == op).map(|(_, r)| *r)
}

/// [`FTP_PROBE_OPS`] for refusal text, with the control marked.
fn ftp_probe_ops_text() -> String {
    FTP_PROBE_OP_TABLE
        .iter()
        .map(|(op, risk)| if *risk == FtpProbeRisk::Control { format!("0x{op:04x} as a control") } else { format!("0x{op:04x}") })
        .collect::<Vec<_>>()
        .join(", ")
}

/// What the caller has agreed to for a probe, beyond the op being on the list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FtpProbeConsent {
    /// Send an op that can change the camera to a model outside [`FTP_WRITE_CONFIRMED_MODELS`], like
    /// `force_unconfirmed_model` on the FTP writes.
    pub force_unconfirmed_model: bool,
    /// The destructive op the caller names as the one it means to send. Must equal the op sent;
    /// naming one op never opens another.
    pub destructive_op: Option<u16>,
}

/// Every refusal that needs no camera, in the order the probe reports them: the op is on the list, the
/// parameters and data are within their limits, and a destructive op was named by the caller. The one
/// copy of these rules, used by [`PtpUsbSession::probe_ftp_op`] and [`probe_ftp_op_locked`] (which calls it
/// before opening a session at all). Returns the op's [`FtpProbeRisk`].
pub fn check_ftp_probe_request(
    op: u16,
    params: &[u32],
    data: Option<&[u8]>,
    consent: FtpProbeConsent,
) -> Result<FtpProbeRisk, PtpUsbError> {
    let Some(risk) = ftp_probe_risk(op) else {
        return Err(PtpUsbError::FtpProbeOpNotAllowed { op });
    };
    if params.len() > FTP_PROBE_MAX_PARAMS || params.contains(&FTP_PROBE_REFUSED_PARAM) {
        return Err(PtpUsbError::FtpProbeBadParams { params: params.to_vec() });
    }
    if let Some(d) = data
        && d.len() > FTP_PROBE_MAX_DATA
    {
        return Err(PtpUsbError::FtpProbeDataTooLong { len: d.len() });
    }
    if risk.is_destructive() && consent.destructive_op != Some(op) {
        return Err(PtpUsbError::FtpProbeDestructiveNotConfirmed { op, risk });
    }
    Ok(risk)
}

/// One stored profile as the probe read it. The raw bytes are kept (and compared) so a change the
/// decoder has no field for still shows; `Debug` shows only their length, they hold the password.
#[derive(Clone, PartialEq, Eq)]
pub struct FtpListEntry {
    pub raw: Vec<u8>,
    /// `None` when the blob does not decode.
    pub decoded: Option<DecodedFtpProfile>,
}

impl std::fmt::Debug for FtpListEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FtpListEntry")
            .field("raw", &format_args!("<{} byte(s) hidden>", self.raw.len()))
            .field("decoded", &self.decoded)
            .finish()
    }
}

/// The camera's stored FTP profile list as the probe sees it: everything `0x90EB` reports (count, limit,
/// the selected index, which follows its profile as the list is reordered, the GUID and the device-name
/// string) and every profile read, in list order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpListState {
    pub info: FtpProfileInfo,
    /// The first [`FTP_LIST_MAX_READ`] profiles at most, in list order.
    pub entries: Vec<FtpListEntry>,
    /// `info.count` is above [`FTP_LIST_MAX_READ`], so `entries` is a prefix of the list.
    pub truncated: bool,
}

/// A field of the `0x90EB` reply, for [`FtpListChange::Info`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpInfoField {
    Header,
    Count,
    Limit,
    Selected,
    Guid,
    DeviceName,
    Trailer,
    Truncated,
}

impl FtpInfoField {
    pub fn name(self) -> &'static str {
        match self {
            FtpInfoField::Header => "header",
            FtpInfoField::Count => "count",
            FtpInfoField::Limit => "limit",
            FtpInfoField::Selected => "selected index",
            FtpInfoField::Guid => "GUID",
            FtpInfoField::DeviceName => "device name",
            FtpInfoField::Trailer => "trailer",
            FtpInfoField::Truncated => "list truncated",
        }
    }
}

/// One difference between two [`FtpListState`]s. Names fields, never values: the profiles hold a password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FtpListChange {
    /// A field of the `0x90EB` reply.
    Info(FtpInfoField),
    /// The profile at `index` differs. `fields` names the decoded fields that differ; it is empty when
    /// the bytes differ and no named field explains it (a blob that does not decode, or a structural byte).
    ProfileChanged { index: usize, fields: Vec<&'static str> },
    ProfileAdded { index: usize },
    ProfileRemoved { index: usize },
}

impl FtpListState {
    /// Profiles that were read but do not decode.
    pub fn undecodable_count(&self) -> usize {
        self.entries.iter().filter(|e| e.decoded.is_none()).count()
    }

    /// Everything that differs between `self` (before) and `after`; empty exactly when they are equal.
    pub fn changes_to(&self, after: &FtpListState) -> Vec<FtpListChange> {
        // Destructured without `..`: a new field of the info must be given a comparison to compile.
        let FtpProfileInfo { header, count, limit, slot, guid, device_name, trailer } = &self.info;
        let mut changes = Vec::new();
        let mut info = |differs: bool, field| {
            if differs {
                changes.push(FtpListChange::Info(field));
            }
        };
        info(*header != after.info.header, FtpInfoField::Header);
        info(*count != after.info.count, FtpInfoField::Count);
        info(*limit != after.info.limit, FtpInfoField::Limit);
        info(*slot != after.info.slot, FtpInfoField::Selected);
        info(*guid != after.info.guid, FtpInfoField::Guid);
        info(*device_name != after.info.device_name, FtpInfoField::DeviceName);
        info(*trailer != after.info.trailer, FtpInfoField::Trailer);
        info(self.truncated != after.truncated, FtpInfoField::Truncated);
        for index in 0..self.entries.len().max(after.entries.len()) {
            match (self.entries.get(index), after.entries.get(index)) {
                (Some(a), Some(b)) if a != b => {
                    let fields = match (&a.decoded, &b.decoded) {
                        (Some(x), Some(y)) => ftp_profile_differences(x, y),
                        _ => Vec::new(),
                    };
                    changes.push(FtpListChange::ProfileChanged { index, fields });
                }
                (Some(_), Some(_)) => {}
                (None, Some(_)) => changes.push(FtpListChange::ProfileAdded { index }),
                (Some(_), None) => changes.push(FtpListChange::ProfileRemoved { index }),
                (None, None) => unreachable!("index is below one of the two lengths"),
            }
        }
        changes
    }
}

/// What the camera did with a probed op. The transport result, typed: a lost reply is not a string.
#[derive(Debug)]
pub enum FtpProbeOutcome {
    /// The camera answered. The code may be non-OK (that is data here, not an error).
    Replied(FtpProbeReply),
    /// Nothing of the probed op reached the camera: the `0x90E8` setup before it failed, or the session
    /// was already unusable.
    NotSent { cause: PtpUsbError },
    /// The op's command went out (`data_sent`: and its data-out) and no usable reply came. The camera
    /// may or may not have acted, whatever the op: a delete whose reply was lost still may have
    /// deleted. The session is poisoned and the list must be re-read with a fresh process.
    Indeterminate { data_sent: bool, cause: PtpUsbError },
}

/// What one probed FTP-range op did. `after` is an error when the session could not read the list again
/// (a timeout or a pipe stall poisons it).
#[derive(Debug)]
pub struct FtpOpProbe {
    pub op: u16,
    pub risk: FtpProbeRisk,
    pub params: Vec<u32>,
    /// Data-out sent with the op, if any.
    pub data: Option<Vec<u8>>,
    pub setup_first: bool,
    pub before: FtpListState,
    pub outcome: FtpProbeOutcome,
    pub after: Result<FtpListState, PtpUsbError>,
    pub warnings: Vec<String>,
}

impl FtpOpProbe {
    /// The op's command went out and its effect is unknown: see [`FtpProbeOutcome::Indeterminate`].
    pub fn is_indeterminate(&self) -> bool {
        matches!(self.outcome, FtpProbeOutcome::Indeterminate { .. })
    }
}

/// The camera's answer to a probed op, verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpProbeReply {
    pub response_code: u16,
    /// The parameters of the response container.
    pub response_params: Vec<u32>,
    pub data: Vec<u8>,
    pub data_containers: usize,
    /// How the reply departed from the command (a different transaction id or operation code).
    pub deviations: Vec<ReplyDeviation>,
    /// Containers of a kind this module does not know that arrived and were skipped.
    pub unrecognized_containers: u32,
}

impl<T: BulkTransport> PtpUsbSession<T> {
    /// `0x90E8` setup, `0x90EB`, then `0x90ED(n)` for every stored profile (at most
    /// [`FTP_LIST_MAX_READ`]). Read-only.
    pub fn read_ftp_list(&mut self) -> Result<FtpListState, PtpUsbError> {
        self.ftp_list_state()
    }

    fn ftp_list_state(&mut self) -> Result<FtpListState, PtpUsbError> {
        let (info, _) = self.read_ftp_info_step()?;
        let truncated = info.count > FTP_LIST_MAX_READ;
        let mut entries = Vec::new();
        for n in 0..info.count.min(FTP_LIST_MAX_READ) {
            let reply = self.read_ftp_profile_reply(n)?;
            let decoded = decode_ftp_profile(&reply.data).ok();
            entries.push(FtpListEntry { raw: reply.data, decoded });
        }
        Ok(FtpListState { info, entries, truncated })
    }

    /// Send one op from [`FTP_PROBE_OP_TABLE`] with the given parameters and data-out, optionally right
    /// after the `0x90E8` setup that the profile ops need, and report the profile list before and after.
    /// EXPERIMENTAL: what these ops do is mostly unknown, so the list is read first and again afterwards
    /// and both are returned. Every refusal that needs no camera ([`check_ftp_probe_request`]) and the
    /// model gate (any op but the control needs a model in [`FTP_WRITE_CONFIRMED_MODELS`] or
    /// `consent.force_unconfirmed_model`, the gate the FTP writes use) happen before any I/O.
    ///
    /// A reply that never came is not an `Err`: the list read before is worth having, so it is
    /// [`FtpProbeOutcome::Indeterminate`] (or [`FtpProbeOutcome::NotSent`] when the op never left).
    pub fn probe_ftp_op(
        &mut self,
        op: u16,
        setup_first: bool,
        params: &[u32],
        data: Option<&[u8]>,
        consent: FtpProbeConsent,
    ) -> Result<FtpOpProbe, PtpUsbError> {
        let risk = check_ftp_probe_request(op, params, data, consent)?;
        if risk.may_change_camera() && !consent.force_unconfirmed_model && !ftp_write_confirmed(&self.model) {
            return Err(PtpUsbError::UnconfirmedModel(self.model.clone()));
        }
        let warnings: Vec<String> = unsurveyed_model_warning(self.model(), "the FTP-profile ops").into_iter().collect();
        let before = self.ftp_list_state()?;
        let setup = if setup_first { self.ftp_setup_step().map(drop) } else { Ok(()) };
        let outcome = match setup {
            Err(cause) => FtpProbeOutcome::NotSent { cause },
            Ok(()) => match self.transact(op, params, data, TxidCheck::Unchecked) {
                Ok(r) => FtpProbeOutcome::Replied(FtpProbeReply {
                    response_code: r.code,
                    response_params: r.params,
                    data: r.data,
                    data_containers: r.data_containers,
                    deviations: r.deviations,
                    unrecognized_containers: r.unrecognized_containers,
                }),
                Err(cause) if cause.nothing_was_sent() => FtpProbeOutcome::NotSent { cause },
                Err(cause) => FtpProbeOutcome::Indeterminate { data_sent: self.data_phase_started, cause },
            },
        };
        let after = self.ftp_list_state();
        Ok(FtpOpProbe {
            op,
            risk,
            params: params.to_vec(),
            data: data.map(<[u8]>::to_vec),
            setup_first,
            before,
            outcome,
            after,
            warnings,
        })
    }
}

/// [`PtpUsbSession::probe_ftp_op`] for a caller already holding the [`CameraLock`]; see
/// [`PtpUsbSession::open_locked`]. One probe per session. The requests that need no camera are refused
/// before a session is opened.
pub fn probe_ftp_op_locked(
    lock: CameraLock,
    serial: Option<&str>,
    op: u16,
    setup_first: bool,
    params: &[u32],
    data: Option<&[u8]>,
    consent: FtpProbeConsent,
) -> Result<FtpOpProbe, PtpUsbError> {
    probe_ftp_op_opening(|| PtpUsbSession::open_locked(lock, serial), op, setup_first, params, data, consent)
}

/// [`probe_ftp_op_locked`] with the session's opening injected: the camera-free refusals come first, so
/// `open` is not called for a request that was going to be refused.
fn probe_ftp_op_opening<T: BulkTransport>(
    open: impl FnOnce() -> Result<PtpUsbSession<T>, PtpUsbError>,
    op: u16,
    setup_first: bool,
    params: &[u32],
    data: Option<&[u8]>,
    consent: FtpProbeConsent,
) -> Result<FtpOpProbe, PtpUsbError> {
    check_ftp_probe_request(op, params, data, consent)?;
    open()?.probe_ftp_op(op, setup_first, params, data, consent)
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

/// Decode a PTP-string property value (`[u8 count incl. NUL][UTF-16LE units]`; count 0 is the
/// empty string). Strict: the byte length must be exactly `1 + 2 * count`, a non-empty string must
/// end in the NUL, and surrogates must pair. The text is returned as the camera holds it, so
/// trailing spaces from a fixed-width field are kept, not trimmed. The error says what was wrong.
pub fn decode_ptp_string_value(bytes: &[u8]) -> Result<String, String> {
    let length_error = |needed: usize, follow: usize| {
        Err(format!("count byte says {} unit(s) = {needed} byte(s), but {follow} byte(s) follow it", needed / 2))
    };
    match decode_ptp_string(bytes) {
        Err(StringFrameFault::NoCountByte) => Err("no bytes (a PTP string needs at least the count byte)".to_string()),
        Err(StringFrameFault::Truncated { needed, available }) => length_error(needed, available),
        Ok((_, used)) if used != bytes.len() => length_error(used - 1, bytes.len() - 1),
        Ok((string, _)) => match string.anomalies.first() {
            Some(anomaly) => Err(anomaly.describe().to_string()),
            None => Ok(string.text),
        },
    }
}

/// Encode `text` as a PTP string value: `[u8 count incl. NUL][UTF-16LE units][NUL]`. The empty
/// string is `01 00 00` (count 1, just the NUL), the form the camera itself reports for an empty
/// property. Refuses control characters and anything over 255 units including the NUL.
pub fn encode_ptp_string_value(text: &str) -> Result<Vec<u8>, String> {
    if text.chars().any(char::is_control) {
        return Err("the text contains a control character (NUL, newline, tab, ...)".to_string());
    }
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0);
    if units.len() > 255 {
        return Err(format!("the text is {} UTF-16 unit(s) with its terminator; a PTP string holds at most 255", units.len()));
    }
    let mut out = Vec::with_capacity(1 + units.len() * 2);
    out.push(units.len() as u8);
    for u in units {
        out.extend_from_slice(&u.to_le_bytes());
    }
    Ok(out)
}

/// [`PtpUsbSession::get_device_prop_value`] (`desc == false`) or
/// [`PtpUsbSession::get_device_prop_desc`] (`desc == true`) for a caller already holding the
/// [`CameraLock`]; see [`PtpUsbSession::open_locked`]. Read-only.
pub fn get_device_prop_locked(
    lock: CameraLock,
    serial: Option<&str>,
    propcode: u16,
    desc: bool,
) -> Result<VendorRead, PtpUsbError> {
    let mut session = PtpUsbSession::open_locked(lock, serial)?;
    if desc {
        session.get_device_prop_desc(propcode)
    } else {
        session.get_device_prop_value(propcode)
    }
}

/// [`PtpUsbSession::read_iptc_slot`] for a caller already holding the [`CameraLock`]. Read-only.
pub fn read_iptc_slot_locked(lock: CameraLock, serial: Option<&str>, slot: u32) -> Result<VendorRead, PtpUsbError> {
    let mut session = PtpUsbSession::open_locked(lock, serial)?;
    session.read_iptc_slot(slot)
}

/// [`PtpUsbSession::set_string_property`] for a caller already holding the [`CameraLock`].
pub fn set_string_property_locked(
    lock: CameraLock,
    serial: Option<&str>,
    propcode: u16,
    text: &str,
    force_unconfirmed_model: bool,
) -> Result<PropWriteReceipt, PtpUsbError> {
    let mut session = PtpUsbSession::open_locked(lock, serial)?;
    session.set_string_property(propcode, text, force_unconfirmed_model)
}

/// [`PtpUsbSession::plan_string_property`] for a caller already holding the [`CameraLock`]. Read-only.
pub fn plan_string_property_locked(
    lock: CameraLock,
    serial: Option<&str>,
    propcode: u16,
    text: &str,
    force_unconfirmed_model: bool,
) -> Result<StringPropPlan, PtpUsbError> {
    check_string_prop_known(propcode)?;
    let mut session = PtpUsbSession::open_locked(lock, serial)?;
    session.plan_string_property(propcode, text, force_unconfirmed_model)
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

/// [`PtpUsbSession::plan_ftp_write`] for a caller already holding the [`CameraLock`]. Read-only.
pub fn plan_ftp_write_locked(
    lock: CameraLock,
    serial: Option<&str>,
    slot: u32,
    param2: u32,
    blob: &[u8],
    force_unconfirmed_model: bool,
) -> Result<FtpWritePlan, PtpUsbError> {
    let mut session = PtpUsbSession::open_locked(lock, serial)?;
    session.plan_ftp_write(slot, param2, blob, force_unconfirmed_model)
}

/// [`PtpUsbSession::write_ftp_profile_blob_verified`] for a caller already holding the [`CameraLock`].
pub fn write_ftp_profile_blob_verified_locked(
    lock: CameraLock,
    serial: Option<&str>,
    slot: u32,
    param2: u32,
    blob: &[u8],
    force_unconfirmed_model: bool,
) -> Result<FtpWriteOutcome, PtpUsbError> {
    let mut session = PtpUsbSession::open_locked(lock, serial)?;
    session.write_ftp_profile_blob_verified(slot, param2, blob, force_unconfirmed_model)
}

/// [`PtpUsbSession::write_ftp_profile_verified`] for a caller already holding the [`CameraLock`].
pub fn write_ftp_profile_verified_locked(
    lock: CameraLock,
    serial: Option<&str>,
    profile: &FtpProfile,
    network: &FtpCameraNetwork,
    force_unconfirmed_model: bool,
) -> Result<FtpWriteOutcome, PtpUsbError> {
    let mut session = PtpUsbSession::open_locked(lock, serial)?;
    session.write_ftp_profile_verified(profile, network, force_unconfirmed_model)
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
    parse_prefixed_number(s, "property code", u16::from_str_radix)
}

/// A `u32` in the same format as [`parse_propcode`] (`0x` hex or plain decimal), for the CLI's
/// numeric parameters. The one parser behind both, so the accepted format cannot drift.
pub fn parse_u32_number(s: &str) -> Result<u32, String> {
    parse_prefixed_number(s, "number", u32::from_str_radix)
}

fn parse_prefixed_number<T>(
    s: &str,
    what: &str,
    parse_radix: fn(&str, u32) -> Result<T, std::num::ParseIntError>,
) -> Result<T, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        parse_radix(hex, 16).map_err(|e| format!("invalid hex {what} {s:?}: {e}"))
    } else {
        parse_radix(s, 10).map_err(|e| format!("invalid {what} {s:?}: {e}"))
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
        let port_tag_pos = blob.windows(2).rposition(|w| w == [0x05, 0x00]).unwrap();
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

    /// Length in bytes of the template block (two timestamps, the network tag, address, prefix, padding).
    const FTP_TEMPLATE_LEN: usize = 82;

    #[test]
    fn template_block_is_82_bytes_and_equals_the_captured_bytes() {
        // The block sits after the header (2) and the profile name ("TEST": 1 + 5 * 2 bytes).
        let blob = encode_ftp_profile_with(&valid_profile(), &FtpCameraNetwork::CAPTURED).unwrap();
        let block = blob[13..13 + FTP_TEMPLATE_LEN].to_vec();
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
            if spec.encoding() == FtpEncoding::Utf16U8Len {
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
            max_len: 1000,
            trimmed_for_required_check: true,
        };
        let encoding = FtpEncoding::Utf16U8Len;
        let ok = ftp_layout::encode_field_with(&spec, encoding, &"a".repeat(254)).unwrap();
        assert_eq!(ok[0], 255);
        assert_eq!(ok.len(), 1 + 255 * 2);
        let err = ftp_layout::encode_field_with(&spec, encoding, &"a".repeat(255)).unwrap_err();
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
        r.skip(2); // header f4 01
        let profile_name = r.utf16_field_1byte_len();
        r.skip(FTP_TEMPLATE_LEN);
        r.skip(4); // 2.4 GHz SSID tag
        let ssid_24ghz = r.ascii_field();
        r.skip(2); // 5 GHz SSID tag
        let ssid_5ghz = r.ascii_field();
        r.skip(1); // host tag
        let host = r.utf16_field_1byte_len();
        r.skip(3); // empty folder string (00) and the FTP port (15 00)
        let username = r.ascii_field();
        let password = r.ascii_field(); // no tag — immediately follows username
        r.skip(2); // option flags (05 00)
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

    // ── decode_ftp_profile_spans ─────────────────────────────────────────

    /// The two secrets' ranges by name: the layout marks exactly the 5 GHz field and the password.
    struct SecretRanges {
        ssid_5ghz: std::ops::Range<usize>,
        password: std::ops::Range<usize>,
    }

    fn secret_ranges(spans: &FtpSecretSpans) -> SecretRanges {
        assert_eq!(spans.spans.len(), 2, "the layout marks the 5 GHz field and the password secret");
        SecretRanges { ssid_5ghz: spans.of("ssid_5ghz").unwrap().clone(), password: spans.of("password").unwrap().clone() }
    }

    #[test]
    fn the_secret_spans_are_the_content_bytes_of_the_password_and_the_5ghz_field() {
        let mut p = valid_profile();
        // Values chosen so that no earlier field repeats them.
        p.password = "Zq9!pw".into();
        p.ssid_5ghz = "Wl4-5g".into();
        let blob = encode_ftp_profile(&p).unwrap();
        let (d, spans) = decode_ftp_profile_spans(&blob).unwrap();
        let spans = secret_ranges(&spans);
        assert_eq!(d, decode_ftp_profile(&blob).unwrap());
        assert_eq!(&blob[spans.password.clone()], b"Zq9!pw");
        assert_eq!(&blob[spans.ssid_5ghz.clone()], b"Wl4-5g");
        // The spans sit just after their own u32 length prefix.
        assert_eq!(&blob[spans.password.start - 4..spans.password.start], &6u32.to_le_bytes());
        assert!(spans.ssid_5ghz.end < spans.password.start, "the 5 GHz field precedes the password in the layout");
    }

    #[test]
    fn the_spans_point_at_the_real_field_when_an_earlier_field_holds_the_same_text() {
        // username == password: a search for the password text finds the USERNAME first.
        let mut p = valid_profile();
        p.username = "samepw".into();
        p.password = "samepw".into();
        let blob = encode_ftp_profile(&p).unwrap();
        let first_by_content = blob.windows(6).position(|w| w == b"samepw").unwrap();
        let (_, spans) = decode_ftp_profile_spans(&blob).unwrap();
        let spans = secret_ranges(&spans);
        assert!(first_by_content < spans.password.start, "the username copy comes first");
        assert_eq!(&blob[spans.password.clone()], b"samepw");
        // An empty password has an empty span, still at the right place.
        p.password = String::new();
        let blob = encode_ftp_profile(&p).unwrap();
        let (_, spans) = decode_ftp_profile_spans(&blob).unwrap();
        let spans = secret_ranges(&spans);
        assert!(spans.password.is_empty());
        assert_eq!(blob[spans.password.start - 4..spans.password.start], 0u32.to_le_bytes());
    }

    #[test]
    fn a_blob_that_does_not_decode_has_no_spans() {
        let mut blob = encode_ftp_profile(&valid_profile()).unwrap();
        blob[0] = 0;
        assert!(decode_ftp_profile_spans(&blob).is_err());
    }

    // ── parse_u32_number ─────────────────────────────────────────────────

    #[test]
    fn parse_u32_number_takes_decimal_and_prefixed_hex_and_nothing_else() {
        assert_eq!(parse_u32_number("10"), Ok(10), "a bare number is decimal even when it looks like hex");
        assert_eq!(parse_u32_number("0x10"), Ok(16));
        assert_eq!(parse_u32_number(" 0XfF "), Ok(255));
        // The u32 boundary, both sides.
        assert_eq!(parse_u32_number("4294967295"), Ok(u32::MAX));
        assert_eq!(parse_u32_number("0xFFFFFFFF"), Ok(u32::MAX));
        assert!(parse_u32_number("4294967296").is_err());
        assert!(parse_u32_number("0x100000000").is_err());
        assert!(parse_u32_number("").is_err());
        assert!(parse_u32_number("0x").is_err());
        assert!(parse_u32_number("ff").is_err(), "hex needs the prefix");
        assert!(parse_u32_number("-1").is_err());
        // Same format and wording as parse_propcode: one parser behind both.
        assert!(parse_u32_number("zz").unwrap_err().starts_with("invalid number"));
        assert!(parse_u32_number("0xzz").unwrap_err().starts_with("invalid hex number"));
        assert!(parse_propcode("zz").unwrap_err().starts_with("invalid property code"));
        assert!(parse_propcode("0xzz").unwrap_err().starts_with("invalid hex property code"));
        assert_eq!(parse_propcode("65535"), Ok(0xFFFF));
        assert!(parse_propcode("65536").is_err());
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
        assert_eq!(r.unrecognized_containers, 2, "the event is not an unrecognized container, the two strays are");
        assert_eq!(s.unrecognized_container_count(), 2);
        assert_eq!(s.event_container_count(), 1, "the skipped event is counted on its own");
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

    // ── diagnostics: every transaction is one span ───────────────────────

    /// A session over a scripted transport with a recorder of its own (leaked: it must be
    /// `'static`, and a test process is short-lived). The temp dir is returned to keep it alive.
    fn recorded(steps: Vec<Step>, model: &str) -> (PtpUsbSession<ScriptedTransport>, ScriptedTransport, &'static crate::diag::Diag, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::diag::Config::new(dir.path().to_path_buf(), crate::diag::Subsystem::Cli);
        cfg.pid = 7000 + dir.path().to_string_lossy().len() as u32; // distinct enough: each test has its own directory
        let d: &'static crate::diag::Diag = Box::leak(Box::new(crate::diag::Diag::new(cfg).unwrap()));
        let (mut s, t) = session(steps, model);
        s.diag = Some(d);
        (s, t, d, dir)
    }
    fn field<'a>(e: &'a crate::diag::Entry, key: &str) -> Option<&'a crate::diag::Val> {
        e.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    #[test]
    fn a_read_transaction_is_one_leaf_span_with_the_op_params_response_and_no_payload() {
        use crate::diag::{Class, Kind, Outcome, Subsystem, Val};
        let (mut s, _t, d, _dir) = recorded(vec![data_c(OP_VENDOR_PROP_READ, &[0xAA, 0xBB]), resp(RESP_OK)], "Z 9");
        s.read_vendor_property(0xD053).unwrap();
        let ring = d.snapshot();
        assert_eq!(ring.len(), 1);
        let e = &ring[0];
        assert_eq!((e.kind, e.class, e.subsystem, e.op.as_str(), e.outcome), (Kind::Span, Class::Leaf, Subsystem::Usb, "ptp_943b", Some(Outcome::Ok)));
        assert_eq!(field(e, "op"), Some(&Val::Hex(0x943B)));
        assert_eq!(field(e, "txid"), Some(&Val::U64(0)));
        assert_eq!(field(e, "params"), Some(&Val::U64(1)));
        assert_eq!(field(e, "p0"), Some(&Val::Hex(0x1D053)));
        assert_eq!(field(e, "response"), Some(&Val::Hex(0x2001)));
        assert_eq!(field(e, "reply_len"), Some(&Val::Len(2)));
        assert_eq!(field(e, "reply_hash"), Some(&Val::hash(&[0xAA, 0xBB])));
        assert_eq!(field(e, "phase"), Some(&Val::text("command_sent")), "no data-out, so never past command_sent");
        assert!(field(e, "data_len").is_none(), "a read has no data-out fields");
        assert!(d.open_spans().is_empty());
    }

    #[test]
    fn a_write_records_the_length_and_hash_of_its_payload_never_the_bytes_and_reaches_data_sent() {
        use crate::diag::{Outcome, Val};
        let payload = b"profile-with-password-hunter2";
        let (mut s, _t, d, dir) = recorded(vec![resp(RESP_OK)], "Z 9");
        s.transact(OP_FTP_PROFILE_WRITE, &[0, 1], Some(payload), TxidCheck::Unchecked).unwrap();
        let e = d.snapshot().pop().unwrap();
        assert_eq!(e.outcome, Some(Outcome::Ok));
        assert_eq!(field(&e, "data_len"), Some(&Val::Len(payload.len() as u64)));
        assert!(field(&e, "data_hash").is_none(), "an FTP profile holds a password: its length only, no hash");
        assert_eq!(field(&e, "phase"), Some(&Val::text("data_sent")));
        assert_eq!((field(&e, "p0"), field(&e, "p1")), (Some(&Val::Hex(0)), Some(&Val::Hex(1))));
        let on_disk: String = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|f| f.path().extension().is_some_and(|x| x == "jsonl" || x == "json"))
            .map(|f| std::fs::read_to_string(f.path()).unwrap())
            .collect();
        assert!(!on_disk.contains("hunter2"), "the payload must not reach any diagnostics file");
        assert!(!on_disk.contains("profile-with"), "{on_disk}");
    }

    #[test]
    fn a_transaction_that_sends_data_is_a_top_span_and_one_that_does_not_is_a_leaf() {
        use crate::diag::{Class, Outcome};
        // Both neighbours of the boundary: no data phase, an empty one, a non-empty one.
        assert_eq!(txn_span_class(None), Class::Leaf);
        assert_eq!(txn_span_class(Some(&[])), Class::Top, "an empty data phase is still a write");
        assert_eq!(txn_span_class(Some(&[1])), Class::Top);
        // The spans the session records carry it: a property write is Top, a property read Leaf.
        let (mut s, _t, d, _dir) = recorded(vec![resp(RESP_OK), data_c(OP_VENDOR_PROP_READ, &[1]), resp(RESP_OK)], "Z 9");
        s.transact(OP_SET_DEVICE_PROP_VALUE, &[0xD090], Some(&[1, 0, 0]), TxidCheck::Unchecked).unwrap();
        s.read_vendor_property(0xD053).unwrap();
        let ring = d.snapshot();
        assert_eq!((ring[0].op.as_str(), ring[0].class, ring[0].outcome), ("ptp_1016", Class::Top, Some(Outcome::Ok)));
        assert_eq!((ring[1].op.as_str(), ring[1].class), ("ptp_943b", Class::Leaf));
    }

    #[test]
    fn up_to_five_parameters_are_recorded_individually_and_the_count_is_always_there() {
        use crate::diag::Val;
        let (mut s, _t, d, _dir) = recorded(vec![resp(RESP_OK), resp(RESP_OK)], "Z 9");
        s.transact(0x9999, &[], None, TxidCheck::Unchecked).unwrap();
        s.transact(0x9999, &[1, 2, 3, 4, 5], None, TxidCheck::Unchecked).unwrap();
        let ring = d.snapshot();
        assert_eq!(field(&ring[0], "params"), Some(&Val::U64(0)));
        assert!(field(&ring[0], "p0").is_none());
        assert_eq!(field(&ring[1], "params"), Some(&Val::U64(5)));
        assert_eq!(field(&ring[1], "p4"), Some(&Val::Hex(5)));
    }

    #[test]
    fn a_non_ok_answer_is_failed_a_timeout_is_timeout_a_stall_is_stalled() {
        use crate::diag::{Outcome, Val};
        // The camera answered and said no: the transaction completed, the operation was refused.
        let (mut s, _t, d, _dir) = recorded(vec![resp(0x200A)], "Z 9");
        let _ = s.read_vendor_property(0xD072);
        let e = d.snapshot().pop().unwrap();
        assert_eq!((e.outcome, field(&e, "response")), (Some(Outcome::Failed), Some(&Val::Hex(0x200A))));
        // Silence from the camera, no data-out sent.
        let (mut s, _t, d, _dir) = recorded(vec![Step::Err(rusb::Error::Timeout)], "Z 9");
        let _ = s.transact(OP_GET_DEVICE_INFO, &[], None, TxidCheck::Unchecked);
        assert_eq!(d.snapshot().pop().unwrap().outcome, Some(Outcome::Timeout));
        // A stalled pipe.
        let (mut s, _t, d, _dir) = recorded(vec![Step::Err(rusb::Error::Pipe)], "Z 9");
        let _ = s.transact(OP_GET_DEVICE_INFO, &[], None, TxidCheck::Unchecked);
        assert_eq!(d.snapshot().pop().unwrap().outcome, Some(Outcome::Stalled));
        // Any other failure before data-out.
        let (mut s, _t, d, _dir) = recorded(vec![Step::Err(rusb::Error::NoDevice)], "Z 9");
        let _ = s.transact(OP_GET_DEVICE_INFO, &[], None, TxidCheck::Unchecked);
        let e = d.snapshot().pop().unwrap();
        assert_eq!(e.outcome, Some(Outcome::Failed));
        assert_eq!(field(&e, "error_kind"), Some(&Val::text("usb")));
        assert_eq!(field(&e, "usb_error"), Some(&Val::text("NoDevice")));
        assert!(field(&e, "error").is_none(), "the Display text is not recorded");
    }

    #[test]
    fn an_error_after_the_data_went_out_is_indeterminate_whatever_the_error() {
        use crate::diag::{Outcome, Val};
        for err in [rusb::Error::Timeout, rusb::Error::Pipe, rusb::Error::NoDevice] {
            let (mut s, _t, d, _dir) = recorded(vec![Step::Err(err)], "Z 9");
            let _ = s.transact(OP_FTP_PROFILE_WRITE, &[0, 0], Some(&[1, 2, 3]), TxidCheck::Unchecked);
            let e = d.snapshot().pop().unwrap();
            assert_eq!(e.outcome, Some(Outcome::Indeterminate), "{err:?}");
            assert_eq!(field(&e, "phase"), Some(&Val::text("data_sent")), "{err:?}: the last stage reached");
        }
        // The same error with no data-out is not indeterminate: nothing the camera could act on went out.
        let (mut s, _t, d, _dir) = recorded(vec![Step::Err(rusb::Error::Timeout)], "Z 9");
        let _ = s.transact(OP_FTP_PROFILE_WRITE, &[0, 0], None, TxidCheck::Unchecked);
        assert_eq!(d.snapshot().pop().unwrap().outcome, Some(Outcome::Timeout));
    }

    #[test]
    fn a_refusal_from_a_poisoned_session_records_nothing_because_nothing_was_sent() {
        let (mut s, t, d, _dir) = recorded(vec![Step::Err(rusb::Error::Pipe)], "Z 9");
        assert!(s.query_vendor_op(0x90ED).is_err());
        assert_eq!(d.snapshot().len(), 1);
        let sent_before = t.sent().len();
        assert!(matches!(s.query_vendor_op(0x90ED), Err(PtpUsbError::SessionPoisoned { .. })));
        assert_eq!(d.snapshot().len(), 1, "no span for a transaction that never started");
        assert_eq!(t.sent().len(), sent_before, "and nothing went to the pipe");
    }

    #[test]
    fn the_span_name_carries_the_operation_code_and_an_empty_reply_has_no_hash() {
        use crate::diag::Val;
        let (mut s, _t, d, _dir) = recorded(vec![resp(RESP_OK), data_c(OP_GET_DEVICE_PROP_VALUE, &[1, 0, 0]), resp(RESP_OK), resp(RESP_OK)], "Z 9");
        s.transact(OP_CLOSE_SESSION, &[], None, TxidCheck::Unchecked).unwrap();
        s.get_device_prop_value(0xD090).unwrap();
        s.transact(OP_CLOSE_SESSION, &[], None, TxidCheck::Unchecked).unwrap();
        let ring = d.snapshot();
        assert_eq!(ring.iter().map(|e| e.op.as_str()).collect::<Vec<_>>(), vec!["ptp_1003", "ptp_1015", "ptp_1003"]);
        assert_eq!(field(&ring[0], "reply_len"), Some(&Val::Len(0)));
        assert!(field(&ring[0], "reply_hash").is_none(), "no data, no hash");
        assert!(field(&ring[1], "reply_hash").is_some());
        // Leading zeros are kept so a name always has four hex digits.
        assert_eq!(format!("ptp_{:04x}", 0x0u16), "ptp_0000");
    }

    #[test]
    fn txn_outcome_pins_each_branch() {
        use crate::diag::Outcome;
        let reply = |code| Reply { code, params: vec![], data: vec![], data_containers: 0, deviations: vec![], unrecognized_containers: 0 };
        assert_eq!(txn_outcome(&Ok(reply(RESP_OK)), false), Outcome::Ok);
        assert_eq!(txn_outcome(&Ok(reply(RESP_OK)), true), Outcome::Ok, "a completed write is ok");
        assert_eq!(txn_outcome(&Ok(reply(0x2002)), true), Outcome::Failed, "a refusal after data-out is definite");
        assert_eq!(txn_outcome(&Ok(reply(0x2001 + 1)), false), Outcome::Failed);
        assert_eq!(txn_outcome(&Err(PtpUsbError::TransactionTimedOut(Duration::from_secs(1))), false), Outcome::Timeout);
        assert_eq!(txn_outcome(&Err(PtpUsbError::TransactionTimedOut(Duration::from_secs(1))), true), Outcome::Indeterminate);
    }

    // ── GetDevicePropValue / GetDevicePropDesc ───────────────────────────

    #[test]
    fn get_device_prop_value_sends_one_param_the_bare_propcode_and_returns_data() {
        let (mut s, t) = session(vec![data_c(OP_GET_DEVICE_PROP_VALUE, &[1, 0x41, 0]), resp(RESP_OK)], "Z 9");
        assert_eq!(s.get_device_prop_value(0xD072).unwrap().data, vec![1, 0x41, 0]);
        // No 0x10000 flag (that is the 0x943B wrapper's), no data-out.
        assert_eq!(t.sent(), vec![(CONTAINER_COMMAND, OP_GET_DEVICE_PROP_VALUE, 0, le(0xD072))]);
    }

    #[test]
    fn get_device_prop_desc_uses_op_0x1014_and_zero_extends_the_propcode() {
        for code in [0x0000u16, 0x7FFF, 0x8000, 0xD072, 0xFFFF] {
            let (mut s, t) = session(vec![data_c(OP_GET_DEVICE_PROP_DESC, &[9]), resp(RESP_OK)], "Z 9");
            s.get_device_prop_desc(code).unwrap();
            assert_eq!(t.sent(), vec![(CONTAINER_COMMAND, OP_GET_DEVICE_PROP_DESC, 0, le(u32::from(code)))], "{code:#x}");
        }
    }

    #[test]
    fn get_device_prop_non_ok_is_operation_failed_and_ok_without_data_is_no_data_phase() {
        let (mut s, _t) = session(vec![resp(0x200A)], "Z 9");
        let err = s.get_device_prop_value(0xD072).unwrap_err();
        assert!(
            matches!(err, PtpUsbError::OperationFailed { op: OP_GET_DEVICE_PROP_VALUE, code: PtpResponseCode(0x200A) }),
            "{err:?}"
        );
        let (mut s, _t) = session(vec![resp(RESP_OK)], "Z 9");
        let err = s.get_device_prop_desc(0xD072).unwrap_err();
        assert!(matches!(err, PtpUsbError::NoDataPhase { op: OP_GET_DEVICE_PROP_DESC }), "{err:?}");
        // An empty data phase is a value, not an absence.
        let (mut s, _t) = session(vec![data_c(OP_GET_DEVICE_PROP_VALUE, &[]), resp(RESP_OK)], "Z 9");
        assert_eq!(s.get_device_prop_value(0xD072).unwrap().data, Vec::<u8>::new());
    }

    #[test]
    fn decode_ptp_string_value_boundaries() {
        // count 0 is the empty string with nothing after it; count 1 is only the NUL: also empty.
        assert_eq!(decode_ptp_string_value(&[0]).unwrap(), "");
        assert_eq!(decode_ptp_string_value(&[1, 0, 0]).unwrap(), "");
        assert_eq!(decode_ptp_string_value(&[3, b'a', 0, b'b', 0, 0, 0]).unwrap(), "ab");
        // Fixed-width padding is data: kept.
        assert_eq!(decode_ptp_string_value(&[3, b'a', 0, b' ', 0, 0, 0]).unwrap(), "a ");
        // Non-BMP pair (U+1F4F7) decodes.
        assert_eq!(decode_ptp_string_value(&[3, 0x3D, 0xD8, 0xF7, 0xDC, 0, 0]).unwrap(), "\u{1F4F7}");
        // Empty input, a count that disagrees with the length (short and long), a missing NUL, an
        // embedded NUL, a lone surrogate: each is refused, not guessed at.
        assert!(decode_ptp_string_value(&[]).is_err());
        assert!(decode_ptp_string_value(&[2, b'a', 0]).is_err());
        assert!(decode_ptp_string_value(&[1, 0, 0, 0]).is_err());
        assert!(decode_ptp_string_value(&[1, b'a', 0]).is_err());
        assert!(decode_ptp_string_value(&[3, b'a', 0, 0, 0, b'b', 0]).is_err());
        assert!(decode_ptp_string_value(&[2, 0x3D, 0xD8, 0, 0]).is_err());
    }

    // ── set_string_property (0x1016) ─────────────────────────────────────
    // Reply bytes are SYNTHETIC but shaped like the Z 9's real ones (read 2026-11-05): a descriptor is
    // PropCode, DataType 0xFFFF, GetSet, default string, current string, form flag.

    fn prop_desc(propcode: u16, data_type: u16, get_set: u8) -> Vec<u8> {
        let mut d = propcode.to_le_bytes().to_vec();
        d.extend_from_slice(&data_type.to_le_bytes());
        d.push(get_set);
        d.extend_from_slice(&[1, 0, 0, 1, 0, 0, 0]); // default "", current "", form flag
        d
    }
    fn prop_str(text: &str) -> Vec<u8> {
        encode_ptp_string_value(text).unwrap()
    }
    /// The container kinds/ops/payloads a session sent, without transaction ids.
    fn sent_ops(t: &ScriptedTransport) -> Vec<(u16, u16, Vec<u8>)> {
        t.sent().into_iter().map(|(c, op, _id, payload)| (c, op, payload)).collect()
    }
    /// descriptor, current value, set, read-back.
    fn set_script(get_set: u8, previous: &str, set_reply: u16, read_back: &str) -> Vec<Step> {
        set_script_for(0xD090, get_set, previous, set_reply, read_back)
    }
    fn set_script_for(propcode: u16, get_set: u8, previous: &str, set_reply: u16, read_back: &str) -> Vec<Step> {
        vec![
            data_c(OP_GET_DEVICE_PROP_DESC, &prop_desc(propcode, 0xFFFF, get_set)),
            resp(RESP_OK),
            data_c(OP_GET_DEVICE_PROP_VALUE, &prop_str(previous)),
            resp(RESP_OK),
            resp(set_reply),
            data_c(OP_GET_DEVICE_PROP_VALUE, &prop_str(read_back)),
            resp(RESP_OK),
        ]
    }

    #[test]
    fn set_string_property_reads_desc_and_value_then_writes_then_reads_back() {
        let (mut s, t) = session(set_script(1, "old", RESP_OK, "new"), "Z 9");
        let r = s.set_string_property(0xD090, "new", false).unwrap();
        assert_eq!((r.previous.as_str(), r.written.as_str()), ("old", "new"));
        assert_eq!(r.read_back, Ok("new".to_string()));
        assert!(r.verified);
        assert_eq!(
            sent_ops(&t),
            vec![
                (CONTAINER_COMMAND, OP_GET_DEVICE_PROP_DESC, le(0xD090)),
                (CONTAINER_COMMAND, OP_GET_DEVICE_PROP_VALUE, le(0xD090)),
                (CONTAINER_COMMAND, OP_SET_DEVICE_PROP_VALUE, le(0xD090)),
                (CONTAINER_DATA, OP_SET_DEVICE_PROP_VALUE, prop_str("new")),
                (CONTAINER_COMMAND, OP_GET_DEVICE_PROP_VALUE, le(0xD090)),
            ]
        );
    }

    #[test]
    fn plan_string_property_checks_everything_the_write_checks_and_sends_no_write() {
        let two = |get_set| vec![data_c(OP_GET_DEVICE_PROP_DESC, &prop_desc(0xD090, 0xFFFF, get_set)), resp(RESP_OK), data_c(OP_GET_DEVICE_PROP_VALUE, &prop_str("old")), resp(RESP_OK)];
        let (mut s, t) = session(two(1), "Z 9");
        let plan = s.plan_string_property(0xD090, "new", false).unwrap();
        assert_eq!((plan.propcode, plan.previous.as_str()), (0xD090, "old"));
        let ops: Vec<u16> = sent_ops(&t).iter().filter(|(k, ..)| *k == CONTAINER_COMMAND).map(|(_, op, _)| *op).collect();
        assert_eq!(ops, vec![OP_GET_DEVICE_PROP_DESC, OP_GET_DEVICE_PROP_VALUE], "no 0x1016");
        // A read-only descriptor, text outside Latin-1, a control character and the model gate are each
        // refused, as the real write refuses them (the property gate has its own test with lists that differ).
        let (mut s, _t) = session(two(0), "Z 9");
        assert!(matches!(s.plan_string_property(0xD090, "new", false), Err(PtpUsbError::PropNotWritable { .. })));
        for bad in ["\u{65e5}", "a\nb", "a\u{7f}b"] {
            let (mut s, t) = session(vec![], "Z 9");
            assert!(matches!(s.plan_string_property(0xD090, bad, false), Err(PtpUsbError::PropValueBad { .. })), "{bad:?}");
            assert!(t.sent().is_empty(), "{bad:?}: refused before any I/O");
        }
        let (mut s, t) = session(vec![], "Z 8");
        assert!(matches!(s.plan_string_property(0xD090, "x", false), Err(PtpUsbError::UnconfirmedModel(_))));
        let (mut s, _t) = session(vec![], "Z 9");
        assert!(matches!(s.plan_string_property(0x1234, "x", false), Err(PtpUsbError::PropWriteUnsupported { propcode: 0x1234, .. })));
        assert!(t.sent().is_empty());
        // Artist passes without the force flag (the descriptor is still read), and forcing changes nothing.
        for force in [false, true] {
            let (mut s, _t) = session(
                vec![data_c(OP_GET_DEVICE_PROP_DESC, &prop_desc(0xD072, 0xFFFF, 1)), resp(RESP_OK), data_c(OP_GET_DEVICE_PROP_VALUE, &prop_str("")), resp(RESP_OK)],
                "Z 9",
            );
            assert_eq!(s.plan_string_property(0xD072, "x", force).unwrap().propcode, 0xD072, "force={force}");
        }
    }

    #[test]
    fn a_read_only_descriptor_is_refused_before_any_write() {
        // GetSet 0 = read-only; anything but exactly 1 is refused (pins 0x02 too, which is not "rw").
        for get_set in [0x00u8, 0x02, 0xFF] {
            let (mut s, t) = session(set_script(get_set, "old", RESP_OK, "new"), "Z 9");
            let err = s.set_string_property(0xD090, "new", false).unwrap_err();
            assert!(matches!(err, PtpUsbError::PropNotWritable { propcode: 0xD090, get_set: g } if g == get_set), "{err:?}");
            assert_eq!(t.sent().len(), 1, "only the descriptor read went out");
        }
    }

    #[test]
    fn a_non_string_descriptor_or_a_short_one_is_refused() {
        let (mut s, t) = session(vec![data_c(OP_GET_DEVICE_PROP_DESC, &prop_desc(0xD090, 0x0002, 1)), resp(RESP_OK)], "Z 9");
        assert!(matches!(s.set_string_property(0xD090, "x", false), Err(PtpUsbError::PropValueBad { .. })));
        assert_eq!(t.sent().len(), 1);
        let (mut s, _t) = session(vec![data_c(OP_GET_DEVICE_PROP_DESC, &[0x90, 0xD0, 0xFF, 0xFF]), resp(RESP_OK)], "Z 9");
        assert!(matches!(s.set_string_property(0xD090, "x", false), Err(PtpUsbError::PropValueBad { .. })));
    }

    #[test]
    fn only_the_three_string_props_are_written_and_nothing_is_sent_for_others() {
        for code in [0xD053u16, 0xD091, 0xD0B4, 0x0000, 0xD071, 0xD074, 0xD08F, 0xD091] {
            let (mut s, t) = session(vec![], "Z 9");
            let err = s.set_string_property(code, "x", false).unwrap_err();
            assert!(matches!(err, PtpUsbError::PropWriteUnsupported { propcode, .. } if propcode == code), "{code:#x}: {err:?}");
            assert!(t.sent().is_empty(), "{code:#x}: nothing may be sent");
        }
        // Each allowed code is accepted, and the write command carries exactly that code.
        for (code, _) in STRING_PROPS {
            let (mut s, t) = session(set_script_for(code, 1, "", RESP_OK, "x"), "Z 9");
            let force = !STRING_PROP_WRITE_CONFIRMED_PROPS.contains(&code);
            assert!(s.set_string_property(code, "x", force).unwrap().verified, "{code:#x}");
            assert_eq!(sent_ops(&t)[2], (CONTAINER_COMMAND, OP_SET_DEVICE_PROP_VALUE, le(u32::from(code))), "{code:#x}");
        }
    }

    #[test]
    fn a_model_outside_the_confirmed_list_is_refused_unless_forced() {
        // The Z 8 is assumed to match the Z 9 but has never been tried: not in the list.
        let (mut s, t) = session(set_script(1, "a", RESP_OK, "b"), "Z 8");
        assert!(matches!(s.set_string_property(0xD090, "b", false), Err(PtpUsbError::UnconfirmedModel(_))));
        assert!(t.sent().is_empty());
        let (mut s, _t) = session(set_script(1, "a", RESP_OK, "b"), "Z 8");
        assert!(s.set_string_property(0xD090, "b", true).unwrap().verified);
        for m in STRING_PROP_WRITE_MODELS {
            let (mut s, _t) = session(set_script(1, "a", RESP_OK, "b"), m);
            assert!(s.set_string_property(0xD090, "b", false).is_ok(), "{m}");
        }
    }

    #[test]
    fn a_camera_that_answers_non_ok_is_a_definite_refusal_not_indeterminate() {
        let (mut s, _t) = session(
            vec![
                data_c(OP_GET_DEVICE_PROP_DESC, &prop_desc(0xD090, 0xFFFF, 1)),
                resp(RESP_OK),
                data_c(OP_GET_DEVICE_PROP_VALUE, &prop_str("old")),
                resp(RESP_OK),
                resp(0x200F),
            ],
            "Z 9",
        );
        let err = s.set_string_property(0xD090, "new", false).unwrap_err();
        assert!(
            matches!(err, PtpUsbError::OperationFailed { op: OP_SET_DEVICE_PROP_VALUE, code: PtpResponseCode(0x200F) }),
            "{err:?}"
        );
    }

    #[test]
    fn a_failure_after_the_value_went_out_is_indeterminate() {
        let (mut s, t) = session(
            vec![
                data_c(OP_GET_DEVICE_PROP_DESC, &prop_desc(0xD090, 0xFFFF, 1)),
                resp(RESP_OK),
                data_c(OP_GET_DEVICE_PROP_VALUE, &prop_str("old")),
                resp(RESP_OK),
                Step::Err(rusb::Error::Timeout),
            ],
            "Z 9",
        );
        let err = s.set_string_property(0xD090, "new", false).unwrap_err();
        assert!(matches!(err, PtpUsbError::PropWriteIndeterminate { propcode: 0xD090, .. }), "{err:?}");
        assert!(t.sent().iter().any(|(c, op, _, _)| *c == CONTAINER_DATA && *op == OP_SET_DEVICE_PROP_VALUE), "the value did go out");
        assert!(err.to_string().contains("may or may not have taken effect"));
    }

    #[test]
    fn a_camera_that_stores_something_else_is_reported_unverified_with_what_it_stored() {
        let (mut s, _t) = session(set_script(1, "old", RESP_OK, "ne"), "Z 9");
        let r = s.set_string_property(0xD090, "new", false).unwrap();
        assert!(!r.verified);
        assert_eq!(r.read_back, Ok("ne".to_string()));
        assert_eq!(r.previous, "old");
    }

    #[test]
    fn a_failed_read_back_is_unverified_with_a_warning_not_an_error() {
        let (mut s, _t) = session(
            vec![
                data_c(OP_GET_DEVICE_PROP_DESC, &prop_desc(0xD090, 0xFFFF, 1)),
                resp(RESP_OK),
                data_c(OP_GET_DEVICE_PROP_VALUE, &prop_str("old")),
                resp(RESP_OK),
                resp(RESP_OK),
                resp(0x2002),
            ],
            "Z 9",
        );
        let r = s.set_string_property(0xD090, "new", false).unwrap();
        assert!(!r.verified);
        assert!(r.read_back.is_err());
        assert!(r.warnings.iter().any(|w| w.contains("could not be confirmed")), "{:?}", r.warnings);
    }

    #[test]
    fn text_outside_latin1_is_refused_before_anything_is_sent_and_the_boundary_is_u00ff() {
        for bad in ["\u{100}", "ü\u{65E5}", "\u{1F4F7}", "a\u{FFFF}"] {
            let (mut s, t) = session(vec![], "Z 9");
            let err = s.set_string_property(0xD090, bad, false).unwrap_err();
            assert!(matches!(err, PtpUsbError::PropValueBad { .. }), "{bad:?}: {err:?}");
            assert!(t.sent().is_empty(), "{bad:?}: nothing may be sent");
        }
        // U+00FF is the last character the camera keeps; U+00A9 (c) and U+00E9 (e acute) are in range.
        for ok in ["\u{FF}", "\u{A9} caf\u{E9}", ""] {
            let (mut s, _t) = session(set_script(1, "", RESP_OK, ok), "Z 9");
            assert!(s.set_string_property(0xD090, ok, false).unwrap().verified, "{ok:?}");
        }
    }

    #[test]
    fn encode_ptp_string_value_boundaries() {
        assert_eq!(encode_ptp_string_value("").unwrap(), vec![1, 0, 0]);
        assert_eq!(encode_ptp_string_value("a").unwrap(), vec![2, b'a', 0, 0, 0]);
        // 254 characters + NUL = 255 units: the most the count byte holds. 255 is refused.
        assert_eq!(encode_ptp_string_value(&"a".repeat(254)).unwrap()[0], 255);
        assert!(encode_ptp_string_value(&"a".repeat(255)).is_err());
        // A non-BMP character is two units: 127 of them = 254 + NUL fits, 128 does not.
        assert_eq!(encode_ptp_string_value(&"\u{1F4F7}".repeat(127)).unwrap()[0], 255);
        assert!(encode_ptp_string_value(&"\u{1F4F7}".repeat(128)).is_err());
        for bad in ["a\0b", "a\nb", "tab\there"] {
            assert!(encode_ptp_string_value(bad).is_err(), "{bad:?}");
        }
        // Round trip, including a space-padded value and a non-ASCII one.
        for t in ["", "Brian L Wong", "(c)Brian L Wong 2026", "trail ", "caf\u{e9}", "\u{1F4F7}"] {
            assert_eq!(decode_ptp_string_value(&encode_ptp_string_value(t).unwrap()).unwrap(), t);
        }
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
    fn query_vendor_op_keeps_the_response_parameters_the_deviation_detail_and_skipped_containers() {
        // For 0x944C the reply is the finding: nothing of it may be dropped.
        let (mut s, _t) = session(vec![Step::Stamped(vec![(CONTAINER_RESPONSE, 0x2005, [le(5), le(6)].concat())])], "Z 9");
        let q = s.query_vendor_op(0x90ED).unwrap();
        assert_eq!(q.response_params, vec![5, 6]);
        assert!(q.deviations.is_empty() && q.reply_matched_command && q.unrecognized_containers == 0);
        let (mut s, _t) = session(vec![Step::Bytes(container(CONTAINER_RESPONSE, RESP_OK, 777, &[]))], "Z 9");
        let q = s.query_vendor_op(0x90ED).unwrap();
        assert_eq!(q.deviations, vec![ReplyDeviation::TransactionId { got: 777 }]);
        assert!(!q.reply_matched_command);
        // A container of a kind this module does not know is skipped and counted.
        let (mut s, _t) = session(vec![Step::Stamped(vec![(0x0009, 0x1234, vec![]), (CONTAINER_RESPONSE, RESP_OK, vec![])])], "Z 9");
        assert_eq!(s.query_vendor_op(0x90ED).unwrap().unrecognized_containers, 1);
    }

    #[test]
    fn a_property_read_keeps_the_response_parameters() {
        let (mut s, _t) = session(vec![data_c(OP_GET_DEVICE_PROP_VALUE, &[1]), Step::Stamped(vec![(CONTAINER_RESPONSE, RESP_OK, le(3))])], "Z 9");
        assert_eq!(s.get_device_prop_value(0xD053).unwrap().response_params, vec![3]);
        let (mut s, _t) = session(vec![data_c(OP_VENDOR_PROP_READ, &[1]), Step::Stamped(vec![(CONTAINER_RESPONSE, RESP_OK, le(4))])], "Z 9");
        assert_eq!(s.read_vendor_property_report(0xD053).unwrap().response_params, vec![4]);
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
        assert!(d.ipv6_fields.iter().all(|f| f == ftp_layout::FTP_IPV6_PLACEHOLDER));
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
    fn decode_accepts_the_00_lead_byte_of_camera_made_profiles_but_will_not_re_encode_it() {
        let mut blob = hex::decode_hex(reference_ftp_blob_hex());
        assert_eq!(blob[99], 0x03, "fixture offset of the 2.4 GHz tag's first byte");
        let usual = decode_ftp_profile(&blob).expect("usual blob decodes");
        assert_eq!(usual.ssid24_lead, 0x03);
        blob[99] = 0x00;
        let camera_made = decode_ftp_profile(&blob).expect("00 lead byte decodes");
        assert_eq!(camera_made.ssid24_lead, 0x00);
        // Nothing else about the profile changed.
        assert_eq!(camera_made.profile_name, usual.profile_name);
        assert_eq!(camera_made.ssid_24ghz, usual.ssid_24ghz);
        assert_eq!(camera_made.host, usual.host);
        // The encoder writes 03, so it cannot reproduce the 00 form.
        assert!(usual.as_encodable().is_some(), "the reference blob is re-encodable");
        assert!(camera_made.as_encodable().is_none());
        // Any other lead byte is still rejected, with the offset.
        blob[99] = 0x02;
        assert!(matches!(
            decode_ftp_profile(&blob),
            Err(PtpUsbError::FtpBlobMalformed { what: "2.4 GHz SSID tag", offset: 99, .. })
        ));
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
        // Two stored profiles, so slot 1 exists: the count is read first, in the same session.
        let (mut s, t) = session(blob_write_script(2), "Z 9");
        s.write_ftp_profile_blob(1, 0, &blob, false).unwrap();
        assert_eq!(
            t.sent(),
            vec![
                (CONTAINER_COMMAND, OP_FTP_SETUP, 0, vec![]),
                (CONTAINER_DATA, OP_FTP_SETUP, 0, FTP_SETUP_DATA.to_vec()),
                (CONTAINER_COMMAND, OP_FTP_PROFILE_INFO, 1, vec![]),
                (CONTAINER_COMMAND, OP_FTP_SETUP, 2, vec![]),
                (CONTAINER_DATA, OP_FTP_SETUP, 2, FTP_SETUP_DATA.to_vec()),
                (CONTAINER_COMMAND, OP_FTP_PROFILE_WRITE, 3, [le(1), le(0)].concat()),
                (CONTAINER_DATA, OP_FTP_PROFILE_WRITE, 3, blob),
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

    /// Replies for one `ftp_list_state` read of a one-profile list: setup, info, one profile.
    fn list_state_replies(blob: &[u8], selected: u32) -> Vec<Step> {
        vec![
            resp(RESP_OK),
            data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(1, selected, "Z_9_0000000")),
            resp(RESP_OK),
            data_c(OP_FTP_PROFILE_READ, blob),
            resp(RESP_OK),
        ]
    }

    /// Replies for one `ftp_list_state` read of a list holding `count` copies of `blob`.
    fn list_state_replies_of(count: u32, blob: &[u8], selected: u32, name: &str) -> Vec<Step> {
        let mut v = vec![resp(RESP_OK), data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(count, selected, name)), resp(RESP_OK)];
        for _ in 0..count.min(FTP_LIST_MAX_READ) {
            v.push(data_c(OP_FTP_PROFILE_READ, blob));
            v.push(resp(RESP_OK));
        }
        v
    }

    fn sent_op_codes(t: &ScriptedTransport) -> Vec<u16> {
        t.sent().iter().map(|(_, op, _, _)| *op).collect()
    }

    /// No consent beyond the op being on the list.
    const NO_CONSENT: FtpProbeConsent = FtpProbeConsent { force_unconfirmed_model: false, destructive_op: None };

    fn names(op: u16) -> FtpProbeConsent {
        FtpProbeConsent { force_unconfirmed_model: false, destructive_op: Some(op) }
    }

    fn replied(p: &FtpOpProbe) -> &FtpProbeReply {
        match &p.outcome {
            FtpProbeOutcome::Replied(r) => r,
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    // ── the probe's allow-list and its destructive ops (b7d3#07, #77) ───────

    #[test]
    fn the_probe_allow_list_and_its_risks_are_pinned_literally() {
        // The list is what the camera may be sent from a command line: spelled out, so replacing one entry
        // (0x90EA with 0x1016 SetDevicePropValue, say) fails here instead of passing a refused-ops test.
        assert_eq!(FTP_PROBE_OPS, [0x90E9, 0x90EA, 0x90EC, 0x90EF, 0x9FFF]);
        assert_eq!(
            FTP_PROBE_OP_TABLE,
            [
                (0x90E9, FtpProbeRisk::Unexplained),
                (0x90EA, FtpProbeRisk::DeleteAll),
                (0x90EC, FtpProbeRisk::Unexplained),
                (0x90EF, FtpProbeRisk::DeleteOne),
                (0x9FFF, FtpProbeRisk::Control),
            ]
        );
        assert_eq!((FTP_PROBE_MAX_PARAMS, FTP_PROBE_MAX_DATA, FTP_PROBE_REFUSED_PARAM), (2, 32, 0xFFFF_FFFF));
    }

    #[test]
    fn exactly_the_listed_ops_pass_the_request_check_and_every_other_u16_is_refused() {
        let listed = [0x90E9u16, 0x90EA, 0x90EC, 0x90EF, 0x9FFF];
        for op in 0..=u16::MAX {
            // Name the op as destructive too, so only the list itself decides.
            let r = check_ftp_probe_request(op, &[], None, names(op));
            assert_eq!(r.is_ok(), listed.contains(&op), "{op:#06x}: {r:?}");
            if !listed.contains(&op) {
                assert!(matches!(r, Err(PtpUsbError::FtpProbeOpNotAllowed { op: o }) if o == op), "{op:#06x}");
            }
        }
        // The neighbours that a one-digit slip would reach: the profile read/write/info ops and SetDevicePropValue.
        for op in [0x90E8u16, 0x90EB, 0x90ED, 0x90EE, 0x90F0, 0x1016, 0x943B, 0x944C] {
            assert!(matches!(check_ftp_probe_request(op, &[], None, names(op)), Err(PtpUsbError::FtpProbeOpNotAllowed { .. })), "{op:#06x}");
        }
    }

    #[test]
    fn only_the_two_delete_ops_are_destructive_and_each_has_to_be_named_for_itself() {
        assert_eq!(ftp_probe_risk(0x90EA), Some(FtpProbeRisk::DeleteAll));
        assert_eq!(ftp_probe_risk(0x90EF), Some(FtpProbeRisk::DeleteOne));
        assert_eq!(ftp_probe_risk(0x1016), None);
        let destructive: Vec<u16> = FTP_PROBE_OPS.iter().copied().filter(|o| ftp_probe_risk(*o).unwrap().is_destructive()).collect();
        assert_eq!(destructive, [0x90EA, 0x90EF]);
        for (op, other) in [(0x90EAu16, 0x90EFu16), (0x90EF, 0x90EA)] {
            // Not named, naming nothing, naming the other delete op, naming an op that is not a delete, and
            // the model flag alone: every one of them is refused with the op that was asked for.
            for consent in [
                NO_CONSENT,
                names(other),
                names(0x9FFF),
                FtpProbeConsent { force_unconfirmed_model: true, destructive_op: None },
            ] {
                let r = check_ftp_probe_request(op, &[], None, consent);
                assert!(
                    matches!(r, Err(PtpUsbError::FtpProbeDestructiveNotConfirmed { op: o, .. }) if o == op),
                    "{op:#06x} with {consent:?}: {r:?}"
                );
            }
            assert!(check_ftp_probe_request(op, &[], None, names(op)).unwrap().is_destructive());
        }
        // The ops that are not deletes need no naming; naming another op does not matter to them.
        for op in [0x90E9u16, 0x90EC, 0x9FFF] {
            assert!(!check_ftp_probe_request(op, &[], None, NO_CONSENT).unwrap().is_destructive(), "{op:#06x}");
            assert!(check_ftp_probe_request(op, &[], None, names(0x90EA)).is_ok(), "{op:#06x}");
        }
        // The refusal tells the person what to pass.
        let text = check_ftp_probe_request(0x90EA, &[], None, NO_CONSENT).unwrap_err().to_string();
        assert!(text.contains("deletes ALL stored FTP profiles") && text.contains("--allow-destructive-op 0x90ea"), "{text}");
    }

    #[test]
    fn a_destructive_probe_that_is_not_named_sends_nothing_not_even_the_list_read() {
        for (op, consent) in [(0x90EAu16, NO_CONSENT), (0x90EF, names(0x90EA)), (0x90EA, names(0x90EF))] {
            let (mut s, t) = session(vec![], "Z 9");
            let err = s.probe_ftp_op(op, false, &[1], None, consent).unwrap_err();
            assert!(matches!(err, PtpUsbError::FtpProbeDestructiveNotConfirmed { .. }), "{err:?}");
            assert!(t.sent().is_empty(), "the camera was written to: {:?}", t.sent());
        }
    }

    #[test]
    fn a_named_destructive_probe_is_sent_once_and_the_deletion_shows_in_the_change_list() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let mut replies = list_state_replies(&blob, 0);
        replies.push(resp(RESP_OK)); // 0x90EA answered OK
        replies.extend(list_state_replies_of(0, &blob, 0xff, "Z_9_0000000"));
        let (mut s, t) = session(replies, "Z 9");
        let p = s.probe_ftp_op(0x90EA, false, &[], None, names(0x90EA)).unwrap();
        assert_eq!(p.risk, FtpProbeRisk::DeleteAll);
        assert_eq!(replied(&p).response_code, RESP_OK);
        assert_eq!(sent_op_codes(&t).iter().filter(|o| **o == 0x90EA).count(), 1);
        let changes = p.before.changes_to(p.after.as_ref().unwrap());
        assert!(changes.contains(&FtpListChange::Info(FtpInfoField::Count)), "{changes:?}");
        assert!(changes.contains(&FtpListChange::Info(FtpInfoField::Selected)), "{changes:?}");
        assert!(changes.contains(&FtpListChange::ProfileRemoved { index: 0 }), "{changes:?}");
    }

    // ── the model gate on the probe ───────────────────────────────────────

    #[test]
    fn every_probe_op_but_the_control_needs_a_write_confirmed_model_or_the_force_flag() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        // "Z 5" is surveyed (reads work) but no FTP write was ever captured on it: not write-confirmed.
        assert!(!ftp_write_confirmed("Z 5"));
        for op in [0x90E9u16, 0x90EC] {
            let (mut s, t) = session(vec![], "Z 5");
            let err = s.probe_ftp_op(op, false, &[], Some(&[1]), NO_CONSENT).unwrap_err();
            assert!(matches!(err, PtpUsbError::UnconfirmedModel(ref m) if m == "Z 5"), "{err:?}");
            assert!(t.sent().is_empty(), "{op:#06x} reached the camera");
        }
        // A destructive op needs both: naming it does not stand in for the model gate.
        let (mut s, t) = session(vec![], "Z 5");
        assert!(matches!(s.probe_ftp_op(0x90EA, false, &[], None, names(0x90EA)).unwrap_err(), PtpUsbError::UnconfirmedModel(_)));
        assert!(t.sent().is_empty());
        // The force flag alone does not unlock a delete either.
        let (mut s, t) = session(vec![], "Z 5");
        let force = FtpProbeConsent { force_unconfirmed_model: true, destructive_op: None };
        assert!(matches!(s.probe_ftp_op(0x90EA, false, &[], None, force).unwrap_err(), PtpUsbError::FtpProbeDestructiveNotConfirmed { .. }));
        assert!(t.sent().is_empty());
        // Both together: sent.
        let mut replies = list_state_replies(&blob, 0);
        replies.push(resp(RESP_OK));
        replies.extend(list_state_replies(&blob, 0));
        let (mut s, t) = session(replies, "Z 5");
        let both = FtpProbeConsent { force_unconfirmed_model: true, destructive_op: Some(0x90EA) };
        s.probe_ftp_op(0x90EA, false, &[], None, both).unwrap();
        assert!(sent_op_codes(&t).contains(&0x90EA));
        // Both confirmed models pass the gate without the flag.
        for model in FTP_WRITE_CONFIRMED_MODELS {
            let mut replies = list_state_replies(&blob, 0);
            replies.push(resp(0x2005));
            replies.extend(list_state_replies(&blob, 0));
            let (mut s, t) = session(replies, model);
            s.probe_ftp_op(0x90EC, false, &[], None, NO_CONSENT).unwrap();
            assert!(sent_op_codes(&t).contains(&0x90EC), "{model}");
        }
    }

    #[test]
    fn the_control_op_is_the_read_only_probe_and_works_on_any_model_without_flags() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        for model in ["Z 5", "Z 9", "Z6_3", "Z 99"] {
            let mut replies = list_state_replies(&blob, 0);
            replies.push(resp(0x2005));
            replies.extend(list_state_replies(&blob, 0));
            let (mut s, _t) = session(replies, model);
            let p = s.probe_ftp_op(0x9FFF, false, &[], None, NO_CONSENT).unwrap();
            assert_eq!(replied(&p).response_code, 0x2005, "{model}");
            assert!(p.before.changes_to(p.after.as_ref().unwrap()).is_empty(), "{model}");
            // The warning is the registry's, not "this was only captured on a Z 9".
            assert_eq!(p.warnings.len(), usize::from(model == "Z 99"), "{model}: {:?}", p.warnings);
        }
    }

    // ── limits, at / below / above (b7d3#81) ───────────────────────────────

    #[test]
    fn probe_parameter_count_is_limited_to_two() {
        let ok = |params: &[u32]| check_ftp_probe_request(0x90EF, params, None, names(0x90EF)).is_ok();
        assert!(ok(&[]));
        assert!(ok(&[1]));
        assert!(ok(&[1, 2]), "two is the limit and is allowed");
        assert!(!ok(&[1, 2, 3]));
        assert!(!ok(&[0; 4]));
        assert!(matches!(
            check_ftp_probe_request(0x90EF, &[1, 2, 3], None, names(0x90EF)),
            Err(PtpUsbError::FtpProbeBadParams { params }) if params == [1, 2, 3]
        ));
    }

    #[test]
    fn the_hang_value_is_refused_in_any_position_and_the_values_beside_it_are_not() {
        let ok = |params: &[u32]| check_ftp_probe_request(0x90EF, params, None, names(0x90EF)).is_ok();
        assert!(!ok(&[0xFFFF_FFFF]));
        assert!(!ok(&[0, 0xFFFF_FFFF]));
        assert!(!ok(&[0xFFFF_FFFF, 0]));
        assert!(ok(&[0xFFFF_FFFE]), "one below the hang value");
        assert!(ok(&[0, 0xFFFF_FFFE]));
        assert!(ok(&[0x8000_0000]));
    }

    #[test]
    fn probe_data_out_is_limited_to_32_bytes() {
        let ok = |n: usize| check_ftp_probe_request(0x90EC, &[], Some(&vec![0xAA; n]), NO_CONSENT).is_ok();
        assert!(ok(0), "an empty data-out is a data phase of zero bytes, allowed as before");
        assert!(ok(1));
        assert!(ok(31));
        assert!(ok(32), "32 is the limit and is allowed");
        assert!(!ok(33));
        assert!(!ok(320));
        assert!(matches!(
            check_ftp_probe_request(0x90EC, &[], Some(&[0; 33]), NO_CONSENT),
            Err(PtpUsbError::FtpProbeDataTooLong { len: 33 })
        ));
        // No data at all is not the same as an empty one, and neither is over the limit.
        assert!(check_ftp_probe_request(0x90EC, &[], None, NO_CONSENT).is_ok());
    }

    #[test]
    fn request_refusals_are_reported_in_a_fixed_order() {
        // Off the list wins over everything; then parameters; then data; then the delete naming.
        assert!(matches!(
            check_ftp_probe_request(0x1016, &[1, 2, 3], Some(&[0; 40]), NO_CONSENT),
            Err(PtpUsbError::FtpProbeOpNotAllowed { .. })
        ));
        assert!(matches!(
            check_ftp_probe_request(0x90EA, &[1, 2, 3], Some(&[0; 40]), NO_CONSENT),
            Err(PtpUsbError::FtpProbeBadParams { .. })
        ));
        assert!(matches!(
            check_ftp_probe_request(0x90EA, &[1], Some(&[0; 40]), NO_CONSENT),
            Err(PtpUsbError::FtpProbeDataTooLong { .. })
        ));
        assert!(matches!(
            check_ftp_probe_request(0x90EA, &[1], Some(&[0; 4]), NO_CONSENT),
            Err(PtpUsbError::FtpProbeDestructiveNotConfirmed { .. })
        ));
    }

    #[test]
    fn the_refusal_texts_are_built_from_the_one_table_and_the_limits() {
        let text = PtpUsbError::FtpProbeOpNotAllowed { op: 0x1234 }.to_string();
        for op in FTP_PROBE_OPS {
            assert!(text.contains(&format!("0x{op:04x}")), "{op:#06x} missing from: {text}");
        }
        assert!(text.contains("0x9fff as a control"), "{text}");
        assert!(PtpUsbError::FtpProbeDataTooLong { len: 40 }.to_string().contains("32 bytes"));
        let bad = PtpUsbError::FtpProbeBadParams { params: vec![] }.to_string();
        assert!(bad.contains("at most 2 parameters") && bad.contains("0xFFFFFFFF"), "{bad}");
    }

    #[test]
    fn probe_ftp_op_locked_refuses_before_a_session_is_opened() {
        let opened = std::cell::Cell::new(0u32);
        let open = || {
            opened.set(opened.get() + 1);
            Ok(session(vec![], "Z 9").0)
        };
        let blob = vec![0xAA; 33];
        let cases: Vec<(&str, u16, Vec<u32>, Option<&[u8]>, FtpProbeConsent)> = vec![
            ("off the list", 0x1016, vec![], None, names(0x1016)),
            ("too many parameters", 0x90EF, vec![1, 2, 3], None, names(0x90EF)),
            ("the hang value", 0x90EF, vec![u32::MAX], None, names(0x90EF)),
            ("data over the limit", 0x90EC, vec![], Some(&blob[..]), NO_CONSENT),
            ("a delete nobody named", 0x90EA, vec![], None, NO_CONSENT),
        ];
        for (what, op, params, data, consent) in cases {
            let err = probe_ftp_op_opening(open, op, false, &params, data, consent).unwrap_err();
            assert!(
                matches!(
                    err,
                    PtpUsbError::FtpProbeOpNotAllowed { .. }
                        | PtpUsbError::FtpProbeBadParams { .. }
                        | PtpUsbError::FtpProbeDataTooLong { .. }
                        | PtpUsbError::FtpProbeDestructiveNotConfirmed { .. }
                ),
                "{what}: {err:?}"
            );
            assert_eq!(opened.get(), 0, "{what}: a session was opened for a request that is refused");
        }
        // A request that passes goes on to open: the session's own error would come back from `open`.
        let r = probe_ftp_op_opening(|| Err::<PtpUsbSession<ScriptedTransport>, _>(PtpUsbError::NoDevice), 0x9FFF, false, &[], None, NO_CONSENT);
        assert!(matches!(r, Err(PtpUsbError::NoDevice)));
    }

    // ── the one unsurveyed-model warning (b7d3#33) ─────────────────────────

    #[test]
    fn the_unsurveyed_model_warning_is_one_helper_with_no_stale_z9_claim() {
        for m in KNOWN_MODELS {
            assert_eq!(unsurveyed_model_warning(m, "x"), None, "{m}");
        }
        for m in ["", "Z 50", "z 9", "Z6_3 "] {
            let w = unsurveyed_model_warning(m, "the thing").unwrap_or_else(|| panic!("{m:?} is not surveyed"));
            assert!(w.contains(&format!("{m:?}")) && w.contains("the thing") && w.contains("Z6_3"), "{w}");
            assert!(!w.contains("only captured on a Z 9"), "the condition is registry membership, not 'is a Z 9': {w}");
        }
    }

    #[test]
    fn every_operation_that_warns_on_an_unsurveyed_model_gives_that_same_warning() {
        let model = "Z 99";
        let expect = |what: &str| unsurveyed_model_warning(model, what).unwrap();
        let blob = hex::decode_hex(reference_ftp_blob_hex());

        let (mut s, _t) = session(vec![data_c(OP_VENDOR_PROP_READ, &[1]), resp(RESP_OK)], model);
        assert_eq!(s.read_vendor_property_report(0xD053).unwrap().warnings, [expect("vendor-property reads")]);

        let (mut s, _t) = session(vec![data_c(OP_GET_DEVICE_PROP_VALUE, &[1]), resp(RESP_OK)], model);
        assert_eq!(s.get_device_prop_value(0xD053).unwrap().warnings, [expect("standard property reads")]);

        let (mut s, _t) = session(vec![resp(RESP_OK)], model);
        assert_eq!(s.query_vendor_op(0x90ED).unwrap().warnings, [expect("0x944C")]);

        let info = ftp_info_bytes(1, 0, "Z_99_0000000");
        let (mut s, _t) = session(
            vec![resp(RESP_OK), data_c(OP_FTP_PROFILE_INFO, &info), resp(RESP_OK), data_c(OP_FTP_PROFILE_READ, &blob), resp(RESP_OK)],
            model,
        );
        assert_eq!(s.read_ftp_profile_slot(Some(0)).unwrap().warnings, [expect("the FTP-profile read")]);

        let mut replies = list_state_replies(&blob, 0);
        replies.push(resp(0x2005));
        replies.extend(list_state_replies(&blob, 0));
        let (mut s, _t) = session(replies, model);
        assert_eq!(s.probe_ftp_op(0x9FFF, false, &[], None, NO_CONSENT).unwrap().warnings, [expect("the FTP-profile ops")]);
    }

    // ── typed diagnostics for a failed transaction (b7d3#31/#48) ───────────

    fn every_ptp_usb_error() -> Vec<PtpUsbError> {
        let path = std::path::PathBuf::from("/x");
        vec![
            PtpUsbError::NoDevice,
            PtpUsbError::AmbiguousDevice(2),
            PtpUsbError::SerialNotFound { requested: "S".into(), unreadable: vec![] },
            PtpUsbError::DuplicateSerial { requested: "S".into(), count: 2 },
            PtpUsbError::BodiesSkipped { skipped: vec!["a".into()] },
            PtpUsbError::Usb(rusb::Error::Pipe),
            PtpUsbError::CameraLock(LockError::Busy { path }),
            PtpUsbError::NoPtpInterface,
            PtpUsbError::ClaimFailed { attempts: 3, last_error: "secret-ish text".into() },
            PtpUsbError::ShortContainer(3),
            PtpUsbError::TruncatedContainer { declared: 9, got: 4 },
            PtpUsbError::ShortWrite { written: 1, expected: 2 },
            PtpUsbError::ContainerTooLarge { payload_len: 7 },
            PtpUsbError::TooManyCommandParams { given: 6, max: 5 },
            PtpUsbError::MisalignedResponseParams(5),
            PtpUsbError::TransactionTimedOut(Duration::from_millis(1500)),
            PtpUsbError::TransactionDataTooLarge { got: 10, limit: 5 },
            PtpUsbError::SessionPoisoned { cause: "an earlier error's text".into() },
            PtpUsbError::OpenSessionFailed(PtpResponseCode(0x201E)),
            PtpUsbError::OperationFailed { op: 0x90EC, code: PtpResponseCode(0x2002) },
            PtpUsbError::TransactionIdMismatch { op: 0x1001, expected: 3, got: 9 },
            PtpUsbError::UnexpectedDataOp { op: 0x1001, got: 0x90EC },
            PtpUsbError::NoDataPhase { op: 0x1001 },
            PtpUsbError::DeviceInfoMalformed { field: "model", offset: 4, fault: DeviceInfoFault::Truncated { needed: 2, available: 1 } },
            PtpUsbError::FtpBlobMalformed { what: "host", offset: 3, detail: "a camera-supplied detail".into() },
            PtpUsbError::FtpInfoMalformed {
                raw: vec![0xc8, 0x00, 0x01],
                cause: Box::new(PtpUsbError::FtpBlobMalformed { what: "info count", offset: 2, detail: "needs 4 byte(s) but only 1 remain".into() }),
            },
            PtpUsbError::FtpSlotOutOfRange { slot: 3, count: 3 },
            PtpUsbError::FtpProbeOpNotAllowed { op: 1 },
            PtpUsbError::FtpProbeDataTooLong { len: 40 },
            PtpUsbError::FtpProbeBadParams { params: vec![1, 2, 3] },
            PtpUsbError::FtpProbeDestructiveNotConfirmed { op: 0x90EA, risk: FtpProbeRisk::DeleteAll },
            PtpUsbError::FtpFieldTooLong { field: "host", len: 99, max: 50, unit: "bytes" },
            PtpUsbError::FtpFieldNotAscii { field: "host" },
            PtpUsbError::FtpFieldControlChar { field: "host" },
            PtpUsbError::FtpWriteIndeterminate { cause: Box::new(PtpUsbError::Usb(rusb::Error::Timeout)) },
            PtpUsbError::FtpWriteRefused { slot: 1, param2: 9, why: FtpWriteRefusal::SlotHangsCamera },
            PtpUsbError::UnconfirmedModel("Z 5".into()),
            PtpUsbError::PropWriteUnsupported { propcode: 1, allowed: "x".into() },
            PtpUsbError::PropWriteUnconfirmed { propcode: 0xD072, model: "Z 9".into() },
            PtpUsbError::PropNotWritable { propcode: 0xD090, get_set: 0 },
            PtpUsbError::PropValueBad { propcode: 0xD090, why: "text from the caller".into() },
            PtpUsbError::PropWriteIndeterminate { propcode: 0xD090, cause: Box::new(PtpUsbError::Usb(rusb::Error::Pipe)) },
        ]
    }

    #[test]
    fn every_ptp_usb_error_has_its_own_closed_tag_recorded_as_error_kind() {
        let errs = every_ptp_usb_error();
        let tags: std::collections::BTreeSet<_> = errs.iter().map(PtpUsbError::tag).collect();
        assert_eq!(tags.len(), errs.len(), "tags are unique");
        assert_eq!(errs.len(), 42, "a new PtpUsbError variant needs an entry here (tag() is exhaustive, this list is not)");
        for e in &errs {
            assert!(e.tag().chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{}", e.tag());
            assert_eq!(e.diag_fields()[0], ("error_kind", crate::diag::Val::text(e.tag())), "{e}");
        }
    }

    #[test]
    fn a_transaction_error_is_recorded_by_typed_fields_never_by_its_display_text() {
        use crate::diag::Val;
        for e in every_ptp_usb_error() {
            for (k, v) in e.diag_fields() {
                if let Some(t) = v.as_text() {
                    assert!(["error_kind", "usb_error", "cause_kind"].contains(&k), "{k} is text for {e}");
                    assert_ne!(t, e.to_string(), "{k} must not be the Display text of {e}");
                }
            }
        }
        let get = |e: &PtpUsbError, k: &str| e.diag_fields().into_iter().find(|(n, _)| *n == k).map(|(_, v)| v);
        let refused = PtpUsbError::OperationFailed { op: 0x90EC, code: PtpResponseCode(0x2002) };
        assert_eq!((get(&refused, "op"), get(&refused, "code")), (Some(Val::Hex(0x90EC)), Some(Val::Hex(0x2002))));
        let t = PtpUsbError::TransactionTimedOut(Duration::from_millis(1500));
        assert_eq!(get(&t, "timeout_ms"), Some(Val::U64(1500)));
        let mismatch = PtpUsbError::TransactionIdMismatch { op: 0x1001, expected: 3, got: 9 };
        assert_eq!((get(&mismatch, "expected"), get(&mismatch, "got")), (Some(Val::U64(3)), Some(Val::U64(9))));
        let wrapped = PtpUsbError::FtpWriteIndeterminate { cause: Box::new(PtpUsbError::Usb(rusb::Error::Timeout)) };
        assert_eq!(get(&wrapped, "cause_kind"), Some(Val::text("usb")));
        // A payload carried by the error never reaches the record.
        let claim = PtpUsbError::ClaimFailed { attempts: 3, last_error: "secret-ish text".into() };
        assert_eq!(claim.diag_fields().len(), 1);
        let bad = PtpUsbError::PropValueBad { propcode: 0xD090, why: "text from the caller".into() };
        assert_eq!(bad.diag_fields().len(), 1);
    }

    // ── what the probe reports ─────────────────────────────────────────────

    #[test]
    fn probe_ftp_op_refuses_an_op_off_the_list_before_any_io() {
        let (mut s, t) = session(vec![], "Z 9");
        assert!(matches!(
            s.probe_ftp_op(0x90EE, false, &[], None, NO_CONSENT).unwrap_err(),
            PtpUsbError::FtpProbeOpNotAllowed { op: 0x90EE }
        ));
        assert!(matches!(
            s.probe_ftp_op(0x90ED, true, &[], None, NO_CONSENT).unwrap_err(),
            PtpUsbError::FtpProbeOpNotAllowed { op: 0x90ED }
        ));
        assert!(t.sent().is_empty());
    }

    #[test]
    fn probe_ftp_op_reads_the_list_before_and_after_and_reports_the_reply_verbatim() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let mut replies = list_state_replies(&blob, 0);
        replies.push(resp(0x2005)); // OperationNotSupported
        replies.extend(list_state_replies(&blob, 0));
        let (mut s, t) = session(replies, "Z 9");
        let p = s.probe_ftp_op(0x9FFF, false, &[], None, NO_CONSENT).unwrap();
        assert_eq!(replied(&p).response_code, 0x2005);
        assert_eq!(&p.before, p.after.as_ref().unwrap());
        assert!(p.before.changes_to(p.after.as_ref().unwrap()).is_empty());
        assert_eq!(p.before.info.count, 1);
        assert_eq!(p.before.info.slot, 0);
        assert_eq!(p.before.entries.len(), 1);
        assert!(!p.before.truncated && p.before.undecodable_count() == 0);
        // Bare means nothing between the "before" read and the op: the op follows the last 0x90ED.
        let ops = sent_op_codes(&t);
        assert_eq!(ops.iter().filter(|o| **o == 0x9FFF).count(), 1);
        let at = ops.iter().position(|o| *o == 0x9FFF).unwrap();
        assert_eq!(ops[at - 1], OP_FTP_PROFILE_READ, "no 0x90E8 setup between the read and the bare op");
    }

    #[test]
    fn probe_ftp_op_with_setup_sends_the_setup_right_before_the_op() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let mut replies = list_state_replies(&blob, 0);
        replies.push(resp(RESP_OK)); // the setup
        replies.push(resp(0x2005)); // the op
        replies.extend(list_state_replies(&blob, 0));
        let (mut s, t) = session(replies, "Z 9");
        let p = s.probe_ftp_op(0x90E9, true, &[], None, NO_CONSENT).unwrap();
        assert!(p.setup_first);
        assert_eq!(replied(&p).response_code, 0x2005);
        let ops = sent_op_codes(&t);
        let at = ops.iter().position(|o| *o == 0x90E9).expect("op sent");
        assert_eq!(ops[at - 1], OP_FTP_SETUP, "the setup immediately precedes the op");
    }

    #[test]
    fn probe_ftp_op_sends_the_given_parameters() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let mut replies = list_state_replies(&blob, 0);
        replies.push(resp(RESP_OK));
        replies.extend(list_state_replies(&blob, 0));
        let (mut s, t) = session(replies, "Z 9");
        let p = s.probe_ftp_op(0x90EF, false, &[1], None, names(0x90EF)).unwrap();
        assert_eq!(p.params, vec![1]);
        let sent = t.sent();
        let (_, _, _, payload) = sent.iter().find(|(_, op, _, _)| *op == 0x90EF).expect("op sent");
        assert_eq!(payload, &1u32.to_le_bytes().to_vec(), "the parameter reached the wire");
    }

    #[test]
    fn probe_ftp_op_sends_data_out_under_the_same_transaction() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let mut replies = list_state_replies(&blob, 0);
        replies.push(resp(RESP_OK));
        replies.extend(list_state_replies(&blob, 0));
        let (mut s, t) = session(replies, "Z 9");
        let p = s.probe_ftp_op(0x90EC, false, &[1], Some(&[0xAA, 0xBB]), NO_CONSENT).unwrap();
        assert_eq!(p.data.as_deref(), Some(&[0xAA, 0xBB][..]));
        let sent = t.sent();
        let frames: Vec<_> = sent.iter().filter(|(_, op, _, _)| *op == 0x90EC).collect();
        assert_eq!(frames.len(), 2, "a command container and a data container");
        assert_eq!(frames[0].2, frames[1].2, "same transaction id");
        assert_eq!(frames[1].3, vec![0xAA, 0xBB]);
    }

    #[test]
    fn probe_ftp_op_reports_the_reply_parameters_and_how_the_reply_deviated() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let mut replies = list_state_replies(&blob, 0);
        replies.push(Step::Stamped(vec![(CONTAINER_RESPONSE, RESP_OK, [le(7), le(9)].concat())]));
        replies.extend(list_state_replies(&blob, 0));
        let (mut s, _t) = session(replies, "Z 9");
        let p = s.probe_ftp_op(0x90EC, false, &[], None, NO_CONSENT).unwrap();
        assert_eq!(replied(&p).response_params, vec![7, 9]);
        assert!(replied(&p).deviations.is_empty());
        // A reply that carries the wrong transaction id is tolerated and recorded with the id it carried.
        let mut replies = list_state_replies(&blob, 0);
        replies.push(Step::Bytes(container(CONTAINER_RESPONSE, RESP_OK, 4242, &[])));
        replies.extend(list_state_replies(&blob, 0));
        let (mut s, _t) = session(replies, "Z 9");
        let p = s.probe_ftp_op(0x90EC, false, &[], None, NO_CONSENT).unwrap();
        assert_eq!(replied(&p).deviations, vec![ReplyDeviation::TransactionId { got: 4242 }]);
    }

    // ── a lost reply is a typed indeterminate result (b7d3#16) ─────────────

    #[test]
    fn a_reply_lost_after_data_out_is_indeterminate_with_the_cause_and_the_data_flag() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        // The scripted transport runs dry after the "before" read: the op's reply is a USB timeout.
        let (mut s, t) = session(list_state_replies(&blob, 0), "Z 9");
        let p = s.probe_ftp_op(0x90EC, false, &[], Some(&[0x01]), NO_CONSENT).unwrap();
        match &p.outcome {
            FtpProbeOutcome::Indeterminate { data_sent: true, cause: PtpUsbError::Usb(rusb::Error::Timeout) } => {}
            other => panic!("{other:?}"),
        }
        assert!(p.is_indeterminate());
        assert!(sent_op_codes(&t).iter().filter(|o| **o == 0x90EC).count() == 2, "command and data container went out");
        assert!(matches!(p.after, Err(PtpUsbError::SessionPoisoned { .. })), "the poisoned session cannot re-read the list: {:?}", p.after);
        // Each cause is its own: a stalled pipe is not a timeout.
        let mut replies = list_state_replies(&blob, 0);
        replies.push(Step::Err(rusb::Error::Pipe));
        let (mut s, _t) = session(replies, "Z 9");
        let p = s.probe_ftp_op(0x90EC, false, &[], Some(&[0x01]), NO_CONSENT).unwrap();
        assert!(matches!(p.outcome, FtpProbeOutcome::Indeterminate { data_sent: true, cause: PtpUsbError::Usb(rusb::Error::Pipe) }), "{:?}", p.outcome);
    }

    #[test]
    fn a_bare_op_whose_reply_is_lost_is_indeterminate_too_without_the_data_flag() {
        // A delete with no reply may still have deleted: the command alone is enough.
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let (mut s, _t) = session(list_state_replies(&blob, 0), "Z 9");
        let p = s.probe_ftp_op(0x90EA, false, &[], None, names(0x90EA)).unwrap();
        assert!(
            matches!(p.outcome, FtpProbeOutcome::Indeterminate { data_sent: false, cause: PtpUsbError::Usb(rusb::Error::Timeout) }),
            "{:?}",
            p.outcome
        );
    }

    #[test]
    fn a_failed_setup_means_the_probed_op_was_not_sent() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let mut replies = list_state_replies(&blob, 0);
        replies.push(resp(0x2002)); // the setup is refused
        replies.extend(list_state_replies(&blob, 0));
        let (mut s, t) = session(replies, "Z 9");
        let p = s.probe_ftp_op(0x90EA, true, &[], None, names(0x90EA)).unwrap();
        assert!(matches!(p.outcome, FtpProbeOutcome::NotSent { cause: PtpUsbError::OperationFailed { op: OP_FTP_SETUP, .. } }), "{:?}", p.outcome);
        assert!(!p.is_indeterminate());
        assert!(!sent_op_codes(&t).contains(&0x90EA), "the delete went out after a refused setup");
        assert!(p.after.is_ok(), "a refused (completed) transaction does not poison the session");
    }

    #[test]
    fn only_errors_raised_before_a_byte_is_written_count_as_nothing_sent() {
        for e in [
            PtpUsbError::SessionPoisoned { cause: String::new() },
            PtpUsbError::TooManyCommandParams { given: 6, max: 5 },
            PtpUsbError::ContainerTooLarge { payload_len: 1 },
        ] {
            assert!(e.nothing_was_sent(), "{e:?}");
        }
        for e in [
            PtpUsbError::Usb(rusb::Error::Timeout),
            PtpUsbError::Usb(rusb::Error::Pipe),
            PtpUsbError::TransactionTimedOut(Duration::from_secs(1)),
            PtpUsbError::ShortWrite { written: 1, expected: 2 },
            PtpUsbError::OperationFailed { op: 0x90EC, code: PtpResponseCode(0x2002) },
            PtpUsbError::NoDataPhase { op: 0x90EC },
        ] {
            assert!(!e.nothing_was_sent(), "{e:?}");
        }
    }

    // ── the before/after list (b7d3#17) ────────────────────────────────────

    #[test]
    fn probe_ftp_op_reports_a_changed_list() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let mut replies = list_state_replies(&blob, 0);
        replies.push(resp(RESP_OK));
        // After: the selection has moved to a profile that is not there to read (count 1, selected 0
        // becomes selected 5), so the two states differ.
        replies.extend(list_state_replies(&blob, 5));
        let (mut s, _t) = session(replies, "Z 9");
        let p = s.probe_ftp_op(0x90EC, false, &[], None, NO_CONSENT).unwrap();
        assert_eq!(p.before.changes_to(p.after.as_ref().unwrap()), vec![FtpListChange::Info(FtpInfoField::Selected)]);
    }

    #[test]
    fn a_change_to_a_profile_field_or_the_device_name_shows_though_the_name_and_length_do_not() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        assert_eq!(decode_ftp_profile(&blob).unwrap().password, "12345678");
        // Same profile name, same length: only the password's bytes differ.
        let mut changed = blob.clone();
        let at = changed.windows(8).position(|w| w == b"12345678").expect("the password is in the blob as ASCII");
        changed[at..at + 8].copy_from_slice(b"87654321");
        let mut replies = list_state_replies(&blob, 0);
        replies.push(resp(RESP_OK));
        replies.extend(list_state_replies(&changed, 0));
        let (mut s, _t) = session(replies, "Z 9");
        let p = s.probe_ftp_op(0x90EC, false, &[], Some(&[1]), NO_CONSENT).unwrap();
        assert_ne!(p.before, *p.after.as_ref().unwrap(), "a changed password is not 'unchanged'");
        assert_eq!(
            p.before.changes_to(p.after.as_ref().unwrap()),
            vec![FtpListChange::ProfileChanged { index: 0, fields: vec!["password"] }]
        );
        // The device-name string: the field the 0x90EC probes blanked on a Z 9.
        let mut replies = list_state_replies(&blob, 0);
        replies.push(resp(RESP_OK));
        replies.extend(list_state_replies_of(1, &blob, 0, ""));
        let (mut s, _t) = session(replies, "Z 9");
        let p = s.probe_ftp_op(0x90EC, false, &[], Some(&[1]), NO_CONSENT).unwrap();
        assert_eq!(p.before.changes_to(p.after.as_ref().unwrap()), vec![FtpListChange::Info(FtpInfoField::DeviceName)]);
    }

    #[test]
    fn an_undecodable_profile_is_counted_and_a_change_in_its_bytes_is_seen() {
        let junk_a = vec![0x11u8; 40];
        let junk_b = vec![0x22u8; 40];
        let mut replies = list_state_replies(&junk_a, 0);
        replies.push(resp(RESP_OK));
        replies.extend(list_state_replies(&junk_b, 0));
        let (mut s, _t) = session(replies, "Z 9");
        let p = s.probe_ftp_op(0x90EC, false, &[], Some(&[1]), NO_CONSENT).unwrap();
        assert_eq!(p.before.undecodable_count(), 1);
        assert_eq!(p.before.entries[0].decoded, None);
        assert_eq!(
            p.before.changes_to(p.after.as_ref().unwrap()),
            vec![FtpListChange::ProfileChanged { index: 0, fields: vec![] }],
            "same length, both undecodable, different bytes"
        );
        // Debug shows the length of a profile's bytes, never the bytes.
        let shown = format!("{:?}", p.before);
        assert!(shown.contains("40 byte(s) hidden") && !shown.contains("17, 17"), "{shown}");
    }

    #[test]
    fn the_list_read_is_capped_at_32_profiles_and_says_so() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        // At the cap: all read, not truncated. Below: all read. Above: the first 32, flagged, and the 33rd never asked for.
        for (count, expect_read, expect_truncated) in [(31u32, 31usize, false), (32, 32, false), (33, 32, true), (u32::MAX, 32, true)] {
            let (mut s, t) = session(list_state_replies_of(count, &blob, 0, "Z_9_0000000"), "Z 9");
            let state = s.ftp_list_state().unwrap();
            assert_eq!(state.entries.len(), expect_read, "count {count}");
            assert_eq!(state.truncated, expect_truncated, "count {count}");
            assert_eq!(state.info.count, count);
            let reads: Vec<u32> = t
                .sent()
                .iter()
                .filter(|(_, op, _, _)| *op == OP_FTP_PROFILE_READ)
                .map(|(_, _, _, p)| u32::from_le_bytes(p[..4].try_into().unwrap()))
                .collect();
            assert_eq!(reads, (0..expect_read as u32).collect::<Vec<_>>(), "count {count}");
        }
    }

    #[test]
    fn a_list_that_is_truncated_in_one_read_and_not_the_other_differs() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let (mut a, _) = session(list_state_replies_of(32, &blob, 0, "N"), "Z 9");
        let (mut b, _) = session(list_state_replies_of(33, &blob, 0, "N"), "Z 9");
        let (sa, sb) = (a.ftp_list_state().unwrap(), b.ftp_list_state().unwrap());
        let changes = sa.changes_to(&sb);
        assert!(changes.contains(&FtpListChange::Info(FtpInfoField::Count)) && changes.contains(&FtpListChange::Info(FtpInfoField::Truncated)), "{changes:?}");
    }

    #[test]
    fn a_state_equals_itself_and_has_no_changes() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let (mut a, _) = session(list_state_replies_of(2, &blob, 1, "N"), "Z 9");
        let s = a.ftp_list_state().unwrap();
        assert!(s.changes_to(&s.clone()).is_empty());
        assert_eq!(s, s.clone());
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

    // ── the readout keeps what each reply carried (b7d3#43) ──────────────

    fn ftp_note(op: u16) -> FtpReplyNotes {
        FtpReplyNotes { op, response_params: vec![], deviations: vec![], unrecognized_containers: 0 }
    }

    #[test]
    fn read_ftp_profile_keeps_each_replys_params_deviations_and_skipped_containers() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let (mut s, _t) = session(
            vec![
                // 0x90E8: OK with two response parameters.
                Step::Stamped(vec![(CONTAINER_RESPONSE, RESP_OK, [le(5), le(6)].concat())]),
                // 0x90EB: the data is the right op, the response carries another transaction id.
                data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(7, 0, "Z_9_0000000")),
                resp_id(RESP_OK, 99),
                // 0x90ED: a container of an unknown kind, then data labelled with another op, then OK.
                Step::Stamped(vec![(0x0077, 0, vec![])]),
                data_c(0x1234, &blob),
                resp(RESP_OK),
            ],
            "Z 9",
        );
        let r = s.read_ftp_profile().unwrap();
        assert_eq!(
            r.replies,
            vec![
                FtpReplyNotes { response_params: vec![5, 6], ..ftp_note(OP_FTP_SETUP) },
                FtpReplyNotes { deviations: vec![ReplyDeviation::TransactionId { got: 99 }], ..ftp_note(OP_FTP_PROFILE_INFO) },
                FtpReplyNotes {
                    deviations: vec![ReplyDeviation::DataOp { got: 0x1234 }],
                    unrecognized_containers: 1,
                    ..ftp_note(OP_FTP_PROFILE_READ)
                },
            ]
        );
        assert!(!r.reply_matched_command, "the flag is the summary of the deviations");
        assert!(r.profile.unwrap().decoded.is_ok(), "the tolerated departures do not stop the decode");
    }

    #[test]
    fn read_ftp_profile_clean_replies_are_empty_notes_in_order_sent() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let (mut s, _t) = session(
            vec![
                resp(RESP_OK),
                data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(7, 0, "Z_9_0000000")),
                resp(RESP_OK),
                data_c(OP_FTP_PROFILE_READ, &blob),
                resp(RESP_OK),
            ],
            "Z 9",
        );
        let r = s.read_ftp_profile().unwrap();
        assert_eq!(r.replies, vec![ftp_note(OP_FTP_SETUP), ftp_note(OP_FTP_PROFILE_INFO), ftp_note(OP_FTP_PROFILE_READ)]);
        assert!(r.replies.iter().all(FtpReplyNotes::is_empty) && r.reply_matched_command);
        // No active profile: 0x90ED is not sent, so it has no entry.
        let (mut none, _t) = session(
            vec![resp(RESP_OK), data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(3, 0xff, "Z6_3_0000000")), resp(RESP_OK)],
            "Z 9",
        );
        let ops: Vec<u16> = none.read_ftp_profile().unwrap().replies.iter().map(|n| n.op).collect();
        assert_eq!(ops, vec![OP_FTP_SETUP, OP_FTP_PROFILE_INFO]);
    }

    #[test]
    fn a_note_is_empty_only_when_all_three_of_its_parts_are() {
        assert!(ftp_note(1).is_empty());
        assert!(!FtpReplyNotes { response_params: vec![0], ..ftp_note(1) }.is_empty());
        assert!(!FtpReplyNotes { deviations: vec![ReplyDeviation::DataOp { got: 1 }], ..ftp_note(1) }.is_empty());
        assert!(!FtpReplyNotes { unrecognized_containers: 1, ..ftp_note(1) }.is_empty());
    }

    #[test]
    fn a_malformed_info_reply_keeps_its_bytes_and_stops_before_0x90ed() {
        let ok = ftp_info_bytes(7, 0, "Z_9_0000000");
        let mut longer = ok.clone();
        longer.push(0);
        for (what, bytes) in [("truncated", ok[..10].to_vec()), ("empty", vec![]), ("trailing byte", longer)] {
            let (mut s, t) = session(vec![resp(RESP_OK), data_c(OP_FTP_PROFILE_INFO, &bytes), resp(RESP_OK)], "Z 9");
            match s.read_ftp_profile().unwrap_err() {
                PtpUsbError::FtpInfoMalformed { raw, cause } => {
                    assert_eq!(raw, bytes, "{what}");
                    assert!(matches!(*cause, PtpUsbError::FtpBlobMalformed { .. }), "{what}: {cause:?}");
                }
                other => panic!("{what}: {other:?}"),
            }
            assert!(t.sent().iter().all(|(_, op, _, _)| *op != OP_FTP_PROFILE_READ), "{what}");
        }
        // The same error from the write planner, which reads the same info reply.
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let (mut s, _t) = session(vec![resp(RESP_OK), data_c(OP_FTP_PROFILE_INFO, &ok[..10]), resp(RESP_OK)], "Z 9");
        assert!(matches!(s.plan_ftp_write(0, 0, &blob, false), Err(PtpUsbError::FtpInfoMalformed { .. })));
    }

    #[test]
    fn a_malformed_info_reply_prints_its_bytes_up_to_the_bound_and_records_only_its_length() {
        use crate::diag::Val;
        let cause = || Box::new(PtpUsbError::FtpBlobMalformed { what: "info end", offset: 0, detail: "x".into() });
        for (len, cut) in [(0, false), (FTP_INFO_SHOWN_MAX - 1, false), (FTP_INFO_SHOWN_MAX, false), (FTP_INFO_SHOWN_MAX + 1, true)] {
            let raw: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let shown_len = len.min(FTP_INFO_SHOWN_MAX);
            let e = PtpUsbError::FtpInfoMalformed { raw: raw.clone(), cause: cause() };
            let text = e.to_string();
            assert!(text.contains(&format!("({len} byte(s)")), "{len}: {text}");
            assert_eq!(text.contains(&format!("first {FTP_INFO_SHOWN_MAX} shown")), cut, "{len}: {text}");
            assert!(text.contains(&hex_bytes(&raw[..shown_len])), "{len}: {text}");
            if cut {
                assert!(!text.contains(&hex_bytes(&raw[..shown_len + 1])), "one byte past the bound is not printed: {text}");
            }
            // Diagnostics: the kind, the length and the cause's kind; no bytes, no hash, no text from the camera.
            let f = e.diag_fields();
            assert_eq!(
                f,
                vec![
                    ("error_kind", Val::text("ftp_info_malformed")),
                    ("len", Val::U64(len as u64)),
                    ("cause_kind", Val::text("ftp_blob_malformed")),
                ]
            );
        }
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


    // ══ remediation E1: string decoders, write gates, FTP write guard and read-back ═════════════

    // ── the string write gate (b7d3#76, #15) ─────────────────────────────

    #[test]
    fn string_write_gate_lists_are_pinned_literally() {
        // Dropping an entry, renaming "Z6_3" (the real Z6III string) or adding an unconfirmed body each
        // changes what a camera is allowed to receive: the lists are contract, so they are spelled out.
        assert_eq!(STRING_PROP_WRITE_MODELS, ["Z 9", "Z6_3", "Z 5"]);
        assert_eq!(STRING_PROP_WRITE_CONFIRMED_PROPS, [0xD072, 0xD073, 0xD090]);
    }

    #[test]
    fn string_write_gate_lists_stay_inside_the_registries_they_draw_on() {
        for m in STRING_PROP_WRITE_MODELS {
            assert!(sdk_model_name(m).is_some(), "{m:?} is write-confirmed but not in KNOWN_MODELS");
            assert!(KNOWN_MODELS.contains(m), "{m:?}");
        }
        for c in STRING_PROP_WRITE_CONFIRMED_PROPS {
            assert!(STRING_PROPS.iter().any(|(p, _)| p == c), "{c:#x} is write-confirmed but not a string property");
        }
    }

    /// Artist and Copyright were run on the Z 9, Z6III and Z 5 (2026-11-06, limits 36 / 54, restored byte for
    /// byte), so every string property is written without the force flag on every confirmed model.
    #[test]
    fn every_string_property_is_written_without_force_on_every_confirmed_model() {
        for model in STRING_PROP_WRITE_MODELS {
            for (code, _) in STRING_PROPS {
                let (mut s, t) = session(set_script_for(code, 1, "a", RESP_OK, "b"), model);
                assert!(s.set_string_property(code, "b", false).unwrap().verified, "{model} {code:#x}");
                assert_eq!(sent_ops(&t)[2], (CONTAINER_COMMAND, OP_SET_DEVICE_PROP_VALUE, le(u32::from(code))), "{model} {code:#x}");
            }
        }
    }

    /// The (property, model) mechanism, with lists that differ (nothing in the real lists is unconfirmed any
    /// more): an unlisted property is `PropWriteUnconfirmed` on a listed model, an unlisted model is
    /// `UnconfirmedModel` first, and a listed pair passes. Each side of each membership test is pinned.
    #[test]
    fn the_write_gate_checks_the_model_first_then_the_property() {
        let models = ["Z 9", "Z 5"];
        let confirmed = [0xD090u16];
        assert!(check_string_write_gate("Z 9", 0xD090, &models, &confirmed).is_ok());
        assert!(check_string_write_gate("Z 5", 0xD090, &models, &confirmed).is_ok());
        for code in [0xD072u16, 0xD073] {
            let err = check_string_write_gate("Z 9", code, &models, &confirmed).unwrap_err();
            assert!(matches!(&err, PtpUsbError::PropWriteUnconfirmed { propcode, model } if *propcode == code && model == "Z 9"), "{err:?}");
            let text = err.to_string();
            assert!(text.contains("has not been confirmed") && text.contains("--force-unconfirmed-model"), "{text}");
        }
        // an unlisted model fails on the model even for an unconfirmed property
        for code in [0xD090u16, 0xD072] {
            assert!(matches!(check_string_write_gate("Z 8", code, &models, &confirmed), Err(PtpUsbError::UnconfirmedModel(m)) if m == "Z 8"));
        }
        // empty lists refuse everything
        assert!(check_string_write_gate("Z 9", 0xD090, &[], &confirmed).is_err());
        assert!(check_string_write_gate("Z 9", 0xD090, &models, &[]).is_err());
    }

    #[test]
    fn the_model_is_checked_before_the_property_so_the_error_names_the_real_cause() {
        let (mut s, _t) = session(vec![], "Z 8");
        assert!(matches!(s.set_string_property(0xD072, "x", false), Err(PtpUsbError::UnconfirmedModel(m)) if m == "Z 8"));
    }

    #[test]
    fn a_descriptor_for_another_property_is_refused_before_any_write() {
        // The echoed PropCode (first two descriptor bytes) must be the one asked for.
        let (mut s, t) = session(vec![data_c(OP_GET_DEVICE_PROP_DESC, &prop_desc(0xD072, 0xFFFF, 1)), resp(RESP_OK)], "Z 9");
        let err = s.set_string_property(0xD090, "x", false).unwrap_err();
        assert!(matches!(&err, PtpUsbError::PropValueBad { propcode: 0xD090, why } if why.contains("0xd072")), "{err:?}");
        assert_eq!(t.sent().len(), 1, "only the descriptor read went out");
    }

    #[test]
    fn a_descriptor_of_exactly_five_bytes_is_enough_and_four_is_not() {
        // PropCode u16, DataType u16, GetSet u8: the get/set flag is the fifth byte.
        let five = vec![0x90, 0xD0, 0xFF, 0xFF, 0x01];
        let (mut s, _t) = session(
            vec![
                data_c(OP_GET_DEVICE_PROP_DESC, &five),
                resp(RESP_OK),
                data_c(OP_GET_DEVICE_PROP_VALUE, &prop_str("old")),
                resp(RESP_OK),
                resp(RESP_OK),
                data_c(OP_GET_DEVICE_PROP_VALUE, &prop_str("new")),
                resp(RESP_OK),
            ],
            "Z 9",
        );
        assert!(s.set_string_property(0xD090, "new", false).unwrap().verified);
        let (mut s, _t) = session(vec![data_c(OP_GET_DEVICE_PROP_DESC, &five[..4]), resp(RESP_OK)], "Z 9");
        assert!(matches!(s.set_string_property(0xD090, "new", false), Err(PtpUsbError::PropValueBad { .. })));
    }

    #[test]
    fn a_set_command_that_never_left_the_host_is_not_indeterminate() {
        // The FTP twin is `a_write_that_never_got_its_data_out_is_not_indeterminate`. Writes: descriptor
        // command (16 bytes), value command (16), then the 0x1016 command, which the pipe takes 5 bytes of.
        let (mut s, _t) = session(set_script(1, "old", RESP_OK, "new"), "Z 9");
        s.handle.0.borrow_mut().write_caps = VecDeque::from([16, 16, 5]);
        let err = s.set_string_property(0xD090, "new", false).unwrap_err();
        assert!(matches!(err, PtpUsbError::ShortWrite { written: 5, expected: 16 }), "{err:?}");
    }

    #[test]
    fn latin1_c1_and_delete_controls_are_refused_and_the_neighbours_are_not() {
        // char::is_control covers U+007F and U+0080..=U+009F, all inside the Latin-1 range the camera keeps.
        for control in ['\u{7f}', '\u{80}', '\u{85}', '\u{9f}'] {
            let text = format!("a{control}b");
            assert!(encode_ptp_string_value(&text).is_err(), "{control:?}");
            let (mut s, t) = session(vec![], "Z 9");
            let err = s.set_string_property(0xD090, &text, false).unwrap_err();
            assert!(matches!(err, PtpUsbError::PropValueBad { .. }), "{control:?}: {err:?}");
            assert!(t.sent().is_empty(), "{control:?}");
        }
        for fine in ['\u{7e}', '\u{a0}', '\u{ff}'] {
            assert!(encode_ptp_string_value(&fine.to_string()).is_ok(), "{fine:?}");
        }
    }

    // ── one PTP string decoder, three consumers (b7d3#35, #83) ───────────

    /// Wire inputs for the string decoders: (name, bytes, text, anomalies, bytes consumed).
    fn string_wire_cases() -> Vec<(&'static str, Vec<u8>, &'static str, Vec<StringAnomaly>, usize)> {
        use StringAnomaly::*;
        vec![
            ("empty by count 0", vec![0], "", vec![], 1),
            ("empty by NUL alone", vec![1, 0, 0], "", vec![], 3),
            ("ordinary", vec![3, b'a', 0, b'b', 0, 0, 0], "ab", vec![], 7),
            ("non-BMP pair", vec![3, 0x3D, 0xD8, 0xF7, 0xDC, 0, 0], "\u{1F4F7}", vec![], 7),
            ("missing terminator", vec![2, b'a', 0, b'b', 0], "ab", vec![MissingTerminator], 5),
            ("embedded NUL", vec![3, b'a', 0, 0, 0, 0, 0], "a\0", vec![EmbeddedNul], 7),
            ("unpaired surrogate", vec![2, 0x3D, 0xD8, 0, 0], "\u{FFFD}", vec![UnpairedSurrogate], 5),
            ("missing terminator and embedded NUL", vec![2, 0, 0, b'a', 0], "\0a", vec![MissingTerminator, EmbeddedNul], 5),
            ("all three", vec![3, 0, 0, 0x3D, 0xD8, b'x', 0], "\0\u{FFFD}x", vec![MissingTerminator, EmbeddedNul, UnpairedSurrogate], 7),
        ]
    }

    #[test]
    fn the_canonical_decoder_reports_text_anomalies_in_a_fixed_order_and_bytes_used() {
        for (name, bytes, text, anomalies, used) in string_wire_cases() {
            let (s, n) = decode_ptp_string(&bytes).unwrap_or_else(|f| panic!("{name}: {f:?}"));
            assert_eq!((s.text.as_str(), &s.anomalies, n), (text, &anomalies, used), "{name}");
            // Bytes after the string are not consumed by it.
            let mut longer = bytes.clone();
            longer.extend_from_slice(&[0xEE, 0xEE]);
            assert_eq!(decode_ptp_string(&longer).unwrap().1, used, "{name} + trailing");
        }
    }

    #[test]
    fn the_three_consumers_agree_on_every_wire_case_and_differ_only_in_strictness() {
        for (name, bytes, text, anomalies, used) in string_wire_cases() {
            // DeviceInfo keeps the anomalies on the value and always advances.
            let mut di = DeviceInfoReader { data: &bytes, pos: 0 };
            let lenient = di.string("f").unwrap();
            assert_eq!((lenient.text.as_str(), &lenient.anomalies, di.pos), (text, &anomalies, used), "{name} lenient");
            // The FTP blob reader and the property-value decoder refuse any anomaly, and agree on the text otherwise.
            let mut fr = FtpBlobReader { data: &bytes, pos: 0 };
            let ftp = fr.ptp_string("f");
            let value = decode_ptp_string_value(&bytes);
            if anomalies.is_empty() {
                let ftp = ftp.unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!((ftp.text.as_str(), &ftp.anomalies, fr.pos), (text, &Vec::new(), used), "{name} ftp");
                assert_eq!(value.as_deref(), Ok(text), "{name} value");
            } else {
                match ftp {
                    Err(PtpUsbError::FtpBlobMalformed { what: "f", offset: 0, detail }) => {
                        for a in &anomalies {
                            assert!(detail.contains(a.tag()), "{name}: {detail}");
                        }
                    }
                    other => panic!("{name}: the FTP reader must refuse at the string's first byte, got {other:?}"),
                }
                assert_eq!(fr.pos, 0, "{name}: a refused string leaves the cursor at its start");
                let msg = value.expect_err(name);
                assert!(msg.contains(anomalies[0].describe()), "{name}: {msg}");
            }
        }
    }

    #[test]
    fn a_truncated_string_is_refused_by_all_three_with_the_same_numbers() {
        // Count byte promises 3 units (6 bytes); 2 follow.
        let bytes = [3u8, b'a', 0];
        let mut di = DeviceInfoReader { data: &bytes, pos: 0 };
        assert!(matches!(
            di.string("f"),
            Err(PtpUsbError::DeviceInfoMalformed { offset: 1, fault: DeviceInfoFault::Truncated { needed: 6, available: 2 }, .. })
        ));
        let mut fr = FtpBlobReader { data: &bytes, pos: 0 };
        assert!(matches!(
            fr.ptp_string("f"),
            Err(PtpUsbError::FtpBlobMalformed { offset: 1, detail, .. }) if detail == "needs 6 byte(s) but only 2 remain"
        ));
        let msg = decode_ptp_string_value(&bytes).unwrap_err();
        assert!(msg.contains("says 3 unit(s) = 6 byte(s)") && msg.contains("2 byte(s) follow"), "{msg}");
        // No bytes at all: no count byte.
        let mut di = DeviceInfoReader { data: &[], pos: 0 };
        assert!(matches!(
            di.string("f"),
            Err(PtpUsbError::DeviceInfoMalformed { offset: 0, fault: DeviceInfoFault::Truncated { needed: 1, available: 0 }, .. })
        ));
        assert!(matches!(FtpBlobReader { data: &[], pos: 0 }.ptp_string("f"), Err(PtpUsbError::FtpBlobMalformed { offset: 0, .. })));
        assert!(decode_ptp_string_value(&[]).is_err());
    }

    #[test]
    fn the_value_decoder_wants_the_exact_length_while_the_framed_readers_leave_the_rest() {
        let exact = [2u8, b'a', 0, 0, 0];
        assert_eq!(decode_ptp_string_value(&exact).as_deref(), Ok("a"));
        let mut longer = exact.to_vec();
        longer.push(0);
        assert!(decode_ptp_string_value(&longer).is_err(), "one stray byte after the string is not a value");
        let mut fr = FtpBlobReader { data: &longer, pos: 0 };
        assert_eq!(fr.ptp_string("f").unwrap().text, "a");
        assert_eq!(fr.pos, 5, "the stray byte stays for the next field");
    }

    /// The reference profile blob with its profile-name string (offset 2, `07 "CLAUDE" 00`) patched.
    fn reference_blob_with_name_unit(unit: usize, value: u16) -> Vec<u8> {
        let mut blob = hex::decode_hex(reference_ftp_blob_hex());
        assert_eq!(blob[2], 7, "fixture: the name is 6 characters and a NUL");
        let at = 3 + unit * 2;
        blob[at..at + 2].copy_from_slice(&value.to_le_bytes());
        blob
    }

    #[test]
    fn a_profile_whose_name_string_is_garbled_is_refused_and_so_never_reaches_the_camera() {
        for (what, blob, tag) in [
            ("embedded NUL", reference_blob_with_name_unit(2, 0), "embedded_nul"),
            ("unpaired surrogate", reference_blob_with_name_unit(1, 0xD800), "unpaired_surrogate"),
            ("missing terminator", reference_blob_with_name_unit(6, u16::from(b'x')), "missing_terminator"),
        ] {
            let err = decode_ftp_profile(&blob).unwrap_err();
            assert!(
                matches!(&err, PtpUsbError::FtpBlobMalformed { what: "profile name", offset: 2, detail } if detail.contains(tag)),
                "{what}: {err:?}"
            );
            // And the write path, which relies on the decoder, sends nothing.
            let (mut s, t) = session(vec![], "Z 9");
            assert!(matches!(s.write_ftp_profile_blob(0, 0, &blob, false), Err(PtpUsbError::FtpBlobMalformed { .. })), "{what}");
            assert!(t.sent().is_empty(), "{what}");
        }
        // The same string, well formed, decodes (the patch helper is not what breaks it).
        assert_eq!(decode_ftp_profile(&reference_blob_with_name_unit(2, u16::from(b'A'))).unwrap().profile_name, "CLAUDE");
    }

    #[test]
    fn the_0x90eb_device_name_is_held_to_the_same_string_rules() {
        let mut info = ftp_info_bytes(1, 0, "Z_9_0000000");
        assert!(parse_ftp_profile_info(&info).is_ok());
        let name_unit_at = 2 + 4 + 4 + 4 + 16 + 1; // first unit of the name
        info[name_unit_at..name_unit_at + 2].copy_from_slice(&0xD800u16.to_le_bytes());
        assert!(matches!(
            parse_ftp_profile_info(&info),
            Err(PtpUsbError::FtpBlobMalformed { what: "info device name", .. })
        ));
    }

    // ── non-ASCII text fields (b7d3#42) ──────────────────────────────────

    /// `blob` with the `[u32 len][old]` field replaced by `[u32 len][new]`.
    fn replace_u32_field(blob: &[u8], old: &str, new: &[u8]) -> Vec<u8> {
        let mut needle = le(old.len() as u32);
        needle.extend_from_slice(old.as_bytes());
        let at = blob.windows(needle.len()).position(|w| w == needle).unwrap_or_else(|| panic!("{old:?} not in the fixture"));
        let mut out = blob[..at].to_vec();
        out.extend_from_slice(&le(new.len() as u32));
        out.extend_from_slice(new);
        out.extend_from_slice(&blob[at + needle.len()..]);
        out
    }

    #[test]
    fn non_ascii_text_in_the_four_length_prefixed_fields_decodes_and_is_not_re_encodable() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let base = decode_ftp_profile(&blob).unwrap();
        let japanese = "ネット-\u{e9}";
        for (field, old) in [
            ("2.4 GHz SSID", base.ssid_24ghz.clone()),
            ("5 GHz SSID", base.ssid_5ghz.clone()),
            ("username", base.username.clone()),
            ("password", base.password.clone()),
        ] {
            let patched = replace_u32_field(&blob, &old, japanese.as_bytes());
            let d = decode_ftp_profile(&patched).unwrap_or_else(|e| panic!("{field}: {e}"));
            let got = match field {
                "2.4 GHz SSID" => &d.ssid_24ghz,
                "5 GHz SSID" => &d.ssid_5ghz,
                "username" => &d.username,
                _ => &d.password,
            };
            assert_eq!(got, japanese, "{field}");
            assert_eq!(d.profile_name, base.profile_name, "{field}: nothing else moved");
            assert!(d.as_encodable().is_none(), "{field}: the encoder writes these fields as ASCII only");
            // A copy of such a profile sends the camera's own bytes, unchanged.
            let (mut s, t) = session(blob_write_script(1), "Z 9");
            s.write_ftp_profile_blob(0, 0, &patched, false).unwrap();
            assert_eq!(t.sent().last().unwrap().3, patched, "{field}");
            // The masked display counts bytes, so a 3-byte character shows as three asterisks.
            assert_eq!(d.with_secrets_masked().password.len(), d.password.len());
        }
    }

    #[test]
    fn bytes_that_are_not_utf8_are_refused_naming_the_field_and_the_first_bad_byte() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let ssid = decode_ftp_profile(&blob).unwrap().ssid_24ghz;
        let start = blob.windows(4 + ssid.len()).position(|w| w[4..] == *ssid.as_bytes()).unwrap();
        // 0xE9 alone is Latin-1 "é", not UTF-8.
        for (bytes, valid_up_to) in [(&[0xE9u8][..], 0usize), (&b"ab\xE9"[..], 2), (&b"a\xE3\x83"[..], 1)] {
            let patched = replace_u32_field(&blob, &ssid, bytes);
            match decode_ftp_profile(&patched) {
                Err(PtpUsbError::FtpBlobMalformed { what: "2.4 GHz SSID", offset, detail }) => {
                    assert_eq!(offset, start + 4 + valid_up_to, "{bytes:?}");
                    assert!(detail.contains("not valid UTF-8"), "{detail}");
                }
                other => panic!("{bytes:?}: {other:?}"),
            }
        }
    }

    // ── the FTP write guard (b7d3#06, #45) ───────────────────────────────

    /// Replies for a guarded blob write over `count` stored profiles: setup, info, setup, write.
    fn blob_write_script(count: u32) -> Vec<Step> {
        vec![resp(RESP_OK), data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(count, 0, "Z_9_0000000")), resp(RESP_OK), resp(RESP_OK), resp(RESP_OK)]
    }

    /// Replies for one `read_ftp_profile_slot(Some(0))`: setup, info (count), profile.
    fn read_back_script(count: u32, blob: &[u8]) -> Vec<Step> {
        vec![
            resp(RESP_OK),
            data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(count, 0, "Z_9_0000000")),
            resp(RESP_OK),
            data_c(OP_FTP_PROFILE_READ, blob),
            resp(RESP_OK),
        ]
    }

    fn ops_sent(t: &ScriptedTransport) -> Vec<u16> {
        t.sent().iter().filter(|(kind, ..)| *kind == CONTAINER_COMMAND).map(|(_, op, ..)| *op).collect()
    }

    #[test]
    fn plan_ftp_write_reads_the_count_and_sends_no_write() {
        let blob = hex::decode_hex(ftp_blob_port22_hex());
        // Setup + info only: a write would need two more replies and would show in the sent ops.
        let script = || vec![resp(RESP_OK), data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(3, 0, "Z_9_0000000")), resp(RESP_OK)];
        let (mut s, t) = session(script(), "Z 9");
        let plan = s.plan_ftp_write(1, 0, &blob, false).unwrap();
        assert_eq!((plan.kind, plan.count, plan.model.as_str()), (FtpWriteKind::Replace, 3, "Z 9"));
        assert_eq!(plan.decoded, decode_ftp_profile(&blob).unwrap());
        assert_eq!(ops_sent(&t), vec![OP_FTP_SETUP, OP_FTP_PROFILE_INFO], "no 0x90EE");
        // The add form: slot == count with 1.
        let (mut s, t) = session(script(), "Z 9");
        assert_eq!(s.plan_ftp_write(3, 1, &blob, false).unwrap().kind, FtpWriteKind::Add);
        assert!(!ops_sent(&t).contains(&OP_FTP_PROFILE_WRITE));
    }

    #[test]
    fn plan_ftp_write_refuses_where_the_write_refuses_and_before_the_same_io() {
        let blob = hex::decode_hex(ftp_blob_port22_hex());
        // The model gate comes first: nothing is sent to a Z 5, and force passes it.
        let (mut s, t) = session(vec![], "Z 5");
        assert!(matches!(s.plan_ftp_write(0, 0, &blob, false), Err(PtpUsbError::UnconfirmedModel(m)) if m == "Z 5"));
        assert!(t.sent().is_empty());
        // A blob that does not decode and a count-independent refusal both come before any I/O.
        let mut bad = blob.clone();
        bad[0] = 0;
        let (mut s, t) = session(vec![], "Z 9");
        assert!(matches!(s.plan_ftp_write(0, 0, &bad, false), Err(PtpUsbError::FtpBlobMalformed { .. })));
        assert!(matches!(
            s.plan_ftp_write(0, 7, &blob, false),
            Err(PtpUsbError::FtpWriteRefused { why: FtpWriteRefusal::UnknownParam2 { param2: 7 }, .. })
        ));
        assert!(matches!(
            s.plan_ftp_write(u32::MAX, 0, &blob, false),
            Err(PtpUsbError::FtpWriteRefused { why: FtpWriteRefusal::SlotHangsCamera, .. })
        ));
        assert!(t.sent().is_empty());
        // The count-dependent rules at the boundary of 2 stored profiles: (2, 0) is the Z 9 stall, (2, 1) the add.
        let script = || vec![resp(RESP_OK), data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(2, 0, "Z_9_0000000")), resp(RESP_OK)];
        let (mut s, _t) = session(script(), "Z 9");
        assert!(matches!(
            s.plan_ftp_write(2, 0, &blob, false),
            Err(PtpUsbError::FtpWriteRefused { why: FtpWriteRefusal::ReplaceSlotNotStored { slot: 2, count: 2 }, .. })
        ));
        let (mut s, _t) = session(script(), "Z 9");
        assert!(matches!(
            s.plan_ftp_write(1, 1, &blob, false),
            Err(PtpUsbError::FtpWriteRefused { why: FtpWriteRefusal::AddNotAtCount { slot: 1, count: 2 }, .. })
        ));
        let (mut s, _t) = session(script(), "Z 9");
        assert_eq!(s.plan_ftp_write(1, 0, &blob, false).unwrap().kind, FtpWriteKind::Replace);
        let (mut s, _t) = session(script(), "Z 9");
        assert_eq!(s.plan_ftp_write(2, 1, &blob, false).unwrap().kind, FtpWriteKind::Add);
    }

    #[test]
    fn classify_ftp_write_pins_both_sides_of_each_boundary() {
        use FtpWriteKind::*;
        use FtpWriteRefusal::*;
        // Replace needs slot < count: 2 of 3 yes; 3 and 4 no.
        assert_eq!(classify_ftp_write(2, 0, Some(3)), Ok(Replace));
        assert_eq!(classify_ftp_write(3, 0, Some(3)), Err(ReplaceSlotNotStored { slot: 3, count: 3 }));
        assert_eq!(classify_ftp_write(4, 0, Some(3)), Err(ReplaceSlotNotStored { slot: 4, count: 3 }));
        assert_eq!(classify_ftp_write(0, 0, Some(1)), Ok(Replace));
        assert_eq!(classify_ftp_write(0, 0, Some(0)), Err(ReplaceSlotNotStored { slot: 0, count: 0 }));
        // Add needs slot == count: 2 and 4 of 3 no; 3 yes. An empty camera adds at 0.
        assert_eq!(classify_ftp_write(3, 1, Some(3)), Ok(Add));
        assert_eq!(classify_ftp_write(2, 1, Some(3)), Err(AddNotAtCount { slot: 2, count: 3 }));
        assert_eq!(classify_ftp_write(4, 1, Some(3)), Err(AddNotAtCount { slot: 4, count: 3 }));
        assert_eq!(classify_ftp_write(0, 1, Some(0)), Ok(Add));
        // The second parameter: 0 and 1 yes, 2 and the hang value no, wherever the slot is.
        assert_eq!(classify_ftp_write(0, 2, Some(3)), Err(UnknownParam2 { param2: 2 }));
        assert_eq!(classify_ftp_write(3, 2, Some(3)), Err(UnknownParam2 { param2: 2 }));
        assert_eq!(classify_ftp_write(0, u32::MAX, None), Err(UnknownParam2 { param2: u32::MAX }));
        // The hang slot is refused for both forms and for any count, even the absurd count that would equal it.
        for param2 in [0, 1] {
            assert_eq!(classify_ftp_write(u32::MAX, param2, None), Err(SlotHangsCamera));
            assert_eq!(classify_ftp_write(u32::MAX, param2, Some(u32::MAX)), Err(SlotHangsCamera));
            assert_eq!(classify_ftp_write(u32::MAX - 1, param2, None).is_ok(), true, "state-free rules only");
        }
        // Without a count only the state-free rules apply: slot 99 is not judged.
        assert_eq!(classify_ftp_write(99, 0, None), Ok(Replace));
        assert_eq!(classify_ftp_write(99, 1, None), Ok(Add));
        // An unknown second parameter outranks the hang slot.
        assert_eq!(classify_ftp_write(u32::MAX, 7, None), Err(UnknownParam2 { param2: 7 }));
    }

    #[test]
    fn a_state_free_refusal_sends_nothing_at_all() {
        let blob = hex::decode_hex(ftp_blob_port22_hex());
        for (slot, param2) in [(0, 2), (0, u32::MAX), (u32::MAX, 0), (u32::MAX, 1)] {
            let (mut s, t) = session(blob_write_script(3), "Z 9");
            let err = s.write_ftp_profile_blob(slot, param2, &blob, false).unwrap_err();
            assert!(matches!(&err, PtpUsbError::FtpWriteRefused { slot: sl, param2: p, .. } if (*sl, *p) == (slot, param2)), "{err:?}");
            assert!(t.sent().is_empty(), "({slot}, {param2}) must be refused before any I/O");
        }
    }

    #[test]
    fn a_slot_the_count_does_not_allow_is_refused_after_the_count_read_and_before_the_write() {
        let blob = hex::decode_hex(ftp_blob_port22_hex());
        for (slot, param2, count) in [(3u32, 0u32, 3u32), (4, 0, 3), (0, 0, 0), (2, 1, 3), (4, 1, 3)] {
            let (mut s, t) = session(blob_write_script(count), "Z 9");
            let err = s.write_ftp_profile_blob(slot, param2, &blob, false).unwrap_err();
            assert!(matches!(err, PtpUsbError::FtpWriteRefused { .. }), "({slot}, {param2}) of {count}: {err:?}");
            assert_eq!(ops_sent(&t), vec![OP_FTP_SETUP, OP_FTP_PROFILE_INFO], "({slot}, {param2}) of {count}: only the count was read");
            let text = err.to_string();
            assert!(text.contains("refused before anything was written"), "{text}");
        }
    }

    #[test]
    fn the_two_known_forms_are_sent_unchanged_after_the_count_read() {
        let blob = hex::decode_hex(ftp_blob_port22_hex());
        for (slot, param2, count) in [(0u32, 0u32, 1u32), (2, 0, 3), (3, 1, 3), (0, 1, 0)] {
            let (mut s, t) = session(blob_write_script(count), "Z 9");
            s.write_ftp_profile_blob(slot, param2, &blob, false).unwrap();
            assert_eq!(
                ops_sent(&t),
                vec![OP_FTP_SETUP, OP_FTP_PROFILE_INFO, OP_FTP_SETUP, OP_FTP_PROFILE_WRITE],
                "the write keeps its own 0x90E8 immediately before it"
            );
            let sent = t.sent();
            let write = sent.iter().find(|(k, op, ..)| *k == CONTAINER_COMMAND && *op == OP_FTP_PROFILE_WRITE).unwrap();
            assert_eq!(write.3, [le(slot), le(param2)].concat());
        }
    }

    #[test]
    fn a_failed_count_read_stops_before_the_write() {
        let blob = hex::decode_hex(ftp_blob_port22_hex());
        let (mut s, t) = session(vec![resp(RESP_OK), resp(0x2002)], "Z 9");
        assert!(matches!(
            s.write_ftp_profile_blob(0, 0, &blob, false),
            Err(PtpUsbError::OperationFailed { op: OP_FTP_PROFILE_INFO, .. })
        ));
        assert!(!ops_sent(&t).contains(&OP_FTP_PROFILE_WRITE));
    }

    #[test]
    fn a_reply_that_did_not_match_during_the_count_read_clears_the_matched_flag() {
        let blob = hex::decode_hex(ftp_blob_port22_hex());
        let mut steps = blob_write_script(1);
        steps[0] = resp_id(RESP_OK, 41);
        let (mut s, _t) = session(steps, "Z 9");
        assert!(!s.write_ftp_profile_blob(0, 0, &blob, false).unwrap().reply_matched_command);
    }

    // ── read-back verification (e5a3#04) ─────────────────────────────────

    fn verified_script(count_before: u32, count_after: u32, written: &[u8], read_back: &[u8]) -> Vec<Step> {
        let _ = written;
        let mut steps = blob_write_script(count_before);
        steps.extend(read_back_script(count_after, read_back));
        steps
    }

    #[test]
    fn a_replace_that_reads_back_identical_with_the_same_count_is_verified() {
        let blob = hex::decode_hex(ftp_blob_port22_hex());
        let (mut s, t) = session(verified_script(2, 2, &blob, &blob), "Z 9");
        let out = s.write_ftp_profile_blob_verified(1, 0, &blob, false).unwrap();
        assert_eq!((out.kind, out.count_before), (FtpWriteKind::Replace, 2));
        assert!(matches!(out.verification, FtpWriteVerification::Verified { count_after: 2 }), "{:?}", out.verification);
        assert!(out.verification.is_verified());
        assert_eq!(
            ops_sent(&t),
            vec![
                OP_FTP_SETUP, OP_FTP_PROFILE_INFO, OP_FTP_SETUP, OP_FTP_PROFILE_WRITE, // the guarded write
                OP_FTP_SETUP, OP_FTP_PROFILE_INFO, OP_FTP_PROFILE_READ,                // the read-back
            ]
        );
        let sent = t.sent();
        let read = sent.iter().find(|(k, op, ..)| *k == CONTAINER_COMMAND && *op == OP_FTP_PROFILE_READ).unwrap();
        assert_eq!(read.3, le(0), "the written profile is looked for at the front, slot 0");
    }

    #[test]
    fn an_add_is_verified_only_when_the_count_rose_by_one() {
        let blob = hex::decode_hex(ftp_blob_port22_hex());
        let (mut s, _t) = session(verified_script(2, 3, &blob, &blob), "Z 9");
        let out = s.write_ftp_profile_blob_verified(2, 1, &blob, false).unwrap();
        assert_eq!(out.kind, FtpWriteKind::Add);
        assert!(matches!(out.verification, FtpWriteVerification::Verified { count_after: 3 }), "{:?}", out.verification);
        // The profile is right but the count did not move: not verified, and the fields list is empty.
        for after in [2u32, 4] {
            let (mut s, _t) = session(verified_script(2, after, &blob, &blob), "Z 9");
            let out = s.write_ftp_profile_blob_verified(2, 1, &blob, false).unwrap();
            assert!(
                matches!(&out.verification, FtpWriteVerification::Mismatch { count_after, count_expected: 3, differing_fields }
                    if *count_after == after && differing_fields.is_empty()),
                "after {after}: {:?}",
                out.verification
            );
            assert!(!out.verification.is_verified());
        }
        // A replace whose count changed is a mismatch too.
        let (mut s, _t) = session(verified_script(2, 3, &blob, &blob), "Z 9");
        let out = s.write_ftp_profile_blob_verified(0, 0, &blob, false).unwrap();
        assert!(matches!(out.verification, FtpWriteVerification::Mismatch { count_after: 3, count_expected: 2, .. }));
    }

    #[test]
    fn a_read_back_that_differs_names_the_fields_and_never_their_values() {
        let blob = hex::decode_hex(ftp_blob_port22_hex());
        let other = replace_u32_field(&blob, "pass1234", b"hunter22");
        let (mut s, _t) = session(verified_script(1, 1, &blob, &other), "Z 9");
        let out = s.write_ftp_profile_blob_verified(0, 0, &blob, false).unwrap();
        match &out.verification {
            FtpWriteVerification::Mismatch { differing_fields, count_after: 1, count_expected: 1 } => {
                assert_eq!(differing_fields, &["password"]);
            }
            other => panic!("{other:?}"),
        }
        let shown = format!("{:?}", out.verification);
        assert!(!shown.contains("hunter22") && !shown.contains("pass1234"), "{shown}");
    }

    #[test]
    fn every_field_of_a_decoded_profile_is_compared() {
        let want = decode_ftp_profile(&hex::decode_hex(reference_ftp_blob_hex())).unwrap();
        assert!(ftp_profile_differences(&want, &want.clone()).is_empty());
        type Mutate = fn(&mut DecodedFtpProfile);
        let cases: [(&str, Mutate); 16] = [
            ("profile_name", |d| d.profile_name.push('x')),
            ("timestamps", |d| d.timestamps[1].push('x')),
            ("camera_network", |d| d.camera_network.prefix_len ^= 1),
            ("ssid_24ghz", |d| d.ssid_24ghz.push('x')),
            ("ssid24_kind", |d| d.ssid24_kind ^= 1),
            ("ssid24_lead", |d| d.ssid24_lead ^= 3),
            ("ssid_5ghz", |d| d.ssid_5ghz.push('x')),
            ("ssid5_kind", |d| d.ssid5_kind ^= 2),
            ("host", |d| d.host.push('x')),
            ("folder", |d| d.folder.push('x')),
            ("username", |d| d.username.push('x')),
            ("password", |d| d.password.push('x')),
            ("ftp_port", |d| d.ftp_port ^= 1),
            ("option_flags", |d| d.option_flags ^= 0x10),
            ("second_port", |d| d.second_port ^= 1),
            ("ipv6_fields", |d| d.ipv6_fields[2].push('x')),
        ];
        for (name, mutate) in cases {
            let mut got = want.clone();
            mutate(&mut got);
            assert_eq!(ftp_profile_differences(&want, &got), [name], "{name}");
        }
    }

    #[test]
    fn a_camera_that_reserialises_the_empty_folder_still_verifies() {
        // Seen on a Z 9: with option bit 0x04 clear the single 00 folder byte comes back as 01 00 00.
        let blob = hex::decode_hex(ftp_blob_317_hex());
        let mut longer = blob.clone();
        let at = 153; // the folder string, offset documented by `the_331_blob_is_the_317_blob_plus_the_folder_string_and_flag_byte`
        assert_eq!(longer[at], 0x00);
        longer.splice(at..=at, [0x01, 0x00, 0x00]);
        assert_ne!(longer.len(), blob.len());
        assert_eq!(decode_ftp_profile(&longer).unwrap(), decode_ftp_profile(&blob).unwrap());
        let (mut s, _t) = session(verified_script(1, 1, &blob, &longer), "Z 9");
        assert!(s.write_ftp_profile_blob_verified(0, 0, &blob, false).unwrap().verification.is_verified());
    }

    #[test]
    fn an_undecodable_read_back_and_a_failed_one_are_reported_apart_from_a_mismatch() {
        let blob = hex::decode_hex(ftp_blob_port22_hex());
        let mut garbled = blob.clone();
        garbled[0] = 0;
        let (mut s, _t) = session(verified_script(1, 1, &blob, &garbled), "Z 9");
        let out = s.write_ftp_profile_blob_verified(0, 0, &blob, false).unwrap();
        assert!(matches!(
            out.verification,
            FtpWriteVerification::ReadBackUndecodable { error: PtpUsbError::FtpBlobMalformed { what: "blob header", .. } }
        ));
        // The camera refuses to read slot 0 back (0x201F).
        let mut steps = blob_write_script(1);
        steps.extend([resp(RESP_OK), data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(1, 0, "Z_9_0000000")), resp(RESP_OK), resp(0x201F)]);
        let (mut s, _t) = session(steps, "Z 9");
        let out = s.write_ftp_profile_blob_verified(0, 0, &blob, false).unwrap();
        assert!(matches!(
            out.verification,
            FtpWriteVerification::ReadBackFailed { error: PtpUsbError::OperationFailed { op: OP_FTP_PROFILE_READ, .. } }
        ));
        // A transport fault in the read-back: the write still succeeded, so this is not an Err.
        let mut steps = blob_write_script(1);
        steps.push(Step::Err(rusb::Error::Timeout));
        let (mut s, _t) = session(steps, "Z 9");
        let out = s.write_ftp_profile_blob_verified(0, 0, &blob, false).unwrap();
        assert!(matches!(out.verification, FtpWriteVerification::ReadBackFailed { error: PtpUsbError::Usb(rusb::Error::Timeout) }));
        assert!(s.poisoned.is_some());
        // The camera now claims to hold nothing: a finding about the profile list, not a read failure.
        let (mut s, _t) = session(verified_script(1, 0, &blob, &blob), "Z 9");
        let out = s.write_ftp_profile_blob_verified(0, 0, &blob, false).unwrap();
        assert!(matches!(
            out.verification,
            FtpWriteVerification::Mismatch { count_after: 0, count_expected: 1, ref differing_fields } if differing_fields.is_empty()
        ));
    }

    #[test]
    fn a_refused_or_failed_write_is_an_err_and_attempts_no_read_back() {
        let blob = hex::decode_hex(ftp_blob_port22_hex());
        let mut steps = blob_write_script(1);
        steps[4] = resp(0x2002); // the write itself is refused
        let (mut s, t) = session(steps, "Z 9");
        let err = s.write_ftp_profile_blob_verified(0, 0, &blob, false).unwrap_err();
        assert!(matches!(err, PtpUsbError::OperationFailed { op: OP_FTP_PROFILE_WRITE, .. }), "{err:?}");
        assert_eq!(ops_sent(&t).last(), Some(&OP_FTP_PROFILE_WRITE));
        // Data sent, reply lost: the usual indeterminate error, and the poisoned session is not asked to read.
        let mut steps = blob_write_script(1);
        steps[4] = Step::Err(rusb::Error::Timeout);
        let (mut s, t) = session(steps, "Z 9");
        assert!(matches!(
            s.write_ftp_profile_blob_verified(0, 0, &blob, false),
            Err(PtpUsbError::FtpWriteIndeterminate { .. })
        ));
        assert_eq!(ops_sent(&t).last(), Some(&OP_FTP_PROFILE_WRITE));
        // A guard refusal is the same typed Err as for the plain write.
        let (mut s, _t) = session(blob_write_script(1), "Z 9");
        assert!(matches!(s.write_ftp_profile_blob_verified(5, 0, &blob, false), Err(PtpUsbError::FtpWriteRefused { .. })));
    }

    #[test]
    fn the_encoder_path_with_read_back_replaces_slot_zero_and_refuses_an_empty_camera() {
        let profile = valid_profile();
        let blob = encode_ftp_profile(&profile).unwrap();
        let (mut s, t) = session(verified_script(1, 1, &blob, &blob), "Z 9");
        let out = s.write_ftp_profile_verified(&profile, &FtpCameraNetwork::CAPTURED, false).unwrap();
        assert!(out.verification.is_verified(), "{:?}", out.verification);
        let sent = t.sent();
        let write = sent.iter().find(|(k, op, ..)| *k == CONTAINER_COMMAND && *op == OP_FTP_PROFILE_WRITE).unwrap();
        assert_eq!(write.3, [le(0), le(0)].concat());
        // Nothing stored: 0x90EE(0, 0) would be a write one past the end.
        let (mut s, t) = session(blob_write_script(0), "Z 9");
        let err = s.write_ftp_profile_verified(&profile, &FtpCameraNetwork::CAPTURED, false).unwrap_err();
        assert!(matches!(err, PtpUsbError::FtpWriteRefused { why: FtpWriteRefusal::ReplaceSlotNotStored { slot: 0, count: 0 }, .. }), "{err:?}");
        assert!(!ops_sent(&t).contains(&OP_FTP_PROFILE_WRITE));
        // An encoding error and the model gate still come before any I/O.
        let mut bad = profile.clone();
        bad.ssid_24ghz = "caf\u{e9}".into();
        let (mut s, t) = session(vec![], "Z 9");
        assert!(matches!(s.write_ftp_profile_verified(&bad, &FtpCameraNetwork::CAPTURED, false), Err(PtpUsbError::FtpFieldNotAscii { .. })));
        let (mut s, _t2) = session(vec![], "Z 5");
        assert!(matches!(s.write_ftp_profile_verified(&profile, &FtpCameraNetwork::CAPTURED, false), Err(PtpUsbError::UnconfirmedModel(_))));
        assert!(t.sent().is_empty());
    }

    // ── reading the active slot (b7d3#61) ────────────────────────────────

    #[test]
    fn the_default_read_never_sends_0x90ed_for_a_slot_the_camera_does_not_hold() {
        // Reported active slot s with count c: the read is sent only when s < c.
        for (count, slot, sent) in [(0u32, 0u32, false), (1, 0, true), (1, 1, false), (3, 2, true), (3, 3, false), (3, 4, false)] {
            let blob = hex::decode_hex(ftp_blob_317_hex());
            let (mut s, t) = session(
                vec![
                    resp(RESP_OK),
                    data_c(OP_FTP_PROFILE_INFO, &ftp_info_bytes(count, slot, "Z_9_0000000")),
                    resp(RESP_OK),
                    data_c(OP_FTP_PROFILE_READ, &blob),
                    resp(RESP_OK),
                ],
                "Z 9",
            );
            let out = s.read_ftp_profile().unwrap();
            assert_eq!(ops_sent(&t).contains(&OP_FTP_PROFILE_READ), sent, "count {count} slot {slot}");
            assert_eq!(out.profile.is_some(), sent, "count {count} slot {slot}");
            assert_eq!(out.warnings.is_empty(), sent, "count {count} slot {slot}: the skipped read is explained: {:?}", out.warnings);
        }
    }

    // ── as_encodable (b7d3#62, #92) ──────────────────────────────────────

    #[test]
    fn as_encodable_needs_every_condition_and_each_one_alone_refuses() {
        let base = decode_ftp_profile(&hex::decode_hex(reference_ftp_blob_hex())).unwrap();
        let p = base.as_encodable().expect("the reference blob is re-encodable");
        assert_eq!(encode_ftp_profile(&p).unwrap(), hex::decode_hex(reference_ftp_blob_hex()), "and it re-encodes byte for byte");
        type Mutate = fn(&mut DecodedFtpProfile);
        let breakers: [(&str, Mutate); 14] = [
            ("folder", |d| d.folder = "x".into()),
            ("option_flags", |d| d.option_flags = 0x0001),
            ("ftp_port", |d| d.ftp_port = 22),
            ("ssid5_kind", |d| d.ssid5_kind = 0x06),
            ("ssid24_kind", |d| d.ssid24_kind = 0x30),
            ("ssid24_lead", |d| d.ssid24_lead = 0x00),
            ("camera_network address", |d| d.camera_network.ipv4 = [10, 0, 0, 1]),
            ("camera_network prefix", |d| d.camera_network.prefix_len = 16),
            ("timestamp 1", |d| d.timestamps[0] = "20260101T000000".into()),
            ("ipv6 field 3", |d| d.ipv6_fields[2] = "::1".into()),
            ("non-ASCII 2.4 GHz SSID", |d| d.ssid_24ghz = "\u{e9}".into()),
            ("non-ASCII 5 GHz field", |d| d.ssid_5ghz = "\u{e9}".into()),
            ("non-ASCII username", |d| d.username = "\u{e9}".into()),
            ("non-ASCII password", |d| d.password = "\u{e9}".into()),
        ];
        for (name, mutate) in breakers {
            let mut d = base.clone();
            mutate(&mut d);
            assert!(d.as_encodable().is_none(), "{name} alone must make the profile not encodable");
        }
        // Differences the encoder does carry stay encodable: the second port and the text.
        let mut d = base.clone();
        d.second_port = 3128;
        d.host = "other.example".into();
        assert!(d.as_encodable().is_some());
    }

    // ── Debug does not print secrets (b7d3#65) ───────────────────────────

    #[test]
    fn debug_output_of_profile_types_hides_the_password_and_the_5ghz_field() {
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let decoded = decode_ftp_profile(&blob).unwrap();
        let (password, five) = (decoded.password.clone(), decoded.ssid_5ghz.clone());
        let readout = FtpProfileReadout {
            info: parse_ftp_profile_info(&ftp_info_bytes(1, 0, "Z_9_0000000")).unwrap(),
            profile: Some(FtpProfileBlob { raw: blob.clone(), decoded: Ok(decoded.clone()) }),
            warnings: vec![],
            reply_matched_command: true,
            replies: vec![],
        };
        let encodable = decoded.as_encodable().unwrap();
        for (what, shown) in [
            ("DecodedFtpProfile", format!("{decoded:?}")),
            ("FtpProfile", format!("{encodable:?}")),
            ("FtpProfileBlob", format!("{:?}", readout.profile.as_ref().unwrap())),
            ("FtpProfileReadout", format!("{readout:?}")),
            ("alternate form", format!("{decoded:#?}")),
        ] {
            assert!(!shown.contains(&password), "{what} printed the password: {shown}");
            assert!(!shown.contains(&five), "{what} printed the 5 GHz field: {shown}");
            assert!(!shown.contains(&hex_bytes(password.as_bytes())), "{what}");
            assert!(shown.contains("CLAUDE") && shown.contains("byte(s) hidden"), "{what} still shows the rest: {shown}");
        }
        let blob_shown = format!("{:?}", readout.profile.as_ref().unwrap());
        assert!(blob_shown.contains(&format!("{} byte(s) hidden", blob.len())), "{blob_shown}");
    }

    // ── diagnostics: no hash of a profile payload (b7d3#47) ──────────────

    #[test]
    fn a_profile_read_records_its_length_but_no_hash_while_other_replies_keep_theirs() {
        use crate::diag::Val;
        let blob = hex::decode_hex(reference_ftp_blob_hex());
        let (mut s, _t, d, _dir) = recorded(vec![data_c(OP_FTP_PROFILE_READ, &blob), resp(RESP_OK)], "Z 9");
        s.transact(OP_FTP_PROFILE_READ, &[0], None, TxidCheck::Unchecked).unwrap();
        let e = d.snapshot().pop().unwrap();
        assert_eq!(field(&e, "reply_len"), Some(&Val::Len(blob.len() as u64)));
        assert!(field(&e, "reply_hash").is_none(), "the blob holds the password");
        // A string property is not secret: its hash is still recorded.
        let (mut s, _t, d, _dir) = recorded(vec![resp(RESP_OK)], "Z 9");
        s.transact(OP_SET_DEVICE_PROP_VALUE, &[0xD090], Some(&[1, 0, 0]), TxidCheck::Enforce).unwrap();
        let e = d.snapshot().pop().unwrap();
        assert_eq!(field(&e, "data_hash"), Some(&Val::hash(&[1, 0, 0])));
    }
}

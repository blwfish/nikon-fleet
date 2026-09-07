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

use crate::sdk::{nikon_usb_devices, read_usb_string};

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

const CLAIM_RETRY_ATTEMPTS: u32 = 40;
const CLAIM_RETRY_DELAY: Duration = Duration::from_millis(50);
const BULK_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_ID: u32 = 1;

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
    #[error("OpenSession failed: {0}")]
    OpenSessionFailed(PtpResponseCode),
    #[error("PTP operation 0x{op:04x} failed: {code}")]
    OperationFailed { op: u16, code: PtpResponseCode },
    #[error("FTP profile field {field} is {len} bytes, exceeds the {max}-byte limit")]
    FtpFieldTooLong { field: &'static str, len: usize, max: usize },
    #[error("FTP profile field {field} contains non-ASCII characters, which this encoding does not support")]
    FtpFieldNotAscii { field: &'static str },
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

fn candidate_devices() -> Vec<(rusb::Device<GlobalContext>, String)> {
    nikon_usb_devices()
        .into_iter()
        .filter_map(|device| {
            let desc = device.device_descriptor().ok()?;
            let handle = device.open().ok()?;
            let serial = read_usb_string(&handle, desc.serial_number_string_index().unwrap_or(0));
            Some((device, serial))
        })
        .collect()
}

fn find_device(serial: Option<&str>) -> Result<rusb::Device<GlobalContext>, PtpUsbError> {
    let candidates = candidate_devices();
    match serial {
        Some(s) => candidates
            .into_iter()
            .find(|(_, ser)| ser == s)
            .map(|(d, _)| d)
            .ok_or_else(|| PtpUsbError::SerialNotFound(s.to_string())),
        None => match candidates.len() {
            0 => Err(PtpUsbError::NoDevice),
            1 => Ok(candidates.into_iter().next().unwrap().0),
            n => Err(PtpUsbError::AmbiguousDevice(n)),
        },
    }
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
    let kind = u16::from_le_bytes(raw[4..6].try_into().unwrap());
    let code = u16::from_le_bytes(raw[6..8].try_into().unwrap());
    let transaction_id = u32::from_le_bytes(raw[8..12].try_into().unwrap());
    let end = length.min(raw.len());
    let payload = raw[12..end].to_vec();
    Ok(Container { kind, code, transaction_id, payload })
}

fn decode_response_params(payload: &[u8]) -> Vec<u32> {
    payload
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect()
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
#[derive(Debug, Clone)]
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
}

impl PtpUsbSession {
    /// Find the Nikon camera (by serial if given, else the sole one
    /// attached), claim its PTP interface (racing macOS's own claimants),
    /// and open a PTP session.
    pub fn open(serial: Option<&str>) -> Result<Self, PtpUsbError> {
        let device = find_device(serial)?;
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
        };
        session.open_ptp_session()?;
        Ok(session)
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
        let mut buf = vec![0u8; 16384];
        let n = self.handle.read_bulk(self.ep_in, &mut buf, BULK_TIMEOUT)?;
        decode_container(&buf[..n])
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
        loop {
            let container = self.read_container()?;
            match container.kind {
                CONTAINER_DATA => data_in.extend(container.payload),
                CONTAINER_RESPONSE => {
                    let resp_params = decode_response_params(&container.payload);
                    self.transaction_id += 1;
                    return Ok((container.code, resp_params, data_in));
                }
                CONTAINER_EVENT => continue, // shouldn't arrive on the bulk pipe; ignore defensively
                _ => continue,
            }
        }
    }

    fn open_ptp_session(&mut self) -> Result<(), PtpUsbError> {
        let (code, _, _) = self.transact(OP_OPEN_SESSION, &[SESSION_ID], None)?;
        if code == RESP_SESSION_ALREADY_OPEN {
            // Leftover session from a prior claimant (typically macOS's own
            // ptpcamerad, which we just evicted) — close it and retry once.
            self.transaction_id = 0;
            let _ = self.transact(OP_CLOSE_SESSION, &[], None);
            self.transaction_id = 0;
            let (code, _, _) = self.transact(OP_OPEN_SESSION, &[SESSION_ID], None)?;
            if code != RESP_OK {
                return Err(PtpUsbError::OpenSessionFailed(PtpResponseCode(code)));
            }
        } else if code != RESP_OK {
            return Err(PtpUsbError::OpenSessionFailed(PtpResponseCode(code)));
        }
        self.session_open = true;
        Ok(())
    }

    /// Confirm the session/pipe is still responsive. Exposed mainly for
    /// tests/diagnostics — `read_vendor_property` doesn't need this itself.
    pub fn ping(&mut self) -> Result<(), PtpUsbError> {
        let (code, _, _) = self.transact(OP_GET_DEVICE_INFO, &[], None)?;
        if code != RESP_OK {
            return Err(PtpUsbError::OperationFailed { op: OP_GET_DEVICE_INFO, code: PtpResponseCode(code) });
        }
        Ok(())
    }

    /// Read a vendor property via the `0x943B` wrapper. `propcode` is the
    /// raw `0xD0xx`/`0x5xxx` property code (without the `0x10000` flag —
    /// this adds it). Returns the property's raw bytes on success.
    pub fn read_vendor_property(&mut self, propcode: u16) -> Result<Vec<u8>, PtpUsbError> {
        let param = 0x10000 | propcode as u32;
        let (code, _, data) = self.transact(OP_VENDOR_PROP_READ, &[param], None)?;
        if code != RESP_OK {
            return Err(PtpUsbError::OperationFailed { op: OP_VENDOR_PROP_READ, code: PtpResponseCode(code) });
        }
        Ok(data)
    }

    /// Write an FTP profile via `0x90EE`. Always sends the `0x90E8` setup
    /// call immediately before it — the first write in a fresh session
    /// doesn't strictly need this, but a second one hangs without it
    /// (confirmed 2026-09-06), and sending it unconditionally is simpler and
    /// safer than tracking write count.
    pub fn write_ftp_profile(&mut self, profile: &FtpProfile) -> Result<(), PtpUsbError> {
        let blob = encode_ftp_profile(profile)?;

        let (code, _, _) = self.transact(OP_FTP_SETUP, &[], Some(&FTP_SETUP_DATA))?;
        if code != RESP_OK {
            return Err(PtpUsbError::OperationFailed { op: OP_FTP_SETUP, code: PtpResponseCode(code) });
        }

        let (code, _, _) = self.transact(OP_FTP_PROFILE_WRITE, &[0, 0], Some(&blob))?;
        if code != RESP_OK {
            return Err(PtpUsbError::OperationFailed { op: OP_FTP_PROFILE_WRITE, code: PtpResponseCode(code) });
        }
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
/// wrapper for one-off writes (e.g. the CLI).
pub fn write_ftp_profile(serial: Option<&str>, profile: &FtpProfile) -> Result<(), PtpUsbError> {
    let mut session = PtpUsbSession::open(serial)?;
    session.write_ftp_profile(profile)
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
        assert_eq!(decode_response_params(&c.payload), vec![0x2A]);
    }

    #[test]
    fn decode_container_truncates_to_declared_length() {
        // Declared length is 12 (no payload) but the buffer has trailing
        // garbage (e.g. leftover bytes from a larger read) — must not leak
        // into the payload.
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
        assert_eq!(decode_response_params(&[]), Vec::<u32>::new());
    }

    #[test]
    fn decode_response_params_multiple() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());
        assert_eq!(decode_response_params(&payload), vec![1, 0xDEADBEEF]);
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
    fn encode_ftp_profile_port_is_little_endian() {
        let profile = valid_profile();
        let blob = encode_ftp_profile(&profile).unwrap();
        // Port section starts right after password's content in this profile.
        let port_tag_pos = blob.windows(2).rposition(|w| w == FTP_TAG_PORT_SECTION).unwrap();
        let port_bytes = &blob[port_tag_pos + 2..port_tag_pos + 4];
        assert_eq!(u16::from_le_bytes(port_bytes.try_into().unwrap()), profile.port);
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

    // ── find_device selection logic ──────────────────────────────────────
    // (candidate_devices() itself needs real USB hardware; the selection
    // logic on top of it doesn't, so it's tested directly against a fake
    // candidate list shape via a thin re-implementation of the match arms.)

    fn select<'a>(candidates: &'a [(u32, &'a str)], serial: Option<&str>) -> Result<u32, &'static str> {
        match serial {
            Some(s) => candidates.iter().find(|(_, ser)| *ser == s).map(|(id, _)| *id).ok_or("not found"),
            None => match candidates.len() {
                0 => Err("no device"),
                1 => Ok(candidates[0].0),
                _ => Err("ambiguous"),
            },
        }
    }

    #[test]
    fn select_by_serial_match() {
        let candidates = [(1, "AAA"), (2, "BBB")];
        assert_eq!(select(&candidates, Some("BBB")), Ok(2));
    }

    #[test]
    fn select_no_serial_single_candidate() {
        let candidates = [(1, "AAA")];
        assert_eq!(select(&candidates, None), Ok(1));
    }

    #[test]
    fn select_no_serial_multiple_candidates_is_ambiguous() {
        let candidates = [(1, "AAA"), (2, "BBB")];
        assert!(select(&candidates, None).is_err());
    }

    #[test]
    fn select_no_candidates_errors() {
        let candidates: [(u32, &str); 0] = [];
        assert!(select(&candidates, None).is_err());
    }
}

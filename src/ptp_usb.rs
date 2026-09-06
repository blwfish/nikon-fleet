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
//! correct wire format — see that doc for the full story, including two
//! wire-format bugs the original by-eye pcap reading introduced. Only the
//! read wrapper (`0x943B`) is wired up here so far; the write ops
//! (`0x9413`/`0x90EE`) still need their data-blob layouts fully decoded
//! before a general-purpose write API is safe to expose (see the doc's open
//! items) — reads carry no risk of corrupting camera state either way.
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

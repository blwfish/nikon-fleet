//! Target selection: which attached body a `--serial` names.
//!
//! There are two rules, both here, and no front end carries a copy of either.
//!
//! - [`select_by_usb_serial`] is the **strict** one, for tools that write to a
//!   camera on an agent's say-so (the MCP): a body is named by its **USB**
//!   serial, nothing else. SDK ids are assigned per session, so `id-1` can be
//!   a different body on the next call; a plan digest or restore point keyed
//!   to one would silently retarget (docs/mcp-plan.md §3, "A target camera is
//!   always named, by USB serial").
//! - [`select_by_cli_serial`] is the **CLI's** (and the GUIs'): it also takes
//!   what `discover` and stored snapshots print for a body with no readable
//!   USB serial, the `id-N` fallback ([`storable_serial`]), and the bare SDK
//!   id older `discover` output used. It never picks the first of several
//!   matches: when a USB serial and an SDK id name different bodies, it
//!   refuses.

use crate::sdk::{DeviceInfo, UsbCameraInfo};

/// Why a serial could not be resolved to exactly one body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetError {
    /// The requested value cannot be a USB serial: it is empty, or it is an
    /// `id-N` fallback (a snapshot stamped that way has no usable identity).
    NoUsbSerial { requested: String },
    /// No attached body reports this USB serial. A body whose USB serial
    /// could not be read can never be found.
    NotFound { requested: String },
    /// More than one attached body reports this USB serial.
    DuplicateSerial { requested: String, count: usize },
    /// (CLI rule only) The value is one attached body's USB serial and a different
    /// body's SDK id (`3` or `id-3`), so it does not say which body is meant.
    AmbiguousSerial { requested: String },
    /// (raw-USB commands) An `id-N` was given, but those commands see only the
    /// USB bus and cannot tell which attached body that SDK id is.
    FallbackUnresolvable { requested: String, attached: usize },
}

impl TargetError {
    /// Closed vocabulary for structured output (no free-text matching).
    pub fn tag(&self) -> &'static str {
        match self {
            TargetError::NoUsbSerial { .. } => "no_usb_serial",
            TargetError::NotFound { .. } => "not_found",
            TargetError::DuplicateSerial { .. } => "duplicate_serial",
            TargetError::AmbiguousSerial { .. } => "ambiguous_serial",
            TargetError::FallbackUnresolvable { .. } => "fallback_unresolvable",
        }
    }
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TargetError::NoUsbSerial { requested } => write!(
                f,
                "{requested:?} is not a USB serial (empty, or an id-N fallback from a camera \
                 whose USB serial was unreadable); name the body by its USB serial"
            ),
            TargetError::NotFound { requested } => {
                write!(f, "no attached camera has USB serial {requested:?}")
            }
            TargetError::DuplicateSerial { requested, count } => write!(
                f,
                "{count} attached cameras report USB serial {requested:?}; refusing to pick one"
            ),
            TargetError::AmbiguousSerial { requested } => write!(
                f,
                "{requested:?} is the USB serial of one attached camera and the SDK id of another, \
                 so it does not say which is meant; name the body by its USB serial (see `discover`)"
            ),
            TargetError::FallbackUnresolvable { requested, attached } => write!(
                f,
                "{requested:?} is an SDK id, and this command talks to the USB bus directly and cannot \
                 tell which of the {attached} attached camera(s) that is. Name the body by its real USB \
                 serial, or (when exactly one Nikon camera is attached and it has no readable USB serial) \
                 leave --serial out"
            ),
        }
    }
}

impl std::error::Error for TargetError {}

/// Whether `s` is the `id-N` fallback form (`id-` then one or more ASCII digits).
pub fn is_id_fallback(s: &str) -> bool {
    s.strip_prefix("id-")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// Reject values that cannot be a USB serial before looking at any camera.
/// Also the check for the serial stamped in a snapshot that will be used to
/// pick a target (`restore`).
pub fn require_usb_serial(s: &str) -> Result<&str, TargetError> {
    if s.is_empty() || is_id_fallback(s) {
        Err(TargetError::NoUsbSerial { requested: s.to_string() })
    } else {
        Ok(s)
    }
}

/// Pick the one body whose USB serial equals `serial`, exactly (no trimming,
/// no case folding). SDK ids and `id-N` forms never match.
///
/// A literal USB serial that happens to look like `id-5` is still matched by
/// exact comparison first; only an unmatched `id-N` request is reported as
/// `NoUsbSerial`, which is the more useful answer than `NotFound`.
pub fn select_by_usb_serial(
    enriched: Vec<(DeviceInfo, Option<UsbCameraInfo>)>,
    serial: &str,
) -> Result<(DeviceInfo, UsbCameraInfo), TargetError> {
    if serial.is_empty() {
        return Err(TargetError::NoUsbSerial { requested: String::new() });
    }
    let mut hits: Vec<(DeviceInfo, UsbCameraInfo)> = enriched
        .into_iter()
        .filter_map(|(dev, usb)| usb.map(|u| (dev, u)))
        .filter(|(_, u)| u.serial == serial)
        .collect();
    match hits.len() {
        0 => Err(match require_usb_serial(serial) {
            Err(e) => e,
            Ok(_) => TargetError::NotFound { requested: serial.to_string() },
        }),
        1 => Ok(hits.remove(0)),
        count => Err(TargetError::DuplicateSerial { requested: serial.to_string(), count }),
    }
}

/// The serial to record for a body: its real USB serial if one was read (and
/// non-empty: the USB layer returns `Some("")` for an unreadable descriptor,
/// which must NOT be taken as a serial), else the `id-{sdk id}` fallback.
/// Snapshots, references, `discover` and the GUIs all key a body by this one
/// value, so a snapshot is listed under the same serial it was taken from.
pub fn storable_serial(usb: &Option<UsbCameraInfo>, sdk_id: u32) -> String {
    usb.as_ref()
        .and_then(|u| (!u.serial.is_empty()).then(|| u.serial.clone()))
        .unwrap_or_else(|| format!("id-{sdk_id}"))
}

/// The firmware to record for a body: the USB descriptor's (`bcdDevice`) when
/// one was read, else the SDK's own version string. The SDK's is empty on every
/// body surveyed, so a body with no USB record has *unknown* firmware, reported
/// as `""` and never invented; the baseline gate fails closed on it
/// (`baseline::check_against`).
pub fn body_firmware(dev: &DeviceInfo, usb: &Option<UsbCameraInfo>) -> String {
    usb.as_ref()
        .map(|u| u.firmware.as_str())
        .filter(|f| !f.is_empty())
        .unwrap_or(dev.version.as_str())
        .to_string()
}

/// The CLI's rule for `--serial` (see the module doc). A request names a body
/// when it is that body's USB serial (exact, non-empty), or when it is the
/// body's SDK id as `N` or `id-N`:
///
/// - exactly one body named, by either form: that body;
/// - two bodies with that USB serial (or two with that SDK id): `DuplicateSerial`;
/// - one body's USB serial and another's SDK id: `AmbiguousSerial` (a body that
///   is both, say serial `"1"` with SDK id 1, is one body and not ambiguous);
/// - nobody: `NotFound`; an empty request: `NoUsbSerial`.
pub fn select_by_cli_serial(
    enriched: Vec<(DeviceInfo, Option<UsbCameraInfo>)>,
    serial: &str,
) -> Result<(DeviceInfo, Option<UsbCameraInfo>), TargetError> {
    if serial.is_empty() {
        return Err(TargetError::NoUsbSerial { requested: String::new() });
    }
    let by_usb: Vec<usize> = (0..enriched.len())
        .filter(|&i| enriched[i].1.as_ref().is_some_and(|u| u.serial == serial))
        .collect();
    let by_id: Vec<usize> = (0..enriched.len())
        .filter(|&i| {
            let id = enriched[i].0.id;
            serial == id.to_string() || serial == format!("id-{id}")
        })
        .filter(|i| !by_usb.contains(i))
        .collect();
    let chosen = match (by_usb.len(), by_id.len()) {
        (0, 0) => return Err(TargetError::NotFound { requested: serial.to_string() }),
        (1, 0) => by_usb[0],
        (0, 1) => by_id[0],
        (n, 0) | (0, n) => return Err(TargetError::DuplicateSerial { requested: serial.to_string(), count: n }),
        _ => return Err(TargetError::AmbiguousSerial { requested: serial.to_string() }),
    };
    Ok(enriched.into_iter().nth(chosen).expect("index came from this list"))
}

/// What to hand the raw-USB commands (`vendor-read`, `vendor-write-ftp`) as
/// their serial, given the Nikon cameras on the bus. Those commands use no SDK
/// session, so they match a serial against the USB descriptor only; the `id-N`
/// that `discover` prints for a body with no readable USB serial means nothing
/// to them.
///
/// - no serial: none (they then take the sole attached camera, or refuse);
/// - anything but an `id-N`: passed through (a real serial, or `usb-<bus>:<addr>`);
/// - an `id-N`: resolvable only when exactly one camera is attached and it has
///   no readable serial, in which case it is that camera and none is passed;
///   in every other case `FallbackUnresolvable`, never a guess.
pub fn raw_usb_serial_arg(serial: Option<&str>, attached: &[UsbCameraInfo]) -> Result<Option<String>, TargetError> {
    let Some(s) = serial else { return Ok(None) };
    if !is_id_fallback(s) {
        return Ok(Some(s.to_string()));
    }
    match attached {
        [only] if only.serial.is_empty() => Ok(None),
        _ => Err(TargetError::FallbackUnresolvable { requested: s.to_string(), attached: attached.len() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: u32) -> DeviceInfo {
        DeviceInfo { id, name: "Z 9".into(), available: true, connected_pid: 0, version: "5.31".into() }
    }
    fn usb(serial: &str) -> UsbCameraInfo {
        UsbCameraInfo { product_id: 0, serial: serial.into(), firmware: "5.31".into(), model: "Z 9".into() }
    }

    #[test]
    fn matches_usb_serial_and_returns_that_body() {
        let e = vec![(dev(1), Some(usb("AAA"))), (dev(2), Some(usb("BBB")))];
        let (d, u) = select_by_usb_serial(e, "BBB").unwrap();
        assert_eq!((d.id, u.serial.as_str()), (2, "BBB"));
    }

    #[test]
    fn bare_sdk_id_never_matches() {
        // The CLI's serial_matches would pick device 2 here.
        let e = vec![(dev(1), Some(usb("AAA"))), (dev(2), Some(usb("BBB")))];
        assert_eq!(
            select_by_usb_serial(e, "2"),
            Err(TargetError::NotFound { requested: "2".into() })
        );
    }

    #[test]
    fn numeric_usb_serial_wins_over_a_different_bodys_sdk_id() {
        // Body with SDK id 1 has USB serial "AAA"; body with SDK id 2 has the
        // USB serial "1". Asking for "1" must mean the USB serial.
        let e = vec![(dev(1), Some(usb("AAA"))), (dev(2), Some(usb("1")))];
        let (d, _) = select_by_usb_serial(e, "1").unwrap();
        assert_eq!(d.id, 2);
    }

    #[test]
    fn id_fallback_is_refused_even_when_that_sdk_id_exists() {
        let e = vec![(dev(2), Some(usb("BBB")))];
        assert_eq!(
            select_by_usb_serial(e, "id-2"),
            Err(TargetError::NoUsbSerial { requested: "id-2".into() })
        );
    }

    #[test]
    fn id_fallback_boundaries() {
        // `id-` + digits only. Pin both sides of the shape check.
        for (s, refused) in [
            ("id-0", true),
            ("id-12", true),
            ("id-", false),   // no digits: not the fallback form
            ("id-1a", false), // trailing junk: not the fallback form
            ("ID-1", false),  // case-sensitive, like the producer
            (" id-1", false),
            ("id--1", false),
        ] {
            let r = select_by_usb_serial(vec![(dev(1), Some(usb("AAA")))], s);
            let want = if refused {
                TargetError::NoUsbSerial { requested: s.into() }
            } else {
                TargetError::NotFound { requested: s.into() }
            };
            assert_eq!(r, Err(want), "input {s:?}");
        }
    }

    #[test]
    fn literal_usb_serial_that_looks_like_the_fallback_still_matches_exactly() {
        let e = vec![(dev(1), Some(usb("id-5")))];
        let (d, _) = select_by_usb_serial(e, "id-5").unwrap();
        assert_eq!(d.id, 1);
    }

    #[test]
    fn empty_request_never_matches_a_body_with_an_empty_serial() {
        // The classic empty == empty hole: an unreadable descriptor can yield
        // Some("") (see snapshot_serial in main.rs).
        let e = vec![(dev(1), Some(usb("")))];
        assert_eq!(
            select_by_usb_serial(e, ""),
            Err(TargetError::NoUsbSerial { requested: String::new() })
        );
    }

    #[test]
    fn body_without_usb_info_is_never_found() {
        let e = vec![(dev(1), None), (dev(2), None)];
        assert_eq!(
            select_by_usb_serial(e, "AAA"),
            Err(TargetError::NotFound { requested: "AAA".into() })
        );
    }

    #[test]
    fn exact_comparison_only() {
        for s in ["aaa", " AAA", "AAA ", "AAA\n"] {
            let e = vec![(dev(1), Some(usb("AAA")))];
            assert_eq!(
                select_by_usb_serial(e, s),
                Err(TargetError::NotFound { requested: s.into() }),
                "input {s:?}"
            );
        }
    }

    #[test]
    fn duplicate_serial_is_refused_not_first_wins() {
        let e = vec![(dev(1), Some(usb("AAA"))), (dev(2), Some(usb("AAA"))), (dev(3), Some(usb("BBB")))];
        assert_eq!(
            select_by_usb_serial(e, "AAA"),
            Err(TargetError::DuplicateSerial { requested: "AAA".into(), count: 2 })
        );
    }

    #[test]
    fn no_devices_is_not_found() {
        assert_eq!(
            select_by_usb_serial(vec![], "AAA"),
            Err(TargetError::NotFound { requested: "AAA".into() })
        );
    }

    #[test]
    fn require_usb_serial_accepts_real_and_rejects_empty_and_fallback() {
        assert_eq!(require_usb_serial("0000003023668"), Ok("0000003023668"));
        assert!(require_usb_serial("").is_err());
        assert!(require_usb_serial("id-7").is_err());
        assert_eq!(require_usb_serial("7"), Ok("7")); // a bare number is a plausible serial
    }

    #[test]
    fn tags_are_distinct_and_stable() {
        let tags = [
            TargetError::NoUsbSerial { requested: String::new() }.tag(),
            TargetError::NotFound { requested: String::new() }.tag(),
            TargetError::DuplicateSerial { requested: String::new(), count: 2 }.tag(),
            TargetError::AmbiguousSerial { requested: String::new() }.tag(),
            TargetError::FallbackUnresolvable { requested: String::new(), attached: 0 }.tag(),
        ];
        assert_eq!(
            tags,
            ["no_usb_serial", "not_found", "duplicate_serial", "ambiguous_serial", "fallback_unresolvable"]
        );
    }

    // ── the CLI's rule: one rule for every caller ────────────────────────

    type Enriched = Vec<(DeviceInfo, Option<UsbCameraInfo>)>;

    fn cli(e: Enriched, s: &str) -> Result<u32, TargetError> {
        select_by_cli_serial(e, s).map(|(d, _)| d.id)
    }

    #[test]
    fn a_usb_serial_that_is_another_bodys_sdk_id_is_refused_not_first_wins() {
        // The review's case: body A (SDK id 1, serial AAA) and body B (SDK id 2, USB serial "1").
        // The CLI used to take the first match, A; the strict rule takes B. The CLI now refuses.
        let e = || vec![(dev(1), Some(usb("AAA"))), (dev(2), Some(usb("1")))];
        assert_eq!(cli(e(), "1"), Err(TargetError::AmbiguousSerial { requested: "1".into() }));
        // Both orders: the answer does not depend on enumeration order.
        let swapped = vec![(dev(2), Some(usb("1"))), (dev(1), Some(usb("AAA")))];
        assert_eq!(cli(swapped, "1"), Err(TargetError::AmbiguousSerial { requested: "1".into() }));
        // The strict rule (MCP) is unchanged and takes the USB serial.
        assert_eq!(select_by_usb_serial(e(), "1").map(|(d, _)| d.id), Ok(2));
        // Unambiguous names still work either way.
        assert_eq!(cli(e(), "AAA"), Ok(1));
        assert_eq!(cli(e(), "2"), Ok(2), "SDK id 2: no body has USB serial \"2\"");
        assert_eq!(cli(e(), "id-2"), Ok(2));
    }

    #[test]
    fn a_body_that_is_both_the_usb_serial_and_the_sdk_id_is_one_body_not_ambiguous() {
        let e = vec![(dev(1), Some(usb("1"))), (dev(2), Some(usb("BBB")))];
        assert_eq!(cli(e, "1"), Ok(1));
    }

    #[test]
    fn the_id_fallback_form_finds_a_body_with_no_readable_usb_serial() {
        // What `discover` prints and a snapshot stores for such a body.
        let e = vec![(dev(1), Some(usb("AAA"))), (dev(2), Some(usb(""))), (dev(3), None)];
        assert_eq!(cli(e.clone(), "id-2"), Ok(2));
        assert_eq!(cli(e.clone(), "id-3"), Ok(3));
        assert_eq!(cli(e, "2"), Ok(2), "the bare SDK id older discover output printed");
    }

    #[test]
    fn a_literal_usb_serial_that_looks_like_an_id_fallback_collides_with_that_sdk_id() {
        let e = vec![(dev(5), Some(usb("AAA"))), (dev(6), Some(usb("id-5")))];
        assert_eq!(cli(e.clone(), "id-5"), Err(TargetError::AmbiguousSerial { requested: "id-5".into() }));
        assert_eq!(select_by_usb_serial(e, "id-5").map(|(d, _)| d.id), Ok(6));
    }

    #[test]
    fn cli_rule_boundaries_empty_missing_duplicate_exact() {
        let e = vec![(dev(1), Some(usb(""))), (dev(2), Some(usb("DUP"))), (dev(3), Some(usb("DUP")))];
        assert_eq!(cli(e.clone(), ""), Err(TargetError::NoUsbSerial { requested: String::new() }));
        assert_eq!(cli(e.clone(), "DUP"), Err(TargetError::DuplicateSerial { requested: "DUP".into(), count: 2 }));
        assert_eq!(cli(e.clone(), "NOPE"), Err(TargetError::NotFound { requested: "NOPE".into() }));
        for near in ["01", "+1", "id-01", "ID-1", " 1", "1 ", "id-"] {
            assert_eq!(cli(e.clone(), near), Err(TargetError::NotFound { requested: near.into() }), "{near:?}");
        }
        assert_eq!(cli(vec![], "1"), Err(TargetError::NotFound { requested: "1".into() }));
    }

    #[test]
    fn two_bodies_with_one_sdk_id_are_a_duplicate_not_a_pick() {
        // SDK ids are unique per session; if that ever failed, the CLI must not take the first.
        let e = vec![(dev(1), None), (dev(1), None)];
        assert_eq!(cli(e, "id-1"), Err(TargetError::DuplicateSerial { requested: "id-1".into(), count: 2 }));
    }

    // ── identity recorded for a body ─────────────────────────────────────

    #[test]
    fn storable_serial_is_the_usb_serial_or_the_id_fallback() {
        assert_eq!(storable_serial(&Some(usb("ABC123")), 7), "ABC123");
        assert_eq!(storable_serial(&None, 7), "id-7");
        // An unreadable descriptor (Some("")) is not a serial.
        assert_eq!(storable_serial(&Some(usb("")), 7), "id-7");
        // And what it stores is always something the CLI rule finds again.
        for u in [Some(usb("ABC123")), Some(usb("")), None] {
            let stored = storable_serial(&u, 7);
            assert_eq!(cli(vec![(dev(7), u)], &stored), Ok(7), "{stored}");
        }
    }

    #[test]
    fn body_firmware_never_invents_one() {
        assert_eq!(body_firmware(&dev(1), &Some(usb("A"))), "5.31");
        // No USB record: the SDK's own string, which is empty on every surveyed body.
        let mut d = dev(1);
        d.version = String::new();
        assert_eq!(body_firmware(&d, &None), "");
        d.version = "9.99".into();
        assert_eq!(body_firmware(&d, &None), "9.99");
        // A USB record whose firmware is empty falls back the same way, not to "".
        let mut u = usb("A");
        u.firmware = String::new();
        assert_eq!(body_firmware(&d, &Some(u)), "9.99");
    }

    // ── the raw-USB commands' serial ─────────────────────────────────────

    #[test]
    fn raw_usb_serial_passes_real_and_usb_address_serials_through() {
        let on_bus = [usb("AAA"), usb("BBB")];
        assert_eq!(raw_usb_serial_arg(None, &on_bus), Ok(None));
        assert_eq!(raw_usb_serial_arg(Some("AAA"), &on_bus), Ok(Some("AAA".into())));
        assert_eq!(raw_usb_serial_arg(Some("usb-20:5"), &on_bus), Ok(Some("usb-20:5".into())));
        // Not the fallback shape, so not reinterpreted.
        assert_eq!(raw_usb_serial_arg(Some("id-"), &on_bus), Ok(Some("id-".into())));
        assert_eq!(raw_usb_serial_arg(Some("7"), &on_bus), Ok(Some("7".into())));
    }

    #[test]
    fn an_id_n_resolves_only_for_the_sole_serial_less_body() {
        // exactly one camera on the bus and it has no readable serial: it is that one.
        assert_eq!(raw_usb_serial_arg(Some("id-1"), &[usb("")]), Ok(None));
        let refused = |on_bus: &[UsbCameraInfo]| {
            assert_eq!(
                raw_usb_serial_arg(Some("id-1"), on_bus),
                Err(TargetError::FallbackUnresolvable { requested: "id-1".into(), attached: on_bus.len() })
            );
        };
        refused(&[]); // nothing attached
        refused(&[usb("AAA")]); // the one body has a serial, so id-1 is a different body
        refused(&[usb(""), usb("")]); // two serial-less bodies: no way to tell which
        refused(&[usb(""), usb("AAA")]); // serial-less among others: cannot tell which bus slot
    }
}

//! Strict target selection: name a body by its **USB** serial, nothing else.
//!
//! The CLI's `serial_matches` (`src/main.rs`) also accepts the bare SDK id
//! and the `id-N` fallback that `snapshot_serial` stamps when no USB serial
//! was readable. SDK ids are assigned per session, so `id-1` can be a
//! different body on the next call; a plan digest or restore point keyed to
//! one would silently retarget. Tools that write to a camera (the MCP) use
//! this matcher instead; the CLI keeps its looser one for the cases where it
//! has always worked (docs/mcp-plan.md §3, "A target camera is always named,
//! by USB serial").

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
}

impl TargetError {
    /// Closed vocabulary for structured output (no free-text matching).
    pub fn tag(&self) -> &'static str {
        match self {
            TargetError::NoUsbSerial { .. } => "no_usb_serial",
            TargetError::NotFound { .. } => "not_found",
            TargetError::DuplicateSerial { .. } => "duplicate_serial",
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
        }
    }
}

impl std::error::Error for TargetError {}

/// Whether `s` is the `id-N` fallback form (`id-` then one or more ASCII digits).
fn is_id_fallback(s: &str) -> bool {
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
        ];
        assert_eq!(tags, ["no_usb_serial", "not_found", "duplicate_serial"]);
    }
}

//! Which cameras are attached, from the USB descriptors alone (no SDK session,
//! no lock, no hang risk), and the same-model ambiguity rule.
//!
//! ## Model names
//!
//! A snapshot keys a camera by the SDK's model name (`"Z 9"`, `"Z6_3"`). The
//! USB product string minus its `"NIKON DSC "` prefix is the same string for
//! every body surveyed (`docs/firmware-archive-spec.md` §2: `Z 5`, `Z6_3`,
//! `Z 9`; the Z6_3 spelling is the odd one, and it matches), so the mapping is
//! the identity, but only for those verified names. A model outside the table
//! is reported `known_model: false` rather than passed through as if it were an
//! SDK name, and it cannot be a target. (The MCP plan expected the two spellings
//! to differ; the survey says they do not.)
//!
//! ## Same-model ambiguity
//!
//! For two bodies of one model, `pair_devices` attaches serial and firmware to
//! SDK devices by enumeration order; nothing in the SDK's device info can
//! confirm it (`warn_if_ambiguous_pairing` in `main.rs`). A capture could stamp
//! the wrong serial, an apply could write the wrong physical body. So every
//! tool that acts on a camera refuses when more than one attached body shares
//! the target's model, and `list_cameras` flags them.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::ops::baseline::baseline_for;
use crate::ops::target::{body_firmware, storable_serial};
use crate::sdk::{DeviceInfo, UsbCameraInfo, UsbScan, UsbUnreadable, usb_camera_scan};

/// Model names whose USB spelling is verified to equal the SDK/schema spelling.
pub const KNOWN_MODELS: &[&str] = &["Z 5", "Z6_3", "Z 9"];

/// Whether `usb_model` (product string already stripped of its prefix) is a
/// verified SDK model name. Exact: no case folding, no whitespace trimming,
/// no slug form.
pub fn sdk_model_name(usb_model: &str) -> Option<&'static str> {
    KNOWN_MODELS.iter().copied().find(|m| *m == usb_model)
}

/// One attached body as the USB descriptors report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsbCamera {
    pub model: String,
    pub serial: String,
    pub firmware: String,
    /// The model is in [`KNOWN_MODELS`]; if not, it cannot be a target.
    pub known_model: bool,
    /// The build ships a capture baseline for this model. A known model with no
    /// baseline (the Z 5) can be read and captured, but every apply refuses it
    /// (`no_baseline`): there is nothing to judge a restore point or read-back by.
    pub baseline: bool,
    /// The USB serial descriptor could be read (a body without one cannot be
    /// named, see `ops::target`).
    pub serial_readable: bool,
    /// Another attached body has the same model.
    pub pairing_ambiguous: bool,
}

/// Models that appear more than once, with how many times.
pub fn duplicate_models<'a>(models: impl IntoIterator<Item = &'a str>) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for m in models {
        *counts.entry(m.to_string()).or_insert(0) += 1;
    }
    counts.retain(|_, n| *n > 1);
    counts
}

/// Describe a USB listing, in the order given. Pure.
pub fn describe(usb: &[UsbCameraInfo]) -> Vec<UsbCamera> {
    let dups = duplicate_models(usb.iter().map(|u| u.model.as_str()));
    usb.iter()
        .map(|u| UsbCamera {
            model: u.model.clone(),
            serial: u.serial.clone(),
            firmware: u.firmware.clone(),
            known_model: sdk_model_name(&u.model).is_some(),
            baseline: baseline_for(&u.model).is_some(),
            serial_readable: !u.serial.is_empty(),
            pairing_ambiguous: dups.contains_key(&u.model),
        })
        .collect()
}

/// The attached cameras, from USB alone.
pub fn list_usb() -> Vec<UsbCamera> {
    describe(&usb_camera_scan().cameras)
}

/// One body as `nikon-fleet discover --json` reports it: the SDK's device paired with its USB
/// record. `serial` is the identity every stored snapshot and reference uses
/// ([`storable_serial`]); `sdk_id` and `usb_serial` are the two things it is made from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveredCamera {
    pub model: String,
    /// The USB serial, or `id-{sdk_id}` when none could be read. Never the bare SDK id.
    pub serial: String,
    /// USB `bcdDevice` firmware, else the SDK's own (empty on every surveyed body).
    pub firmware: String,
    /// This session's SDK device id; not stable across sessions.
    pub sdk_id: u32,
    /// The USB `iSerialNumber`; `null` when there is no USB record or it was empty.
    pub usb_serial: Option<String>,
    /// The model is in [`KNOWN_MODELS`].
    pub known_model: bool,
    /// The build ships a capture baseline for this model (see [`UsbCamera::baseline`]).
    pub baseline: bool,
}

/// Describe one paired SDK device. Pure.
pub fn describe_device(dev: &DeviceInfo, usb: &Option<UsbCameraInfo>) -> DiscoveredCamera {
    DiscoveredCamera {
        model: dev.name.clone(),
        serial: storable_serial(usb, dev.id),
        firmware: body_firmware(dev, usb),
        sdk_id: dev.id,
        usb_serial: usb.as_ref().map(|u| u.serial.clone()).filter(|s| !s.is_empty()),
        known_model: sdk_model_name(&dev.name).is_some(),
        baseline: baseline_for(&dev.name).is_some(),
    }
}

/// `list_cameras`: the bodies read, the ones that could not be, and whether the
/// list can be taken as everything that is attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CameraList {
    pub cameras: Vec<UsbCamera>,
    /// Nikon devices that are attached but whose identity could not be read.
    pub unreadable: Vec<UsbUnreadable>,
    /// The USB bus could not be enumerated at all; `cameras` being empty then
    /// says nothing about what is attached.
    pub enumeration_error: Option<String>,
    /// No device was dropped and enumeration worked.
    pub complete: bool,
}

/// Describe one USB pass. Pure.
pub fn describe_scan(scan: &UsbScan) -> CameraList {
    CameraList {
        cameras: describe(&scan.cameras),
        unreadable: scan.unreadable.clone(),
        enumeration_error: scan.enumeration_error.clone(),
        complete: scan.unreadable.is_empty() && scan.enumeration_error.is_none(),
    }
}

/// Scan the bus and describe it. No SDK session, no lock, no reset.
pub fn list_cameras() -> CameraList {
    describe_scan(&usb_camera_scan())
}

/// More than one attached body shares the target's model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousPairing {
    pub model: String,
    pub count: usize,
}

impl AmbiguousPairing {
    pub fn tag(&self) -> &'static str {
        "ambiguous_pairing"
    }
}

impl std::fmt::Display for AmbiguousPairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {:?} bodies are attached; serial and firmware are paired to SDK devices by enumeration \
             order, which cannot be verified, so the target could be the wrong physical body. \
             Attach one at a time.",
            self.count, self.model
        )
    }
}

impl std::error::Error for AmbiguousPairing {}

/// Refuse when more than one SDK device has `target_model`. Other models'
/// duplicates do not matter.
pub fn require_unambiguous(
    enriched: &[(DeviceInfo, Option<UsbCameraInfo>)],
    target_model: &str,
) -> Result<(), AmbiguousPairing> {
    let count = enriched.iter().filter(|(d, _)| d.name == target_model).count();
    if count > 1 {
        Err(AmbiguousPairing { model: target_model.to_string(), count })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usb(model: &str, serial: &str) -> UsbCameraInfo {
        UsbCameraInfo { product_id: 0, serial: serial.into(), firmware: "5.31".into(), model: model.into() }
    }

    fn dev(id: u32, name: &str) -> (DeviceInfo, Option<UsbCameraInfo>) {
        (DeviceInfo { id, name: name.into(), available: true, connected_pid: 0, version: String::new() }, None)
    }

    // ── model mapping ────────────────────────────────────────────────────

    #[test]
    fn the_known_models_are_exactly_the_surveyed_spellings() {
        // docs/firmware-archive-spec.md §2 / §3.1; note Z6_3 has no space.
        assert_eq!(KNOWN_MODELS, ["Z 5", "Z6_3", "Z 9"]);
        for m in KNOWN_MODELS {
            assert_eq!(sdk_model_name(m), Some(*m), "identity for verified names");
        }
    }

    #[test]
    fn mapping_is_exact_not_normalised() {
        for wrong in ["z 9", "Z9", "Z_9", "Z 9 ", " Z 9", "NIKON DSC Z 9", "Z 6III", "Z6III", "Z 6_3", "", "Z 8"] {
            assert_eq!(sdk_model_name(wrong), None, "{wrong:?}");
        }
    }

    #[test]
    fn known_models_all_exist_as_sections_of_the_real_schema() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk-runtime/MaidLayer.config");
        let Ok(cfg) = crate::maid_layer::MaidLayerConfig::parse_file(&path) else { return };
        let sections = cfg.known_models();
        for m in KNOWN_MODELS {
            assert!(sections.iter().any(|s| s == m), "{m} is not a model section in MaidLayer.config");
        }
    }

    #[test]
    fn known_models_all_have_a_shipped_baseline_or_are_the_documented_exception() {
        // Z 5 has no baseline yet (docs/mcp-plan.md §5.5); the other two do.
        for m in KNOWN_MODELS {
            let has = crate::ops::baseline::baseline_for(m).is_some();
            assert_eq!(has, *m != "Z 5", "{m}");
        }
    }

    // ── describe ─────────────────────────────────────────────────────────

    #[test]
    fn a_single_known_body_is_unambiguous_and_fully_described() {
        let d = describe(&[usb("Z 9", "0000003023668")]);
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!([{"model": "Z 9", "serial": "0000003023668", "firmware": "5.31",
                    "known_model": true, "baseline": true, "serial_readable": true,
                    "pairing_ambiguous": false}])
        );
    }

    #[test]
    fn two_bodies_of_one_model_are_both_flagged_and_others_are_not() {
        let d = describe(&[usb("Z 9", "A"), usb("Z6_3", "B"), usb("Z 9", "C"), usb("Z 5", "D")]);
        let flags: Vec<(&str, bool)> = d.iter().map(|c| (c.serial.as_str(), c.pairing_ambiguous)).collect();
        assert_eq!(flags, [("A", true), ("B", false), ("C", true), ("D", false)], "order preserved");
    }

    #[test]
    fn three_of_a_model_and_two_of_another() {
        let d = describe(&[usb("Z 9", "1"), usb("Z 9", "2"), usb("Z 9", "3"), usb("Z6_3", "4"), usb("Z6_3", "5")]);
        assert!(d.iter().all(|c| c.pairing_ambiguous));
    }

    #[test]
    fn unknown_model_and_missing_serial_are_flagged_not_hidden_or_defaulted() {
        let d = describe(&[usb("Z 8", "X"), usb("Z 9", "")]);
        assert_eq!((d[0].known_model, d[0].serial_readable), (false, true));
        assert_eq!((d[1].known_model, d[1].serial_readable), (true, false));
        assert_eq!(d[1].serial, "", "reported as read, not replaced by an id-N");
    }

    #[test]
    fn a_known_model_without_a_baseline_is_visible_as_such() {
        // The Z 5 is a known model (it can be named and captured) with no baseline: every apply
        // refuses it, and the listing is where that must show.
        let d = describe(&[usb("Z 5", "S5"), usb("Z 9", "S9"), usb("Z6_3", "S6"), usb("Z 8", "S8")]);
        let got: Vec<(&str, bool, bool)> = d.iter().map(|c| (c.model.as_str(), c.known_model, c.baseline)).collect();
        assert_eq!(
            got,
            [("Z 5", true, false), ("Z 9", true, true), ("Z6_3", true, true), ("Z 8", false, false)]
        );
    }

    #[test]
    fn discover_rows_carry_the_storable_serial_and_both_its_parts() {
        let row = |dev: DeviceInfo, usb: Option<UsbCameraInfo>| serde_json::to_value(describe_device(&dev, &usb)).unwrap();
        // A body with a USB serial: serial is it, usb_serial repeats it, sdk_id is separate.
        assert_eq!(
            row(dev(3, "Z 9").0, Some(usb("Z 9", "0000003023668"))),
            json!({"model": "Z 9", "serial": "0000003023668", "firmware": "5.31", "sdk_id": 3,
                   "usb_serial": "0000003023668", "known_model": true, "baseline": true})
        );
        // No USB record at all: the id-N fallback (NOT the bare SDK id), no usb_serial, unknown firmware.
        assert_eq!(
            row(dev(2, "Z 5").0, None),
            json!({"model": "Z 5", "serial": "id-2", "firmware": "", "sdk_id": 2,
                   "usb_serial": null, "known_model": true, "baseline": false})
        );
        // An unreadable descriptor (Some("")) is no serial either; the firmware it did read is kept.
        let v = row(dev(4, "Z6_3").0, Some(usb("Z6_3", "")));
        assert_eq!((v["serial"].as_str(), v["usb_serial"].clone(), v["firmware"].as_str()), (Some("id-4"), json!(null), Some("5.31")));
        // An unsurveyed model: reported, flagged, and without a baseline.
        let v = row(dev(1, "Z 8").0, Some(usb("Z 8", "S")));
        assert_eq!((v["known_model"].clone(), v["baseline"].clone()), (json!(false), json!(false)));
    }

    #[test]
    fn nothing_attached_is_an_empty_list() {
        assert!(describe(&[]).is_empty());
    }

    #[test]
    fn duplicate_models_counts_only_repeats() {
        let d = duplicate_models(["Z 9", "Z6_3", "Z 9", "Z 9"]);
        assert_eq!(d, BTreeMap::from([("Z 9".to_string(), 3)]));
        assert!(duplicate_models(["Z 9", "Z6_3"]).is_empty());
        assert!(duplicate_models([]).is_empty());
    }

    // ── the refusal ──────────────────────────────────────────────────────

    #[test]
    fn require_unambiguous_boundaries() {
        // zero, one: fine; two, three: refused with the count.
        assert!(require_unambiguous(&[], "Z 9").is_ok());
        assert!(require_unambiguous(&[dev(1, "Z 9")], "Z 9").is_ok());
        let two = require_unambiguous(&[dev(1, "Z 9"), dev(2, "Z 9")], "Z 9").unwrap_err();
        assert_eq!(two, AmbiguousPairing { model: "Z 9".into(), count: 2 });
        let three = require_unambiguous(&[dev(1, "Z 9"), dev(2, "Z 9"), dev(3, "Z 9")], "Z 9").unwrap_err();
        assert_eq!(three.count, 3);
    }

    #[test]
    fn another_models_duplicates_do_not_block_the_target() {
        let list = [dev(1, "Z6_3"), dev(2, "Z 9"), dev(3, "Z 9")];
        assert!(require_unambiguous(&list, "Z6_3").is_ok());
        assert!(require_unambiguous(&list, "Z 9").is_err());
        assert!(require_unambiguous(&list, "Z 5").is_ok(), "a model with no attached body has nothing to confuse");
    }

    #[test]
    fn the_refusal_is_typed_and_explains_itself() {
        let e = AmbiguousPairing { model: "Z 9".into(), count: 2 };
        assert_eq!(e.tag(), "ambiguous_pairing");
        let s = e.to_string();
        assert!(s.contains("2") && s.contains("Z 9") && s.contains("one at a time"), "{s}");
    }

    // ── list_cameras ─────────────────────────────────────────────────────

    use crate::sdk::UsbStage;

    fn unreadable() -> UsbUnreadable {
        UsbUnreadable { bus: 1, address: 4, stage: UsbStage::Open, error: "Access denied".into() }
    }

    #[test]
    fn a_clean_scan_is_complete() {
        let l = describe_scan(&UsbScan { cameras: vec![usb("Z 9", "S1")], ..UsbScan::default() });
        assert!(l.complete && l.unreadable.is_empty() && l.enumeration_error.is_none());
        assert_eq!(l.cameras.len(), 1);
    }

    #[test]
    fn nothing_attached_is_complete_and_empty() {
        let l = describe_scan(&UsbScan::default());
        assert!(l.complete && l.cameras.is_empty());
    }

    #[test]
    fn a_body_that_could_not_be_read_makes_the_list_incomplete_and_is_named() {
        let l = describe_scan(&UsbScan { cameras: vec![usb("Z 9", "S1")], unreadable: vec![unreadable()], enumeration_error: None });
        assert!(!l.complete);
        assert_eq!(l.cameras.len(), 1, "the readable one is still listed");
        assert_eq!(l.unreadable, vec![unreadable()]);
    }

    #[test]
    fn a_failure_to_enumerate_is_not_reported_as_no_cameras() {
        let l = describe_scan(&UsbScan { enumeration_error: Some("no libusb".into()), ..UsbScan::default() });
        assert!(!l.complete);
        assert!(l.cameras.is_empty());
        assert_eq!(l.enumeration_error.as_deref(), Some("no libusb"));
    }

    #[test]
    fn the_list_serialises_with_closed_stage_tags() {
        let l = describe_scan(&UsbScan { unreadable: vec![unreadable(), UsbUnreadable { stage: UsbStage::Descriptor, ..unreadable() }], ..UsbScan::default() });
        let v = serde_json::to_value(&l).unwrap();
        assert_eq!(v["unreadable"][0]["stage"], json!("open"));
        assert_eq!(v["unreadable"][1]["stage"], json!("descriptor"));
        assert_eq!(v["complete"], json!(false));
    }

    #[test]
    fn same_model_bodies_in_a_scan_are_flagged_and_unknown_models_are_not_passed_through() {
        let l = describe_scan(&UsbScan { cameras: vec![usb("Z 9", "A"), usb("Z 9", "B"), usb("Z 99", "C")], ..UsbScan::default() });
        assert!(l.cameras[0].pairing_ambiguous && l.cameras[1].pairing_ambiguous && !l.cameras[2].pairing_ambiguous);
        assert!(!l.cameras[2].known_model);
    }
}

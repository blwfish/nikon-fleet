//! `gui/fixtures/discover.json` is what the Python GUI's tests feed its `discover` parser. It is
//! written by hand because `discover` needs a camera, so it can drift from what the CLI prints.
//! This pins it to the real serialisation (full-review nikon-fleet-20261028-e5a3, cold review of
//! the Python tests): same keys, same JSON types, for every camera in the fixture.

use nikon_fleet::ops::cameras::DiscoveredCamera;
use serde_json::Value;

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[test]
fn the_python_discover_fixture_has_the_keys_and_types_the_cli_prints() {
    let real = |usb_serial: Option<&str>| {
        serde_json::to_value(DiscoveredCamera {
            model: "Z 9".into(),
            serial: "x".into(),
            firmware: "5.31".into(),
            sdk_id: 1,
            usb_serial: usb_serial.map(String::from),
            known_model: true,
            baseline: true,
        })
        .expect("serialises")
    };
    let with_serial = real(Some("0000003023668"));
    let without = real(None);

    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/gui/fixtures/discover.json")).expect("fixture");
    let fixture: Value = serde_json::from_str(&text).expect("fixture is JSON");
    let cameras = fixture["cameras"].as_array().expect("a cameras list");
    assert!(!cameras.is_empty());
    let mut saw_null_usb_serial = false;
    for cam in cameras {
        let sample = if cam["usb_serial"].is_null() { saw_null_usb_serial = true; &without } else { &with_serial };
        let mut got: Vec<_> = cam.as_object().expect("an object").keys().cloned().collect();
        let mut want: Vec<_> = sample.as_object().unwrap().keys().cloned().collect();
        got.sort();
        want.sort();
        assert_eq!(got, want, "fixture camera {cam} has different keys from the CLI's discover --json");
        for (k, v) in cam.as_object().unwrap() {
            assert_eq!(kind(v), kind(&sample[k]), "key {k:?} has another JSON type in the fixture than in the CLI's output");
        }
    }
    assert!(saw_null_usb_serial, "the fixture must include a body with no USB serial (usb_serial: null)");
}

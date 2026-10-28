//! `PtpUsbError::SerialNotFound` as a caller sees it: the unreadable bodies are a typed field of
//! the variant, and the text built from them says the serial may be on one of those bodies. A GUI
//! that hides stderr shows only this text, so a failed descriptor read must not read as a typo.

use nikon_fleet::ptp_usb::PtpUsbError;

fn not_found(unreadable: &[&str]) -> PtpUsbError {
    PtpUsbError::SerialNotFound { requested: "0000003023668".into(), unreadable: unreadable.iter().map(|s| s.to_string()).collect() }
}

#[test]
fn with_nothing_unreadable_the_text_is_the_plain_refusal() {
    assert_eq!(not_found(&[]).to_string(), "no Nikon USB camera with serial \"0000003023668\"");
}

#[test]
fn with_one_unreadable_body_the_text_names_it_and_says_the_serial_may_be_there() {
    let text = not_found(&["usb-20:5 (reading the USB serial string failed: timeout)"]).to_string();
    assert_eq!(
        text,
        "no Nikon USB camera with serial \"0000003023668\"; 1 attached body(ies) could not be read: \
         usb-20:5 (reading the USB serial string failed: timeout); the serial may be on one of them"
    );
}

#[test]
fn with_several_unreadable_bodies_the_count_matches_the_list() {
    let err = not_found(&["usb-20:5 (a)", "usb-20:6 (b)", "usb-20:7 (c)"]);
    let text = err.to_string();
    assert!(text.contains("3 attached body(ies) could not be read: usb-20:5 (a); usb-20:6 (b); usb-20:7 (c);"), "{text}");
    assert!(matches!(&err, PtpUsbError::SerialNotFound { unreadable, .. } if unreadable.len() == 3), "typed, not only text");
}

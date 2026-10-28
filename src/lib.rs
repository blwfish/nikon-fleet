//! nikon-fleet library — settings management for Nikon Z cameras.
//!
//! The binary in `src/main.rs` is a thin CLI wrapper around this library.
//! Keeping the logic in `lib.rs` makes it directly testable and reusable.

pub mod camera_lock;
mod config_parse;
pub mod diff;
pub mod firmware;
pub mod firmware_browse;
pub mod firmware_source;
#[cfg(feature = "webview")]
pub mod firmware_webview;
pub mod json_out;
pub mod maid_layer;
pub mod ops;
pub mod process_exit;
pub mod ptp_usb;
pub mod range_policy;
pub mod range_value;
pub mod sdk;
pub mod snapshot;
pub mod transplant;
#[cfg(test)]
pub(crate) mod test_support;
pub mod vendor_argv;

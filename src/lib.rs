//! nikon-fleet library — settings management for Nikon Z cameras.
//!
//! The binary in `src/main.rs` is a thin CLI wrapper around this library.
//! Keeping the logic in `lib.rs` makes it directly testable and reusable.

pub mod camera_lock;
pub mod code_policy;
mod config_parse;
pub mod diag;
pub mod diff;
pub mod exit_status;
pub mod extended;
pub mod extended_diff;
pub mod firmware;
pub mod ftp_edit;
pub mod firmware_browse;
pub mod firmware_source;
#[cfg(feature = "webview")]
pub mod firmware_webview;
pub mod iptc;
pub mod json_out;
pub mod maid_layer;
pub mod ops;
pub mod process_exit;
pub mod prop_sweep;
pub mod ptp_usb;
pub mod range_policy;
pub mod range_value;
pub mod sdk;
pub mod settings;
pub mod snapshot;
pub mod snapshot_edit;
pub mod transplant;
pub(crate) mod value_shape;
#[cfg(test)]
pub(crate) mod test_support;
pub mod gui_argv;
pub mod vendor_argv;

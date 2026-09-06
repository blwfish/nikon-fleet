//! nikon-fleet library — settings management for Nikon Z cameras.
//!
//! The binary in `src/main.rs` is a thin CLI wrapper around this library.
//! Keeping the logic in `lib.rs` makes it directly testable and reusable.

mod config_parse;
pub mod diff;
pub mod firmware;
pub mod maid_layer;
pub mod ptp_usb;
pub mod range_value;
pub mod sdk;
pub mod snapshot;

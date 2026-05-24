//! nikon-fleet library — settings management for Nikon Z cameras.
//!
//! The binary in `src/main.rs` is a thin CLI wrapper around this library.
//! Keeping the logic in `lib.rs` makes it directly testable and reusable.

pub mod diff;
pub mod maid_layer;
pub mod range_value;
pub mod snapshot;

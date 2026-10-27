//! Workflow operations shared by the CLI and (planned) the MCP server.
//!
//! `docs/mcp-plan.md` §2 explains why this module exists: the workflows live in
//! `src/main.rs` today, so a second front end would have to re-implement their
//! rules. Phase 0 moves them here a piece at a time; each piece returns
//! structured values, never prints, and is covered without hardware.

pub mod apply;
pub mod baseline;
pub mod cameras;
pub mod capture;
pub mod capture_snapshot;
pub mod check;
pub mod firmware_latest;
pub mod list;
pub mod paths;
pub mod plan;
pub mod port;
pub mod readback;
pub mod restore;
pub mod restore_flow;
pub mod retention;
pub mod target;
pub mod views;
pub mod worker;

//! Workflow operations shared by the CLI and (planned) the MCP server.
//!
//! `docs/mcp-plan.md` §2 explains why this module exists: the workflows live in
//! `src/main.rs` today, so a second front end would have to re-implement their
//! rules. Phase 0 moves them here a piece at a time; each piece returns
//! structured values, never prints, and is covered without hardware.

pub mod list;
pub mod target;

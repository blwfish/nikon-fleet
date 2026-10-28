//! `fleet-mcp`: the optional MCP server for nikon-fleet (docs/mcp-plan.md).
//!
//! Nothing else in the repository depends on this crate; deleting it leaves the
//! CLI and GUI unchanged.

pub mod notes;
pub mod server;
pub mod supervisor;

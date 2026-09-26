//! The plugin's MCP server: the agent's only way to touch the dashboard.
//!
//! A minimal Model Context Protocol server over stdio — newline-delimited
//! JSON-RPC 2.0 with `initialize`, `ping`, `tools/list` and `tools/call`
//! (requirement DASHR-MCP-001). Hand-rolled rather than pulled from an SDK:
//! the surface is four methods, and owning it keeps the privacy boundary in
//! code this repository tests.
//!
//! [`protocol`] is the transport; [`tools`] is what the agent can do.

pub mod protocol;
pub mod tools;

pub use protocol::{Server, ToolOutput, Tools};

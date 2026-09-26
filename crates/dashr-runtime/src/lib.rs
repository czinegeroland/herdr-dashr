//! Orchestration shared by the pane, the hooks and the MCP server.
//!
//! [`session`] starts and stops a pane-owned Grafana; [`apply`] pushes a
//! dashboard; [`status`] reports panel health without values; [`browser`]
//! drives terminal-browser; [`monitor`] is one tick of the pane's
//! background loop. Each takes its collaborators as arguments, so the
//! binaries decide where configuration and state live.

pub mod apply;
pub mod browser;
pub mod monitor;
pub mod paths;
pub mod promote;
pub mod session;
pub mod skill;
pub mod status;

pub use paths::Paths;

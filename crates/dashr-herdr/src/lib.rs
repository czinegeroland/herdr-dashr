//! Everything Herdr-shaped: the plugin manifest, the runtime environment
//! Herdr gives plugin processes, and a wrapper over the `herdr` CLI.
//!
//! The CLI rather than the raw socket: Herdr's plugin docs recommend
//! `HERDR_BIN_PATH` because it hides the Unix-socket/named-pipe difference,
//! and every call dashr makes is a one-shot request (decision DEC-005).

pub mod cli;
pub mod env;
pub mod manifest;

pub use cli::Herdr;
pub use env::PluginEnv;

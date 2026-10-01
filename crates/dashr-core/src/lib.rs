//! dashr's core: spans from any tracing system in one shape, the session's
//! trace store, flows (expected traces) and their verdicts, sequence
//! diagrams, and the masking that keeps personal data from the agent. No
//! I/O: the runtime crate feeds it.

pub mod catalog;
pub mod config;
pub mod flow;
pub mod ingest;
pub mod model;
pub mod privacy;
pub mod sequence;
pub mod store;

pub use config::Config;
pub use flow::{Flow, Verdict};
pub use model::Span;
pub use store::TraceStore;

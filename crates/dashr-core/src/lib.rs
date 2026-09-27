//! Pure building blocks shared by every other dashr crate.
//!
//! Nothing in this crate performs I/O beyond reading and writing the small
//! session files in [`session`]. Configuration, masking, dashboard
//! validation, Grafana frame handling and watch evaluation are all functions
//! of their inputs, which is what lets the privacy guarantees be unit tested
//! without a Grafana or a Docker daemon.

pub mod collect;
pub mod config;
pub mod dashboard;
pub mod dbperf;
pub mod frames;
pub mod ids;
pub mod library;
pub mod logx;
pub mod masking;
pub mod otlp;
pub mod provisioning;
pub mod session;
pub mod shell;
pub mod watch;

pub use config::Config;

//! dashr's configuration file, `config.toml` in the configuration
//! directory. Every setting has a default; the file is optional.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::privacy::MaskingConfig;

pub const FILE_NAME: &str = "config.toml";
/// The Jaeger image the session pane runs (all-in-one, in-memory storage).
pub const JAEGER_IMAGE: &str = "jaegertracing/jaeger:2.11.0";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub jaeger: JaegerConfig,
    pub masking: MaskingConfig,
    pub sources: SourcesConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JaegerConfig {
    pub image: String,
    /// The Docker command (`docker`, `podman`).
    pub docker: String,
    /// The address Jaeger's ports are published on. `127.0.0.1` keeps them
    /// on this machine; `0.0.0.0` lets services in VMs or other hosts send
    /// spans, and anyone who can reach the machine read them.
    pub bind: String,
    /// Try the standard ports first (4317 OTLP/gRPC, 4318 OTLP/HTTP, 16686
    /// UI), so SDKs work without an endpoint setting; fall back to free
    /// ports when they are taken.
    pub standard_ports: bool,
    /// How far back the pane reads traces from Jaeger, in minutes.
    pub lookback_minutes: u64,
    /// Memory limit for the container.
    pub memory: String,
}

impl Default for JaegerConfig {
    fn default() -> Self {
        Self {
            image: JAEGER_IMAGE.into(),
            docker: "docker".into(),
            bind: "127.0.0.1".into(),
            standard_ports: true,
            lookback_minutes: 30,
            memory: "1g".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourcesConfig {
    /// How far back a new pull source reads on its first run, in minutes.
    pub first_lookback_minutes: u64,
    /// Seconds each run re-reads before the previous one, for spans that
    /// arrive late (X-Ray indexes in seconds to a minute).
    pub overlap_secs: u64,
}

impl Default for SourcesConfig {
    fn default() -> Self {
        Self {
            first_lookback_minutes: 15,
            overlap_secs: 120,
        }
    }
}

impl Config {
    /// Reads `config.toml` from `dir`; defaults when there is none.
    pub fn load_from_dir(dir: &Path) -> Result<Self, String> {
        let path = dir.join(FILE_NAME);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_overrides() {
        assert_eq!(Config::default().jaeger.bind, "127.0.0.1");
        let config: Config =
            toml::from_str("[jaeger]\nbind = \"0.0.0.0\"\n[masking]\nenabled = false\n").unwrap();
        assert_eq!(config.jaeger.bind, "0.0.0.0");
        assert!(!config.masking.enabled);
        assert_eq!(config.jaeger.image, JAEGER_IMAGE);
        assert!(
            toml::from_str::<Config>("[grafana]\n").is_err(),
            "unknown sections are refused"
        );
    }
}

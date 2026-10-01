//! The pane's Jaeger container, driven through the `docker` CLI
//! (DASHR-SESSION-001).
//!
//! One container per session: Jaeger all-in-one with in-memory storage,
//! receiving OTLP on 4317 (gRPC) and 4318 (HTTP) and serving its UI and
//! query API on 16686, published on loopback only by default. It runs
//! read-only, with every capability dropped, and is removed when the pane
//! closes; nothing it held survives the session.

use std::time::Duration;

use crate::command;

/// Label on every dashr container, so the reaper never touches others.
pub const LABEL_OWNER: &str = "herdr.dashr";
/// The dashr session that owns the container.
pub const LABEL_SESSION: &str = "herdr.dashr.session";

pub const OTLP_GRPC: u16 = 4317;
pub const OTLP_HTTP: u16 = 4318;
pub const UI: u16 = 16686;

#[derive(Debug, Clone, PartialEq)]
pub struct RunSpec {
    pub name: String,
    pub image: String,
    pub session: String,
    /// The address ports are published on.
    pub bind: String,
    /// Publish on the standard host ports rather than free ones.
    pub standard_ports: bool,
    pub memory: String,
}

impl RunSpec {
    /// The `docker run` arguments: tested without a daemon.
    pub fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "run",
            "-d",
            "--rm",
            "--name",
            &self.name,
            "--label",
            &format!("{LABEL_OWNER}=1"),
            "--label",
            &format!("{LABEL_SESSION}={}", self.session),
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--memory",
            &self.memory,
            "--log-driver",
            "none",
            // Jaeger traces its own query API; every poll would add a trace.
            "-e",
            "OTEL_TRACES_SAMPLER=always_off",
        ]
        .map(str::to_owned)
        .to_vec();
        for port in [OTLP_GRPC, OTLP_HTTP, UI] {
            args.push("-p".into());
            args.push(if self.standard_ports {
                format!("{}:{port}:{port}", self.bind)
            } else {
                format!("{}::{port}", self.bind)
            });
        }
        args.push(self.image.clone());
        args
    }
}

/// The host ports a running container got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Ports {
    pub otlp_grpc: u16,
    pub otlp_http: u16,
    pub ui: u16,
}

pub struct Docker {
    command: String,
}

/// `docker port` output (`127.0.0.1:4318`, `[::]:4318`) as a port.
pub fn parse_port(output: &str) -> Option<u16> {
    output
        .lines()
        .find_map(|line| line.trim().rsplit(':').next()?.parse().ok())
}

impl Docker {
    pub fn new(command: &str) -> Self {
        Self {
            command: command.to_owned(),
        }
    }

    fn run(&self, args: &[String], timeout: Duration) -> Result<String, String> {
        let argv: Vec<String> = std::iter::once(self.command.clone())
            .chain(args.iter().cloned())
            .collect();
        command::run(&argv, &[], timeout)
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .map_err(|error| {
                // A spawn failure reads "docker: <reason>"; a failed run
                // reads "docker exited with ...".
                if error.starts_with(&format!("{}: ", self.command)) {
                    format!(
                        "`{}` was not found; install Docker (https://docs.docker.com/get-docker/)",
                        self.command
                    )
                } else {
                    error
                }
            })
    }

    /// Whether the daemon answers.
    pub fn available(&self) -> Result<(), String> {
        self.run(
            &[
                "version".into(),
                "--format".into(),
                "{{.Server.Version}}".into(),
            ],
            Duration::from_secs(15),
        )
        .map(|_| ())
    }

    pub fn has_image(&self, image: &str) -> bool {
        self.run(
            &["image".into(), "inspect".into(), image.into()],
            Duration::from_secs(15),
        )
        .is_ok()
    }

    pub fn pull(&self, image: &str) -> Result<(), String> {
        self.run(
            &["pull".into(), "-q".into(), image.into()],
            Duration::from_secs(600),
        )
        .map(|_| ())
    }

    /// Starts the container; when the standard ports are taken, starts it
    /// on free ones instead. Returns the ports it got.
    pub fn start(&self, spec: &RunSpec) -> Result<Ports, String> {
        let first = self.run(&spec.args(), Duration::from_secs(120));
        if let Err(error) = &first {
            let taken =
                error.contains("already allocated") || error.contains("address already in use");
            if !(spec.standard_ports && taken) {
                return Err(error.clone());
            }
            let _ = self.stop(&spec.name);
            let fallback = RunSpec {
                standard_ports: false,
                ..spec.clone()
            };
            self.run(&fallback.args(), Duration::from_secs(120))?;
        }
        let port = |container: u16| -> Result<u16, String> {
            let out = self.run(
                &["port".into(), spec.name.clone(), format!("{container}/tcp")],
                Duration::from_secs(15),
            )?;
            parse_port(&out).ok_or_else(|| format!("no host port for {container}"))
        };
        Ok(Ports {
            otlp_grpc: port(OTLP_GRPC)?,
            otlp_http: port(OTLP_HTTP)?,
            ui: port(UI)?,
        })
    }

    pub fn stop(&self, name: &str) -> Result<(), String> {
        self.run(
            &["rm".into(), "-f".into(), name.into()],
            Duration::from_secs(60),
        )
        .map(|_| ())
    }

    /// dashr's containers as `(name, session)`.
    pub fn list(&self) -> Result<Vec<(String, String)>, String> {
        let out = self.run(
            &[
                "ps".into(),
                "-a".into(),
                "--filter".into(),
                format!("label={LABEL_OWNER}=1"),
                "--format".into(),
                format!("{{{{.Names}}}}\t{{{{.Label \"{LABEL_SESSION}\"}}}}"),
            ],
            Duration::from_secs(15),
        )?;
        Ok(out
            .lines()
            .filter_map(|line| {
                let (name, session) = line.split_once('\t')?;
                Some((name.trim().to_owned(), session.trim().to_owned()))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(standard_ports: bool) -> RunSpec {
        RunSpec {
            name: "dashr-s1".into(),
            image: "jaegertracing/jaeger:2.11.0".into(),
            session: "s1".into(),
            bind: "127.0.0.1".into(),
            standard_ports,
            memory: "1g".into(),
        }
    }

    #[test]
    fn the_container_is_locked_down_and_on_loopback() {
        let args = spec(true).args().join(" ");
        for flag in [
            "--rm",
            "--read-only",
            "--cap-drop ALL",
            "--security-opt no-new-privileges",
            "--log-driver none",
            "-e OTEL_TRACES_SAMPLER=always_off",
            "--label herdr.dashr=1",
            "--label herdr.dashr.session=s1",
        ] {
            assert!(args.contains(flag), "{flag} missing: {args}");
        }
        assert!(
            args.contains("-p 127.0.0.1:4318:4318") && args.contains("-p 127.0.0.1:16686:16686")
        );
        assert!(spec(false).args().join(" ").contains("-p 127.0.0.1::4317"));
        assert!(args.ends_with("jaegertracing/jaeger:2.11.0"));
    }

    #[test]
    fn ports_from_docker_port() {
        assert_eq!(parse_port("127.0.0.1:32769\n"), Some(32769));
        assert_eq!(parse_port("0.0.0.0:4318\n[::]:4318\n"), Some(4318));
        assert_eq!(parse_port(""), None);
    }
}

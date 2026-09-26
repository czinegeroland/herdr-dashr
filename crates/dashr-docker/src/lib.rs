//! The pane-owned Grafana container, driven through the `docker` CLI.
//!
//! The CLI rather than the Engine API: it already knows the user's context,
//! Docker Desktop's socket location and rootless setups, and it is what the
//! design specifies verbatim (docs/DESIGN.md, "Grafana container"). Every
//! argv is built by a pure function so the "stores nothing" flags are tested
//! without a daemon (requirements DASHR-GRAF-001, DASHR-GRAF-002).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Label on every dashr container, so the reaper never touches others.
pub const LABEL_OWNER: &str = "herdr.dashr";
/// The Herdr pane that owns the container.
pub const LABEL_PANE: &str = "herdr.pane";
/// Hash of the Herdr socket, telling sessions' `w1:p1`s apart (DEC-009).
pub const LABEL_SOCKET: &str = "herdr.socket";
/// The dashr session id.
pub const LABEL_SESSION: &str = "herdr.dashr.session";

#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    #[error("`{command}` was not found; install Docker (https://docs.docker.com/get-docker/)")]
    Missing { command: String },
    #[error("docker {action} failed: {message}")]
    Failed { action: String, message: String },
}

/// Which image a session runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// Plain Grafana: datasources only.
    Grafana,
    /// `grafana/otel-lgtm`: Grafana plus an OpenTelemetry collector, Loki,
    /// Tempo and Prometheus in the same container, receiving OTLP on 4317
    /// (gRPC) and 4318 (HTTP) (requirement DASHR-OTEL-001).
    OtelLgtm,
}

/// Where the otel-lgtm image reads Grafana datasource provisioning. dashr's
/// file is mounted beside the image's own, not over the directory, so the
/// Loki, Tempo and Prometheus datasources it ships stay provisioned.
pub const OTEL_PROVISIONING_FILE: &str =
    "/otel-lgtm/grafana/conf/provisioning/datasources/dashr.yaml";

/// dashr's Tempo configuration for the otel-lgtm image (DEC-033): the
/// image's own plus the settings that make traces searchable in seconds.
pub const OTEL_TEMPO_CONFIG: &str = include_str!("../assets/tempo.yaml");
/// Where the image reads it.
pub const OTEL_TEMPO_CONFIG_FILE: &str = "/otel-lgtm/tempo-config.yaml";
/// The file name, in the provisioning directory, that
/// [`OTEL_TEMPO_CONFIG`] is written to and mounted from.
pub const TEMPO_CONFIG_NAME: &str = "tempo.yaml";

/// Everything that decides how a session container runs.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSpec {
    pub name: String,
    pub image: String,
    pub flavor: Flavor,
    pub labels: BTreeMap<String, String>,
    pub memory: String,
    pub provisioning_dir: PathBuf,
    /// Non-secret settings, passed as `-e KEY=VALUE`.
    pub env: BTreeMap<String, String>,
    /// Secrets, passed as `-e NAME` so Docker copies the value from the
    /// docker process's own environment. The value never enters an argv
    /// (requirement DASHR-DS-003).
    pub inherit_env: Vec<String>,
    /// Add `host.docker.internal` pointing at the host gateway (Linux).
    pub host_gateway: bool,
}

impl RunSpec {
    /// Grafana's hardened defaults for a disposable, anonymous instance
    /// (requirement DASHR-GRAF-003).
    pub fn grafana_env() -> BTreeMap<String, String> {
        [
            ("GF_AUTH_ANONYMOUS_ENABLED", "true"),
            ("GF_AUTH_ANONYMOUS_ORG_ROLE", "Admin"),
            ("GF_AUTH_DISABLE_LOGIN_FORM", "true"),
            ("GF_ANALYTICS_REPORTING_ENABLED", "false"),
            ("GF_ANALYTICS_CHECK_FOR_UPDATES", "false"),
            ("GF_ANALYTICS_CHECK_FOR_PLUGIN_UPDATES", "false"),
            ("GF_NEWS_NEWS_FEED_ENABLED", "false"),
            ("GF_SECURITY_ALLOW_EMBEDDING", "true"),
            ("GF_USERS_DEFAULT_THEME", "dark"),
            ("GF_LOG_MODE", "console"),
            ("GF_LOG_LEVEL", "warn"),
            // A live log trail refreshes every second or two; Grafana's
            // default floor is 5s and it refuses to save faster dashboards.
            ("GF_DASHBOARDS_MIN_REFRESH_INTERVAL", "1s"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
    }

    /// The `docker run` argv, without the leading `docker`.
    pub fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "-d".into(),
            "--name".into(),
            self.name.clone(),
        ];
        for (key, value) in &self.labels {
            args.push("--label".into());
            args.push(format!("{key}={value}"));
        }
        // Loopback only, on ports Docker picks.
        let mut ports = vec!["127.0.0.1::3000"];
        // Plain Grafana writes nothing outside memory. The OpenTelemetry
        // components keep logs, traces and metrics on disk instead, in
        // anonymous volumes that `--rm` deletes with the container, so a
        // long test session does not eat the memory limit (DEC-033).
        let (tmpfs, volumes): (&[&str], &[&str]) = match self.flavor {
            Flavor::Grafana => (
                &[
                    "/var/lib/grafana:uid=472,gid=0,mode=0770",
                    "/tmp",
                    "/var/log/grafana:uid=472,gid=0",
                ],
                &[],
            ),
            // Every component writes under /data; Tempo also keeps its
            // live-store files in /var/tempo.
            Flavor::OtelLgtm => (&["/tmp"], &["/data", "/var/tempo"]),
        };
        if self.flavor == Flavor::OtelLgtm {
            ports.extend(["127.0.0.1::4317", "127.0.0.1::4318"]);
        }
        for port in ports {
            args.push("-p".into());
            args.push(port.into());
        }
        args.push("--read-only".into());
        for mount in tmpfs {
            args.push("--tmpfs".into());
            args.push((*mount).into());
        }
        for target in volumes {
            args.push("--mount".into());
            args.push(format!("type=volume,dst={target}"));
        }
        args.extend([
            "--log-driver".into(),
            "none".into(),
            "--memory".into(),
            self.memory.clone(),
            // Equal to --memory: no swap, so tmpfs pages never reach disk.
            "--memory-swap".into(),
            self.memory.clone(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "-v".into(),
            match self.flavor {
                Flavor::Grafana => format!(
                    "{}:/etc/grafana/provisioning:ro",
                    self.provisioning_dir.display()
                ),
                Flavor::OtelLgtm => format!(
                    "{}:{OTEL_PROVISIONING_FILE}:ro",
                    self.provisioning_dir
                        .join("datasources")
                        .join("dashr.yaml")
                        .display()
                ),
            },
        ]);
        if self.flavor == Flavor::OtelLgtm {
            args.push("-v".into());
            args.push(format!(
                "{}:{OTEL_TEMPO_CONFIG_FILE}:ro",
                self.provisioning_dir.join(TEMPO_CONFIG_NAME).display()
            ));
        }
        if self.host_gateway {
            args.push("--add-host".into());
            args.push("host.docker.internal:host-gateway".into());
        }
        let mut env = self.env.clone();
        if self.flavor == Flavor::OtelLgtm {
            // The image preinstalls a plugin from the internet at every start;
            // a disposable, offline-capable session does not want that.
            env.insert("GF_PLUGINS_PREINSTALL_DISABLED".into(), "true".into());
        }
        for (key, value) in &env {
            args.push("-e".into());
            args.push(format!("{key}={value}"));
        }
        for name in &self.inherit_env {
            args.push("-e".into());
            args.push(name.clone());
        }
        args.push(self.image.clone());
        args
    }
}

/// A running dashr container as `docker ps` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub pane: String,
    pub socket: String,
    pub session: String,
}

/// Parses the tab-separated `docker ps` format used by [`Docker::list`].
pub fn parse_list(output: &str) -> Vec<Listed> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let name = parts.next()?.trim();
            if name.is_empty() {
                return None;
            }
            Some(Listed {
                name: name.to_owned(),
                pane: parts.next().unwrap_or("").trim().to_owned(),
                socket: parts.next().unwrap_or("").trim().to_owned(),
                session: parts.next().unwrap_or("").trim().to_owned(),
            })
        })
        .collect()
}

/// Parses `docker port <c> 3000/tcp`, e.g. `127.0.0.1:32768`.
pub fn parse_port(output: &str) -> Option<u16> {
    output
        .lines()
        .filter_map(|line| line.trim().rsplit(':').next()?.parse().ok())
        .next()
}

/// Which containers the reaper should stop: dashr containers of this Herdr
/// server whose pane no longer exists (requirement DASHR-HERDR-004).
pub fn orphans<'a>(
    listed: &'a [Listed],
    socket_hash: &str,
    live_panes: &[String],
) -> Vec<&'a Listed> {
    listed
        .iter()
        .filter(|container| container.socket == socket_hash)
        .filter(|container| !live_panes.contains(&container.pane))
        .collect()
}

/// The Dockerfile of the custom image with community plugins baked in.
///
/// Plugins go to `/usr/share/grafana/plugins-dashr` rather than the default
/// `/var/lib/grafana/plugins`, because the session mounts a tmpfs over
/// `/var/lib/grafana` and would hide them (decision DEC-008).
pub fn dockerfile(base_image: &str) -> String {
    format!(
        r#"FROM {base_image}
USER root
RUN mkdir -p /usr/share/grafana/plugins-dashr && \
    grafana cli --pluginsDir /usr/share/grafana/plugins-dashr plugins install yesoreyeram-infinity-datasource && \
    grafana cli --pluginsDir /usr/share/grafana/plugins-dashr plugins install alexanderzobnin-zabbix-app && \
    chown -R 472:0 /usr/share/grafana/plugins-dashr
ENV GF_PATHS_PLUGINS=/usr/share/grafana/plugins-dashr
ENV GF_PLUGINS_ALLOW_LOADING_UNSIGNED_PLUGINS=alexanderzobnin-zabbix-datasource
USER 472
"#
    )
}

/// The `docker` CLI.
#[derive(Debug, Clone)]
pub struct Docker {
    command: String,
}

impl Docker {
    pub fn new(command: &str) -> Self {
        Self {
            command: command.to_owned(),
        }
    }

    fn run(
        &self,
        action: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<String, DockerError> {
        let output = Command::new(&self.command)
            .args(args)
            .envs(env)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => DockerError::Missing {
                    command: self.command.clone(),
                },
                _ => DockerError::Failed {
                    action: action.to_owned(),
                    message: error.to_string(),
                },
            })?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(DockerError::Failed {
                action: action.to_owned(),
                message: stderr.trim().chars().take(600).collect(),
            })
        }
    }

    /// Whether the daemon answers.
    pub fn available(&self) -> Result<(), DockerError> {
        self.run(
            "info",
            &[
                "info".into(),
                "--format".into(),
                "{{.ServerVersion}}".into(),
            ],
            &BTreeMap::new(),
        )
        .map(|_| ())
    }

    /// Starts a container. `secrets` supplies the values of
    /// `spec.inherit_env` to the docker process only.
    pub fn start(
        &self,
        spec: &RunSpec,
        secrets: &BTreeMap<String, String>,
    ) -> Result<String, DockerError> {
        self.run("run", &spec.args(), secrets)
            .map(|id| id.trim().to_owned())
    }

    /// The loopback port Docker mapped to Grafana's 3000.
    pub fn port(&self, name: &str) -> Result<u16, DockerError> {
        self.mapped_port(name, 3000)
    }

    /// The loopback port Docker mapped to a container port.
    pub fn mapped_port(&self, name: &str, container_port: u16) -> Result<u16, DockerError> {
        let output = self.run(
            "port",
            &["port".into(), name.into(), format!("{container_port}/tcp")],
            &BTreeMap::new(),
        )?;
        parse_port(&output).ok_or_else(|| DockerError::Failed {
            action: "port".into(),
            message: format!("no port mapping in {output:?}"),
        })
    }

    /// Stops a container; `--rm` then removes it. Already gone is success.
    pub fn stop(&self, name: &str) -> Result<(), DockerError> {
        match self.run(
            "stop",
            &["stop".into(), "-t".into(), "5".into(), name.into()],
            &BTreeMap::new(),
        ) {
            Ok(_) => Ok(()),
            Err(DockerError::Failed { message, .. })
                if message.contains("No such container") || message.contains("is not running") =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Every running dashr container.
    pub fn list(&self) -> Result<Vec<Listed>, DockerError> {
        let format = format!(
            "{{{{.Names}}}}\t{{{{.Label \"{LABEL_PANE}\"}}}}\t{{{{.Label \"{LABEL_SOCKET}\"}}}}\t{{{{.Label \"{LABEL_SESSION}\"}}}}"
        );
        let output = self.run(
            "ps",
            &[
                "ps".into(),
                "--filter".into(),
                format!("label={LABEL_OWNER}=1"),
                "--format".into(),
                format,
            ],
            &BTreeMap::new(),
        )?;
        Ok(parse_list(&output))
    }

    /// Whether an image exists locally.
    pub fn has_image(&self, image: &str) -> bool {
        self.run(
            "image inspect",
            &["image".into(), "inspect".into(), image.into()],
            &BTreeMap::new(),
        )
        .is_ok()
    }

    /// Builds the custom image from [`dockerfile`], streaming build output.
    pub fn build_image(
        &self,
        tag: &str,
        base_image: &str,
        context: &Path,
    ) -> Result<(), DockerError> {
        let dockerfile_path = context.join("Dockerfile");
        std::fs::write(&dockerfile_path, dockerfile(base_image)).map_err(|error| {
            DockerError::Failed {
                action: "build".into(),
                message: error.to_string(),
            }
        })?;
        let status = Command::new(&self.command)
            .args(["build", "-t", tag])
            .arg(context)
            .status()
            .map_err(|error| DockerError::Failed {
                action: "build".into(),
                message: error.to_string(),
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(DockerError::Failed {
                action: "build".into(),
                message: format!("exited with {status}"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RunSpec {
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_OWNER.to_owned(), "1".to_owned());
        labels.insert(LABEL_PANE.to_owned(), "w1:p1".to_owned());
        RunSpec {
            name: "herdr-grafana-abc-w1-p1".into(),
            image: "grafana/grafana:12.1.1".into(),
            flavor: Flavor::Grafana,
            labels,
            memory: "768m".into(),
            provisioning_dir: "/dev/shm/dashr/x/provisioning".into(),
            env: RunSpec::grafana_env(),
            inherit_env: vec!["AWS_SECRET_ACCESS_KEY".into()],
            host_gateway: true,
        }
    }

    fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2)
            .any(|pair| pair[0] == flag && pair[1] == value)
    }

    #[test]
    fn run_args_store_nothing_and_bind_loopback() {
        let args = spec().args();
        assert_eq!(&args[..3], ["run", "--rm", "-d"]);
        assert!(args.contains(&"--read-only".to_owned()));
        assert!(has_pair(&args, "-p", "127.0.0.1::3000"));
        assert!(has_pair(&args, "--log-driver", "none"));
        assert!(has_pair(&args, "--memory", "768m"));
        assert!(has_pair(&args, "--memory-swap", "768m"));
        assert!(
            args.iter()
                .any(|arg| arg.starts_with("/var/lib/grafana:uid=472"))
        );
        assert!(has_pair(&args, "--label", "herdr.pane=w1:p1"));
        assert!(has_pair(
            &args,
            "-v",
            "/dev/shm/dashr/x/provisioning:/etc/grafana/provisioning:ro"
        ));
        assert!(has_pair(
            &args,
            "--add-host",
            "host.docker.internal:host-gateway"
        ));
        assert!(has_pair(&args, "-e", "GF_AUTH_ANONYMOUS_ENABLED=true"));
        assert!(has_pair(
            &args,
            "-e",
            "GF_ANALYTICS_REPORTING_ENABLED=false"
        ));
        assert_eq!(args.last().unwrap(), "grafana/grafana:12.1.1");
    }

    #[test]
    fn otel_flavor_is_one_hardened_container_receiving_otlp_on_loopback() {
        let mut otel = spec();
        otel.flavor = Flavor::OtelLgtm;
        otel.image = "grafana/otel-lgtm:0.34.0".into();
        let args = otel.args();
        assert!(args.contains(&"--read-only".to_owned()));
        for port in ["127.0.0.1::3000", "127.0.0.1::4317", "127.0.0.1::4318"] {
            assert!(has_pair(&args, "-p", port), "{port}");
        }
        assert!(has_pair(&args, "--tmpfs", "/tmp"));
        // Telemetry lives on disk in anonymous volumes, deleted with the
        // container by --rm.
        assert!(args.contains(&"--rm".to_owned()));
        for target in ["/data", "/var/tempo"] {
            assert!(
                has_pair(&args, "--mount", &format!("type=volume,dst={target}")),
                "{target}"
            );
            assert!(!has_pair(&args, "--tmpfs", target), "{target}");
        }
        assert!(has_pair(
            &args,
            "-v",
            &format!(
                "{}:{OTEL_TEMPO_CONFIG_FILE}:ro",
                otel.provisioning_dir.join(TEMPO_CONFIG_NAME).display()
            )
        ));
        assert!(OTEL_TEMPO_CONFIG.contains("query_end_cutoff: 1s"));
        assert!(has_pair(
            &args,
            "-v",
            &format!(
                "{}:{OTEL_PROVISIONING_FILE}:ro",
                otel.provisioning_dir
                    .join("datasources")
                    .join("dashr.yaml")
                    .display()
            )
        ));
        assert!(has_pair(&args, "-e", "GF_PLUGINS_PREINSTALL_DISABLED=true"));
        assert!(has_pair(&args, "--cap-drop", "ALL"));
        assert!(has_pair(&args, "--log-driver", "none"));
        assert!(
            !has_pair(&spec().args(), "-p", "127.0.0.1::4318"),
            "plain Grafana stays as it was"
        );
    }

    #[test]
    fn secrets_are_passed_by_name_only() {
        let args = spec().args();
        assert!(has_pair(&args, "-e", "AWS_SECRET_ACCESS_KEY"));
        assert!(
            !args
                .iter()
                .any(|arg| arg.starts_with("AWS_SECRET_ACCESS_KEY="))
        );
    }

    #[test]
    fn parses_ps_and_port_output() {
        let listed = parse_list(
            "herdr-grafana-a-w1-p1\tw1:p1\tabcd\ta-w1-p1\nherdr-grafana-b\tw1:p2\tffff\tb\n\n",
        );
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].pane, "w1:p1");
        assert_eq!(listed[1].socket, "ffff");
        assert_eq!(parse_port("127.0.0.1:32768\n"), Some(32768));
        assert_eq!(parse_port("0.0.0.0:49153\n[::]:49153\n"), Some(49153));
        assert_eq!(parse_port(""), None);
    }

    #[test]
    fn orphans_are_this_servers_containers_without_a_live_pane() {
        let listed = parse_list("a\tw1:p1\tmine\ts1\nb\tw1:p2\tmine\ts2\nc\tw1:p9\tother\ts3\n");
        let live = vec!["w1:p1".to_owned()];
        let orphaned: Vec<&str> = orphans(&listed, "mine", &live)
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(orphaned, vec!["b"]);
    }

    #[test]
    fn dockerfile_keeps_plugins_out_of_the_tmpfs() {
        let text = dockerfile("grafana/grafana:12.1.1");
        assert!(text.starts_with("FROM grafana/grafana:12.1.1"));
        assert!(text.contains("GF_PATHS_PLUGINS=/usr/share/grafana/plugins-dashr"));
        assert!(!text.contains("/var/lib/grafana/plugins"));
        assert!(text.contains("yesoreyeram-infinity-datasource"));
    }

    #[test]
    fn missing_binary_is_a_clear_error() {
        let docker = Docker::new("definitely-not-docker-xyz");
        assert!(matches!(
            docker.available(),
            Err(DockerError::Missing { .. })
        ));
    }
}

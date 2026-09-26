//! Starting and stopping a pane-owned Grafana.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashr_aws::cli::AwsCli;
use dashr_aws::{Inventory, PipelineRef};
use dashr_core::config::{Config, DEFAULT_IMAGE, DatasourceKind};
use dashr_core::session::{Otlp, SessionRecord, SessionStore};
use dashr_core::{dashboard, ids, provisioning};
use dashr_docker::{Docker, DockerError, Flavor, RunSpec};
use dashr_grafana::{Client, GrafanaError};

use crate::paths;

/// Who owns a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub session_id: String,
    pub pane_id: Option<String>,
    pub socket_hash: Option<String>,
}

impl Identity {
    /// A session owned by a Herdr pane.
    pub fn herdr(socket_path: &str, pane_id: &str) -> Self {
        Self {
            session_id: ids::session_id(socket_path, pane_id),
            pane_id: Some(pane_id.to_owned()),
            socket_hash: Some(ids::socket_hash(socket_path)),
        }
    }

    /// A session started by hand, outside Herdr.
    pub fn standalone(name: &str) -> Self {
        Self {
            session_id: format!("local-{}", ids::sanitize(name)),
            pane_id: None,
            socket_hash: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("{0}")]
    Docker(#[from] DockerError),
    #[error("could not prepare runtime files: {0}")]
    Io(#[from] std::io::Error),
    #[error("Grafana did not start: {0}")]
    Grafana(#[from] GrafanaError),
    #[error("the OpenTelemetry endpoint did not start: {0}")]
    Otlp(String),
    #[error("{0}")]
    Session(#[from] dashr_core::session::SessionError),
}

/// A started session.
#[derive(Debug)]
pub struct Started {
    pub record: SessionRecord,
    /// Whether runtime files are on a memory-backed file system.
    pub in_memory: bool,
    /// The pipeline bootstrap, when a pipeline was given.
    pub inventory: Option<Result<Inventory, String>>,
    pub warnings: Vec<String>,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Environment values the container needs, by name, from this process's
/// environment and from exported AWS credentials.
fn resolve_secrets(
    config: &Config,
    env_names: &[String],
    has_cloudwatch: bool,
    aws: Option<&AwsCli>,
    warnings: &mut Vec<String>,
) -> BTreeMap<String, String> {
    let mut secrets = BTreeMap::new();
    for name in env_names {
        match std::env::var(name) {
            Ok(value) if !value.is_empty() => {
                secrets.insert(name.clone(), value);
            }
            _ => warnings.push(format!(
                "{name} is not set; datasources that reference it will fail to authenticate"
            )),
        }
    }
    if has_cloudwatch {
        let exported = aws.map(AwsCli::export_credentials);
        match exported {
            Some(Ok(credentials)) if !credentials.is_empty() => secrets.extend(credentials),
            other => {
                // Fall back to credentials already in the environment.
                let mut found = false;
                for name in dashr_aws::cli::CREDENTIAL_VARS {
                    if let Ok(value) = std::env::var(name)
                        && !value.is_empty()
                    {
                        secrets.insert((*name).to_owned(), value);
                        found = true;
                    }
                }
                if !found {
                    let reason = match other {
                        Some(Err(error)) => error.to_string(),
                        _ => "no credentials exported".to_owned(),
                    };
                    let profile = config.aws.profile.as_deref().unwrap_or("default");
                    warnings.push(format!(
                        "CloudWatch has no AWS credentials ({reason}); run `aws sso login --profile {profile}` and reopen"
                    ));
                }
            }
        }
    }
    secrets
}

fn write_provisioning(runtime_dir: &Path, contents: &str) -> std::io::Result<std::path::PathBuf> {
    let root = runtime_dir.join("provisioning");
    let datasources = root.join("datasources");
    std::fs::create_dir_all(&datasources)?;
    std::fs::write(datasources.join("dashr.yaml"), contents)?;
    // Grafana runs as uid 472 inside the container and reads this bind
    // mount directly. The file holds env references, never values, and the
    // enclosing runtime directory stays 0700 on the host.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))?;
        std::fs::set_permissions(&datasources, std::fs::Permissions::from_mode(0o755))?;
        std::fs::set_permissions(
            datasources.join("dashr.yaml"),
            std::fs::Permissions::from_mode(0o644),
        )?;
    }
    Ok(root)
}

/// Writes a file the container user can read through a bind mount.
fn write_readable(path: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

/// Starts Grafana for a session and pushes a first dashboard.
pub fn start(
    config: &Config,
    identity: &Identity,
    pipeline: Option<&PipelineRef>,
    store: &SessionStore,
    docker: &Docker,
    aws: Option<&AwsCli>,
) -> Result<Started, StartError> {
    docker.available()?;
    let mut warnings = Vec::new();
    let (runtime_dir, in_memory) = paths::create_runtime_dir(&identity.session_id)?;
    if !in_memory {
        warnings.push(format!(
            "no memory-backed directory; runtime files are in {} and deleted on close",
            runtime_dir.display()
        ));
    }

    let extra: Vec<_> = pipeline
        .map(|pipeline| vec![provisioning::cloudwatch_for_region(&pipeline.region)])
        .unwrap_or_default();
    let provisioned = provisioning::build(config, &extra);
    if config.otel.enabled {
        for datasource in &config.datasources {
            let uid = datasource.effective_uid();
            if dashr_core::config::OTEL_DATASOURCE_UIDS.contains(&uid.as_str()) {
                warnings.push(format!(
                    "datasource {} is not provisioned in OpenTelemetry mode: its uid {uid} is the image's own; give it another uid",
                    datasource.name
                ));
            }
        }
    }
    let written = write_provisioning(&runtime_dir, &provisioned.datasources_file).and_then(|dir| {
        if config.otel.enabled {
            write_readable(
                &dir.join(dashr_docker::TEMPO_CONFIG_NAME),
                dashr_docker::OTEL_TEMPO_CONFIG,
            )?;
        }
        Ok(dir)
    });
    let provisioning_dir = match written {
        Ok(dir) => dir,
        Err(error) => {
            paths::remove_runtime_dir(&runtime_dir);
            return Err(error.into());
        }
    };
    let has_cloudwatch = config
        .datasources
        .iter()
        .any(|datasource| datasource.kind == DatasourceKind::Cloudwatch)
        || pipeline.is_some();
    let secrets = resolve_secrets(
        config,
        &provisioned.env_names,
        has_cloudwatch,
        aws,
        &mut warnings,
    );

    if config.needs_custom_image() && config.grafana.image == DEFAULT_IMAGE && !config.otel.enabled
    {
        warnings.push(
            "Seq and Zabbix datasources need the custom image: run `dashr image build` and set grafana.image"
                .to_owned(),
        );
    }

    let container = ids::container_name(&identity.session_id);
    let mut labels = BTreeMap::new();
    labels.insert(dashr_docker::LABEL_OWNER.to_owned(), "1".to_owned());
    labels.insert(
        dashr_docker::LABEL_SESSION.to_owned(),
        identity.session_id.clone(),
    );
    if let Some(pane) = &identity.pane_id {
        labels.insert(dashr_docker::LABEL_PANE.to_owned(), pane.clone());
    }
    if let Some(hash) = &identity.socket_hash {
        labels.insert(dashr_docker::LABEL_SOCKET.to_owned(), hash.clone());
    }
    // OpenTelemetry mode swaps the image for one that also runs a collector,
    // Loki, Tempo and Prometheus: still one container per pane (DASHR-OTEL-001).
    let otel = config.otel.enabled;
    let (flavor, image, memory, refresh) = if otel {
        (
            Flavor::OtelLgtm,
            config.otel.image.clone(),
            config.otel.memory.clone(),
            config.otel.refresh.clone(),
        )
    } else {
        (
            Flavor::Grafana,
            config.grafana.image.clone(),
            config.grafana.memory.clone(),
            config.grafana.refresh.clone(),
        )
    };
    let spec = RunSpec {
        name: container.clone(),
        image,
        flavor,
        labels,
        memory,
        provisioning_dir,
        env: RunSpec::grafana_env(),
        inherit_env: secrets.keys().cloned().collect(),
        host_gateway: cfg!(target_os = "linux"),
    };

    // A container left by a crash of this very pane would hold the name.
    let _ = docker.stop(&container);
    let cleanup = |error: StartError| {
        let _ = docker.stop(&container);
        paths::remove_runtime_dir(&runtime_dir);
        error
    };
    docker
        .start(&spec, &secrets)
        .map_err(|e| cleanup(e.into()))?;
    let port = docker.port(&container).map_err(|e| cleanup(e.into()))?;
    let client = Client::local(&format!("http://127.0.0.1:{port}"));
    client
        .wait_healthy(Duration::from_secs(config.grafana.startup_timeout_secs))
        .map_err(|e| cleanup(e.into()))?;
    let otlp = if otel {
        let otlp = Otlp {
            grpc_port: docker
                .mapped_port(&container, 4317)
                .map_err(|e| cleanup(e.into()))?,
            http_port: docker
                .mapped_port(&container, 4318)
                .map_err(|e| cleanup(e.into()))?,
        };
        crate::otlp::Exporter::new(&otlp.http_endpoint())
            .wait_ready(Duration::from_secs(config.grafana.startup_timeout_secs))
            .map_err(|e| cleanup(StartError::Otlp(e)))?;
        Some(otlp)
    } else {
        None
    };

    let record = SessionRecord {
        session_id: identity.session_id.clone(),
        pane_id: identity.pane_id.clone(),
        socket_hash: identity.socket_hash.clone(),
        container: container.clone(),
        port,
        dashboard_uid: ids::dashboard_uid(&identity.session_id),
        chat_pane: None,
        runtime_dir: runtime_dir.clone(),
        refresh,
        datasources: provisioned.policies,
        pipeline: pipeline.map(|p| format!("{} ({})", p.name, p.region)),
        otlp,
        started_unix: now_unix(),
    };
    store.save(&record).map_err(|e| cleanup(e.into()))?;

    let mut inventory = None;
    let first = match (pipeline, aws) {
        (Some(pipeline), Some(aws)) => match aws.discover(pipeline) {
            Ok(found) => {
                let proposal = dashr_aws::propose::propose(
                    &found,
                    &provisioning::cloudwatch_uid(&pipeline.region),
                );
                inventory = Some(Ok(found));
                proposal
            }
            Err(error) => {
                let message = error.to_string();
                inventory = Some(Err(message.clone()));
                dashboard::welcome(
                    &pipeline.name,
                    &format!(
                        "Could not inspect pipeline `{}`: {}\n\nAsk the agent below to build the dashboard by hand.",
                        pipeline.name,
                        dashr_core::masking::Masker::new(&config.masking).mask_text(&message)
                    ),
                )
            }
        },
        _ if otel => {
            let otlp = record.otlp.unwrap_or(Otlp {
                grpc_port: 0,
                http_port: 0,
            });
            dashboard::otel_welcome(&otlp.http_endpoint(), &otlp.grpc_endpoint())
        }
        _ => dashboard::welcome(
            "dashr",
            "A disposable Grafana owned by this pane. Nothing is stored; it stops when the pane closes.\n\nAsk the agent below for the dashboard you need.",
        ),
    };
    if let Err(error) = crate::apply::apply(
        &record,
        &client,
        None,
        &config.grafana.time_from,
        &first,
        "initial dashboard",
    ) {
        warnings.push(format!("first dashboard not applied: {error}"));
    }

    Ok(Started {
        record,
        in_memory,
        inventory,
        warnings,
    })
}

/// Stops a session's Grafana and deletes everything it had on disk.
pub fn stop(
    record: &SessionRecord,
    docker: &Docker,
    store: &SessionStore,
) -> Result<(), DockerError> {
    let result = docker.stop(&record.container);
    paths::remove_runtime_dir(&record.runtime_dir);
    store.remove(&record.session_id);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities() {
        let herdr = Identity::herdr("/run/h.sock", "w1:p2");
        assert!(herdr.session_id.ends_with("-w1-p2"));
        assert_eq!(herdr.pane_id.as_deref(), Some("w1:p2"));
        assert_eq!(herdr.socket_hash.as_deref().map(str::len), Some(8));
        assert_eq!(Identity::standalone("My Test").session_id, "local-my-test");
    }

    #[test]
    fn missing_secret_env_is_a_warning_not_a_leak() {
        let mut warnings = Vec::new();
        let secrets = resolve_secrets(
            &Config::default(),
            &["DASHR_TEST_SURELY_UNSET_VAR".to_owned()],
            false,
            None,
            &mut warnings,
        );
        assert!(secrets.is_empty());
        assert!(warnings[0].contains("DASHR_TEST_SURELY_UNSET_VAR"));
    }

    #[test]
    fn provisioning_is_readable_by_the_container_user() {
        let dir = tempfile::tempdir().unwrap();
        let root = write_provisioning(dir.path(), "{}").unwrap();
        let file = root.join("datasources/dashr.yaml");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o644
            );
        }
    }

    #[test]
    fn start_fails_cleanly_without_docker() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let result = start(
            &Config::default(),
            &Identity::standalone("nodocker"),
            None,
            &store,
            &Docker::new("definitely-not-docker-xyz"),
            None,
        );
        assert!(matches!(
            result,
            Err(StartError::Docker(DockerError::Missing { .. }))
        ));
        assert!(store.list().is_empty());
    }
}

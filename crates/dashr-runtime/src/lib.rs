//! dashr's runtime: one session's Jaeger container, its API and viewer,
//! the poller that reads Jaeger, and the pull sources.

pub mod api;
pub mod code;
pub mod command;
pub mod docker;
pub mod http;
pub mod jaeger;
pub mod paths;
pub mod session;
pub mod state;

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dashr_core::Config;

pub use paths::Paths;
pub use session::{Registry, SessionRecord};
pub use state::Shared;

use docker::{Docker, RunSpec};
use jaeger::Jaeger;
use session::{now_ms, random_hex};
use state::Info;

/// A started session. Dropping it does not stop it; call [`Running::stop`].
pub struct Running {
    pub record: SessionRecord,
    pub shared: Arc<Shared>,
    /// The browser link, carrying the viewer token in its fragment so it
    /// never reaches a server log or a referrer.
    pub viewer_url: String,
    docker: Docker,
    registry: Registry,
}

/// The container name for a session.
pub fn container_name(session_id: &str) -> String {
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("dashr-{safe}")
}

/// Starts a session: Jaeger, the API and viewer, the poller, the record.
pub fn start(
    paths: &Paths,
    config: &Config,
    session_id: &str,
    pane_id: Option<String>,
    stop: Arc<AtomicBool>,
    progress: &dyn Fn(&str),
) -> Result<Running, String> {
    let docker = Docker::new(&config.jaeger.docker);
    docker
        .available()
        .map_err(|error| format!("Docker is not available: {error}"))?;
    if !docker.has_image(&config.jaeger.image) {
        progress(&format!(
            "pulling {} (first start only)…",
            config.jaeger.image
        ));
        docker.pull(&config.jaeger.image)?;
    }
    let name = container_name(session_id);
    let _ = docker.stop(&name);
    progress("starting Jaeger…");
    let ports = docker.start(&RunSpec {
        name: name.clone(),
        image: config.jaeger.image.clone(),
        session: session_id.to_owned(),
        bind: config.jaeger.bind.clone(),
        standard_ports: config.jaeger.standard_ports,
        memory: config.jaeger.memory.clone(),
    })?;
    let jaeger = Jaeger::new(ports.ui, ports.otlp_http);
    let deadline = Instant::now() + Duration::from_secs(90);
    while !jaeger.healthy() {
        if stop.load(Ordering::SeqCst) || Instant::now() > deadline {
            let _ = docker.stop(&name);
            return Err("Jaeger did not become ready within 90 seconds".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("cannot open the session port: {e}"))?;
    let api_port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let agent_token = random_hex(16);
    // Short, so the pane's link fits on one line of a narrow pane and the
    // terminal makes all of it clickable; it guards a loopback port only.
    let viewer_token = random_hex(6);
    let record = SessionRecord {
        session_id: session_id.to_owned(),
        pane_id,
        pid: std::process::id(),
        api_port,
        agent_token: agent_token.clone(),
        container: name,
        ports,
        bind: config.jaeger.bind.clone(),
        started_ms: now_ms(),
    };
    let info = Info {
        session_id: session_id.to_owned(),
        otlp_http: record.otlp_http(),
        otlp_grpc: record.otlp_grpc(),
        jaeger_ui: record.jaeger_ui(),
        started_ms: record.started_ms,
    };
    let shared = Shared::new(info, config.clone(), jaeger, Arc::clone(&stop));
    {
        let handler = api::handler(Arc::clone(&shared), agent_token, viewer_token.clone());
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || http::serve(listener, handler, stop));
    }
    {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || shared.poll_jaeger());
    }
    let registry = Registry::new(&paths.state_dir);
    registry
        .save(&record)
        .map_err(|e| format!("cannot write the session record: {e}"))?;
    Ok(Running {
        viewer_url: format!("http://127.0.0.1:{api_port}/#{viewer_token}"),
        record,
        shared,
        docker,
        registry,
    })
}

impl Running {
    /// Stops Jaeger (and with it every span) and removes the record.
    pub fn stop(self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        let _ = self.docker.stop(&self.record.container);
        self.registry.remove(&self.record.session_id);
    }
}

/// Whether a session's API answers as that session.
pub fn alive(record: &SessionRecord) -> bool {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .http_status_as_error(false)
        .build()
        .into();
    agent
        .get(format!("{}/api/ping", record.api_url()))
        .call()
        .ok()
        .and_then(|mut r| r.body_mut().read_json::<serde_json::Value>().ok())
        .is_some_and(|v| {
            v.get("session").and_then(serde_json::Value::as_str) == Some(record.session_id.as_str())
        })
}

/// Removes sessions whose process is gone: their records and containers
/// (a pane killed without a chance to clean up). Returns what it removed.
pub fn reap(paths: &Paths, config: &Config) -> Vec<String> {
    let registry = Registry::new(&paths.state_dir);
    let docker = Docker::new(&config.jaeger.docker);
    let mut live = Vec::new();
    let mut removed = Vec::new();
    for record in registry.list() {
        if alive(&record) {
            live.push(record.session_id.clone());
        } else {
            registry.remove(&record.session_id);
            removed.push(record.session_id.clone());
        }
    }
    if let Ok(containers) = docker.list() {
        for (name, session) in containers {
            if !live.contains(&session) && docker.stop(&name).is_ok() {
                removed.push(name);
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    #[test]
    fn container_names_are_docker_safe() {
        assert_eq!(
            super::container_name("a1b2c3d4-w1:p2"),
            "dashr-a1b2c3d4-w1-p2"
        );
    }
}

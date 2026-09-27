//! Actions, event hooks and the startup hook.

use dashr_core::Config;
use dashr_core::ids;
use dashr_core::session::SessionStore;
use dashr_docker::Docker;
use dashr_herdr::manifest::PLUGIN_ID;
use dashr_herdr::{Herdr, PluginEnv};
use dashr_runtime::Paths;

use crate::Result;

/// Environment passed to the dashboard pane by the `pipeline` action.
pub const PIPELINE_ENV: &str = "DASHR_PIPELINE_URL";
/// Set to `1` by the `otel` action: the pane starts in OpenTelemetry mode.
pub const OTEL_ENV: &str = "DASHR_OTEL";
/// Working directory for the chat pane, from the invoking context.
pub const ORIGIN_CWD_ENV: &str = "DASHR_ORIGIN_CWD";
/// Set to `1` by the Herdr actions: the pane opens its own chat pane with an
/// agent. A pane the human's AI session opened itself (`herdr plugin pane
/// open`, as the skill says) has no chat pane: that session drives it
/// (DEC-039).
pub const CHAT_ENV: &str = "DASHR_CHAT";

pub fn load_config(paths: &Paths) -> Result<Config> {
    Config::load_from_dir(&paths.config_dir).map_err(|error| error.to_string())
}

fn open_dashboard(
    env: &PluginEnv,
    herdr: &Herdr,
    pipeline: Option<&str>,
    otel: bool,
) -> Result<()> {
    let mut pane_env = vec![(CHAT_ENV.to_owned(), "1".to_owned())];
    if otel {
        pane_env.push((OTEL_ENV.to_owned(), "1".to_owned()));
    }
    if let Some(cwd) = env.focused_cwd() {
        pane_env.push((ORIGIN_CWD_ENV.to_owned(), cwd));
    }
    if let Some(url) = pipeline {
        pane_env.push((PIPELINE_ENV.to_owned(), url.to_owned()));
    }
    herdr
        .plugin_pane_open(PLUGIN_ID, "dashboard", "tab", &pane_env)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Finds the session the focused pane belongs to: its dashboard or chat pane.
fn focused_session(
    env: &PluginEnv,
    store: &SessionStore,
) -> Option<dashr_core::session::SessionRecord> {
    let focused = env.focused_pane()?;
    let hash = env.socket.as_deref().map(ids::socket_hash);
    store.list().into_iter().find(|record| {
        record.socket_hash == hash
            && (record.pane_id.as_deref() == Some(&focused)
                || record.chat_pane.as_deref() == Some(&focused))
    })
}

pub fn action(paths: &Paths, id: &str) -> Result<()> {
    let env = PluginEnv::from_process();
    let herdr = Herdr::new(&env.herdr_bin());
    match id {
        "open" => open_dashboard(&env, &herdr, None, false),
        "otel" => open_dashboard(&env, &herdr, None, true),
        "pipeline" => match env.clicked_url.as_deref() {
            Some(url) => {
                // Refuse early, in a notification, rather than in a pane that
                // opens only to say the URL was wrong.
                if let Err(error) = dashr_aws::url::parse(url) {
                    let _ = herdr.notify("dashr", Some(&error.to_string()), false);
                    return Err(error.to_string());
                }
                open_dashboard(&env, &herdr, Some(url), false)
            }
            None => {
                let message = "Ctrl-click a CodePipeline console URL to open its dashboard";
                let _ = herdr.notify("dashr", Some(message), false);
                Err(message.to_owned())
            }
        },
        "promote" => {
            let config = load_config(paths)?;
            let store = SessionStore::new(&paths.state_dir);
            let Some(record) = focused_session(&env, &store) else {
                let message = "focus a dashr dashboard or chat pane to promote its dashboard";
                let _ = herdr.notify("dashr", Some(message), false);
                return Err(message.to_owned());
            };
            let local = dashr_grafana::Client::local(&record.grafana_url());
            match dashr_runtime::promote::promote(&record, &local, config.promote.as_ref(), None) {
                Ok(promoted) => {
                    let _ = herdr.notify("dashr: dashboard promoted", Some(&promoted.url), false);
                    println!("{}", promoted.url);
                    Ok(())
                }
                Err(error) => {
                    let _ = herdr.notify("dashr: promote failed", Some(&error.to_string()), false);
                    Err(error.to_string())
                }
            }
        }
        "doctor" => herdr
            .call(&dashr_herdr::cli::argv::plugin_pane_open(
                PLUGIN_ID,
                "doctor",
                "popup",
                &[],
            ))
            .map(|_| ())
            .map_err(|error| error.to_string()),
        other => Err(format!("unknown action {other}")),
    }
}

/// The `pane.closed` hook: the second cleanup layer after the pane's own
/// signal handling (docs/DESIGN.md, lifecycle cleanup).
pub fn pane_closed(paths: &Paths) -> Result<()> {
    let env = PluginEnv::from_process();
    let Some(pane) = env.event_pane_id() else {
        return Ok(());
    };
    let Some(socket) = env.socket.as_deref() else {
        return Ok(());
    };
    let hash = ids::socket_hash(socket);
    let config = load_config(paths)?;
    let docker = Docker::new(&config.docker.command);
    let store = SessionStore::new(&paths.state_dir);
    if let Some(record) = store.find_by_pane(&hash, &pane) {
        let _ = dashr_runtime::session::stop(&record, &docker, &store);
        println!("stopped session {}", record.session_id);
    }
    // A container whose record was lost is still found by its labels.
    if let Ok(listed) = docker.list() {
        for container in listed.iter().filter(|c| c.socket == hash && c.pane == pane) {
            let _ = docker.stop(&container.name);
            println!("stopped {}", container.name);
        }
    }
    Ok(())
}

/// The startup hook: reap containers and records whose pane is gone.
pub fn startup(paths: &Paths) -> Result<()> {
    let env = PluginEnv::from_process();
    let Some(socket) = env.socket.as_deref() else {
        return Ok(());
    };
    let herdr = Herdr::new(&env.herdr_bin());
    let live = herdr.pane_list().map_err(|error| error.to_string())?;
    let config = load_config(paths)?;
    let docker = Docker::new(&config.docker.command);
    let reaped = reap(
        &docker,
        &SessionStore::new(&paths.state_dir),
        &ids::socket_hash(socket),
        &live,
    );
    println!("reaped {reaped} orphaned session(s)");
    Ok(())
}

/// Stops this server's containers and records whose pane is not in `live`.
pub fn reap(docker: &Docker, store: &SessionStore, socket_hash: &str, live: &[String]) -> usize {
    let mut reaped = 0;
    for record in store.list() {
        let orphaned = record.socket_hash.as_deref() == Some(socket_hash)
            && record
                .pane_id
                .as_ref()
                .is_some_and(|pane| !live.contains(pane));
        if orphaned {
            let _ = dashr_runtime::session::stop(&record, docker, store);
            reaped += 1;
        }
    }
    if let Ok(listed) = docker.list() {
        for container in dashr_docker::orphans(&listed, socket_hash, live) {
            if docker.stop(&container.name).is_ok() {
                reaped += 1;
            }
        }
    }
    reaped
}

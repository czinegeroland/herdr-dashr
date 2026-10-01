//! Herdr's hooks and actions.

use dashr_core::Config;
use dashr_herdr::manifest::PLUGIN_ID;
use dashr_herdr::{Herdr, PluginEnv};
use dashr_runtime::Paths;

use crate::pane::pane_session_id;

/// `herdr startup`: removes sessions a crash or a kill left behind.
pub fn startup(paths: &Paths, config: &Config) -> Result<(), String> {
    for removed in dashr_runtime::reap(paths, config) {
        println!("dashr: removed stale {removed}");
    }
    Ok(())
}

/// `pane.closed`: Windows has no SIGHUP, so the closed pane's container is
/// stopped here (on Unix the pane stopped it already; this is a no-op).
pub fn pane_closed(paths: &Paths, config: &Config) -> Result<(), String> {
    let env = PluginEnv::from_process();
    let (Some(pane), Some(socket)) = (env.event_pane_id(), env.socket.clone()) else {
        return Ok(());
    };
    let session = pane_session_id(&socket, &pane);
    let docker = dashr_runtime::docker::Docker::new(&config.jaeger.docker);
    let _ = docker.stop(&dashr_runtime::container_name(&session));
    dashr_runtime::Registry::new(&paths.state_dir).remove(&session);
    Ok(())
}

pub fn action(id: &str) -> Result<(), String> {
    let env = PluginEnv::from_process();
    let herdr = Herdr::new(&env.herdr_bin());
    let entrypoint = match id {
        "open" => "traces",
        "doctor" => "doctor",
        other => return Err(format!("unknown action {other:?}")),
    };
    let result = match (entrypoint, env.focused_pane()) {
        ("traces", Some(target)) => herdr
            .call(&dashr_herdr::cli::argv::plugin_pane_split(
                PLUGIN_ID,
                "traces",
                &target,
                "right",
                &[],
            ))
            .map(|_| ()),
        (entrypoint, _) => herdr
            .plugin_pane_open(
                PLUGIN_ID,
                entrypoint,
                if entrypoint == "doctor" {
                    "popup"
                } else {
                    "tab"
                },
                &[],
            )
            .map(|_| ()),
    };
    result.map_err(|error| error.to_string())
}

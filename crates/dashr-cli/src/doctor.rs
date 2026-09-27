//! Prerequisite checks (requirement DASHR-HERDR-009).

use dashr_docker::Docker;
use dashr_runtime::Paths;
use dashr_runtime::browser::on_path;

use crate::Result;
use crate::herdr_cmds::load_config;

/// One check: what, whether it passed, and what to do otherwise.
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub required: bool,
    pub detail: String,
}

pub fn checks(paths: &Paths) -> Vec<Check> {
    let mut checks = Vec::new();
    let config = match load_config(paths) {
        Ok(config) => {
            checks.push(Check {
                name: "configuration".into(),
                ok: true,
                required: true,
                detail: format!(
                    "{}",
                    paths
                        .config_dir
                        .join(dashr_core::config::FILE_NAME)
                        .display()
                ),
            });
            config
        }
        Err(error) => {
            checks.push(Check {
                name: "configuration".into(),
                ok: false,
                required: true,
                detail: error,
            });
            dashr_core::Config::default()
        }
    };
    let docker = Docker::new(&config.docker.command);
    let docker_ok = docker.available();
    checks.push(Check {
        name: "docker".into(),
        ok: docker_ok.is_ok(),
        required: true,
        detail: match &docker_ok {
            Ok(()) => "daemon reachable".into(),
            Err(error) => error.to_string(),
        },
    });
    if docker_ok.is_ok() {
        let has = docker.has_image(&config.grafana.image);
        checks.push(Check {
            name: "grafana image".into(),
            ok: has,
            required: false,
            detail: if has {
                config.grafana.image.clone()
            } else {
                format!(
                    "{} is pulled on first use (docker pull {})",
                    config.grafana.image, config.grafana.image
                )
            },
        });
    }
    let herdr = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    checks.push(tool(
        "herdr",
        &herdr,
        true,
        "https://herdr.dev/docs/install/",
    ));
    if let Some(agent) = config.agent.command.first() {
        checks.push(tool(
            "agent",
            agent,
            false,
            "the chat pane runs this command",
        ));
    }
    checks.push(tool(
        "aws",
        &config.aws.cli,
        false,
        "optional: needed for CodePipeline and CloudWatch",
    ));
    checks
}

fn tool(name: &str, command: &str, required: bool, hint: &str) -> Check {
    let ok = on_path(command);
    Check {
        name: name.into(),
        ok,
        required,
        detail: if ok {
            format!("{command} found")
        } else {
            format!("{command} not found — {hint}")
        },
    }
}

/// Prints the checks; fails when a required one fails.
pub fn run(paths: &Paths) -> Result<Vec<Check>> {
    let checks = checks(paths);
    for check in &checks {
        let mark = match (check.ok, check.required) {
            (true, _) => "\x1b[32m✓\x1b[0m",
            (false, true) => "\x1b[31m✗\x1b[0m",
            (false, false) => "\x1b[33m!\x1b[0m",
        };
        println!("{mark} {:<17} {}", check.name, check.detail);
    }
    if checks.iter().any(|check| check.required && !check.ok) {
        Err("a required prerequisite is missing".into())
    } else {
        Ok(checks)
    }
}

/// The doctor popup: print, then wait so the person can read it.
pub fn pane(paths: &Paths) -> Result<()> {
    let result = run(paths);
    println!("\nPress Enter to close.");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    result.map(|_| ())
}

#[cfg(all(test, unix))] // uses `sh`
mod tests {
    use super::*;

    #[test]
    fn reports_missing_tools_with_hints() {
        let check = tool("x", "definitely-not-a-tool-xyz", false, "hint here");
        assert!(!check.ok);
        assert!(check.detail.contains("hint here"));
        assert!(tool("sh", "sh", true, "").ok);
    }
}

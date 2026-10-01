//! Prerequisite checks (DASHR-SESSION-005).

use dashr_core::Config;
use dashr_runtime::Paths;
use dashr_runtime::docker::Docker;

pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub required: bool,
    pub detail: String,
}

fn on_path(program: &str) -> bool {
    let names: Vec<String> = if cfg!(windows) {
        ["exe", "cmd", "bat"]
            .iter()
            .map(|ext| format!("{program}.{ext}"))
            .collect()
    } else {
        vec![program.to_owned()]
    };
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|dir| names.iter().any(|n| dir.join(n).is_file()))
        })
        .unwrap_or(false)
}

pub fn checks(paths: &Paths) -> Vec<Check> {
    let mut checks = Vec::new();
    let config = match Config::load_from_dir(&paths.config_dir) {
        Ok(config) => {
            checks.push(Check {
                name: "configuration",
                ok: true,
                required: true,
                detail: paths
                    .config_dir
                    .join(dashr_core::config::FILE_NAME)
                    .display()
                    .to_string(),
            });
            config
        }
        Err(error) => {
            checks.push(Check {
                name: "configuration",
                ok: false,
                required: true,
                detail: error,
            });
            Config::default()
        }
    };
    let docker = Docker::new(&config.jaeger.docker);
    let available = docker.available();
    checks.push(Check {
        name: "docker",
        ok: available.is_ok(),
        required: true,
        detail: available.err().unwrap_or_else(|| "daemon reachable".into()),
    });
    let has = docker.has_image(&config.jaeger.image);
    checks.push(Check {
        name: "jaeger image",
        ok: has,
        required: false,
        detail: if has {
            config.jaeger.image.clone()
        } else {
            format!("{} is pulled on the first start", config.jaeger.image)
        },
    });
    for (program, why) in [
        (
            "herdr",
            "the trace pane runs in Herdr (`dashr serve` works without it)",
        ),
        ("aws", "pull sources from AWS X-Ray"),
        ("az", "pull sources from Azure Application Insights"),
        ("gcloud", "pull sources from Google Cloud Trace"),
    ] {
        let ok = on_path(program);
        checks.push(Check {
            name: program_name(program),
            ok,
            required: false,
            detail: if ok {
                "on PATH".into()
            } else {
                format!("not found: {why}")
            },
        });
    }
    checks
}

fn program_name(program: &str) -> &'static str {
    match program {
        "herdr" => "herdr",
        "aws" => "aws cli",
        "az" => "azure cli",
        _ => "gcloud cli",
    }
}

/// Prints the checks; an error when a required one fails.
pub fn run(paths: &Paths) -> Result<(), String> {
    let checks = checks(paths);
    for check in &checks {
        let mark = match (check.ok, check.required) {
            (true, _) => "\x1b[32m✓\x1b[0m",
            (false, true) => "\x1b[31m✗\x1b[0m",
            (false, false) => "\x1b[33m-\x1b[0m",
        };
        println!("{mark} {:<14} {}", check.name, check.detail);
    }
    if checks.iter().any(|c| c.required && !c.ok) {
        Err("a required check failed".into())
    } else {
        Ok(())
    }
}

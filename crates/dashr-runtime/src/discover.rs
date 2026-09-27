//! `dashr discover`: what is running here and what can be measured
//! (DEC-043).
//!
//! The agent's first step towards a dashboard. It reports facts — container
//! names, images, kinds, published ports, which log lines look like
//! requests, which tools and cloud CLIs are installed, which project files
//! are present — and the `dashr collect` commands that would make the data
//! live. It never returns a data value or a log line: a container's recent
//! log is only counted and classified.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dashr_core::collect::{image_kind, parse_request, prometheus_text, published_ports};
use serde_json::{Value, json};

use crate::collect::run_command;

/// Whether `command` is on `PATH`, trying Windows' executable extensions.
pub fn which(command: &str) -> bool {
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|e| e.to_ascii_lowercase())
            .chain(std::iter::once(String::new()))
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|dir| {
                extensions
                    .iter()
                    .any(|ext| dir.join(format!("{command}{ext}")).is_file())
            })
        })
        .unwrap_or(false)
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Tools the agent can build on, and which clouds look configured.
fn tools() -> Value {
    let names = [
        "docker",
        "kubectl",
        "helm",
        "aws",
        "az",
        "gcloud",
        "doctl",
        "flyctl",
        "psql",
        "mysql",
        "redis-cli",
        "dotnet",
        "node",
        "python3",
        "java",
        "go",
    ];
    let installed: Vec<&str> = names.iter().copied().filter(|n| which(n)).collect();
    let home = home();
    let configured = |parts: &[&str]| {
        home.as_ref()
            .map(|h| {
                parts
                    .iter()
                    .fold(h.clone(), |p, part| p.join(part))
                    .exists()
            })
            .unwrap_or(false)
    };
    json!({
        "installed": installed,
        "configured": {
            "aws": configured(&[".aws"]) || std::env::var_os("AWS_ACCESS_KEY_ID").is_some(),
            "azure": configured(&[".azure"]),
            "gcloud": configured(&[".config", "gcloud"]) || configured(&["AppData", "Roaming", "gcloud"]),
            "kubernetes": configured(&[".kube", "config"]) || std::env::var_os("KUBECONFIG").is_some()
        }
    })
}

/// Project files at the top of `dir` that say how the thing is built or
/// deployed. Names only.
fn project(dir: &Path) -> Value {
    let known = [
        "Dockerfile",
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
        "package.json",
        "pom.xml",
        "build.gradle",
        "go.mod",
        "Cargo.toml",
        "requirements.txt",
        "pyproject.toml",
        "Chart.yaml",
        "serverless.yml",
        "template.yaml",
        "samconfig.toml",
        "cdk.json",
        "azure.yaml",
        "host.json",
        "app.yaml",
        "cloudbuild.yaml",
        "fly.toml",
        "vercel.json",
        "netlify.toml",
        "Procfile",
    ];
    let mut found: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().to_string();
            let lower = name.to_ascii_lowercase();
            if known.iter().any(|k| k.eq_ignore_ascii_case(&name))
                || [".csproj", ".sln", ".tf", ".bicep"]
                    .iter()
                    .any(|ext| lower.ends_with(ext))
                || ["k8s", "kubernetes", "helm", "terraform", "infra", ".github"]
                    .contains(&lower.as_str())
            {
                found.push(name);
            }
        }
    }
    found.sort();
    json!({"directory": dir.display().to_string(), "files": found})
}

fn host() -> Value {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    system.refresh_cpu_list(sysinfo::CpuRefreshKind::nothing());
    json!({
        "os": sysinfo::System::long_os_version().unwrap_or_default(),
        "cpus": system.cpus().len(),
        "memory_bytes": system.total_memory()
    })
}

/// Counts request-shaped lines in a container's recent log. Returns the
/// count and the formats seen; the lines themselves stay here.
fn request_lines(container: &str) -> (usize, Vec<&'static str>) {
    let args: Vec<String> = ["docker", "logs", "--tail", "300", container]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    // `docker logs` writes the container's stderr to its own stderr: merge.
    let text = match std::process::Command::new(&args[0])
        .args(&args[1..])
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            text
        }
        Err(_) => return (0, Vec::new()),
    };
    let mut formats: Vec<&'static str> = Vec::new();
    let mut count = 0;
    for line in text.lines() {
        if let Some(request) = parse_request(line) {
            count += 1;
            if !formats.contains(&request.format) {
                formats.push(request.format);
            }
        }
    }
    (count, formats)
}

/// Whether `http://127.0.0.1:<port>/metrics` answers Prometheus text.
fn metrics_endpoint(port: u16) -> Option<String> {
    let url = format!("http://127.0.0.1:{port}/metrics");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(800)))
        .build()
        .into();
    let text = agent
        .get(&url)
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;
    (!prometheus_text(&text).is_empty() && text.contains("# TYPE")).then_some(url)
}

fn labels(text: &str) -> BTreeMap<String, String> {
    text.split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

fn containers() -> Value {
    let Ok(text) = run_command(
        &[
            "docker".into(),
            "ps".into(),
            "--format".into(),
            "{{json .}}".into(),
        ],
        Duration::from_secs(10),
    ) else {
        return json!({"available": false});
    };
    let mut list = Vec::new();
    let mut suggestions: Vec<String> = Vec::new();
    for line in text.lines() {
        let Ok(c) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let field = |key: &str| c.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
        let name = field("Names");
        if name.is_empty() {
            continue;
        }
        let image = field("Image");
        let kind = image_kind(&image);
        if kind == "observability" && name.starts_with("herdr-grafana-") {
            continue; // dashr's own Grafana
        }
        let ports = published_ports(&field("Ports"));
        let compose = labels(&field("Labels")).remove("com.docker.compose.project");
        let (requests, formats) = request_lines(&name);
        let metrics: Vec<String> = ports.iter().filter_map(|p| metrics_endpoint(*p)).collect();
        let mut collect = vec![format!("dashr collect logs {name}")];
        match kind {
            "postgres" => collect.push(format!("dashr collect postgres {name}")),
            "mysql" => collect.push(format!("dashr collect mysql {name}")),
            "redis" => collect.push(format!("dashr collect redis {name}")),
            _ => {}
        }
        for url in &metrics {
            collect.push(format!("dashr collect scrape {url} --service {name}"));
        }
        suggestions.extend(collect.iter().cloned());
        list.push(json!({
            "name": name,
            "image": image,
            "kind": kind,
            "status": field("Status"),
            "published_ports": ports,
            "compose_project": compose,
            "request_lines_in_recent_log": requests,
            "request_log_formats": formats,
            "metrics_endpoints": metrics,
            "collect": collect
        }));
    }
    if !list.is_empty() {
        suggestions.insert(0, "dashr collect docker".into());
    }
    json!({"available": true, "containers": list, "collect": suggestions})
}

/// Everything discovery found, as JSON.
pub fn discover(cwd: &Path) -> Value {
    let docker = if which("docker") {
        containers()
    } else {
        json!({"available": false})
    };
    let mut collect: Vec<Value> = docker
        .get("collect")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    collect.push(json!("dashr collect host"));
    json!({
        "host": host(),
        "tools": tools(),
        "project": project(cwd),
        "docker": docker,
        "collect": collect,
        "next": "Enable the collectors that match what the human asked about, then build the system dashboard (see the herdr-dashr skill). For anything not local (a cloud, Kubernetes, a remote host), use the installed CLIs to discover it and feed it with `dashr collect exec` and `dashr collect stream`."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_files_are_named_not_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("docker-compose.yml"), "secret: x").unwrap();
        std::fs::write(dir.path().join("App.csproj"), "").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("terraform")).unwrap();
        let found = project(dir.path());
        let files: Vec<&str> = found["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(files, ["App.csproj", "docker-compose.yml", "terraform"]);
        assert!(!found.to_string().contains("secret"));
    }

    #[test]
    fn discovery_always_offers_the_host_collector() {
        let found = discover(Path::new("."));
        assert!(
            found["collect"]
                .as_array()
                .unwrap()
                .contains(&json!("dashr collect host"))
        );
        assert!(found["host"]["cpus"].as_u64().unwrap() > 0);
    }
}

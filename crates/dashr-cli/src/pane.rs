//! The dashboard pane: owns Grafana for as long as it lives.
//!
//! Start Grafana, split the chat pane off below, show the dashboard (browser
//! or text view), run the monitor loop, and on exit — including the SIGHUP
//! Herdr sends when the pane closes — stop everything and delete every file
//! (requirements DASHR-HERDR-002, DASHR-GRAF-006, DASHR-VIEW-001/003).

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashr_core::Config;
use dashr_core::masking::Masker;
use dashr_core::session::{SessionRecord, SessionStore};
use dashr_docker::Docker;
use dashr_herdr::cli::AgentState;
use dashr_herdr::{Herdr, PluginEnv};
use dashr_runtime::Paths;
use dashr_runtime::browser::Browser;
use dashr_runtime::monitor::{self, Reporter, Tick};
use dashr_runtime::session::{self, Identity, Started};
use dashr_runtime::status::PanelState;
use serde_json::json;

use crate::Result;
use crate::herdr_cmds::{ORIGIN_CWD_ENV, OTEL_ENV, PIPELINE_ENV, load_config};

/// Reports monitor ticks to Herdr for one pane.
struct HerdrReporter {
    herdr: Herdr,
    pane: String,
    ttl_ms: u64,
}

impl Reporter for HerdrReporter {
    fn token(&mut self, text: &str) {
        let _ = self.herdr.report_token(&self.pane, text, Some(self.ttl_ms));
    }
    fn blocked(&mut self, message: &str) {
        let _ = self
            .herdr
            .report_agent(&self.pane, AgentState::Blocked, Some(message));
    }
    fn clear(&mut self) {
        let _ = self.herdr.report_agent(&self.pane, AgentState::Idle, None);
    }
    fn notify(&mut self, title: &str, body: &str) {
        let _ = self.herdr.notify(title, Some(body), true);
    }
}

/// The MCP configuration Claude Code (and most MCP clients) read.
pub fn mcp_config(
    record: &SessionRecord,
    config: &Config,
    paths: &Paths,
    exe: &Path,
    herdr_bin: &str,
) -> serde_json::Value {
    let mut servers = serde_json::Map::new();
    servers.insert(
        "dashr".into(),
        json!({
            "type": "stdio",
            "command": exe.display().to_string(),
            "args": [
                "--config-dir", paths.config_dir.display().to_string(),
                "--state-dir", paths.state_dir.display().to_string(),
                "mcp", "--session", record.session_id, "--herdr-bin", herdr_bin
            ]
        }),
    );
    if config.agent.mcp_grafana {
        servers.insert(
            "grafana".into(),
            json!({
                "type": "stdio",
                "command": config.agent.mcp_grafana_command,
                "args": [],
                "env": {"GRAFANA_URL": record.grafana_url()}
            }),
        );
    }
    json!({"mcpServers": servers})
}

/// The opening prompt for the agent.
pub fn opening_prompt(started: &Started) -> String {
    let mut prompt = String::from(
        "You are working in a herdr-dashr session: the pane above shows a live Grafana dashboard that only the human sees. \
Use the dashr MCP tools and follow their privacy rules.",
    );
    match &started.inventory {
        Some(Ok(inventory)) => prompt.push_str(&format!(
            " A first dashboard for CodePipeline {} ({}) is already applied: {} log groups, {} queues, {} state machines, {} Lambdas. \
Call panel_status, fix what is empty or failing, then ask the human what they are debugging.",
            inventory.pipeline.name,
            inventory.pipeline.region,
            inventory.resources.all_log_groups().len(),
            inventory.resources.queues.len(),
            inventory.resources.state_machines.len(),
            inventory.resources.lambdas.len(),
        )),
        Some(Err(_)) => prompt.push_str(
            " Inspecting the pipeline failed (see the dashboard's text panel). Ask the human for what to look at.",
        ),
        None if started.record.otlp.is_some() => {}
        None => prompt.push_str(" Call list_datasources, then ask the human what they want to see."),
    }
    if let Some(otlp) = &started.record.otlp {
        prompt.push_str(&format!(
            " This session also receives OpenTelemetry: logs, traces and metrics sent to {} (OTLP/HTTP; OTEL_EXPORTER_OTLP_ENDPOINT is set in your shell) land in its Loki, Tempo and Prometheus. \
`dashr tail -- <command>` ships any command's output as logs. When the human wants to check that the right log messages fire, arm them with expect_logs and read the verdict with log_expectations. \
Ask the human what they are testing.",
            otlp.http_endpoint()
        ));
    }
    prompt
}

/// Substitutes `{mcp_config}` and `{prompt}` in the agent argv template.
pub fn agent_argv(template: &[String], mcp_config: &Path, prompt: &str) -> Vec<String> {
    template
        .iter()
        .map(|argument| {
            argument
                .replace("{mcp_config}", &mcp_config.display().to_string())
                .replace("{prompt}", prompt)
        })
        .collect()
}

fn render_text_view(out: &mut impl Write, record: &SessionRecord, tick: Option<&Tick>, note: &str) {
    let _ = write!(out, "\x1b[2J\x1b[H");
    let _ = writeln!(out, "\x1b[1mdashr\x1b[0m  {}", record.session_id);
    let _ = writeln!(out);
    let _ = writeln!(out, "Dashboard: {}", record.kiosk_url());
    let _ = writeln!(out, "           (Ctrl-click to open it in your browser)");
    if let Some(pipeline) = &record.pipeline {
        let _ = writeln!(out, "Pipeline:  {pipeline}");
    }
    if let Some(otlp) = &record.otlp {
        let _ = writeln!(
            out,
            "OTLP:      {} (HTTP) · {} (gRPC)",
            otlp.http_endpoint(),
            otlp.grpc_endpoint()
        );
        let _ = writeln!(
            out,
            "           dashr tail -- <command>   ships its output here"
        );
    }
    let _ = writeln!(out, "{note}");
    let _ = writeln!(out);
    match tick {
        None => {
            let _ = writeln!(out, "checking panels…");
        }
        Some(tick) if tick.error.is_some() => {
            let _ = writeln!(out, "Grafana: {}", tick.error.as_deref().unwrap_or(""));
        }
        Some(tick) => {
            let _ = writeln!(out, "Panels: {}", tick.summary.token());
            for panel in &tick.panels {
                let mark = match panel.state {
                    PanelState::Ok => "\x1b[32m●\x1b[0m",
                    PanelState::Empty => "\x1b[33m○\x1b[0m",
                    PanelState::Error => "\x1b[31m✗\x1b[0m",
                    PanelState::NoQueries => " ",
                };
                let error = panel
                    .targets
                    .iter()
                    .find_map(|target| target.error.as_deref())
                    .map(|error| format!("  {}", error.chars().take(60).collect::<String>()))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    " {mark} #{:<3} {:<40} {:>6} rows{error}",
                    panel.panel_id,
                    panel.title.chars().take(40).collect::<String>(),
                    panel.rows
                );
            }
            for alert in &tick.breached {
                let _ = writeln!(out, " \x1b[31m⚠ {alert}\x1b[0m");
            }
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Close this pane to stop Grafana and delete everything."
    );
    let _ = out.flush();
}

fn wait(stop: &AtomicBool, duration: Duration) {
    let started = Instant::now();
    while started.elapsed() < duration && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub fn dashboard(paths: &Paths) -> Result<()> {
    let env = PluginEnv::from_process();
    let pane_id = env
        .pane_id
        .clone()
        .ok_or("HERDR_PANE_ID is not set; run this from Herdr")?;
    let socket = env
        .socket
        .clone()
        .ok_or("HERDR_SOCKET_PATH is not set; run this from Herdr")?;
    let herdr_bin = env.herdr_bin();
    let herdr = Herdr::new(&herdr_bin);
    let mut config = load_config(paths)?;
    if std::env::var(OTEL_ENV).is_ok_and(|value| value == "1") {
        config.otel.enabled = true;
    }
    let store = SessionStore::new(&paths.state_dir);
    let docker = Docker::new(&config.docker.command);
    let aws = dashr_aws::cli::AwsCli::new(&config.aws.cli, config.aws.profile.as_deref());
    let identity = Identity::herdr(&socket, &pane_id);

    // Signals first: a pane closed during startup still cleans up.
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .map_err(|error| error.to_string())?;
    }

    let pipeline_url = std::env::var(PIPELINE_ENV)
        .ok()
        .filter(|url| !url.is_empty());
    let pipeline = match pipeline_url.as_deref().map(dashr_aws::url::parse) {
        Some(Ok(pipeline)) => Some(pipeline),
        Some(Err(error)) => return Err(error.to_string()),
        None => None,
    };

    println!(
        "dashr: starting a disposable Grafana ({})…",
        if config.otel.enabled {
            &config.otel.image
        } else {
            &config.grafana.image
        }
    );
    let _ = herdr.report_agent(&pane_id, AgentState::Working, Some("starting Grafana"));
    let started = match session::start(
        &config,
        &identity,
        pipeline.as_ref(),
        &store,
        &docker,
        Some(&aws),
    ) {
        Ok(started) => started,
        Err(error) => {
            let _ = herdr.report_agent(
                &pane_id,
                AgentState::Blocked,
                Some("Grafana failed to start"),
            );
            eprintln!("dashr: {error}");
            eprintln!(
                "Run `dashr doctor` (or the \"Check dashr prerequisites\" action). Press Enter to close."
            );
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            let _ = herdr.release_agent(&pane_id);
            return Err(error.to_string());
        }
    };
    for warning in &started.warnings {
        println!("dashr: {warning}");
    }
    let mut record = started.record.clone();

    // Keep the dashboard-building skill current before the agent starts
    // (DASHR-SKILL-002). Best effort: a failure is reported, not fatal.
    if config.agent.enabled && config.agent.install_skill {
        for dir in &config.agent.skill_dirs {
            let skills_dir = dashr_runtime::skill::expand_home(dir);
            match dashr_runtime::skill::install(&skills_dir, false) {
                Ok(dashr_runtime::skill::Outcome::UpToDate(_)) => {}
                Ok(outcome) => println!("dashr: {outcome}"),
                Err(error) => println!(
                    "dashr: skill not installed in {}: {error}",
                    skills_dir.display()
                ),
            }
        }
    }

    // Chat pane.
    if config.agent.enabled && !stop.load(Ordering::SeqCst) {
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let mcp_path = record.runtime_dir.join("mcp.json");
        let mcp = mcp_config(&record, &config, paths, &exe, &herdr_bin);
        std::fs::write(
            &mcp_path,
            serde_json::to_vec_pretty(&mcp).unwrap_or_default(),
        )
        .map_err(|error| error.to_string())?;
        let argv = agent_argv(&config.agent.command, &mcp_path, &opening_prompt(&started));
        let cwd = std::env::var(ORIGIN_CWD_ENV)
            .ok()
            .filter(|cwd| Path::new(cwd).is_dir());
        let mut split_env = vec![("DASHR_SESSION".to_owned(), record.session_id.clone())];
        // Programs started from the chat pane export to this session.
        if let Some(otlp) = &record.otlp {
            split_env.push((
                "OTEL_EXPORTER_OTLP_ENDPOINT".to_owned(),
                otlp.http_endpoint(),
            ));
            split_env.push((
                "OTEL_EXPORTER_OTLP_PROTOCOL".to_owned(),
                "http/protobuf".to_owned(),
            ));
        }
        // Herdr's ratio is the share the split pane keeps (observed, 0.9.1).
        match herdr.pane_split(
            &pane_id,
            "down",
            config.agent.split_ratio,
            cwd.as_deref(),
            &split_env,
        ) {
            Ok(chat) => {
                // The shell needs a moment to draw its prompt before input.
                std::thread::sleep(Duration::from_millis(300));
                if let Err(error) = herdr.pane_run(&chat, &dashr_core::shell::join(&argv)) {
                    println!("dashr: could not start the agent: {error}");
                }
                record.chat_pane = Some(chat);
                let _ = store.save(&record);
            }
            Err(error) => println!("dashr: could not open the chat pane: {error}"),
        }
    }
    let _ = herdr.report_agent(&pane_id, AgentState::Idle, None);

    // Monitor loop.
    let latest: Arc<Mutex<Option<Tick>>> = Arc::new(Mutex::new(None));
    let monitor_thread = {
        let stop = Arc::clone(&stop);
        let latest = Arc::clone(&latest);
        let record = record.clone();
        let store = store.clone();
        let masker = Masker::new(&config.masking);
        let interval = Duration::from_secs(config.monitor.interval_secs);
        let notify = config.monitor.notify;
        let mut reporter = HerdrReporter {
            herdr: herdr.clone(),
            pane: pane_id.clone(),
            ttl_ms: (config.monitor.interval_secs * 4 * 1000).max(10_000),
        };
        std::thread::spawn(move || {
            let client = dashr_grafana::Client::local(&record.grafana_url());
            while !stop.load(Ordering::SeqCst) {
                let tick = monitor::tick(&record, &client, &masker, &store);
                monitor::report(&tick, &mut reporter, notify);
                if let Ok(mut slot) = latest.lock() {
                    *slot = Some(tick);
                }
                wait(&stop, interval);
            }
        })
    };

    // View.
    let browser = Browser::new(&config.browser.command);
    let mut note = String::new();
    if config.browser.enabled && browser.installed() {
        match browser
            .open_command(&record.kiosk_url(), &record.runtime_dir)
            .spawn()
        {
            Ok(mut child) => loop {
                if stop.load(Ordering::SeqCst) {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => {
                        note = "terminal-browser exited; showing the text view.".into();
                        break;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                }
            },
            Err(error) => note = format!("terminal-browser failed to start: {error}"),
        }
    } else if config.browser.enabled {
        note = "terminal-browser is not installed (https://github.com/zenbu-labs/terminal-browser); text view.".into();
    }
    while !stop.load(Ordering::SeqCst) {
        let tick = latest.lock().ok().and_then(|slot| slot.clone());
        render_text_view(&mut std::io::stdout(), &record, tick.as_ref(), &note);
        wait(&stop, Duration::from_secs(2));
    }

    // Teardown.
    let _ = monitor_thread.join();
    if let Some(chat) = &record.chat_pane {
        let _ = herdr.pane_close(chat);
    }
    let _ = herdr.release_agent(&pane_id);
    let result = session::stop(&record, &docker, &store);
    println!("dashr: stopped {}", record.container);
    result.map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> SessionRecord {
        serde_json::from_value(json!({
            "session_id": "abcd-w1-p1", "pane_id": "w1:p1", "socket_hash": "abcd", "container": "herdr-grafana-abcd-w1-p1",
            "port": 32768, "dashboard_uid": "dashr-abcd-w1-p1", "runtime_dir": "/dev/shm/herdr-dashr/abcd-w1-p1",
            "refresh": "5s", "datasources": [], "started_unix": 0
        }))
        .unwrap()
    }

    #[test]
    fn mcp_config_names_the_session_and_paths() {
        let paths = Paths {
            config_dir: "/c".into(),
            state_dir: "/s".into(),
        };
        let config = Config::default();
        let value = mcp_config(
            &record(),
            &config,
            &paths,
            Path::new("/p/bin/dashr"),
            "/usr/bin/herdr",
        );
        let server = &value["mcpServers"]["dashr"];
        assert_eq!(server["command"], "/p/bin/dashr");
        let args: Vec<&str> = server["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert_eq!(
            args,
            vec![
                "--config-dir",
                "/c",
                "--state-dir",
                "/s",
                "mcp",
                "--session",
                "abcd-w1-p1",
                "--herdr-bin",
                "/usr/bin/herdr"
            ]
        );
        assert!(
            value["mcpServers"].get("grafana").is_none(),
            "mcp-grafana is opt-in"
        );
        let mut config = Config::default();
        config.agent.mcp_grafana = true;
        let value = mcp_config(&record(), &config, &paths, Path::new("/x"), "herdr");
        assert_eq!(
            value["mcpServers"]["grafana"]["env"]["GRAFANA_URL"],
            "http://127.0.0.1:32768"
        );
    }

    #[test]
    fn agent_argv_substitutes_placeholders() {
        let argv = agent_argv(
            &Config::default().agent.command,
            Path::new("/r/mcp.json"),
            "hi 'there'",
        );
        assert_eq!(
            argv,
            vec!["claude", "hi 'there'", "--mcp-config", "/r/mcp.json"]
        );
        assert_eq!(
            dashr_core::shell::join(&argv),
            r"claude 'hi '\''there'\''' --mcp-config /r/mcp.json"
        );
    }

    #[test]
    fn text_view_shows_url_panels_and_alerts_but_no_values() {
        let tick = Tick {
            summary: dashr_runtime::status::Summary {
                ok: 1,
                empty: 0,
                error: 1,
            },
            panels: vec![],
            breached: vec!["DLQ not empty".into()],
            ..Tick::default()
        };
        let mut out = Vec::new();
        render_text_view(&mut out, &record(), Some(&tick), "note");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("http://127.0.0.1:32768/d/dashr-abcd-w1-p1?orgId=1&kiosk&refresh=5s")
        );
        assert!(text.contains("1 ok · 1 err"));
        assert!(text.contains("DLQ not empty"));
    }
}

//! The dashboard pane: owns Grafana for as long as it lives.
//!
//! Start Grafana, split the chat pane off below when an action asked for
//! one, show the dashboard link, run the monitor loop, and on exit — including the SIGHUP
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
use dashr_runtime::monitor::{self, Reporter, Tick};
use dashr_runtime::session::{self, Identity, Started};
use serde_json::json;

use crate::Result;
use crate::herdr_cmds::{CHAT_ENV, ORIGIN_CWD_ENV, OTEL_ENV, PIPELINE_ENV, load_config};

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

/// The briefing file in a session's runtime directory: what `dashr wait`
/// hands the AI session that opened the pane (DEC-039).
pub const BRIEF_FILE: &str = "brief.txt";

/// Who reads the opening briefing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reader {
    /// The agent in the chat pane this pane opened, with the MCP tools.
    ChatPane,
    /// The human's own AI session, which opened this pane beside itself and
    /// drives it with `dashr tool`.
    Opener,
}

/// The opening prompt for the agent.
pub fn opening_prompt(started: &Started, saved: &[String]) -> String {
    briefing(started, saved, Reader::ChatPane)
}

/// The opening briefing for `reader`.
pub fn briefing(started: &Started, saved: &[String], reader: Reader) -> String {
    let mut prompt = String::from(match reader {
        Reader::ChatPane => {
            "You are working in a herdr-dashr session: the pane above shows a live Grafana dashboard that only the human sees. \
Use the dashr MCP tools and follow their privacy rules."
        }
        Reader::Opener => {
            "The dashboard pane is open beside you: a narrow column with the link to a live Grafana dashboard that the human opens in their browser and only they see. \
Drive it with `dashr tool <name> --session <session> --args '<json>'` (tool names as in the herdr-dashr skill) and follow the skill's privacy rules."
        }
    });
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
    if !saved.is_empty() {
        prompt.push_str(&format!(
            " Dashboards saved on this machine: {}. If the human names one, load_dashboard reopens it; save_dashboard keeps a new one.",
            saved
                .iter()
                .take(10)
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(otlp) = &started.record.otlp {
        prompt.push_str(&format!(
            " This session also receives OpenTelemetry: logs, traces and metrics sent to {} (OTLP/HTTP; {}) land in its Loki, Tempo and Prometheus. \
`dashr tail -- <command>` ships any command's output as logs. When the human wants to check that the right log messages fire, arm them with expect_logs and read the verdict with log_expectations. \
Ask the human what they are testing.",
            otlp.http_endpoint(),
            match reader {
                Reader::ChatPane => "OTEL_EXPORTER_OTLP_ENDPOINT is set in your shell",
                Reader::Opener => "set OTEL_EXPORTER_OTLP_ENDPOINT to it for programs you start",
            }
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

/// The pane's whole view: the link to open in a browser and one line of
/// health. Built for a narrow pane (DEC-040); per-panel detail is the
/// agent's business (`panel_status`), not the human's.
fn render_status(
    out: &mut impl Write,
    link: &str,
    tick: Option<&Tick>,
    collectors: &std::collections::BTreeMap<String, std::result::Result<(), String>>,
    note: &str,
) {
    let _ = write!(out, "\x1b[2J\x1b[H");
    let _ = writeln!(out, "\x1b[1mdashr\x1b[0m");
    let _ = writeln!(out);
    let _ = writeln!(out, "{link}");
    let _ = writeln!(out, "\x1b[2mCtrl-click to open\x1b[0m");
    let _ = writeln!(out);
    match tick {
        None => {
            let _ = writeln!(out, "starting…");
        }
        Some(tick) if tick.error.is_some() => {
            let _ = writeln!(out, "\x1b[31m●\x1b[0m Grafana not answering");
        }
        Some(tick) => {
            let summary = &tick.summary;
            let colour = if !tick.breached.is_empty() || summary.error > 0 {
                "31"
            } else if summary.empty > 0 {
                "33"
            } else {
                "32"
            };
            let _ = writeln!(out, "\x1b[{colour}m●\x1b[0m panels {}", summary.token());
            for alert in &tick.breached {
                let _ = writeln!(out, "\x1b[31m⚠ {alert}\x1b[0m");
            }
        }
    }
    if !collectors.is_empty() {
        let failing: Vec<(&String, &String)> = collectors
            .iter()
            .filter_map(|(id, state)| state.as_ref().err().map(|error| (id, error)))
            .collect();
        let colour = if failing.is_empty() { "32" } else { "33" };
        let _ = writeln!(
            out,
            "\x1b[{colour}m●\x1b[0m {} live collector{}",
            collectors.len(),
            if collectors.len() == 1 { "" } else { "s" }
        );
        for (id, error) in failing.iter().take(3) {
            let _ = writeln!(
                out,
                "\x1b[33m⚠ {id}: {}\x1b[0m",
                error.chars().take(60).collect::<String>()
            );
        }
    }
    if !note.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "{note}");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "\x1b[2mClose to stop Grafana.\x1b[0m");
    let _ = out.flush();
}

/// Consecutive monitor ticks without the pane before the pane process stops
/// itself and cleans up (DASHR-GRAF-010).
pub const PANE_GONE_AFTER: u32 = 2;

/// The share of its split's width the pane beside it keeps, so the dashboard
/// pane is a narrow column on the right (DEC-040).
pub const LEFT_SHARE: f64 = 0.8;

/// How much to move the split edge so `pane` — when it is the right-hand
/// side of a left/right split — keeps `1 - LEFT_SHARE` of it. `None` when
/// it is not a right-hand pane or already narrow enough.
pub fn narrowing(layout: &serde_json::Value, pane: &str) -> Option<f64> {
    let layout = layout.pointer("/result/layout")?;
    let rect = layout
        .get("panes")?
        .as_array()?
        .iter()
        .find(|p| p.get("pane_id").and_then(serde_json::Value::as_str) == Some(pane))?
        .get("rect")?;
    let field =
        |value: &serde_json::Value, name: &str| value.get(name).and_then(serde_json::Value::as_f64);
    let (x, y, width) = (field(rect, "x")?, field(rect, "y")?, field(rect, "width")?);
    // The innermost left/right split whose right-hand side is this pane.
    let split = layout
        .get("splits")?
        .as_array()?
        .iter()
        .filter(|split| split.get("direction").and_then(serde_json::Value::as_str) == Some("right"))
        .filter_map(|split| {
            let area = split.get("rect")?;
            let (sx, sy, sw, sh) = (
                field(area, "x")?,
                field(area, "y")?,
                field(area, "width")?,
                field(area, "height")?,
            );
            let right_side = x > sx && (x + width - (sx + sw)).abs() < 1.0;
            let covers = y >= sy && y < sy + sh;
            (right_side && covers).then(|| (sw, field(split, "ratio")))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))?;
    let ratio = split.1?;
    let amount = LEFT_SHARE - ratio;
    (amount > 0.01).then_some(amount)
}

/// Makes the pane a narrow column when it was opened as a right-hand split.
fn narrow(herdr: &Herdr, pane: &str) {
    let Ok(layout) = herdr.call(&dashr_herdr::cli::argv::pane_layout(pane)) else {
        return;
    };
    if let Some(amount) = narrowing(&layout, pane) {
        let _ = herdr.call(&dashr_herdr::cli::argv::pane_resize(pane, "right", amount));
    }
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
    // SIGHUP is what Herdr sends on Unix when the pane closes. Windows has
    // no SIGHUP: there the pane.closed hook and the startup reaper stop the
    // container (DEC-038).
    #[cfg(unix)]
    let signals = [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ];
    #[cfg(not(unix))]
    let signals = [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM];
    for signal in signals {
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
    // A chat pane only when a Herdr action asked for one; a pane the human's
    // AI session opened is driven by that session (DEC-039).
    let chat = config.agent.enabled && std::env::var(CHAT_ENV).is_ok_and(|value| value == "1");
    // What the pane decided about its chat pane, for diagnosing a missing
    // one (AC-OPEN occasionally found none, with no error shown).
    let chat_log = record.runtime_dir.join("pane.log");
    let log_chat = |line: &str| {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&chat_log)
        {
            let _ = writeln!(file, "{line}");
        }
    };
    log_chat(&format!(
        "chat: agent.enabled={} {CHAT_ENV}={:?} -> {chat}",
        config.agent.enabled,
        std::env::var(CHAT_ENV).ok()
    ));
    let saved: Vec<String> = dashr_core::library::Library::new(&paths.state_dir)
        .list()
        .into_iter()
        .map(|summary| summary.name)
        .collect();
    // The AI session's `dashr tool` masks with this pane's configuration;
    // written before the briefing, which `dashr wait` waits for.
    let _ = std::fs::write(
        record
            .runtime_dir
            .join(dashr_runtime::paths::CONFIG_DIR_FILE),
        paths.config_dir.to_string_lossy().as_bytes(),
    );
    if let Err(error) = std::fs::write(
        record.runtime_dir.join(BRIEF_FILE),
        briefing(&started, &saved, Reader::Opener),
    ) {
        println!("dashr: could not write the briefing: {error}");
    }

    // Keep the dashboard-building skill current before the agent starts
    // (DASHR-SKILL-002). Best effort: a failure is reported, not fatal.
    if chat && config.agent.install_skill {
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

    // Chat pane. A failure is kept for the text view, which clears the
    // screen and would otherwise hide it.
    let mut chat_note = String::new();
    if chat && !stop.load(Ordering::SeqCst) {
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let mcp_path = record.runtime_dir.join("mcp.json");
        let mcp = mcp_config(&record, &config, paths, &exe, &herdr_bin);
        std::fs::write(
            &mcp_path,
            serde_json::to_vec_pretty(&mcp).unwrap_or_default(),
        )
        .map_err(|error| error.to_string())?;
        let argv = agent_argv(
            &config.agent.command,
            &mcp_path,
            &opening_prompt(&started, &saved),
        );
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
        // A tab opened a moment ago can refuse the first split; retry
        // briefly rather than start without the agent.
        let mut split = herdr.pane_split(
            &pane_id,
            "down",
            config.agent.split_ratio,
            cwd.as_deref(),
            &split_env,
        );
        for _ in 0..4 {
            let Err(error) = &split else { break };
            if stop.load(Ordering::SeqCst) {
                break;
            }
            println!("dashr: chat pane not opened yet ({error}); retrying");
            std::thread::sleep(Duration::from_secs(1));
            split = herdr.pane_split(
                &pane_id,
                "down",
                config.agent.split_ratio,
                cwd.as_deref(),
                &split_env,
            );
        }
        match split {
            Ok(chat) => {
                // The shell needs a moment to draw its prompt before input.
                std::thread::sleep(Duration::from_millis(300));
                if let Err(error) = herdr.pane_run(&chat, &dashr_core::shell::join(&argv)) {
                    println!("dashr: could not start the agent: {error}");
                }
                log_chat(&format!("chat: split -> {chat}"));
                record.chat_pane = Some(chat);
                match store.save(&record) {
                    Ok(()) => log_chat("chat: recorded"),
                    Err(error) => {
                        log_chat(&format!("chat: not recorded: {error}"));
                        chat_note = format!("Chat pane not recorded: {error}");
                    }
                }
            }
            Err(error) => {
                log_chat(&format!("chat: split failed: {error}"));
                println!("dashr: could not open the chat pane: {error}");
                chat_note = format!("Chat pane not opened: {error}");
            }
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
            let mut missing = 0;
            while !stop.load(Ordering::SeqCst) {
                let tick = monitor::tick(&record, &client, &masker, &store);
                monitor::report(&tick, &mut reporter, notify);
                if let Ok(mut slot) = latest.lock() {
                    *slot = Some(tick);
                }
                // The pane is gone but this process was not told: on Windows
                // Herdr kills the `node` launcher and its child `dashr.exe`
                // lives on, holding the plugin's files and the container.
                // Two misses in a row, so one failed call does not stop it.
                missing = if reporter.herdr.pane_exists(&reporter.pane) {
                    0
                } else {
                    missing + 1
                };
                if missing >= PANE_GONE_AFTER {
                    stop.store(true, Ordering::SeqCst);
                    break;
                }
                wait(&stop, interval);
            }
        })
    };

    // View: the link and one line of health in a narrow pane. The human
    // opens Grafana in their own browser, which follows dashboard changes by
    // itself (Grafana Live) — no browser inside the pane (DEC-040).
    narrow(&herdr, &pane_id);
    // The link is a page showing only the dashboard (DEC-041); if Grafana
    // will not share it, fall back to its kiosk view and say so.
    let shared = dashr_grafana::Client::local(&record.grafana_url())
        .shared_dashboard(&record.dashboard_uid)
        .map_err(|error| error.to_string())
        .and_then(|token| {
            crate::viewer::serve(
                record.grafana_url(),
                record.dashboard_uid.clone(),
                token,
                Arc::clone(&stop),
            )
            .map_err(|error| error.to_string())
        });
    let (link, note) = match shared {
        Ok(port) => (format!("http://127.0.0.1:{port}/"), chat_note),
        Err(error) => (
            format!("{}?kiosk", record.dashboard_url()),
            format!("Dashboard-only view unavailable: {error}\n{chat_note}")
                .trim()
                .to_owned(),
        ),
    };
    // Live collectors (DEC-043): this pane is the background job that keeps
    // the dashboard's data flowing, for as long as it lives.
    let health: dashr_runtime::collect::Health = Arc::default();
    if let Some(otlp) = &record.otlp {
        let exporter = dashr_runtime::otlp::Exporter::new(&otlp.http_endpoint());
        let store = store.clone();
        let session_id = record.session_id.clone();
        let stop = Arc::clone(&stop);
        let health = Arc::clone(&health);
        std::thread::spawn(move || {
            dashr_runtime::collect::run(exporter, store, session_id, stop, health);
        });
    }
    while !stop.load(Ordering::SeqCst) {
        let tick = latest.lock().ok().and_then(|slot| slot.clone());
        let collectors = health.lock().map(|h| h.clone()).unwrap_or_default();
        // For `dashr collect list`.
        let summary: serde_json::Map<String, serde_json::Value> = collectors
            .iter()
            .map(|(id, state)| {
                (
                    id.clone(),
                    match state {
                        Ok(()) => json!("ok"),
                        Err(error) => json!({"error": error}),
                    },
                )
            })
            .collect();
        let _ = std::fs::write(
            record.runtime_dir.join(crate::agent::COLLECTOR_HEALTH_FILE),
            serde_json::Value::Object(summary).to_string(),
        );
        render_status(
            &mut std::io::stdout(),
            &link,
            tick.as_ref(),
            &collectors,
            &note,
        );
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
    fn opening_prompt_names_saved_dashboards() {
        let started = Started {
            record: record(),
            in_memory: true,
            inventory: None,
            warnings: vec![],
        };
        let prompt = opening_prompt(&started, &["checkout debug".into(), "dlq".into()]);
        assert!(prompt.contains("\"checkout debug\", \"dlq\""), "{prompt}");
        assert!(prompt.contains("load_dashboard"));
        assert!(!opening_prompt(&started, &[]).contains("load_dashboard"));
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
            dashr_core::shell::join_posix(&argv),
            r"claude 'hi '\''there'\''' --mcp-config /r/mcp.json"
        );
        // What the chat pane's PowerShell gets on Windows.
        assert_eq!(
            dashr_core::shell::join_powershell(&argv),
            "& claude 'hi ''there''' '--mcp-config' /r/mcp.json"
        );
    }

    #[test]
    fn status_shows_the_link_and_one_health_line_only() {
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
        let mut collectors = std::collections::BTreeMap::new();
        collectors.insert("docker".to_owned(), Ok(()));
        collectors.insert("logs:api".to_owned(), Err("docker: not found".to_owned()));
        render_status(
            &mut out,
            "http://127.0.0.1:41234/",
            Some(&tick),
            &collectors,
            "",
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\nhttp://127.0.0.1:41234/\n"), "{text}");
        assert!(text.contains("panels 1 ok · 1 err"));
        assert!(text.contains("DLQ not empty"));
        assert!(text.contains("2 live collectors"));
        assert!(text.contains("logs:api: docker: not found"));
        assert!(!text.contains("OTLP"));
        // Narrow: no line of text wider than the link.
        let widest = text
            .lines()
            .map(|line| strip_ansi(line).chars().count())
            .max()
            .unwrap();
        // Narrow: a collector warning is the widest line it can print.
        assert!(widest <= 70, "{text}");
    }

    fn strip_ansi(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn a_right_hand_pane_is_narrowed_to_a_fifth() {
        // `herdr pane layout` after `pane split w1:p1 --direction right`.
        let layout = json!({"result": {"layout": {
            "panes": [
                {"pane_id": "w1:p1", "rect": {"height": 40, "width": 60, "x": 0, "y": 0}},
                {"pane_id": "w1:p2", "rect": {"height": 40, "width": 60, "x": 60, "y": 0}}
            ],
            "splits": [{"direction": "right", "ratio": 0.5, "rect": {"height": 40, "width": 120, "x": 0, "y": 0}}]
        }}});
        let amount = narrowing(&layout, "w1:p2").unwrap();
        assert!((amount - 0.3).abs() < 1e-9, "{amount}");
        // The left pane, or a pane already narrow, is left alone.
        assert_eq!(narrowing(&layout, "w1:p1"), None);
        let mut narrow = layout.clone();
        narrow["result"]["layout"]["splits"][0]["ratio"] = json!(0.8);
        assert_eq!(narrowing(&narrow, "w1:p2"), None);
        // A pane on top of a down split (the action's tab) is left alone.
        let tab = json!({"result": {"layout": {
            "panes": [{"pane_id": "w1:p2", "rect": {"height": 25, "width": 120, "x": 0, "y": 0}}],
            "splits": [{"direction": "down", "ratio": 0.62, "rect": {"height": 40, "width": 120, "x": 0, "y": 0}}]
        }}});
        assert_eq!(narrowing(&tab, "w1:p2"), None);
    }
}

//! The trace pane Herdr runs, and `dashr serve`, its stand-alone twin.
//!
//! The pane owns the session: it starts Jaeger, serves the API and the
//! viewer, keeps the pull sources running, and stops everything when it
//! closes. It is a narrow column showing the links and one line per flow
//! and source; the human watches the traces in their own browser.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;

use dashr_core::Config;
use dashr_core::flow::VerdictStatus;
use dashr_herdr::cli::{AgentState, argv};
use dashr_herdr::{Herdr, PluginEnv};
use dashr_runtime::{Paths, Running};

/// Consecutive checks without the pane before the pane process stops
/// itself (Windows kills the launcher, not its child).
pub const PANE_GONE_AFTER: u32 = 3;
/// The share of its split the pane beside it keeps, so this pane is a
/// narrow column on the right.
pub const LEFT_SHARE: f64 = 0.75;

pub fn socket_hash(socket: &str) -> String {
    format!(
        "{:08x}",
        dashr_core::model::fnv1a64(socket.as_bytes()) as u32
    )
}

/// The session id of a Herdr pane: unique across Herdr servers, whose
/// pane ids repeat (`w1:p1` in every session).
pub fn pane_session_id(socket: &str, pane: &str) -> String {
    format!("{}-{}", socket_hash(socket), pane.replace(':', "-"))
}

fn stop_flag() -> Result<Arc<AtomicBool>, String> {
    let stop = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    let signals = [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ];
    #[cfg(not(unix))]
    let signals = [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM];
    for signal in signals {
        signal_hook::flag::register(signal, Arc::clone(&stop)).map_err(|e| e.to_string())?;
    }
    Ok(stop)
}

fn ago(ms: Option<u64>) -> String {
    match ms {
        Some(ms) => {
            let secs = dashr_runtime::session::now_ms().saturating_sub(ms) / 1000;
            if secs < 90 {
                format!("{secs}s ago")
            } else {
                format!("{}m ago", secs / 60)
            }
        }
        None => "never".into(),
    }
}

/// The pane's text: links, counts, one line per flow and per source.
pub fn render(out: &mut impl Write, running: &Running, status: &Value) {
    let _ = write!(out, "\x1b[2J\x1b[H");
    let _ = writeln!(out, "\x1b[1mdashr\x1b[0m  trace session");
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", running.viewer_url);
    let _ = writeln!(out, "\x1b[2mCtrl-click: sequence, flows, spans\x1b[0m");
    let _ = writeln!(out);
    let _ = writeln!(out, "Jaeger  {}", running.record.jaeger_ui());
    let _ = writeln!(out, "OTLP    {} (HTTP)", running.record.otlp_http());
    let _ = writeln!(out, "        {} (gRPC)", running.record.otlp_grpc());
    let _ = writeln!(out);
    let colour = if status["jaeger_error"].is_null() {
        "32"
    } else {
        "31"
    };
    let _ = writeln!(
        out,
        "\x1b[{colour}m●\x1b[0m {} traces · {} spans",
        status["traces"], status["spans"]
    );
    if let Some(error) = status["jaeger_error"].as_str() {
        let _ = writeln!(
            out,
            "\x1b[31m⚠ Jaeger: {}\x1b[0m",
            error.chars().take(60).collect::<String>()
        );
    }
    for flow in status["flows"].as_array().into_iter().flatten() {
        let name = flow["name"].as_str().unwrap_or_default();
        let line = match flow["status"].as_str().unwrap_or_default() {
            "pass" => format!("\x1b[32m✓ {name}: pass\x1b[0m"),
            "fail" => format!(
                "\x1b[31m✗ {name}: {}\x1b[0m",
                flow["summary"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(50)
                    .collect::<String>()
            ),
            "running" => format!("… {name}: trace arriving"),
            _ => format!("○ {name}: waiting for a run"),
        };
        let _ = writeln!(out, "{line}");
    }
    for edit in status["human_edits"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "\x1b[36m✎ {}\x1b[0m: {} lines, {}",
            edit["file"].as_str().unwrap_or_default(),
            edit["lines_changed"],
            ago(edit["at_ms"].as_u64())
        );
    }
    for source in status["sources"].as_array().into_iter().flatten() {
        let name = source["spec"]["name"].as_str().unwrap_or_default();
        let health = &source["health"];
        let line = match health["last_error"].as_str() {
            Some(error) if health["last_ok_ms"] != health["last_run_ms"] => {
                format!(
                    "\x1b[33m⇣ {name}: {}\x1b[0m",
                    error.chars().take(50).collect::<String>()
                )
            }
            _ => format!(
                "⇣ {name}: {} spans, {}",
                health["spans_total"],
                ago(health["last_ok_ms"].as_u64())
            ),
        };
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "\x1b[2mClose to stop Jaeger and drop every span.\x1b[0m"
    );
    let _ = out.flush();
}

/// How much to move the split edge so `pane` — the right-hand side of a
/// left/right split — keeps `1 - LEFT_SHARE` of it.
pub fn narrowing(layout: &Value, pane: &str) -> Option<f64> {
    let layout = layout.pointer("/result/layout")?;
    let rect = layout
        .get("panes")?
        .as_array()?
        .iter()
        .find(|p| p.get("pane_id").and_then(Value::as_str) == Some(pane))?
        .get("rect")?;
    let field = |value: &Value, name: &str| value.get(name).and_then(Value::as_f64);
    let (x, y, width) = (field(rect, "x")?, field(rect, "y")?, field(rect, "width")?);
    let split = layout
        .get("splits")?
        .as_array()?
        .iter()
        .filter(|split| split.get("direction").and_then(Value::as_str) == Some("right"))
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
    let amount = LEFT_SHARE - split.1?;
    (amount > 0.01).then_some(amount)
}

fn print_progress(message: &str) {
    println!("dashr: {message}");
}

/// The Herdr pane.
pub fn traces(paths: &Paths, config: &Config) -> Result<(), String> {
    let env = PluginEnv::from_process();
    let pane = env
        .pane_id
        .clone()
        .ok_or("HERDR_PANE_ID is not set; run this from Herdr (or use `dashr serve`)")?;
    let socket = env
        .socket
        .clone()
        .ok_or("HERDR_SOCKET_PATH is not set; run this from Herdr")?;
    let herdr = Herdr::new(&env.herdr_bin());
    let stop = stop_flag()?;
    let _ = herdr.report_agent(&pane, AgentState::Working, Some("starting Jaeger"));
    let running = match dashr_runtime::start(
        paths,
        config,
        &pane_session_id(&socket, &pane),
        Some(pane.clone()),
        Arc::clone(&stop),
        &print_progress,
    ) {
        Ok(running) => running,
        Err(error) => {
            let _ = herdr.report_agent(&pane, AgentState::Blocked, Some("dashr could not start"));
            eprintln!("dashr: {error}");
            eprintln!("Run `dashr doctor`. Press Enter to close.");
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            let _ = herdr.release_agent(&pane);
            return Err(error);
        }
    };
    if let Ok(layout) = herdr.call(&argv::pane_layout(&pane))
        && let Some(amount) = narrowing(&layout, &pane)
    {
        let _ = herdr.call(&argv::pane_resize(&pane, "right", amount));
    }
    // Nothing waits on the human: the pane is idle once Jaeger runs.
    let _ = herdr.report_agent(&pane, AgentState::Idle, None);
    let mut missing = 0;
    let mut tick = 0u64;
    while !stop.load(Ordering::SeqCst) {
        let status = running.shared.status();
        render(&mut std::io::stdout(), &running, &status);
        for (name, status, summary) in running.shared.changed_verdicts() {
            let title = match status {
                VerdictStatus::Pass => format!("dashr: {name} passed"),
                _ => format!("dashr: {name} failed"),
            };
            let _ = herdr.notify(&title, Some(&summary), status == VerdictStatus::Fail);
        }
        tick += 1;
        if tick % 5 == 0 {
            missing = if herdr.pane_exists(&pane) {
                0
            } else {
                missing + 1
            };
            if missing >= PANE_GONE_AFTER {
                break;
            }
        }
        dashr_runtime::state::sleep_unless(Duration::from_secs(1), &[&stop]);
    }
    let _ = herdr.release_agent(&pane);
    running.stop();
    Ok(())
}

/// `dashr serve`: a session in the foreground, without Herdr.
pub fn serve(paths: &Paths, config: &Config, session: Option<String>) -> Result<(), String> {
    let stop = stop_flag()?;
    let id = session.unwrap_or_else(|| format!("local-{}", &dashr_runtime::session::random_hex(3)));
    let running =
        dashr_runtime::start(paths, config, &id, None, Arc::clone(&stop), &print_progress)?;
    println!("dashr: session {id}");
    println!("dashr: viewer  {}", running.viewer_url);
    println!("dashr: Jaeger  {}", running.record.jaeger_ui());
    println!(
        "dashr: OTLP    {} (HTTP), {} (gRPC)",
        running.record.otlp_http(),
        running.record.otlp_grpc()
    );
    println!("dashr: Ctrl-C stops Jaeger and drops every span.");
    while !stop.load(Ordering::SeqCst) {
        for (name, status, summary) in running.shared.changed_verdicts() {
            println!("dashr: flow {name}: {status:?}: {summary}");
        }
        dashr_runtime::state::sleep_unless(Duration::from_secs(1), &[&stop]);
    }
    running.stop();
    println!("dashr: stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pane_sessions_differ_across_herdr_servers() {
        assert_ne!(
            pane_session_id("/a.sock", "w1:p1"),
            pane_session_id("/b.sock", "w1:p1")
        );
        assert!(pane_session_id("/a.sock", "w1:p1").ends_with("-w1-p1"));
    }

    #[test]
    fn a_right_hand_pane_is_narrowed() {
        let layout = json!({"result": {"layout": {
            "panes": [{"pane_id": "w1:p1", "rect": {"x": 0, "y": 0, "width": 100, "height": 40}},
                      {"pane_id": "w1:p2", "rect": {"x": 100, "y": 0, "width": 100, "height": 40}}],
            "splits": [{"direction": "right", "ratio": 0.5, "rect": {"x": 0, "y": 0, "width": 200, "height": 40}}]
        }}});
        assert_eq!(narrowing(&layout, "w1:p2"), Some(0.25));
        assert_eq!(narrowing(&layout, "w1:p1"), None);
    }
}

//! Sending to a session's OpenTelemetry endpoint, and `dashr tail`.
//!
//! `dashr tail -- cargo run` runs the command exactly as it would run on its
//! own (output still reaches the terminal, the exit code passes through) and
//! ships every stdout and stderr line to the session's Loki over OTLP/HTTP,
//! where the live trail shows it (DASHR-OTEL-004). Shipping never gets in the
//! way of the command: a failed POST is reported once and the lines dropped.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashr_core::otlp::{LogLine, Stream, clean_line, logs_payload};
use serde_json::{Value, json};

/// Posts OTLP/HTTP JSON to one endpoint.
#[derive(Clone)]
pub struct Exporter {
    endpoint: String,
    agent: ureq::Agent,
}

impl Exporter {
    /// `endpoint` is the OTLP/HTTP base, e.g. `http://127.0.0.1:4318`.
    pub fn new(endpoint: &str) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(2)))
            .timeout_global(Some(Duration::from_secs(5)))
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            endpoint: endpoint.trim_end_matches('/').to_owned(),
            agent,
        }
    }

    /// POSTs to `/v1/logs`, `/v1/traces` or `/v1/metrics`.
    pub fn post(&self, signal: &str, payload: &Value) -> Result<(), String> {
        let url = format!("{}/v1/{signal}", self.endpoint);
        let response = self
            .agent
            .post(&url)
            .header("Content-Type", "application/json")
            .send(payload.to_string())
            .map_err(|error| format!("{url}: {error}"))?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(format!("{url} answered {status}"))
        }
    }

    /// Waits until the collector accepts an empty log batch.
    pub fn wait_ready(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.post("logs", &json!({"resourceLogs": []})) {
                Ok(()) => return Ok(()),
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => thread::sleep(Duration::from_millis(250)),
            }
        }
    }
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

/// Longest wait before a batch is sent.
const BATCH_INTERVAL: Duration = Duration::from_millis(250);
/// Most lines in one POST.
const BATCH_LINES: usize = 500;

/// Collects lines and POSTs them in batches until every sender is gone.
fn ship(exporter: Exporter, service: String, lines: Receiver<LogLine>) -> usize {
    let mut batch: Vec<LogLine> = Vec::new();
    let mut failures = 0usize;
    let mut warned = false;
    let mut flush = |batch: &mut Vec<LogLine>| {
        if batch.is_empty() {
            return;
        }
        if let Err(error) = exporter.post("logs", &logs_payload(&service, batch)) {
            failures += batch.len();
            if !warned {
                warned = true;
                eprintln!("dashr tail: lines are not reaching the dashboard ({error})");
            }
        }
        batch.clear();
    };
    let mut deadline = Instant::now() + BATCH_INTERVAL;
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        match lines.recv_timeout(wait) {
            Ok(line) => {
                batch.push(line);
                if batch.len() >= BATCH_LINES {
                    flush(&mut batch);
                    deadline = Instant::now() + BATCH_INTERVAL;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                flush(&mut batch);
                deadline = Instant::now() + BATCH_INTERVAL;
            }
            Err(RecvTimeoutError::Disconnected) => {
                flush(&mut batch);
                return failures;
            }
        }
    }
}

/// Copies `input` to `echo` unchanged and sends each line on.
fn pump(input: impl Read, mut echo: impl Write, stream: Stream, lines: Sender<LogLine>) {
    let mut reader = BufReader::new(input);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let _ = echo.write_all(&buffer);
                let _ = echo.flush();
                let text = String::from_utf8_lossy(&buffer);
                let body = clean_line(text.trim_end_matches('\n'));
                if body.trim().is_empty() {
                    continue;
                }
                let _ = lines.send(LogLine {
                    time_unix_nano: now_nanos(),
                    stream,
                    body,
                });
            }
        }
    }
}

/// How a tail ended.
#[derive(Debug)]
pub struct TailOutcome {
    /// The command's exit code; `None` when it was killed by a signal.
    pub code: Option<i32>,
    /// Lines that could not be shipped.
    pub dropped: usize,
}

fn exit_code(status: ExitStatus) -> Option<i32> {
    status.code().or_else(|| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            status.signal().map(|signal| 128 + signal)
        }
        #[cfg(not(unix))]
        {
            None
        }
    })
}

/// Runs `command` and ships its output. The command inherits stdin.
pub fn tail_command(
    exporter: Exporter,
    service: &str,
    command: &[String],
) -> std::io::Result<TailOutcome> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| std::io::Error::other("no command given"))?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (sender, receiver) = mpsc::channel();
    let shipper = {
        let service = service.to_owned();
        thread::spawn(move || ship(exporter, service, receiver))
    };
    let mut pumps = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let sender = sender.clone();
        pumps.push(thread::spawn(move || {
            pump(stdout, std::io::stdout(), Stream::Stdout, sender)
        }));
    }
    if let Some(stderr) = child.stderr.take() {
        let sender = sender.clone();
        pumps.push(thread::spawn(move || {
            pump(stderr, std::io::stderr(), Stream::Stderr, sender)
        }));
    }
    drop(sender);
    let status = child.wait()?;
    for pump in pumps {
        let _ = pump.join();
    }
    let dropped = shipper.join().unwrap_or(0);
    Ok(TailOutcome {
        code: exit_code(status),
        dropped,
    })
}

/// Ships this process's stdin, echoing it to stdout: `app | dashr tail`.
pub fn tail_stdin(exporter: Exporter, service: &str) -> TailOutcome {
    let (sender, receiver) = mpsc::channel();
    let shipper = {
        let service = service.to_owned();
        thread::spawn(move || ship(exporter, service, receiver))
    };
    pump(std::io::stdin(), std::io::stdout(), Stream::Stdout, sender);
    TailOutcome {
        code: Some(0),
        dropped: shipper.join().unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn pump_echoes_everything_and_ships_clean_non_empty_lines() {
        let (sender, receiver) = mpsc::channel();
        let mut echo = Vec::new();
        let input = "one\n\n\u{1b}[31mERROR two\u{1b}[0m\r\nthree";
        pump(Cursor::new(input), &mut echo, Stream::Stderr, sender);
        assert_eq!(String::from_utf8(echo).unwrap(), input);
        let shipped: Vec<LogLine> = receiver.iter().collect();
        let bodies: Vec<&str> = shipped.iter().map(|l| l.body.as_str()).collect();
        assert_eq!(bodies, ["one", "ERROR two", "three"]);
        assert!(shipped.iter().all(|l| l.stream == Stream::Stderr));
    }

    #[test]
    fn unreachable_endpoint_drops_lines_without_failing() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(LogLine {
                time_unix_nano: 1,
                stream: Stream::Stdout,
                body: "x".into(),
            })
            .unwrap();
        drop(sender);
        // Port 9 (discard) on loopback: nothing listens in CI.
        let dropped = ship(Exporter::new("http://127.0.0.1:9"), "t".into(), receiver);
        assert_eq!(dropped, 1);
    }

    #[cfg(unix)]
    #[test]
    fn tail_passes_the_exit_code_through() {
        let outcome = tail_command(
            Exporter::new("http://127.0.0.1:9"),
            "t",
            &["sh".into(), "-c".into(), "echo hi; exit 3".into()],
        )
        .unwrap();
        assert_eq!(outcome.code, Some(3));
        assert_eq!(outcome.dropped, 1);
    }
}

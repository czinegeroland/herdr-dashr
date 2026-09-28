//! Live collectors: what they are, and the pure parsing behind them
//! (DEC-043).
//!
//! A collector turns something that is running — a container, the host, a
//! log stream, a database, any command's output — into metrics and logs in
//! the session's Prometheus and Loki. The dashboard pane runs them, so the
//! dashboard stays live with no agent in the loop. Nothing here knows about a
//! particular cloud: `exec` runs whatever command the agent wrote (around
//! `aws`, `az`, `gcloud`, `kubectl`, a script) and reads Prometheus text from
//! it, and `stream` follows whatever command prints log lines.
//!
//! Everything in this module is a function of its input, so every format is
//! unit tested without Docker, a database or a cloud.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// One collector, as `dashr collect` records it for the pane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Collector {
    /// CPU, memory, network and disk IO of containers (all running ones
    /// when the list is empty).
    Docker {
        #[serde(default)]
        containers: Vec<String>,
    },
    /// The machine's CPU, memory, disks and network.
    Host,
    /// Processes whose name contains `name`: CPU, memory, disk IO.
    Process { name: String },
    /// A long-running command printing log lines: the lines go to Loki and
    /// request rate, errors and latency are read from them.
    Stream {
        service: String,
        command: Vec<String>,
    },
    /// A command run every `every_secs` that prints Prometheus text.
    Exec {
        service: String,
        every_secs: u64,
        command: Vec<String>,
    },
    /// A Prometheus `/metrics` endpoint.
    Scrape { service: String, url: String },
    /// A Postgres container: connections, transactions, cache hit rate,
    /// slowest normalized queries (`pg_stat_statements` when installed).
    Postgres { container: String },
    /// A MySQL or MariaDB container.
    Mysql { container: String },
    /// A Redis container.
    Redis { container: String },
    /// A database added with `dashr db add`, sampled through its Grafana
    /// datasource (DEC-044). `password_command` prints a fresh password
    /// (an IAM token, a secret) and is re-run every `refresh_secs`;
    /// `tunnel_command` (SSM port forwarding, `ssh -L`) is kept running.
    Database {
        name: String,
        engine: crate::dbperf::Engine,
        #[serde(default)]
        password_command: Vec<String>,
        #[serde(default)]
        refresh_secs: u64,
        #[serde(default)]
        tunnel_command: Vec<String>,
        /// Whether per-statement statistics are available.
        #[serde(default)]
        statements: bool,
        /// Azure SQL Database: database-scoped waits and resource stats.
        #[serde(default)]
        azure: bool,
        /// A loopback port the Grafana container reaches through a relay
        /// (Linux; a tunnel or a server bound to 127.0.0.1).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relay_port: Option<u16>,
    },
}

impl Collector {
    /// A stable id, so adding the same collector twice replaces it.
    pub fn id(&self) -> String {
        match self {
            Collector::Docker { containers } if containers.is_empty() => "docker".into(),
            Collector::Docker { containers } => format!("docker:{}", containers.join(",")),
            Collector::Host => "host".into(),
            Collector::Process { name } => format!("process:{name}"),
            Collector::Stream { service, .. } => format!("stream:{service}"),
            Collector::Exec { service, .. } => format!("exec:{service}"),
            Collector::Scrape { service, .. } => format!("scrape:{service}"),
            Collector::Postgres { container } => format!("postgres:{container}"),
            Collector::Mysql { container } => format!("mysql:{container}"),
            Collector::Redis { container } => format!("redis:{container}"),
            Collector::Database { name, .. } => format!("db:{name}"),
        }
    }

    /// Seconds between two samples.
    pub fn every_secs(&self) -> u64 {
        match self {
            Collector::Exec { every_secs, .. } => (*every_secs).max(MIN_EXEC_SECS),
            Collector::Scrape { .. } => 15,
            Collector::Database { .. } => 30,
            _ => 5,
        }
    }
}

/// `exec` never runs more often than this: cloud APIs are rate limited and
/// some are billed per call.
pub const MIN_EXEC_SECS: u64 = 10;

/// One metric sample.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

impl Point {
    pub fn new(name: &str, labels: &[(&str, &str)], value: f64) -> Self {
        Self {
            name: name.to_owned(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            value,
        }
    }
}

// ---------------------------------------------------------------- sizes

/// `12.5MiB`, `1.2kB`, `0B`, `3.4GB` → bytes. Docker writes both binary
/// (`MiB`) and decimal (`MB`, `kB`) units.
pub fn parse_size(text: &str) -> Option<f64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    let factor = match unit.trim() {
        "" | "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0_f64.powi(4),
        _ => return None,
    };
    Some(number * factor)
}

fn pair(text: &str) -> Option<(f64, f64)> {
    let (a, b) = text.split_once('/')?;
    Some((parse_size(a)?, parse_size(b)?))
}

// ---------------------------------------------------------------- docker

/// One line of `docker stats --no-stream --format '{{json .}}'`.
#[derive(Debug, Clone, PartialEq)]
pub struct DockerSample {
    pub name: String,
    pub cpu_percent: f64,
    pub memory_bytes: f64,
    pub memory_limit_bytes: f64,
    pub memory_percent: f64,
    /// Cumulative since the container started.
    pub net_rx_bytes: f64,
    pub net_tx_bytes: f64,
    pub block_read_bytes: f64,
    pub block_write_bytes: f64,
    pub pids: f64,
}

pub fn docker_stats_line(line: &str) -> Option<DockerSample> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let text = |key: &str| value.get(key).and_then(serde_json::Value::as_str);
    let percent = |key: &str| {
        text(key)
            .and_then(|t| t.trim().trim_end_matches('%').parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    let (memory, limit) = text("MemUsage").and_then(pair).unwrap_or((0.0, 0.0));
    let (rx, tx) = text("NetIO").and_then(pair).unwrap_or((0.0, 0.0));
    let (read, write) = text("BlockIO").and_then(pair).unwrap_or((0.0, 0.0));
    Some(DockerSample {
        name: text("Name")?.to_owned(),
        cpu_percent: percent("CPUPerc"),
        memory_bytes: memory,
        memory_limit_bytes: limit,
        memory_percent: percent("MemPerc"),
        net_rx_bytes: rx,
        net_tx_bytes: tx,
        block_read_bytes: read,
        block_write_bytes: write,
        pids: text("PIDs").and_then(|t| t.parse().ok()).unwrap_or(0.0),
    })
}

// ---------------------------------------------------------------- requests

/// A request read from a log line. Only the method, the status and the
/// duration: never the path, which can carry ids or personal data and would
/// make one series per URL.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: String,
    pub status: u16,
    pub millis: Option<f64>,
    pub format: &'static str,
}

fn duration_ms(number: &str, unit: &str) -> Option<f64> {
    let number: f64 = number.parse().ok()?;
    Some(match unit {
        "ns" => number / 1e6,
        "µs" | "us" | "μs" => number / 1e3,
        "ms" => number,
        "s" => number * 1e3,
        "m" => number * 60e3,
        _ => return None,
    })
}

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("pattern compiles"))
}

/// Reads one request from a log line in a common format: ASP.NET Core,
/// Gin (Go, Ollama), nginx / Apache access logs, JSON logs, or a generic
/// `METHOD ... status ... 12ms` line. `None` for anything else.
pub fn parse_request(line: &str) -> Option<Request> {
    static ASPNET: OnceLock<Regex> = OnceLock::new();
    static GIN: OnceLock<Regex> = OnceLock::new();
    static COMBINED: OnceLock<Regex> = OnceLock::new();
    static GENERIC: OnceLock<Regex> = OnceLock::new();

    // Request finished HTTP/1.1 GET http://localhost:5000/ - 400 - application/json 0.2416ms
    if let Some(c) = regex(
        &ASPNET,
        r"Request finished \S+ ([A-Z]+) \S+.*? - (\d{3}) .*?([\d.]+)ms",
    )
    .captures(line)
    {
        return Some(Request {
            method: c[1].to_owned(),
            status: c[2].parse().ok()?,
            millis: c[3].parse().ok(),
            format: "aspnet",
        });
    }
    // [GIN] 2026/09/27 - 19:28:10 | 200 |  298.82µs |  172.21.0.1 | GET  "/api/tags"
    if let Some(c) = regex(
        &GIN,
        r"\|\s*(\d{3})\s*\|\s*([\d.]+)\s*(ns|µs|μs|us|ms|s|m)\s*\|[^|]*\|\s*([A-Z]+)\s",
    )
    .captures(line)
    {
        return Some(Request {
            method: c[4].to_owned(),
            status: c[1].parse().ok()?,
            millis: duration_ms(&c[2], &c[3]),
            format: "gin",
        });
    }
    // "GET /x HTTP/1.1" 200 612 "-" "curl/8" [0.004]
    if let Some(c) = regex(
        &COMBINED,
        r#""([A-Z]+) \S+ HTTP/[\d.]+" (\d{3}) \S+(?:.*?\s([\d.]+)\s*$)?"#,
    )
    .captures(line)
    {
        return Some(Request {
            method: c[1].to_owned(),
            status: c[2].parse().ok()?,
            // nginx's $request_time is in seconds.
            millis: c
                .get(3)
                .and_then(|m| m.as_str().parse::<f64>().ok())
                .map(|s| s * 1e3),
            format: "access-log",
        });
    }
    if let Some(request) = json_request(line) {
        return Some(request);
    }
    // GET /orders 201 12.3ms
    if let Some(c) = regex(
        &GENERIC,
        r"\b(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\b.*?\b([1-5]\d\d)\b.*?\b([\d.]+)\s?(ns|µs|μs|us|ms|s)\b",
    )
    .captures(line)
    {
        return Some(Request {
            method: c[1].to_owned(),
            status: c[2].parse().ok()?,
            millis: duration_ms(&c[3], &c[4]),
            format: "generic",
        });
    }
    None
}

fn json_request(line: &str) -> Option<Request> {
    let start = line.find('{')?;
    let value: serde_json::Value = serde_json::from_str(&line[start..]).ok()?;
    let find = |keys: &[&str]| -> Option<serde_json::Value> {
        keys.iter().find_map(|key| {
            value
                .get(*key)
                .or_else(|| value.get("http").and_then(|h| h.get(*key)))
                .or_else(|| value.get("req").and_then(|h| h.get(*key)))
                .or_else(|| value.get("res").and_then(|h| h.get(*key)))
                .cloned()
        })
    };
    let status = find(&["status", "statusCode", "status_code", "StatusCode"])?;
    let status: u16 = match status {
        serde_json::Value::Number(n) => n.as_u64()? as u16,
        serde_json::Value::String(s) => s.parse().ok()?,
        _ => return None,
    };
    if !(100..600).contains(&status) {
        return None;
    }
    let method = find(&["method", "Method", "request_method"])
        .and_then(|m| m.as_str().map(str::to_owned))
        .unwrap_or_else(|| "-".into());
    let millis = find(&[
        "duration_ms",
        "elapsed_ms",
        "response_time",
        "responseTime",
        "latency_ms",
        "ElapsedMilliseconds",
        "duration",
        "elapsed",
    ])
    .and_then(|d| d.as_f64());
    Some(Request {
        method,
        status,
        millis,
        format: "json",
    })
}

/// Requests seen since the last flush: rates and latency percentiles.
#[derive(Debug, Default, Clone)]
pub struct RequestWindow {
    pub count: u64,
    pub client_errors: u64,
    pub server_errors: u64,
    pub total: u64,
    latencies: Vec<f64>,
}

impl RequestWindow {
    pub fn record(&mut self, request: &Request) {
        self.count += 1;
        self.total += 1;
        match request.status {
            400..=499 => self.client_errors += 1,
            500..=599 => self.server_errors += 1,
            _ => {}
        }
        if let Some(millis) = request.millis {
            if self.latencies.len() < 100_000 {
                self.latencies.push(millis);
            }
        }
    }

    /// The window's points, per second over `seconds`; empties the window.
    pub fn flush(&mut self, service: &str, seconds: f64) -> Vec<Point> {
        let seconds = seconds.max(0.001);
        let labels = [("service", service)];
        let mut points = vec![
            Point::new(
                "dashr_http_requests_per_second",
                &labels,
                self.count as f64 / seconds,
            ),
            Point::new(
                "dashr_http_client_errors_per_second",
                &labels,
                self.client_errors as f64 / seconds,
            ),
            Point::new(
                "dashr_http_server_errors_per_second",
                &labels,
                self.server_errors as f64 / seconds,
            ),
            Point::new("dashr_http_requests_total", &labels, self.total as f64),
        ];
        if self.count > 0 {
            points.push(Point::new(
                "dashr_http_error_ratio",
                &labels,
                self.server_errors as f64 / self.count as f64,
            ));
        }
        if !self.latencies.is_empty() {
            self.latencies.sort_by(f64::total_cmp);
            for (name, q) in [
                ("dashr_http_latency_p50_ms", 0.5),
                ("dashr_http_latency_p95_ms", 0.95),
                ("dashr_http_latency_p99_ms", 0.99),
            ] {
                points.push(Point::new(name, &labels, percentile(&self.latencies, q)));
            }
        }
        self.count = 0;
        self.client_errors = 0;
        self.server_errors = 0;
        self.latencies.clear();
        points
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    let index = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

// ---------------------------------------------------------------- prometheus

/// Most series one `exec` or `scrape` sample may produce.
pub const MAX_SERIES: usize = 2000;

/// Parses Prometheus text exposition: `name{a="b"} 1.5 [timestamp]`.
/// Comments, blank lines and non-finite values are skipped.
pub fn prometheus_text(text: &str) -> Vec<Point> {
    let mut points = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, labels, rest) = match line.find('{') {
            Some(open) => {
                let Some(close) = line[open..].rfind('}').map(|c| c + open) else {
                    continue;
                };
                (
                    &line[..open],
                    parse_labels(&line[open + 1..close]),
                    &line[close + 1..],
                )
            }
            None => match line.split_once(char::is_whitespace) {
                Some((name, rest)) => (name, Vec::new(), rest),
                None => continue,
            },
        };
        let Some(value) = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<f64>().ok())
        else {
            continue;
        };
        if !value.is_finite() || !valid_metric_name(name.trim()) {
            continue;
        }
        points.push(Point {
            name: name.trim().to_owned(),
            labels,
            value,
        });
        if points.len() >= MAX_SERIES {
            break;
        }
    }
    points
}

fn valid_metric_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == ':')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

fn parse_labels(text: &str) -> Vec<(String, String)> {
    let mut labels = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq].trim().trim_start_matches(',').trim().to_owned();
        rest = rest[eq + 1..].trim_start();
        if !rest.starts_with('"') {
            break;
        }
        let mut value = String::new();
        let mut chars = rest[1..].char_indices();
        let mut end = None;
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => {
                    if let Some((_, escaped)) = chars.next() {
                        value.push(match escaped {
                            'n' => '\n',
                            other => other,
                        });
                    }
                }
                '"' => {
                    end = Some(i + 2);
                    break;
                }
                other => value.push(other),
            }
        }
        let Some(end) = end else { break };
        if !key.is_empty() {
            labels.push((key, value));
        }
        rest = rest[end..]
            .trim_start()
            .trim_start_matches(',')
            .trim_start();
    }
    labels
}

// ---------------------------------------------------------------- databases

/// `key<TAB>value` or `key|value` lines (MySQL `SHOW GLOBAL STATUS`,
/// `psql -At`) into a map.
pub fn key_values(text: &str) -> BTreeMap<String, f64> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line
                .split_once('\t')
                .or_else(|| line.split_once('|'))
                .or_else(|| line.split_once(':'))?;
            Some((key.trim().to_owned(), value.trim().parse().ok()?))
        })
        .collect()
}

/// A normalized query as a label: whitespace collapsed, at most 80
/// characters. `pg_stat_statements` already replaces literals with `$1`.
pub fn query_label(query: &str) -> String {
    let collapsed: String = query.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > 80 {
        format!("{}…", collapsed.chars().take(79).collect::<String>())
    } else {
        collapsed
    }
}

/// Turns cumulative counters into per-second rates between two samples.
#[derive(Debug, Default)]
pub struct Rates {
    previous: BTreeMap<String, (f64, f64)>,
}

impl Rates {
    /// The rate of `key` since its last value, `None` on the first sample
    /// or after a counter reset.
    pub fn per_second(&mut self, key: &str, value: f64, now_secs: f64) -> Option<f64> {
        let previous = self.previous.insert(key.to_owned(), (value, now_secs));
        let (before, then) = previous?;
        let elapsed = now_secs - then;
        (elapsed > 0.0 && value >= before).then(|| (value - before) / elapsed)
    }
}

// ---------------------------------------------------------------- discovery

/// What a container image most likely is, from its name.
pub fn image_kind(image: &str) -> &'static str {
    let image = image.to_ascii_lowercase();
    let name = image.rsplit('/').next().unwrap_or(&image);
    let name = name.split([':', '@']).next().unwrap_or(name);
    let has = |needle: &str| name.contains(needle) || image.contains(&format!("/{needle}"));
    if has("postgres") || has("postgis") || has("timescale") {
        "postgres"
    } else if has("mysql") || has("mariadb") || has("percona") {
        "mysql"
    } else if has("mssql") || image.contains("sql-server") {
        "sqlserver"
    } else if has("redis") || has("valkey") || has("keydb") {
        "redis"
    } else if has("mongo") {
        "mongodb"
    } else if has("rabbitmq") {
        "rabbitmq"
    } else if has("kafka") || has("redpanda") {
        "kafka"
    } else if has("elasticsearch") || has("opensearch") {
        "search"
    } else if has("nginx")
        || has("traefik")
        || has("caddy")
        || has("haproxy")
        || has("envoy")
        || has("httpd")
    {
        "proxy"
    } else if has("ollama") {
        "ollama"
    } else if has("grafana") || has("prometheus") || has("otel") || has("loki") {
        "observability"
    } else {
        "app"
    }
}

/// Published host ports in a `docker ps` Ports string
/// (`0.0.0.0:5000->5000/tcp, :::5000->5000/tcp`).
pub fn published_ports(ports: &str) -> Vec<u16> {
    let mut out: Vec<u16> = ports
        .split(',')
        .filter_map(|mapping| {
            let (host, _) = mapping.split_once("->")?;
            host.rsplit(':').next()?.trim().parse().ok()
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_in_both_unit_systems() {
        assert_eq!(parse_size("0B"), Some(0.0));
        assert_eq!(parse_size("1.5kB"), Some(1500.0));
        assert_eq!(parse_size("2MiB"), Some(2.0 * 1024.0 * 1024.0));
        assert_eq!(parse_size(" 3.2GB "), Some(3.2e9));
        assert_eq!(parse_size("x"), None);
    }

    #[test]
    fn docker_stats_json_line() {
        let line = r#"{"BlockIO":"12.3MB / 4.1kB","CPUPerc":"3.52%","Container":"a1","ID":"a1","MemPerc":"1.20%","MemUsage":"48.5MiB / 3.8GiB","Name":"mcp-server","NetIO":"1.2kB / 648B","PIDs":"23"}"#;
        let sample = docker_stats_line(line).unwrap();
        assert_eq!(sample.name, "mcp-server");
        assert!((sample.cpu_percent - 3.52).abs() < 1e-9);
        assert_eq!(sample.memory_bytes, 48.5 * 1024.0 * 1024.0);
        assert_eq!(sample.net_tx_bytes, 648.0);
        assert_eq!(sample.block_read_bytes, 12.3e6);
        assert_eq!(sample.pids, 23.0);
        assert!(docker_stats_line("not json").is_none());
    }

    #[test]
    fn request_lines_from_common_servers() {
        let aspnet = parse_request("info: Microsoft.AspNetCore.Hosting.Diagnostics[2]       Request finished HTTP/1.1 GET http://localhost:5000/ - 400 - application/json;+charset=utf-8 0.2416ms").unwrap();
        assert_eq!(
            (aspnet.method.as_str(), aspnet.status, aspnet.format),
            ("GET", 400, "aspnet")
        );
        assert!((aspnet.millis.unwrap() - 0.2416).abs() < 1e-9);

        let gin = parse_request(
            r#"[GIN] 2026/09/27 - 19:28:10 | 200 |  298.82µs |  172.21.0.1 | GET      "/api/tags""#,
        )
        .unwrap();
        assert_eq!(
            (gin.method.as_str(), gin.status, gin.format),
            ("GET", 200, "gin")
        );
        assert!((gin.millis.unwrap() - 0.29882).abs() < 1e-9);
        let slow = parse_request(
            r#"[GIN] 2026/09/27 - 19:28:10 | 500 |  2.5s |  1.2.3.4 | POST     "/api/embed""#,
        )
        .unwrap();
        assert_eq!((slow.status, slow.millis), (500, Some(2500.0)));

        let nginx = parse_request(r#"172.17.0.1 - - [27/Sep/2026:19:00:00 +0000] "POST /orders HTTP/1.1" 201 12 "-" "curl/8.5.0" 0.012"#).unwrap();
        assert_eq!(
            (nginx.method.as_str(), nginx.status, nginx.format),
            ("POST", 201, "access-log")
        );
        assert!((nginx.millis.unwrap() - 12.0).abs() < 1e-9);
        let apache =
            parse_request(r#"1.2.3.4 - - [27/Sep/2026:19:00:00 +0000] "GET /x HTTP/1.0" 404 209"#)
                .unwrap();
        assert_eq!((apache.status, apache.millis), (404, None));

        let json = parse_request(
            r#"{"level":"info","method":"PUT","status":503,"duration_ms":41.5,"msg":"done"}"#,
        )
        .unwrap();
        assert_eq!(
            (json.method.as_str(), json.status, json.millis, json.format),
            ("PUT", 503, Some(41.5), "json")
        );
        let nested =
            parse_request(r#"{"req":{"method":"GET"},"res":{"statusCode":200},"responseTime":3}"#)
                .unwrap();
        assert_eq!(
            (nested.method.as_str(), nested.status, nested.millis),
            ("GET", 200, Some(3.0))
        );

        let generic = parse_request("2026-09-27 12:00:01 INFO GET /health 200 1.5ms").unwrap();
        assert_eq!(
            (generic.status, generic.millis, generic.format),
            (200, Some(1.5), "generic")
        );

        assert!(parse_request("Now listening on: http://[::]:5000").is_none());
        assert!(parse_request("loaded 200 embeddings in 3s").is_none());
    }

    #[test]
    fn a_window_gives_rates_errors_and_percentiles() {
        let mut window = RequestWindow::default();
        for (status, millis) in [(200, 10.0), (200, 20.0), (404, 5.0), (500, 100.0)] {
            window.record(&Request {
                method: "GET".into(),
                status,
                millis: Some(millis),
                format: "x",
            });
        }
        let points = window.flush("api", 2.0);
        let get = |name: &str| points.iter().find(|p| p.name == name).map(|p| p.value);
        assert_eq!(get("dashr_http_requests_per_second"), Some(2.0));
        assert_eq!(get("dashr_http_client_errors_per_second"), Some(0.5));
        assert_eq!(get("dashr_http_server_errors_per_second"), Some(0.5));
        assert_eq!(get("dashr_http_error_ratio"), Some(0.25));
        assert_eq!(get("dashr_http_latency_p99_ms"), Some(100.0));
        assert!(
            points
                .iter()
                .all(|p| p.labels == vec![("service".into(), "api".into())])
        );
        // Flushed: the next window starts empty, the total keeps counting.
        let again = window.flush("api", 1.0);
        assert_eq!(
            again
                .iter()
                .find(|p| p.name == "dashr_http_requests_per_second")
                .unwrap()
                .value,
            0.0
        );
        assert_eq!(
            again
                .iter()
                .find(|p| p.name == "dashr_http_requests_total")
                .unwrap()
                .value,
            4.0
        );
        assert!(again.iter().all(|p| !p.name.contains("latency")));
    }

    #[test]
    fn prometheus_text_with_labels_escapes_and_junk() {
        let text = r#"# HELP up whether up
# TYPE up gauge
up 1
http_requests_total{method="GET",path="/a \"b\"",code="200"} 1027 1395066363000
queue_depth{queue="orders"}  3.5
bad-name 1
nan_value NaN
no_value
"#;
        let points = prometheus_text(text);
        assert_eq!(points.len(), 3, "{points:?}");
        assert_eq!(points[0], Point::new("up", &[], 1.0));
        assert_eq!(points[1].labels[1], ("path".into(), r#"/a "b""#.into()));
        assert_eq!(points[1].value, 1027.0);
        assert_eq!(points[2].value, 3.5);
    }

    #[test]
    fn database_status_lines_and_query_labels() {
        let mysql = key_values("Threads_connected\t7\nQuestions\t1234\nUptime\t99\nVersion\tabc");
        assert_eq!(mysql["Threads_connected"], 7.0);
        assert!(!mysql.contains_key("Version"));
        let redis = key_values("# Clients\r\nconnected_clients:3\r\nused_memory:1024\r\n");
        assert_eq!(redis["connected_clients"], 3.0);
        assert_eq!(
            query_label("SELECT *\n  FROM orders WHERE id = $1"),
            "SELECT * FROM orders WHERE id = $1"
        );
        assert_eq!(query_label(&"x ".repeat(100)).chars().count(), 80);
    }

    #[test]
    fn rates_between_samples() {
        let mut rates = Rates::default();
        assert_eq!(rates.per_second("rx", 100.0, 0.0), None);
        assert_eq!(rates.per_second("rx", 300.0, 2.0), Some(100.0));
        assert_eq!(
            rates.per_second("rx", 10.0, 3.0),
            None,
            "a reset is not a negative rate"
        );
    }

    #[test]
    fn kinds_and_ports_from_docker_ps() {
        assert_eq!(image_kind("postgres:16-alpine"), "postgres");
        assert_eq!(
            image_kind("mcr.microsoft.com/mssql/server:2022-latest"),
            "sqlserver"
        );
        assert_eq!(image_kind("bitnami/redis"), "redis");
        assert_eq!(image_kind("ollama/ollama:latest"), "ollama");
        assert_eq!(image_kind("mta-mapper-mcp:dev"), "app");
        assert_eq!(image_kind("nginx:alpine"), "proxy");
        assert_eq!(
            published_ports("0.0.0.0:5000->5000/tcp, :::5000->5000/tcp, 11434/tcp"),
            vec![5000]
        );
        assert!(published_ports("").is_empty());
    }

    #[test]
    fn collectors_have_stable_ids_and_sane_intervals() {
        let exec = Collector::Exec {
            service: "aws".into(),
            every_secs: 1,
            command: vec!["x".into()],
        };
        assert_eq!(exec.id(), "exec:aws");
        assert_eq!(exec.every_secs(), MIN_EXEC_SECS);
        assert_eq!(Collector::Docker { containers: vec![] }.id(), "docker");
        let json = serde_json::to_string(&Collector::Postgres {
            container: "db".into(),
        })
        .unwrap();
        assert_eq!(json, r#"{"kind":"postgres","container":"db"}"#);
    }
}

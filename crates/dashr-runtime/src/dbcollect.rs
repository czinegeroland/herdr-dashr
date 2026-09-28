//! Database collectors (DEC-044): a database added with `dashr db add` is
//! sampled through its Grafana datasource, so dashr needs no database
//! driver and the credentials stay in Grafana's encrypted store.
//!
//! The worker keeps the database's tunnel command running, re-runs its
//! password command before the password expires (an RDS IAM token lives 15
//! minutes), and every 30 seconds turns the server's cumulative statistics
//! into per-second series.

use std::collections::BTreeMap;
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dashr_core::collect::{Collector, Point, Rates, query_label};
use dashr_core::dbperf::{self, Deltas, Engine};
use dashr_core::otlp::metrics_payload;
use dashr_grafana::Client;
use serde_json::Value;

use crate::collect::{Health, run_command, shell_argv};
use crate::otlp::Exporter;

/// Seconds between password refreshes when the collector does not say.
pub const DEFAULT_REFRESH_SECS: u64 = 600;

/// One result row, by column name.
pub type Row = BTreeMap<String, Value>;

/// Runs `sql` against the database's datasource and returns its rows.
pub fn rows(client: &Client, name: &str, engine: Engine, sql: &str) -> Result<Vec<Row>, String> {
    let target = dbperf::sql_target(name, engine, "A", sql);
    let result = client
        .query(&[target], "now-5m", "now")
        .into_iter()
        .next()
        .ok_or("no result")?;
    if let Some(error) = result.error {
        return Err(error);
    }
    let mut out = Vec::new();
    for frame in &result.frames {
        let names: Vec<String> = frame.fields.iter().map(|f| f.name.clone()).collect();
        for row in frame.rows() {
            out.push(names.iter().cloned().zip(row).collect());
        }
    }
    Ok(out)
}

fn number(row: &Row, key: &str) -> Option<f64> {
    match row.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

fn text(row: &Row, key: &str) -> String {
    match row.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Runs the password command and hands the password to Grafana.
pub fn refresh_password(client: &Client, name: &str, command: &[String]) -> Result<(), String> {
    let output = run_command(&shell_argv(command), Duration::from_secs(60))?;
    let password = output.trim();
    if password.is_empty() {
        return Err("the password command printed nothing".into());
    }
    client
        .set_datasource_password(&dbperf::datasource_uid(name), password)
        .map_err(|error| error.to_string())
}

/// A tunnel command (SSM port forwarding, `ssh -L`) kept running.
pub struct Tunnel {
    argv: Vec<String>,
    child: Option<Child>,
}

impl Tunnel {
    pub fn new(command: &[String]) -> Self {
        Self {
            argv: shell_argv(command),
            child: None,
        }
    }

    /// Starts the command if it is not running; `Ok(true)` when it was
    /// (re)started just now.
    pub fn ensure(&mut self) -> Result<bool, String> {
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(None) => return Ok(false),
                _ => self.child = None,
            }
        }
        let (program, args) = self.argv.split_first().ok_or("empty tunnel command")?;
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Its own process group, so stopping it also stops what it started
        // (the AWS CLI runs session-manager-plugin as a child).
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .spawn()
            .map_err(|error| format!("{program}: {error}"))?;
        self.child = Some(child);
        Ok(true)
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // The child leads its own group (`process_group(0)`), so its pid
            // is the group id. `--` is required: without it procps-ng
            // 4.0.4's `kill -TERM -<pid>` reads the pid as an option and
            // signals the group of its first digit, which for a pid
            // starting with 1 is -1, every process on the machine.
            #[cfg(unix)]
            if let Some(group) = group_target(child.id()) {
                let _ = Command::new("kill")
                    .args(["-TERM", "--", &group])
                    .stderr(Stdio::null())
                    .status();
            }
            #[cfg(windows)]
            let _ = Command::new("taskkill")
                .args(["/T", "/F", "/PID", &child.id().to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The `kill` operand for the process group led by `pid`; never 0, -1 or
/// an init's group, which would signal far more than the tunnel.
#[cfg(unix)]
fn group_target(pid: u32) -> Option<String> {
    (pid > 1).then(|| format!("-{pid}"))
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Makes a database on this machine's loopback reachable from the Grafana
/// container on Linux, where `host.docker.internal` is the Docker bridge's
/// gateway and a port bound to 127.0.0.1 (an SSM tunnel always is) cannot
/// be reached through it. The relay listens on the gateway address only,
/// never on the network. Docker Desktop forwards loopback itself.
pub struct Relay {
    stop: Arc<AtomicBool>,
    listener: Option<std::thread::JoinHandle<()>>,
}

impl Relay {
    /// Whether a connection to `host` needs a relay here.
    pub fn needed(host: &str) -> bool {
        cfg!(target_os = "linux") && matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
    }

    /// The Docker bridge gateway, which `host-gateway` resolves to.
    pub fn gateway() -> Option<String> {
        let argv: Vec<String> = [
            "docker",
            "network",
            "inspect",
            "bridge",
            "--format",
            "{{range .IPAM.Config}}{{.Gateway}}{{end}}",
        ]
        .map(str::to_owned)
        .to_vec();
        let out = run_command(&argv, Duration::from_secs(10)).ok()?;
        let ip = out.trim().to_owned();
        (!ip.is_empty()).then_some(ip)
    }

    /// Relays `listen:port` to `127.0.0.1:port`. `Ok(None)` when the port
    /// is already served on that address (the database listens on all
    /// interfaces), so no relay is needed.
    pub fn start(listen: &str, port: u16) -> Result<Option<Relay>, String> {
        let listener = match TcpListener::bind((listen, port)) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => return Ok(None),
            Err(error) => return Err(format!("relay on {listen}:{port}: {error}")),
        };
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let accepting = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((inbound, _)) => {
                        let _ = inbound.set_nonblocking(false);
                        std::thread::spawn(move || {
                            let Ok(outbound) = TcpStream::connect(("127.0.0.1", port)) else {
                                return;
                            };
                            pipe(inbound, outbound);
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            }
        });
        Ok(Some(Relay {
            stop,
            listener: Some(accepting),
        }))
    }
}

impl Drop for Relay {
    /// Returns once the port is free, so the pane can bind it right away.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

fn pipe(a: TcpStream, b: TcpStream) {
    let (Ok(a2), Ok(b2)) = (a.try_clone(), b.try_clone()) else {
        return;
    };
    let forward = std::thread::spawn(move || {
        let (mut from, mut to) = (a, b2);
        let _ = std::io::copy(&mut from, &mut to);
        let _ = to.shutdown(std::net::Shutdown::Write);
    });
    let (mut from, mut to) = (b, a2);
    let _ = std::io::copy(&mut from, &mut to);
    let _ = to.shutdown(std::net::Shutdown::Write);
    let _ = forward.join();
}

/// Starts a relay for `host:port` when this machine needs one.
pub fn relay_for(host: &str, port: u16) -> Result<Option<Relay>, String> {
    if !Relay::needed(host) {
        return Ok(None);
    }
    let gateway = Relay::gateway().ok_or("cannot find the Docker bridge gateway")?;
    Relay::start(&gateway, port)
}

/// Waits until `host:port` accepts connections, up to `timeout`.
pub fn wait_for_port(host: &str, port: u16, timeout: Duration, stop: &[&AtomicBool]) -> bool {
    let until = Instant::now() + timeout;
    while Instant::now() < until {
        if stop.iter().any(|flag| flag.load(Ordering::SeqCst)) {
            return false;
        }
        let open = (host, port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next())
            .is_some_and(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok());
        if open {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

/// Turns a database's statistics into points.
pub struct DbSampler {
    name: String,
    engine: Engine,
    statements: bool,
    azure: bool,
    rates: Rates,
    deltas: Deltas,
}

impl DbSampler {
    pub fn new(name: &str, engine: Engine, statements: bool, azure: bool) -> Self {
        Self {
            name: name.to_owned(),
            engine,
            statements,
            azure,
            rates: Rates::default(),
            deltas: Deltas::default(),
        }
    }

    fn point(&self, metric: &str, labels: &[(&str, &str)], value: f64) -> Point {
        let mut all = vec![("db", self.name.as_str())];
        all.extend_from_slice(labels);
        Point::new(metric, &all, value)
    }

    fn rate(&mut self, key: &str, value: f64, now: f64) -> Option<f64> {
        self.rates.per_second(key, value, now)
    }

    pub fn sample(&mut self, client: &Client, now: f64) -> Result<Vec<Point>, String> {
        match self.engine {
            Engine::Postgres => self.postgres(client, now),
            Engine::Mssql => self.mssql(client, now),
        }
    }

    fn query(&self, client: &Client, sql: &str) -> Result<Vec<Row>, String> {
        rows(client, &self.name, self.engine, sql)
    }

    fn statements(&mut self, rows: &[Row], now: f64, names: &[&str]) -> Vec<Point> {
        let mut points = Vec::new();
        for row in rows {
            let id = text(row, "id");
            let values: Vec<f64> = names
                .iter()
                .map(|n| number(row, n).unwrap_or(0.0))
                .collect();
            let Some(rates) = self.deltas.rates(&format!("q:{id}"), now, &values) else {
                continue;
            };
            let database = text(row, "database");
            let database = if database.is_empty() {
                "-".into()
            } else {
                database
            };
            let query = query_label(&text(row, "query"));
            points.extend(dbperf::statement_points(
                &self.name, &database, &query, &rates, names,
            ));
        }
        points
    }

    fn postgres(&mut self, client: &Client, now: f64) -> Result<Vec<Point>, String> {
        let mut points = Vec::new();
        for row in self.query(client, dbperf::PG_COLLECT)? {
            let metric = text(&row, "metric");
            let label = text(&row, "label");
            let Some(value) = number(&row, "value") else {
                continue;
            };
            let (name, key) = dbperf::pg_metric(&metric);
            let value = if metric.ends_with("_total") {
                match self.rate(&format!("{metric}:{label}"), value, now) {
                    Some(rate) => rate,
                    None => continue,
                }
            } else {
                value
            };
            let labels: Vec<(&str, &str)> = if label.is_empty() {
                vec![]
            } else {
                vec![(key, label.as_str())]
            };
            points.push(self.point(&name, &labels, value));
        }
        if self.statements {
            let rows = self.query(client, dbperf::PG_COLLECT_STATEMENTS)?;
            points.extend(self.statements(&rows, now, &["calls", "time_ms", "rows", "io_blocks"]));
            for row in self.query(client, dbperf::PG_COLLECT_DATABASE_TIME)? {
                let database = text(&row, "database");
                if let Some(value) = number(&row, "time_ms")
                    && let Some(rate) = self.rate(&format!("dbtime:{database}"), value, now)
                {
                    points.push(self.point(
                        "dashr_db_database_time_ms_per_second",
                        &[("database", &database)],
                        rate,
                    ));
                }
            }
        }
        Ok(points)
    }

    fn mssql(&mut self, client: &Client, now: f64) -> Result<Vec<Point>, String> {
        let mut points = Vec::new();
        let mut hit_ratio = (None, None);
        for row in self.query(client, dbperf::MSSQL_COLLECT_COUNTERS)? {
            let counter = text(&row, "metric");
            // `_Total` is the server-wide instance, not a database.
            let label = Some(text(&row, "label"))
                .filter(|label| label != "_Total")
                .unwrap_or_default();
            let Some(value) = number(&row, "value") else {
                continue;
            };
            match text(&row, "kind").as_str() {
                "ratio" => hit_ratio.0 = Some(value),
                "base" => hit_ratio.1 = Some(value),
                "rate" => {
                    if let Some(rate) = self.rate(&format!("{counter}:{label}"), value, now) {
                        let labels: Vec<(&str, &str)> = if label.is_empty() {
                            vec![]
                        } else {
                            vec![("database", label.as_str())]
                        };
                        points.push(self.point(&dbperf::mssql_metric(&counter), &labels, rate));
                    }
                }
                _ => {
                    // Page life expectancy exists per NUMA node too; the
                    // Buffer Manager's (no instance name) is the server's.
                    if label.is_empty() {
                        points.push(self.point(&dbperf::mssql_metric(&counter), &[], value));
                    }
                }
            }
        }
        if let (Some(ratio), Some(base)) = hit_ratio
            && base > 0.0
        {
            points.push(self.point("dashr_db_buffer_cache_hit_ratio", &[], ratio / base));
        }
        let view = if self.azure {
            "sys.dm_db_wait_stats"
        } else {
            "sys.dm_os_wait_stats"
        };
        for row in self.query(client, &dbperf::mssql_collect_waits(view))? {
            let wait = text(&row, "label");
            if let Some(value) = number(&row, "value")
                && let Some(rate) = self.rate(&format!("wait:{wait}"), value, now)
            {
                points.push(self.point("dashr_db_wait_ms_per_second", &[("wait", &wait)], rate));
            }
        }
        if self.statements {
            for row in self.query(client, dbperf::MSSQL_COLLECT_DATABASES)? {
                let database = text(&row, "database");
                for (column, metric) in [
                    ("cpu_ms", "dashr_db_database_cpu_ms_per_second"),
                    ("elapsed_ms", "dashr_db_database_time_ms_per_second"),
                    (
                        "logical_reads",
                        "dashr_db_database_logical_reads_per_second",
                    ),
                ] {
                    if let Some(value) = number(&row, column)
                        && let Some(rate) = self.rate(&format!("{column}:{database}"), value, now)
                    {
                        points.push(self.point(metric, &[("database", &database)], rate));
                    }
                }
            }
            let rows = self.query(client, &dbperf::mssql_collect_statements())?;
            points.extend(self.statements(
                &rows,
                now,
                &["calls", "cpu_ms", "time_ms", "logical_reads"],
            ));
        }
        if self.azure
            && let Some(row) = self.query(client, dbperf::MSSQL_AZURE_RESOURCES)?.first()
        {
            for (column, resource) in [
                ("cpu_percent", "cpu"),
                ("data_io_percent", "data io"),
                ("log_write_percent", "log write"),
                ("memory_percent", "memory"),
            ] {
                if let Some(value) = number(row, column) {
                    points.push(self.point(
                        "dashr_db_azure_resource_percent",
                        &[("resource", resource)],
                        value,
                    ));
                }
            }
        }
        Ok(points)
    }
}

/// The worker for a [`Collector::Database`], run by the pane.
pub fn run(
    id: &str,
    collector: &Collector,
    grafana: &str,
    exporter: &Exporter,
    stop: &AtomicBool,
    removed: &AtomicBool,
    health: &Health,
) {
    let Collector::Database {
        name,
        engine,
        password_command,
        refresh_secs,
        tunnel_command,
        statements,
        azure,
        relay_port,
    } = collector
    else {
        return;
    };
    let report = |result: Result<(), String>| {
        if let Ok(mut health) = health.lock() {
            health.insert(id.to_owned(), result);
        }
    };
    let flags = [stop, removed];
    let client = Client::local(grafana);
    let mut tunnel = (!tunnel_command.is_empty()).then(|| Tunnel::new(tunnel_command));
    let refresh = Duration::from_secs(if *refresh_secs == 0 {
        DEFAULT_REFRESH_SECS
    } else {
        *refresh_secs
    });
    let every = Duration::from_secs(collector.every_secs());
    let mut refreshed: Option<Instant> = None;
    let mut sampler = DbSampler::new(name, *engine, *statements, *azure);
    // Retried until held: `AddrInUse` may be `db add`'s own relay, which
    // lingers a moment after it hands over (or the database listens on all
    // interfaces, and no relay is needed).
    let mut relay: Option<Relay> = None;
    let running = || !stop.load(Ordering::SeqCst) && !removed.load(Ordering::SeqCst);
    while running() {
        if let Some(port) = relay_port
            && relay.is_none()
        {
            match relay_for("127.0.0.1", *port) {
                Ok(started) => relay = started,
                Err(error) => {
                    report(Err(error));
                    sleep(&flags, Duration::from_secs(10));
                    continue;
                }
            }
        }
        if let Some(tunnel) = tunnel.as_mut() {
            match tunnel.ensure() {
                // A fresh tunnel needs a moment before it forwards.
                Ok(true) => std::thread::sleep(Duration::from_secs(3)),
                Ok(false) => {}
                Err(error) => {
                    report(Err(format!("tunnel: {error}")));
                    sleep(&flags, Duration::from_secs(10));
                    continue;
                }
            }
        }
        if !password_command.is_empty() && refreshed.is_none_or(|at| at.elapsed() >= refresh) {
            match refresh_password(&client, name, password_command) {
                Ok(()) => refreshed = Some(Instant::now()),
                Err(error) => {
                    report(Err(format!("password command: {error}")));
                    sleep(&flags, Duration::from_secs(15));
                    continue;
                }
            }
        }
        let now = crate::collect::now_secs();
        let result = sampler.sample(&client, now).and_then(|points| {
            if points.is_empty() {
                return Ok(());
            }
            exporter.post(
                "metrics",
                &metrics_payload(name, &points, crate::collect::now_nanos()),
            )
        });
        report(result);
        sleep(&flags, every);
    }
    if let Some(mut tunnel) = tunnel {
        tunnel.stop();
    }
}

fn sleep(flags: &[&AtomicBool], duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        if flags.iter().any(|flag| flag.load(Ordering::SeqCst)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_values_are_read_as_numbers_or_text() {
        let row: Row = [
            ("a".to_owned(), serde_json::json!(1.5)),
            ("b".to_owned(), serde_json::json!("42")),
            ("c".to_owned(), Value::Null),
        ]
        .into_iter()
        .collect();
        assert_eq!(number(&row, "a"), Some(1.5));
        assert_eq!(number(&row, "b"), Some(42.0));
        assert_eq!(number(&row, "c"), None);
        assert_eq!(text(&row, "b"), "42");
        assert_eq!(text(&row, "c"), "");
    }

    /// The tests that bind ports or spawn processes, one at a time: a child
    /// forked by one holds the other's listening socket until it execs,
    /// so a rebind right after a drop fails at random.
    static PORTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn waiting_for_a_port_ends_when_it_opens_or_on_stop() {
        let _ports = PORTS.lock().unwrap_or_else(|e| e.into_inner());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let never = AtomicBool::new(false);
        assert!(wait_for_port(
            "127.0.0.1",
            port,
            Duration::from_secs(2),
            &[&never]
        ));
        // A port nothing listens on (bound, never listened, released).
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .unwrap()
            .port();
        let stop = AtomicBool::new(true);
        assert!(
            !wait_for_port("127.0.0.1", closed, Duration::from_secs(5), &[&stop]),
            "a stop flag ends the wait"
        );
        drop(listener);
    }

    #[cfg(unix)]
    /// On an address of its own, so no parallel test takes the port.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_dropped_relay_frees_its_port_at_once() {
        let _ports = PORTS.lock().unwrap_or_else(|e| e.into_inner());
        let port = std::net::TcpListener::bind("127.0.0.2:0")
            .and_then(|l| l.local_addr())
            .unwrap()
            .port();
        let relay = Relay::start("127.0.0.2", port).unwrap();
        assert!(relay.is_some());
        drop(relay);
        assert!(
            Relay::start("127.0.0.2", port).unwrap().is_some(),
            "the next relay binds the same port"
        );
    }

    #[cfg(unix)]
    #[test]
    fn only_a_real_process_group_is_signalled() {
        assert_eq!(group_target(0), None);
        assert_eq!(group_target(1), None);
        assert_eq!(group_target(48213).as_deref(), Some("-48213"));
    }

    #[test]
    fn tunnels_restart_and_stop_with_their_children() {
        let _ports = PORTS.lock().unwrap_or_else(|e| e.into_inner());
        let mut tunnel = Tunnel::new(&["sh".into(), "-c".into(), "sleep 30 & wait".into()]);
        assert_eq!(tunnel.ensure(), Ok(true));
        assert_eq!(tunnel.ensure(), Ok(false), "a running tunnel is left alone");
        let pid = tunnel.child.as_ref().unwrap().id();
        tunnel.stop();
        let gone = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            let alive = Command::new("ps")
                .args(["-o", "stat=", "-g", &pid.to_string()])
                .output()
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .any(|l| !l.trim_start().starts_with('Z'))
                })
                .unwrap_or(false);
            !alive
        });
        assert!(gone, "the whole process group is gone");
    }
}

//! Running collectors (DEC-043).
//!
//! The dashboard pane runs [`run`] for as long as it lives: it reads the
//! session's collector list every two seconds and keeps one worker thread
//! per collector — sampling on an interval, or following a log stream — and
//! each worker sends what it reads to the session's OTLP endpoint. Adding or
//! removing a collector with `dashr collect` takes effect within seconds;
//! closing the pane stops every worker and kills every command it started.
//!
//! Nothing a collector reads is returned to the agent. It goes to Prometheus
//! and Loki, where the agent only sees it through the masked tools.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashr_core::collect::{
    self as parse, Collector, Point, Rates, RequestWindow, key_values, query_label,
};
use dashr_core::otlp::{LogLine, Stream, clean_line, logs_payload, metrics_payload};
use dashr_core::session::SessionStore;

use crate::otlp::Exporter;

/// Each collector's state for the pane: `Ok(())` or the last error.
pub type Health = Arc<Mutex<BTreeMap<String, Result<(), String>>>>;

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn now_secs() -> f64 {
    now_nanos() as f64 / 1e9
}

fn sleep_unless(flags: &[&AtomicBool], duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        if flags.iter().any(|flag| flag.load(Ordering::SeqCst)) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Runs the session's collectors until `stop`.
pub fn run(
    exporter: Exporter,
    store: SessionStore,
    session_id: String,
    stop: Arc<AtomicBool>,
    health: Health,
) {
    let mut workers: BTreeMap<String, (Collector, Arc<AtomicBool>)> = BTreeMap::new();
    while !stop.load(Ordering::SeqCst) {
        let wanted: BTreeMap<String, Collector> = store
            .load_collectors(&session_id)
            .into_iter()
            .map(|c| (c.id(), c))
            .collect();
        // Stop removed or changed collectors.
        let stale: Vec<String> = workers
            .iter()
            .filter(|(id, (collector, _))| wanted.get(*id) != Some(collector))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            if let Some((_, removed)) = workers.remove(&id) {
                removed.store(true, Ordering::SeqCst);
            }
            if let Ok(mut health) = health.lock() {
                health.remove(&id);
            }
        }
        for (id, collector) in wanted {
            if workers.contains_key(&id) {
                continue;
            }
            let removed = Arc::new(AtomicBool::new(false));
            spawn_worker(
                collector.clone(),
                exporter.clone(),
                Arc::clone(&stop),
                Arc::clone(&removed),
                Arc::clone(&health),
            );
            workers.insert(id, (collector, removed));
        }
        sleep_unless(&[&stop], Duration::from_secs(2));
    }
    for (_, removed) in workers.values() {
        removed.store(true, Ordering::SeqCst);
    }
}

fn report(health: &Health, id: &str, result: Result<(), String>) {
    if let Ok(mut health) = health.lock() {
        health.insert(id.to_owned(), result);
    }
}

fn spawn_worker(
    collector: Collector,
    exporter: Exporter,
    stop: Arc<AtomicBool>,
    removed: Arc<AtomicBool>,
    health: Health,
) {
    thread::spawn(move || {
        let id = collector.id();
        if let Collector::Stream { service, command } = &collector {
            stream(&id, service, command, &exporter, &stop, &removed, &health);
            return;
        }
        let every = Duration::from_secs(collector.every_secs());
        let mut sampler = Sampler::new(collector);
        while !stop.load(Ordering::SeqCst) && !removed.load(Ordering::SeqCst) {
            let result = sampler.sample().and_then(|points| {
                if points.is_empty() {
                    return Ok(());
                }
                exporter.post(
                    "metrics",
                    &metrics_payload(&sampler.service(), &points, now_nanos()),
                )
            });
            report(&health, &id, result);
            sleep_unless(&[&stop, &removed], every);
        }
    });
}

// ---------------------------------------------------------------- commands

/// The argv to run a user-given command: through `cmd /c` on Windows, where
/// `az`, `gcloud` and `npm` are `.cmd` files a bare spawn does not find.
pub fn shell_argv(command: &[String]) -> Vec<String> {
    if cfg!(windows) {
        ["cmd".to_owned(), "/c".to_owned()]
            .into_iter()
            .chain(command.iter().cloned())
            .collect()
    } else {
        command.to_vec()
    }
}

/// Runs `argv` to completion within `timeout`, returning its stdout; the
/// last stderr line on failure.
pub fn run_command(argv: &[String], timeout: Duration) -> Result<String, String> {
    let (program, args) = argv.split_first().ok_or("empty command")?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out = thread::spawn(move || {
        let mut text = String::new();
        if let Some(pipe) = stdout.as_mut() {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    });
    let err = thread::spawn(move || {
        let mut text = String::new();
        if let Some(pipe) = stderr.as_mut() {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} took longer than {}s", timeout.as_secs()));
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => return Err(error.to_string()),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if status.success() {
        Ok(stdout)
    } else {
        let last = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim()
            .chars()
            .take(300)
            .collect::<String>();
        Err(format!("{program} exited with {status}: {last}"))
    }
}

fn docker(args: &[&str]) -> Result<String, String> {
    let argv: Vec<String> = std::iter::once("docker")
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect();
    run_command(&argv, Duration::from_secs(20))
}

fn docker_exec(container: &str, script: &str) -> Result<String, String> {
    docker(&["exec", container, "sh", "-c", script])
}

// ---------------------------------------------------------------- samplers

/// A collector that samples on an interval.
pub struct Sampler {
    collector: Collector,
    rates: Rates,
    system: Option<sysinfo::System>,
    disks: Option<sysinfo::Disks>,
    networks: Option<sysinfo::Networks>,
}

impl Sampler {
    pub fn new(collector: Collector) -> Self {
        Self {
            collector,
            rates: Rates::default(),
            system: None,
            disks: None,
            networks: None,
        }
    }

    /// The `service.name` the points are stored under.
    pub fn service(&self) -> String {
        match &self.collector {
            Collector::Docker { .. } => "docker".into(),
            Collector::Host => "host".into(),
            Collector::Process { name } => name.clone(),
            Collector::Stream { service, .. }
            | Collector::Exec { service, .. }
            | Collector::Scrape { service, .. } => service.clone(),
            Collector::Postgres { container }
            | Collector::Mysql { container }
            | Collector::Redis { container } => container.clone(),
        }
    }

    pub fn sample(&mut self) -> Result<Vec<Point>, String> {
        match self.collector.clone() {
            Collector::Docker { containers } => self.docker(&containers),
            Collector::Host => Ok(self.host()),
            Collector::Process { name } => Ok(self.process(&name)),
            Collector::Exec {
                service, command, ..
            } => exec(&service, &command),
            Collector::Scrape { service, url } => scrape(&service, &url),
            Collector::Postgres { container } => self.postgres(&container),
            Collector::Mysql { container } => self.mysql(&container),
            Collector::Redis { container } => self.redis(&container),
            Collector::Stream { .. } => Ok(Vec::new()),
        }
    }

    fn rate(&mut self, key: &str, value: f64) -> Option<f64> {
        self.rates.per_second(key, value, now_secs())
    }

    fn docker(&mut self, containers: &[String]) -> Result<Vec<Point>, String> {
        let mut args = vec!["stats", "--no-stream", "--format", "{{json .}}"];
        args.extend(containers.iter().map(String::as_str));
        let text = docker(&args)?;
        let mut points = Vec::new();
        let mut seen = BTreeSet::new();
        for sample in text.lines().filter_map(parse::docker_stats_line) {
            let c = sample.name.as_str();
            seen.insert(sample.name.clone());
            let l = [("container", c)];
            points.push(Point::new("dashr_container_up", &l, 1.0));
            points.push(Point::new(
                "dashr_container_cpu_percent",
                &l,
                sample.cpu_percent,
            ));
            points.push(Point::new(
                "dashr_container_memory_bytes",
                &l,
                sample.memory_bytes,
            ));
            points.push(Point::new(
                "dashr_container_memory_limit_bytes",
                &l,
                sample.memory_limit_bytes,
            ));
            points.push(Point::new(
                "dashr_container_memory_percent",
                &l,
                sample.memory_percent,
            ));
            points.push(Point::new("dashr_container_pids", &l, sample.pids));
            for (name, value) in [
                (
                    "dashr_container_network_rx_bytes_per_second",
                    sample.net_rx_bytes,
                ),
                (
                    "dashr_container_network_tx_bytes_per_second",
                    sample.net_tx_bytes,
                ),
                (
                    "dashr_container_disk_read_bytes_per_second",
                    sample.block_read_bytes,
                ),
                (
                    "dashr_container_disk_write_bytes_per_second",
                    sample.block_write_bytes,
                ),
            ] {
                if let Some(rate) = self.rate(&format!("{name}/{c}"), value) {
                    points.push(Point::new(name, &l, rate));
                }
            }
        }
        // A named container that is not running shows as down.
        for name in containers.iter().filter(|name| !seen.contains(*name)) {
            points.push(Point::new(
                "dashr_container_up",
                &[("container", name)],
                0.0,
            ));
        }
        Ok(points)
    }

    fn host(&mut self) -> Vec<Point> {
        let system = self.system.get_or_insert_with(sysinfo::System::new);
        system.refresh_cpu_usage();
        system.refresh_memory();
        let mut points = vec![
            Point::new(
                "dashr_host_cpu_percent",
                &[],
                f64::from(system.global_cpu_usage()),
            ),
            Point::new("dashr_host_cpu_count", &[], system.cpus().len() as f64),
            Point::new(
                "dashr_host_memory_used_bytes",
                &[],
                system.used_memory() as f64,
            ),
            Point::new(
                "dashr_host_memory_total_bytes",
                &[],
                system.total_memory() as f64,
            ),
            Point::new("dashr_host_swap_used_bytes", &[], system.used_swap() as f64),
        ];
        let load = sysinfo::System::load_average();
        if load.one > 0.0 {
            points.push(Point::new("dashr_host_load1", &[], load.one));
        }
        let first = self.disks.is_none();
        let disks = self
            .disks
            .get_or_insert_with(sysinfo::Disks::new_with_refreshed_list);
        disks.refresh(true);
        let every = 5.0;
        let (mut read, mut written) = (0.0, 0.0);
        for disk in disks.list() {
            let mount = disk.mount_point().to_string_lossy().to_string();
            let total = disk.total_space() as f64;
            if total > 0.0 {
                let used = total - disk.available_space() as f64;
                points.push(Point::new(
                    "dashr_host_disk_used_percent",
                    &[("mount", &mount)],
                    used / total * 100.0,
                ));
            }
            let usage = disk.usage();
            read += usage.read_bytes as f64;
            written += usage.written_bytes as f64;
        }
        let networks = self
            .networks
            .get_or_insert_with(sysinfo::Networks::new_with_refreshed_list);
        networks.refresh(true);
        let (mut rx, mut tx) = (0.0, 0.0);
        for data in networks.list().values() {
            rx += data.received() as f64;
            tx += data.transmitted() as f64;
        }
        if !first {
            points.push(Point::new(
                "dashr_host_disk_read_bytes_per_second",
                &[],
                read / every,
            ));
            points.push(Point::new(
                "dashr_host_disk_write_bytes_per_second",
                &[],
                written / every,
            ));
            points.push(Point::new(
                "dashr_host_network_rx_bytes_per_second",
                &[],
                rx / every,
            ));
            points.push(Point::new(
                "dashr_host_network_tx_bytes_per_second",
                &[],
                tx / every,
            ));
        }
        points
    }

    fn process(&mut self, name: &str) -> Vec<Point> {
        let first = self.system.is_none();
        let system = self.system.get_or_insert_with(sysinfo::System::new);
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let needle = name.to_ascii_lowercase();
        let (mut cpu, mut memory, mut read, mut written, mut count) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for process in system.processes().values() {
            if !process
                .name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains(&needle)
            {
                continue;
            }
            count += 1.0;
            cpu += f64::from(process.cpu_usage());
            memory += process.memory() as f64;
            let usage = process.disk_usage();
            read += usage.read_bytes as f64;
            written += usage.written_bytes as f64;
        }
        let l = [("process", name)];
        let mut points = vec![
            Point::new("dashr_process_count", &l, count),
            Point::new("dashr_process_memory_bytes", &l, memory),
        ];
        if !first {
            points.push(Point::new("dashr_process_cpu_percent", &l, cpu));
            points.push(Point::new(
                "dashr_process_disk_read_bytes_per_second",
                &l,
                read / 5.0,
            ));
            points.push(Point::new(
                "dashr_process_disk_write_bytes_per_second",
                &l,
                written / 5.0,
            ));
        }
        points
    }

    fn postgres(&mut self, container: &str) -> Result<Vec<Point>, String> {
        const PSQL: &str = r#"export PGPASSWORD="${POSTGRES_PASSWORD:-$PGPASSWORD}"; psql -X -U "${POSTGRES_USER:-postgres}" -d "${POSTGRES_DB:-${POSTGRES_USER:-postgres}}" -At -F "$(printf '\037')" -c"#;
        let query = |sql: &str| docker_exec(container, &format!("{PSQL} \"{sql}\""));
        let row = query(
            "select sum(numbackends), sum(xact_commit), sum(xact_rollback), sum(blks_hit), sum(blks_read), sum(tup_returned), sum(tup_fetched), sum(tup_inserted), sum(tup_updated), sum(tup_deleted), sum(deadlocks), sum(temp_bytes) from pg_stat_database",
        )?;
        let values: Vec<f64> = row
            .trim()
            .split('\u{1f}')
            .map(|v| v.trim().parse().unwrap_or(0.0))
            .collect();
        if values.len() < 12 {
            return Err(format!(
                "unexpected pg_stat_database answer from {container}"
            ));
        }
        let l = [("database", container)];
        let mut points = vec![Point::new("dashr_pg_connections", &l, values[0])];
        for (index, name) in [
            (1, "dashr_pg_commits_per_second"),
            (2, "dashr_pg_rollbacks_per_second"),
            (5, "dashr_pg_rows_returned_per_second"),
            (6, "dashr_pg_rows_fetched_per_second"),
            (7, "dashr_pg_rows_inserted_per_second"),
            (8, "dashr_pg_rows_updated_per_second"),
            (9, "dashr_pg_rows_deleted_per_second"),
            (10, "dashr_pg_deadlocks_per_second"),
            (11, "dashr_pg_temp_bytes_per_second"),
        ] {
            if let Some(rate) = self.rate(name, values[index]) {
                points.push(Point::new(name, &l, rate));
            }
        }
        let hits = self.rate("hits", values[3]);
        let reads = self.rate("reads", values[4]);
        if let (Some(hits), Some(reads)) = (hits, reads)
            && hits + reads > 0.0
        {
            points.push(Point::new(
                "dashr_pg_cache_hit_ratio",
                &l,
                hits / (hits + reads),
            ));
        }
        if let Ok(states) =
            query("select coalesce(state, 'background'), count(*) from pg_stat_activity group by 1")
        {
            for line in states.lines() {
                if let Some((state, count)) = line.split_once('\u{1f}') {
                    points.push(Point::new(
                        "dashr_pg_connections_by_state",
                        &[("database", container), ("state", state)],
                        count.trim().parse().unwrap_or(0.0),
                    ));
                }
            }
        }
        if let Ok(longest) = query(
            "select coalesce(max(extract(epoch from now() - query_start)), 0) from pg_stat_activity where state = 'active' and pid <> pg_backend_pid()",
        ) {
            points.push(Point::new(
                "dashr_pg_longest_query_seconds",
                &l,
                longest.trim().parse().unwrap_or(0.0),
            ));
        }
        if let Ok(size) = query("select pg_database_size(current_database())") {
            points.push(Point::new(
                "dashr_pg_database_size_bytes",
                &l,
                size.trim().parse().unwrap_or(0.0),
            ));
        }
        // Normalized statements, slowest total first; absent without the
        // pg_stat_statements extension.
        if let Ok(statements) = query(
            "select queryid, calls, total_exec_time, mean_exec_time, query from pg_stat_statements where query not like '%pg_stat%' order by total_exec_time desc limit 10",
        ) {
            for line in statements.lines() {
                let fields: Vec<&str> = line.split('\u{1f}').collect();
                if fields.len() < 5 {
                    continue;
                }
                let label = query_label(fields[4]);
                let ql = [("database", container), ("query", label.as_str())];
                points.push(Point::new(
                    "dashr_pg_query_mean_ms",
                    &ql,
                    fields[3].parse().unwrap_or(0.0),
                ));
                if let Some(calls) = self.rate(
                    &format!("calls/{}", fields[0]),
                    fields[1].parse().unwrap_or(0.0),
                ) {
                    points.push(Point::new("dashr_pg_query_calls_per_second", &ql, calls));
                }
                if let Some(time) = self.rate(
                    &format!("time/{}", fields[0]),
                    fields[2].parse().unwrap_or(0.0),
                ) {
                    points.push(Point::new("dashr_pg_query_time_ms_per_second", &ql, time));
                }
            }
        }
        Ok(points)
    }

    fn mysql(&mut self, container: &str) -> Result<Vec<Point>, String> {
        let text = docker_exec(
            container,
            r#"P="${MYSQL_ROOT_PASSWORD:-$MARIADB_ROOT_PASSWORD}"; B=$(command -v mysql || command -v mariadb); MYSQL_PWD="$P" "$B" -uroot -N -B -e "SHOW GLOBAL STATUS""#,
        )?;
        let status = key_values(&text);
        let get = |key: &str| status.get(key).copied();
        let l = [("database", container)];
        let mut points = Vec::new();
        for (key, name) in [
            ("Threads_connected", "dashr_mysql_connections"),
            ("Threads_running", "dashr_mysql_threads_running"),
        ] {
            if let Some(value) = get(key) {
                points.push(Point::new(name, &l, value));
            }
        }
        for (key, name) in [
            ("Questions", "dashr_mysql_queries_per_second"),
            ("Slow_queries", "dashr_mysql_slow_queries_per_second"),
            ("Bytes_received", "dashr_mysql_received_bytes_per_second"),
            ("Bytes_sent", "dashr_mysql_sent_bytes_per_second"),
            (
                "Aborted_connects",
                "dashr_mysql_aborted_connects_per_second",
            ),
            (
                "Innodb_row_lock_waits",
                "dashr_mysql_row_lock_waits_per_second",
            ),
        ] {
            if let Some(value) = get(key)
                && let Some(rate) = self.rate(name, value)
            {
                points.push(Point::new(name, &l, rate));
            }
        }
        if let (Some(requests), Some(reads)) = (
            get("Innodb_buffer_pool_read_requests").and_then(|v| self.rate("rr", v)),
            get("Innodb_buffer_pool_reads").and_then(|v| self.rate("r", v)),
        ) && requests > 0.0
        {
            points.push(Point::new(
                "dashr_mysql_buffer_pool_hit_ratio",
                &l,
                1.0 - reads / requests,
            ));
        }
        if points.is_empty() {
            return Err(format!("no status from {container}"));
        }
        Ok(points)
    }

    fn redis(&mut self, container: &str) -> Result<Vec<Point>, String> {
        let text = docker_exec(
            container,
            r#"redis-cli ${REDIS_PASSWORD:+-a "$REDIS_PASSWORD"} --no-auth-warning INFO"#,
        )?;
        let info = key_values(&text);
        let get = |key: &str| info.get(key).copied();
        let l = [("database", container)];
        let mut points = Vec::new();
        for (key, name) in [
            ("connected_clients", "dashr_redis_clients"),
            ("blocked_clients", "dashr_redis_blocked_clients"),
            ("used_memory", "dashr_redis_memory_bytes"),
            ("maxmemory", "dashr_redis_max_memory_bytes"),
            ("instantaneous_ops_per_sec", "dashr_redis_ops_per_second"),
        ] {
            if let Some(value) = get(key) {
                points.push(Point::new(name, &l, value));
            }
        }
        for (key, name) in [
            ("evicted_keys", "dashr_redis_evicted_keys_per_second"),
            ("expired_keys", "dashr_redis_expired_keys_per_second"),
        ] {
            if let Some(value) = get(key)
                && let Some(rate) = self.rate(name, value)
            {
                points.push(Point::new(name, &l, rate));
            }
        }
        if let (Some(hits), Some(misses)) = (
            get("keyspace_hits").and_then(|v| self.rate("hits", v)),
            get("keyspace_misses").and_then(|v| self.rate("misses", v)),
        ) && hits + misses > 0.0
        {
            points.push(Point::new(
                "dashr_redis_hit_ratio",
                &l,
                hits / (hits + misses),
            ));
        }
        if points.is_empty() {
            return Err(format!("no INFO from {container}"));
        }
        Ok(points)
    }
}

fn with_service(points: Vec<Point>, service: &str) -> Vec<Point> {
    points
        .into_iter()
        .map(|mut point| {
            if !point.labels.iter().any(|(key, _)| key == "service") {
                point.labels.push(("service".into(), service.to_owned()));
            }
            point
        })
        .collect()
}

fn exec(service: &str, command: &[String]) -> Result<Vec<Point>, String> {
    let text = run_command(&shell_argv(command), Duration::from_secs(60))?;
    let points = parse::prometheus_text(&text);
    if points.is_empty() {
        return Err(
            "the command printed no Prometheus samples (`name{label=\"v\"} 1.5` lines)".into(),
        );
    }
    Ok(with_service(points, service))
}

fn scrape(service: &str, url: &str) -> Result<Vec<Point>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into();
    let text = agent
        .get(url)
        .call()
        .map_err(|error| format!("{url}: {error}"))?
        .body_mut()
        .read_to_string()
        .map_err(|error| error.to_string())?;
    let points = parse::prometheus_text(&text);
    if points.is_empty() {
        return Err(format!("{url} returned no Prometheus samples"));
    }
    Ok(with_service(points, service))
}

// ---------------------------------------------------------------- streams

/// Follows a log command: every line to Loki, request metrics every 5 s.
/// A command that ends (a container restarted) is started again.
fn stream(
    id: &str,
    service: &str,
    command: &[String],
    exporter: &Exporter,
    stop: &AtomicBool,
    removed: &AtomicBool,
    health: &Health,
) {
    let argv = shell_argv(command);
    while !stop.load(Ordering::SeqCst) && !removed.load(Ordering::SeqCst) {
        let Some((program, args)) = argv.split_first() else {
            report(health, id, Err("empty command".into()));
            return;
        };
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child: Child = match child {
            Ok(child) => child,
            Err(error) => {
                report(health, id, Err(format!("{program}: {error}")));
                sleep_unless(&[stop, removed], Duration::from_secs(5));
                continue;
            }
        };
        report(health, id, Ok(()));
        let (sender, lines) = mpsc::channel::<LogLine>();
        for (pipe, which) in [
            (
                child
                    .stdout
                    .take()
                    .map(|p| Box::new(p) as Box<dyn Read + Send>),
                Stream::Stdout,
            ),
            (
                child
                    .stderr
                    .take()
                    .map(|p| Box::new(p) as Box<dyn Read + Send>),
                Stream::Stderr,
            ),
        ] {
            let Some(pipe) = pipe else { continue };
            let sender = sender.clone();
            thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    let body = clean_line(&line);
                    if body.is_empty() {
                        continue;
                    }
                    if sender
                        .send(LogLine {
                            time_unix_nano: now_nanos(),
                            stream: which,
                            body,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
        drop(sender);
        let mut window = RequestWindow::default();
        let mut batch = Vec::new();
        let mut last_logs = Instant::now();
        let mut last_metrics = Instant::now();
        loop {
            if stop.load(Ordering::SeqCst) || removed.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            let ended = match lines.recv_timeout(Duration::from_millis(200)) {
                Ok(line) => {
                    if let Some(request) = parse::parse_request(&line.body) {
                        window.record(&request);
                    }
                    batch.push(line);
                    false
                }
                Err(mpsc::RecvTimeoutError::Timeout) => false,
                Err(mpsc::RecvTimeoutError::Disconnected) => true,
            };
            if !batch.is_empty()
                && (last_logs.elapsed() >= Duration::from_secs(1) || batch.len() >= 500 || ended)
            {
                if let Err(error) = exporter.post("logs", &logs_payload(service, &batch)) {
                    report(health, id, Err(error));
                }
                batch.clear();
                last_logs = Instant::now();
            }
            if last_metrics.elapsed() >= Duration::from_secs(5) || ended {
                let seconds = last_metrics.elapsed().as_secs_f64();
                let points = window.flush(service, seconds);
                let _ = exporter.post("metrics", &metrics_payload(service, &points, now_nanos()));
                last_metrics = Instant::now();
            }
            if ended {
                break;
            }
        }
        let status = child.wait().map(|s| s.to_string()).unwrap_or_default();
        report(
            health,
            id,
            Err(format!("the log command ended ({status}); restarting")),
        );
        sleep_unless(&[stop, removed], Duration::from_secs(5));
    }
}

/// What one trial sample produced: metric names and label keys, never
/// values (they are data; the agent reads them through the masked tools).
#[derive(Debug, serde::Serialize)]
pub struct Trial {
    pub series: usize,
    pub metrics: Vec<String>,
    pub label_keys: Vec<String>,
}

/// Samples a collector once, so `dashr collect` can say straight away
/// whether it works. Streams only check that the command starts.
pub fn trial(collector: &Collector) -> Result<Trial, String> {
    if let Collector::Stream { command, .. } = collector {
        let argv = shell_argv(command);
        let (program, args) = argv.split_first().ok_or("empty command")?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("{program}: {error}"))?;
        thread::sleep(Duration::from_millis(800));
        let early = child.try_wait().ok().flatten();
        let _ = child.kill();
        let _ = child.wait();
        if let Some(status) = early
            && !status.success()
        {
            return Err(format!("the log command exited straight away ({status})"));
        }
        return Ok(Trial {
            series: 0,
            metrics: Vec::new(),
            label_keys: Vec::new(),
        });
    }
    let mut sampler = Sampler::new(collector.clone());
    let points = sampler.sample()?;
    let metrics: BTreeSet<String> = points.iter().map(|p| p.name.clone()).collect();
    let label_keys: BTreeSet<String> = points
        .iter()
        .flat_map(|p| p.labels.iter().map(|(k, _)| k.clone()))
        .collect();
    Ok(Trial {
        series: points.len(),
        metrics: metrics.into_iter().take(40).collect(),
        label_keys: label_keys.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_run_with_a_timeout_and_report_stderr() {
        if cfg!(windows) {
            return;
        }
        let ok = run_command(
            &["sh".into(), "-c".into(), "echo up 1".into()],
            Duration::from_secs(5),
        );
        assert_eq!(ok.unwrap().trim(), "up 1");
        let failed = run_command(
            &["sh".into(), "-c".into(), "echo boom >&2; exit 3".into()],
            Duration::from_secs(5),
        );
        assert!(failed.unwrap_err().contains("boom"));
        let slow = run_command(&["sleep".into(), "5".into()], Duration::from_millis(300));
        assert!(slow.unwrap_err().contains("longer than"));
    }

    #[test]
    fn exec_reads_prometheus_text_and_labels_the_service() {
        if cfg!(windows) {
            return;
        }
        let points = exec(
            "cloud",
            &[
                "sh".into(),
                "-c".into(),
                "printf 'lambda_errors{fn=\"a\"} 2\\n'".into(),
            ],
        )
        .unwrap();
        assert_eq!(points[0].name, "lambda_errors");
        assert!(
            points[0]
                .labels
                .contains(&("service".into(), "cloud".into()))
        );
        assert!(
            exec("x", &["true".into()])
                .unwrap_err()
                .contains("no Prometheus")
        );
    }

    #[test]
    fn host_and_process_samples_carry_the_expected_metrics() {
        let mut host = Sampler::new(Collector::Host);
        let first = host.sample().unwrap();
        assert!(
            first
                .iter()
                .any(|p| p.name == "dashr_host_memory_total_bytes" && p.value > 0.0)
        );
        let second = host.sample().unwrap();
        assert!(
            second
                .iter()
                .any(|p| p.name == "dashr_host_network_rx_bytes_per_second")
        );
        let mut process = Sampler::new(Collector::Process {
            name: "definitely-not-running-xyz".into(),
        });
        let points = process.sample().unwrap();
        assert_eq!(
            points
                .iter()
                .find(|p| p.name == "dashr_process_count")
                .unwrap()
                .value,
            0.0
        );
    }

    #[test]
    fn trial_reports_names_not_values() {
        let trial = trial(&Collector::Host).unwrap();
        assert!(trial.series > 0);
        assert!(trial.metrics.contains(&"dashr_host_cpu_percent".to_owned()));
        let json = serde_json::to_string(&trial).unwrap();
        assert!(!json.contains("value"));
    }
}

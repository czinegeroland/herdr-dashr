//! Database query performance: PostgreSQL and SQL Server (DEC-044).
//!
//! `dashr db add` connects a database to the session's Grafana as a
//! datasource, so Grafana is the only database client: it runs the live
//! tables (top queries, per-database share, waits, blocking, index advice)
//! on every refresh, and the collector samples the same statistics through it
//! and turns their cumulative counters into per-second series. Credentials go
//! to Grafana's encrypted store only; a password can come from a command
//! (an IAM token, a secrets manager) that the pane re-runs, and a tunnel
//! command (SSM port forwarding, `ssh -L`) is kept running by the pane.
//!
//! Everything here is pure: connection-string parsing, datasource JSON, the
//! SQL, the dashboards and the counter arithmetic.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::collect::Point;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Postgres,
    Mssql,
}

impl Engine {
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "pg" => Some(Engine::Postgres),
            "mssql" | "sqlserver" | "sql-server" | "azuresql" => Some(Engine::Mssql),
            _ => None,
        }
    }

    pub fn datasource_type(self) -> &'static str {
        match self {
            Engine::Postgres => "grafana-postgresql-datasource",
            Engine::Mssql => "mssql",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Engine::Postgres => 5432,
            Engine::Mssql => 1433,
        }
    }
}

/// A database connection, parsed from a connection string or given as
/// parts. `password` is never serialized.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Connection {
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(skip)]
    pub password: String,
    /// Postgres `sslmode`, or SQL Server `encrypt` (`true`, `false`, `disable`).
    pub tls: String,
    /// SQL Server `TrustServerCertificate`.
    #[serde(default)]
    pub trust_server_certificate: bool,
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn split_host_port(text: &str, default: u16) -> (String, u16) {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('[')
        && let Some((host, tail)) = rest.split_once(']')
    {
        let port = tail.trim_start_matches(':').parse().unwrap_or(default);
        return (host.to_owned(), port);
    }
    match text.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
            (host.to_owned(), port.parse().unwrap_or(default))
        }
        _ => (text.to_owned(), default),
    }
}

/// Parses a connection string: a `postgres://` / `postgresql://` /
/// `sqlserver://` URI, libpq `key=value` pairs, or an ADO.NET
/// `Server=...;Database=...;User ID=...;Password=...` string. `engine`
/// decides when the text alone cannot.
pub fn parse_connection(text: &str, engine: Option<Engine>) -> Result<Connection, String> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
        return parse_uri(text, Engine::Postgres);
    }
    if lower.starts_with("sqlserver://") || lower.starts_with("mssql://") {
        return parse_uri(text, Engine::Mssql);
    }
    if lower.contains("server=")
        || lower.contains("data source=")
        || lower.contains("initial catalog=")
    {
        return parse_ado(text);
    }
    if lower.contains("host=") || lower.contains("dbname=") {
        return parse_libpq(text);
    }
    Err(format!(
        "not a connection string dashr understands{}: use postgres://user:pass@host:5432/db, host=... dbname=..., or Server=...;Database=...;User ID=...;Password=...",
        engine.map(|_| "").unwrap_or("")
    ))
}

fn parse_uri(text: &str, engine: Engine) -> Result<Connection, String> {
    let rest = text.split_once("://").map(|(_, r)| r).unwrap_or(text);
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (credentials, hostpart) = match rest.rsplit_once('@') {
        Some((c, h)) => (Some(c), h),
        None => (None, rest),
    };
    let (hostport, path) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = split_host_port(hostport, engine.default_port());
    let (user, password) = match credentials {
        Some(c) => match c.split_once(':') {
            Some((u, p)) => (percent_decode(u), percent_decode(p)),
            None => (percent_decode(c), String::new()),
        },
        None => (String::new(), String::new()),
    };
    let params: BTreeMap<String, String> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_ascii_lowercase(), percent_decode(v)))
        .collect();
    let database = if path.is_empty() {
        params
            .get("database")
            .or_else(|| params.get("dbname"))
            .cloned()
            .unwrap_or_default()
    } else {
        percent_decode(path)
    };
    let mut connection = Connection {
        engine,
        host,
        port,
        database,
        user: params.get("user").cloned().unwrap_or(user),
        password: params.get("password").cloned().unwrap_or(password),
        tls: String::new(),
        trust_server_certificate: false,
    };
    match engine {
        Engine::Postgres => {
            connection.tls = params
                .get("sslmode")
                .cloned()
                .unwrap_or_else(|| "require".into());
        }
        Engine::Mssql => {
            connection.tls = tls_from_encrypt(params.get("encrypt").map(String::as_str));
            connection.trust_server_certificate = params
                .get("trustservercertificate")
                .is_some_and(|v| v.eq_ignore_ascii_case("true"));
        }
    }
    finish(connection)
}

fn parse_libpq(text: &str) -> Result<Connection, String> {
    let mut map = BTreeMap::new();
    for pair in text.split_whitespace() {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(k.to_ascii_lowercase(), v.trim_matches('\'').to_owned());
        }
    }
    finish(Connection {
        engine: Engine::Postgres,
        host: map.get("host").cloned().unwrap_or_default(),
        port: map.get("port").and_then(|p| p.parse().ok()).unwrap_or(5432),
        database: map.get("dbname").cloned().unwrap_or_default(),
        user: map.get("user").cloned().unwrap_or_default(),
        password: map.get("password").cloned().unwrap_or_default(),
        tls: map
            .get("sslmode")
            .cloned()
            .unwrap_or_else(|| "require".into()),
        trust_server_certificate: false,
    })
}

fn tls_from_encrypt(value: Option<&str>) -> String {
    match value.map(str::to_ascii_lowercase).as_deref() {
        Some("false" | "no" | "optional") => "false".into(),
        Some("disable") => "disable".into(),
        _ => "true".into(),
    }
}

fn parse_ado(text: &str) -> Result<Connection, String> {
    let mut map = BTreeMap::new();
    for pair in text.split(';') {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(
                k.trim().to_ascii_lowercase().replace(' ', ""),
                v.trim().trim_matches('"').trim_matches('\'').to_owned(),
            );
        }
    }
    let get = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| map.get(*k).cloned())
            .unwrap_or_default()
    };
    let server = get(&["server", "datasource", "address", "addr"]);
    let server = server.strip_prefix("tcp:").unwrap_or(&server).to_owned();
    if server.contains('\\') && !server.contains(',') {
        return Err(format!(
            "named instance `{server}` needs its port: use Server=host,port (SQL Browser is not used)"
        ));
    }
    let (host, port) = match server.split_once(',') {
        Some((h, p)) => (
            h.split('\\').next().unwrap_or(h).to_owned(),
            p.trim().parse().unwrap_or(1433),
        ),
        None => (server.clone(), 1433),
    };
    finish(Connection {
        engine: Engine::Mssql,
        host,
        port,
        database: get(&["database", "initialcatalog"]),
        user: get(&["userid", "uid", "user", "username"]),
        password: get(&["password", "pwd"]),
        tls: tls_from_encrypt(map.get("encrypt").map(String::as_str)),
        trust_server_certificate: get(&["trustservercertificate"]).eq_ignore_ascii_case("true"),
    })
}

fn finish(connection: Connection) -> Result<Connection, String> {
    if connection.host.is_empty() {
        return Err("the connection string has no host".into());
    }
    if connection.user.is_empty() {
        return Err("the connection string has no user (integrated Windows authentication is not supported)".into());
    }
    Ok(connection)
}

/// The datasource uid for a database named `name`.
pub fn datasource_uid(name: &str) -> String {
    format!("db-{}", crate::ids::slug(name))
}

/// The Grafana datasource for a connection. `host` is rewritten so a
/// loopback address (a tunnel, a local server) resolves from the container.
pub fn datasource(name: &str, connection: &Connection) -> Value {
    let host = crate::provisioning::container_url(&connection.host);
    let url = format!("{host}:{}", connection.port);
    let json_data = match connection.engine {
        Engine::Postgres => json!({
            "database": connection.database,
            "sslmode": connection.tls,
            "postgresVersion": 1500,
            "maxOpenConns": 4,
            "maxIdleConns": 2,
            "connMaxLifetime": 300
        }),
        Engine::Mssql => json!({
            "database": connection.database,
            "encrypt": connection.tls,
            "tlsSkipVerify": connection.trust_server_certificate,
            "maxOpenConns": 4,
            "maxIdleConns": 2,
            "connMaxLifetime": 300
        }),
    };
    json!({
        "uid": datasource_uid(name),
        "name": format!("db {name}"),
        "type": connection.engine.datasource_type(),
        "access": "proxy",
        "url": url,
        "user": connection.user,
        "database": connection.database,
        "jsonData": json_data,
        "secureJsonData": {"password": connection.password}
    })
}

/// Every query dashr sends starts with this comment, so its own queries are
/// left out of the statement statistics it shows.
pub const DASHR_TAG: &str = "/* dashr */";

/// A raw SQL query against the database's datasource.
pub fn sql_target(name: &str, engine: Engine, ref_id: &str, sql: &str) -> Value {
    json!({
        "refId": ref_id,
        "datasource": {"type": engine.datasource_type(), "uid": datasource_uid(name)},
        "rawSql": format!("{DASHR_TAG} {sql}"),
        "rawQuery": true,
        "editorMode": "code",
        "format": "table"
    })
}

// ================================================================ PostgreSQL
//
// PostgreSQL 13 or later. Per-statement statistics need the
// pg_stat_statements extension (`shared_preload_libraries` and
// `CREATE EXTENSION pg_stat_statements`; on RDS, Aurora, Azure and Cloud SQL
// it is a parameter plus the CREATE EXTENSION). Everything else works on a
// stock server with the `pg_monitor` role.

pub const PG_HAS_STATEMENTS: &str =
    "SELECT count(*) AS installed FROM pg_extension WHERE extname = 'pg_stat_statements'";

fn pg_top(order: &str, filter: &str) -> String {
    format!(
        "SELECT left(regexp_replace(s.query, '\\s+', ' ', 'g'), 300) AS query,
  d.datname AS database,
  s.calls,
  round(s.total_exec_time::numeric, 1) AS total_ms,
  round(s.mean_exec_time::numeric, 2) AS mean_ms,
  round(s.max_exec_time::numeric, 1) AS max_ms,
  round((100 * s.total_exec_time / nullif(sum(s.total_exec_time) OVER (), 0))::numeric, 2) AS pct_of_all_time,
  s.rows,
  round((100.0 * s.shared_blks_hit / nullif(s.shared_blks_hit + s.shared_blks_read, 0))::numeric, 1) AS cache_hit_pct,
  s.shared_blks_read AS disk_blocks_read,
  s.temp_blks_written AS temp_blocks_written
FROM pg_stat_statements s
JOIN pg_database d ON d.oid = s.dbid
WHERE s.query NOT LIKE '/* dashr */%'{filter}
ORDER BY {order} DESC
LIMIT 20"
    )
}

/// Statements that took the most time in total: where the database's time goes.
pub fn pg_top_by_total_time() -> String {
    pg_top("s.total_exec_time", "")
}

/// Statements that are slowest per call (run at least 5 times).
pub fn pg_top_by_mean_time() -> String {
    pg_top("s.mean_exec_time", " AND s.calls >= 5")
}

/// Statements that read the most from disk or spill to temp files.
pub fn pg_top_by_io() -> String {
    pg_top("s.shared_blks_read + s.temp_blks_written", "")
}

/// Each database's share of statement time, disk reads, buffer access,
/// temp writes and rows — the counterpart of SQL Server's per-database view.
pub const PG_DATABASE_SHARE: &str = "SELECT d.datname AS database,
  sum(s.calls) AS calls,
  round(sum(s.total_exec_time)::numeric, 1) AS total_ms,
  round((100 * sum(s.total_exec_time) / nullif(sum(sum(s.total_exec_time)) OVER (), 0))::numeric, 2) AS pct_time,
  round((100 * sum(s.shared_blks_read)::numeric / nullif(sum(sum(s.shared_blks_read)) OVER (), 0)), 2) AS pct_disk_reads,
  round((100 * sum(s.shared_blks_hit + s.shared_blks_read)::numeric / nullif(sum(sum(s.shared_blks_hit + s.shared_blks_read)) OVER (), 0)), 2) AS pct_buffer_access,
  round((100 * sum(s.temp_blks_written)::numeric / nullif(sum(sum(s.temp_blks_written)) OVER (), 0)), 2) AS pct_temp_writes,
  round((100 * sum(s.rows)::numeric / nullif(sum(sum(s.rows)) OVER (), 0)), 2) AS pct_rows
FROM pg_stat_statements s
JOIN pg_database d ON d.oid = s.dbid
WHERE s.query NOT LIKE '/* dashr */%'
GROUP BY d.datname
ORDER BY pct_time DESC NULLS LAST";

/// What runs right now: state, wait event, durations, who blocks whom.
pub const PG_ACTIVITY: &str = "SELECT pid,
  datname AS database,
  usename AS \"user\",
  state,
  coalesce(wait_event_type || ': ' || wait_event, 'CPU') AS waiting_on,
  round(extract(epoch FROM now() - query_start)::numeric, 1) AS query_seconds,
  round(extract(epoch FROM now() - xact_start)::numeric, 1) AS transaction_seconds,
  array_to_string(pg_blocking_pids(pid), ',') AS blocked_by,
  left(regexp_replace(query, '\\s+', ' ', 'g'), 300) AS query
FROM pg_stat_activity
WHERE pid <> pg_backend_pid() AND backend_type = 'client backend' AND state <> 'idle'
ORDER BY query_start NULLS LAST
LIMIT 50";

/// Tables read by sequential scans (index candidates) and bloat.
pub const PG_TABLES: &str = "SELECT schemaname || '.' || relname AS \"table\",
  seq_scan,
  seq_tup_read AS rows_read_by_seq_scans,
  idx_scan,
  n_live_tup AS live_rows,
  n_dead_tup AS dead_rows,
  round((100.0 * n_dead_tup / nullif(n_live_tup + n_dead_tup, 0))::numeric, 1) AS dead_pct,
  pg_size_pretty(pg_total_relation_size(relid)) AS size,
  greatest(last_autovacuum, last_vacuum) AS last_vacuum,
  greatest(last_autoanalyze, last_analyze) AS last_analyze
FROM pg_stat_user_tables
ORDER BY seq_tup_read DESC
LIMIT 20";

/// Indexes never used since statistics were reset, biggest first.
pub const PG_UNUSED_INDEXES: &str = "SELECT s.schemaname || '.' || s.relname AS \"table\",
  s.indexrelname AS \"index\",
  s.idx_scan AS scans,
  pg_size_pretty(pg_relation_size(s.indexrelid)) AS size
FROM pg_stat_user_indexes s
JOIN pg_index i ON i.indexrelid = s.indexrelid
WHERE s.idx_scan = 0 AND NOT i.indisunique AND NOT i.indisprimary
ORDER BY pg_relation_size(s.indexrelid) DESC
LIMIT 20";

/// One row per statistic for the collector: `metric`, `label`, `value`.
/// `_total` metrics are cumulative and become per-second rates.
pub const PG_COLLECT: &str = "SELECT 'connections' AS metric, coalesce(state, 'other') AS label, count(*)::float8 AS value
  FROM pg_stat_activity WHERE backend_type = 'client backend' GROUP BY state
UNION ALL SELECT 'active_sessions', coalesce(wait_event_type, 'CPU'), count(*)::float8
  FROM pg_stat_activity WHERE state = 'active' AND pid <> pg_backend_pid() AND backend_type = 'client backend' GROUP BY 2
UNION ALL SELECT 'blocked_sessions', '', count(*)::float8
  FROM pg_stat_activity WHERE cardinality(pg_blocking_pids(pid)) > 0
UNION ALL SELECT 'longest_transaction_seconds', '', coalesce(max(extract(epoch FROM now() - xact_start)), 0)::float8
  FROM pg_stat_activity WHERE state <> 'idle' AND pid <> pg_backend_pid() AND backend_type = 'client backend'
UNION ALL SELECT 'transactions_total', datname, (xact_commit + xact_rollback)::float8 FROM pg_stat_database WHERE datname IS NOT NULL
UNION ALL SELECT 'rollbacks_total', datname, xact_rollback::float8 FROM pg_stat_database WHERE datname IS NOT NULL
UNION ALL SELECT 'blocks_hit_total', datname, blks_hit::float8 FROM pg_stat_database WHERE datname IS NOT NULL
UNION ALL SELECT 'blocks_read_total', datname, blks_read::float8 FROM pg_stat_database WHERE datname IS NOT NULL
UNION ALL SELECT 'deadlocks_total', datname, deadlocks::float8 FROM pg_stat_database WHERE datname IS NOT NULL
UNION ALL SELECT 'temp_bytes_total', datname, temp_bytes::float8 FROM pg_stat_database WHERE datname IS NOT NULL
UNION ALL SELECT 'rows_read_total', datname, (tup_returned + tup_fetched)::float8 FROM pg_stat_database WHERE datname IS NOT NULL
UNION ALL SELECT 'rows_written_total', datname, (tup_inserted + tup_updated + tup_deleted)::float8 FROM pg_stat_database WHERE datname IS NOT NULL";

/// Per-statement counters for the collector (needs pg_stat_statements).
pub const PG_COLLECT_STATEMENTS: &str = "SELECT s.queryid::text AS id,
  d.datname AS database,
  left(regexp_replace(s.query, '\\s+', ' ', 'g'), 80) AS query,
  s.calls::float8 AS calls,
  s.total_exec_time AS time_ms,
  s.rows::float8 AS rows,
  (s.shared_blks_read + s.temp_blks_written)::float8 AS io_blocks
FROM pg_stat_statements s
JOIN pg_database d ON d.oid = s.dbid
WHERE s.query NOT LIKE '/* dashr */%'
ORDER BY s.total_exec_time DESC
LIMIT 15";

/// Statement time per database, for the time-share series.
pub const PG_COLLECT_DATABASE_TIME: &str =
    "SELECT d.datname AS database, sum(s.total_exec_time) AS time_ms
FROM pg_stat_statements s JOIN pg_database d ON d.oid = s.dbid
WHERE s.query NOT LIKE '/* dashr */%' GROUP BY d.datname";

// ================================================================ SQL Server
//
// SQL Server 2016 or later, Azure SQL Database and Managed Instance. The
// DMVs need VIEW SERVER STATE (VIEW SERVER PERFORMANCE STATE on 2022), or
// VIEW DATABASE STATE on Azure SQL Database, where they cover the current
// database only.

/// The statement's own text within its batch or procedure.
const MSSQL_STATEMENT: &str = "SUBSTRING(st.text, (qs.statement_start_offset / 2) + 1,
    ((CASE qs.statement_end_offset WHEN -1 THEN DATALENGTH(st.text) ELSE qs.statement_end_offset END
      - qs.statement_start_offset) / 2) + 1)";

fn mssql_top(order: &str) -> String {
    format!(
        "WITH q AS (
  SELECT qs.query_hash,
    SUM(qs.execution_count) AS executions,
    SUM(qs.total_worker_time) AS cpu_us,
    SUM(qs.total_elapsed_time) AS elapsed_us,
    SUM(qs.total_logical_reads) AS logical_reads,
    SUM(qs.total_physical_reads) AS physical_reads,
    SUM(qs.total_logical_writes) AS logical_writes,
    SUM(qs.total_rows) AS total_rows,
    MAX(qs.last_execution_time) AS last_execution_time,
    MIN(qs.creation_time) AS cached_since,
    COUNT(DISTINCT qs.plan_handle) AS plans
  FROM sys.dm_exec_query_stats qs
  CROSS APPLY sys.dm_exec_sql_text(qs.sql_handle) st
  WHERE st.text NOT LIKE '/* dashr */%'
  GROUP BY qs.query_hash
)
SELECT TOP 20
  sample.[database],
  sample.[statement],
  q.executions,
  CAST(q.cpu_us / 1000.0 AS decimal(18, 1)) AS total_cpu_ms,
  CAST(q.cpu_us / 1000.0 / NULLIF(q.executions, 0) AS decimal(18, 2)) AS avg_cpu_ms,
  CAST(q.elapsed_us / 1000.0 / NULLIF(q.executions, 0) AS decimal(18, 2)) AS avg_elapsed_ms,
  CAST(100.0 * q.cpu_us / NULLIF(SUM(q.cpu_us) OVER (), 0) AS decimal(6, 2)) AS pct_cpu,
  q.logical_reads / NULLIF(q.executions, 0) AS avg_logical_reads,
  q.physical_reads,
  q.logical_writes,
  q.total_rows / NULLIF(q.executions, 0) AS avg_rows,
  q.plans,
  q.last_execution_time,
  q.cached_since,
  CONVERT(varchar(18), q.query_hash, 1) AS query_hash
FROM q
CROSS APPLY (
  SELECT TOP 1
    COALESCE(DB_NAME(CONVERT(int, pa.value)), DB_NAME(st.dbid), '-') AS [database],
    LEFT({MSSQL_STATEMENT}, 4000) AS [statement]
  FROM sys.dm_exec_query_stats qs
  CROSS APPLY sys.dm_exec_sql_text(qs.sql_handle) st
  OUTER APPLY (SELECT value FROM sys.dm_exec_plan_attributes(qs.plan_handle) WHERE attribute = N'dbid') pa
  WHERE qs.query_hash = q.query_hash
  ORDER BY qs.total_worker_time DESC
) sample
ORDER BY {order} DESC"
    )
}

/// The top 20 statements by CPU — a rewrite of
/// `tools.GetTop20QueryStatsWithCpuAndPlan`: grouped by `query_hash` (the
/// same query with different literals is one row), the statement's own text
/// instead of the whole batch, averages without integer truncation, the
/// database from the plan attributes (ad-hoc queries have no `st.dbid`), and
/// no plan XML in the table (fetch it with `dashr db plan`).
pub fn mssql_top_by_cpu() -> String {
    mssql_top("q.cpu_us")
}

/// The top 20 statements by elapsed time.
pub fn mssql_top_by_elapsed() -> String {
    mssql_top("q.elapsed_us")
}

/// The top 20 statements by logical reads (memory and IO pressure).
pub fn mssql_top_by_reads() -> String {
    mssql_top("q.logical_reads")
}

/// Each database's share of CPU, elapsed time, reads, writes and memory
/// grants — a rewrite of `tools.GetQueryStatsByDatabasePercentage`: one
/// pass, `NULLIF` so an idle server does not divide by zero, times in ms.
pub const MSSQL_DATABASE_SHARE: &str = "SELECT [database],
  cpu_ms,
  CAST(100.0 * cpu_ms / NULLIF(SUM(cpu_ms) OVER (), 0) AS decimal(6, 2)) AS pct_cpu,
  elapsed_ms,
  CAST(100.0 * elapsed_ms / NULLIF(SUM(elapsed_ms) OVER (), 0) AS decimal(6, 2)) AS pct_elapsed,
  logical_reads,
  CAST(100.0 * logical_reads / NULLIF(SUM(logical_reads) OVER (), 0) AS decimal(6, 2)) AS pct_logical_reads,
  logical_writes,
  CAST(100.0 * logical_writes / NULLIF(SUM(logical_writes) OVER (), 0) AS decimal(6, 2)) AS pct_logical_writes,
  physical_reads,
  CAST(100.0 * physical_reads / NULLIF(SUM(physical_reads) OVER (), 0) AS decimal(6, 2)) AS pct_physical_reads,
  grant_kb,
  CAST(100.0 * grant_kb / NULLIF(SUM(grant_kb) OVER (), 0) AS decimal(6, 2)) AS pct_grant_kb
FROM (
  SELECT COALESCE(DB_NAME(CONVERT(int, pa.value)), '-') AS [database],
    SUM(qs.total_worker_time) / 1000 AS cpu_ms,
    SUM(qs.total_elapsed_time) / 1000 AS elapsed_ms,
    SUM(qs.total_logical_reads) AS logical_reads,
    SUM(qs.total_logical_writes) AS logical_writes,
    SUM(qs.total_physical_reads) AS physical_reads,
    SUM(qs.total_grant_kb) AS grant_kb
  FROM sys.dm_exec_query_stats qs
  CROSS APPLY (SELECT value FROM sys.dm_exec_plan_attributes(qs.plan_handle) WHERE attribute = N'dbid') pa
  CROSS APPLY sys.dm_exec_sql_text(qs.sql_handle) st
  WHERE st.text NOT LIKE '/* dashr */%'
  GROUP BY COALESCE(DB_NAME(CONVERT(int, pa.value)), '-')
) x
ORDER BY pct_cpu DESC";

/// Waits that are idle or background noise, left out of wait statistics.
const MSSQL_BENIGN_WAITS: &str = "'BROKER_EVENTHANDLER','BROKER_RECEIVE_WAITFOR','BROKER_TASK_STOP','BROKER_TO_FLUSH','BROKER_TRANSMITTER','CHECKPOINT_QUEUE','CHKPT','CLR_AUTO_EVENT','CLR_MANUAL_EVENT','CLR_SEMAPHORE','CXCONSUMER','DBMIRROR_DBM_EVENT','DBMIRROR_EVENTS_QUEUE','DBMIRROR_WORKER_QUEUE','DBMIRRORING_CMD','DIRTY_PAGE_POLL','DISPATCHER_QUEUE_SEMAPHORE','EXECSYNC','FSAGENT','FT_IFTS_SCHEDULER_IDLE_WAIT','FT_IFTSHC_MUTEX','HADR_CLUSAPI_CALL','HADR_FILESTREAM_IOMGR_IOCOMPLETION','HADR_LOGCAPTURE_WAIT','HADR_NOTIFICATION_DEQUEUE','HADR_TIMER_TASK','HADR_WORK_QUEUE','KSOURCE_WAKEUP','LAZYWRITER_SLEEP','LOGMGR_QUEUE','MEMORY_ALLOCATION_EXT','ONDEMAND_TASK_QUEUE','PARALLEL_REDO_DRAIN_WORKER','PARALLEL_REDO_LOG_CACHE','PARALLEL_REDO_TRAN_LIST','PARALLEL_REDO_WORKER_SYNC','PARALLEL_REDO_WORKER_WAIT_WORK','PREEMPTIVE_OS_FLUSHFILEBUFFERS','PREEMPTIVE_XE_GETTARGETSTATE','PVS_PREALLOCATE','PWAIT_ALL_COMPONENTS_INITIALIZED','PWAIT_DIRECTLOGCONSUMER_GETNEXT','PWAIT_EXTENSIBILITY_CLEANUP_TASK','QDS_PERSIST_TASK_MAIN_LOOP_SLEEP','QDS_ASYNC_QUEUE','QDS_CLEANUP_STALE_QUERIES_TASK_MAIN_LOOP_SLEEP','QDS_SHUTDOWN_QUEUE','REDO_THREAD_PENDING_WORK','REQUEST_FOR_DEADLOCK_SEARCH','RESOURCE_QUEUE','SERVER_IDLE_CHECK','SLEEP_BPOOL_FLUSH','SLEEP_DBSTARTUP','SLEEP_DCOMSTARTUP','SLEEP_MASTERDBREADY','SLEEP_MASTERMDREADY','SLEEP_MASTERUPGRADED','SLEEP_MSDBSTARTUP','SLEEP_SYSTEMTASK','SLEEP_TASK','SLEEP_TEMPDBSTARTUP','SNI_HTTP_ACCEPT','SOS_WORK_DISPATCHER','SP_SERVER_DIAGNOSTICS_SLEEP','SQLTRACE_BUFFER_FLUSH','SQLTRACE_INCREMENTAL_FLUSH_SLEEP','SQLTRACE_WAIT_ENTRIES','STARTUP_DEPENDENCY_MANAGER','VDI_CLIENT_OTHER','WAIT_FOR_RESULTS','WAITFOR','WAITFOR_TASKSHUTDOWN','WAIT_XTP_RECOVERY','WAIT_XTP_HOST_WAIT','WAIT_XTP_OFFLINE_CKPT_NEW_LOG','WAIT_XTP_CKPT_CLOSE','XE_LIVE_TARGET_TVF','PREEMPTIVE_OS_QUERYREGISTRY','PREEMPTIVE_SP_SERVER_DIAGNOSTICS','PREEMPTIVE_HADR_LEASE_MECHANISM','HADR_FABRIC_CALLBACK','XE_DISPATCHER_JOIN','XE_DISPATCHER_WAIT','XE_TIMER_EVENT'";

/// Where SQL Server's time goes: top waits since start (or since the stats
/// were cleared), idle waits excluded. `sys.dm_db_wait_stats` on Azure SQL
/// Database.
pub fn mssql_waits(view: &str) -> String {
    format!(
        "SELECT TOP 15 wait_type,
  waiting_tasks_count AS waits,
  wait_time_ms,
  signal_wait_time_ms AS cpu_queue_ms,
  CAST(100.0 * wait_time_ms / NULLIF(SUM(wait_time_ms) OVER (), 0) AS decimal(6, 2)) AS pct,
  CAST(1.0 * wait_time_ms / NULLIF(waiting_tasks_count, 0) AS decimal(18, 2)) AS avg_wait_ms
FROM {view}
WHERE wait_time_ms > 0 AND wait_type NOT IN ({MSSQL_BENIGN_WAITS})
ORDER BY wait_time_ms DESC"
    )
}

/// What runs right now, with waits and blocking.
pub const MSSQL_ACTIVITY: &str = "SELECT r.session_id,
  DB_NAME(r.database_id) AS [database],
  s.login_name,
  r.status,
  r.command,
  r.wait_type,
  r.wait_time AS wait_ms,
  NULLIF(r.blocking_session_id, 0) AS blocked_by,
  r.cpu_time AS cpu_ms,
  r.total_elapsed_time AS elapsed_ms,
  r.logical_reads,
  LEFT(SUBSTRING(t.text, (r.statement_start_offset / 2) + 1,
    ((CASE r.statement_end_offset WHEN -1 THEN DATALENGTH(t.text) ELSE r.statement_end_offset END
      - r.statement_start_offset) / 2) + 1), 4000) AS [statement]
FROM sys.dm_exec_requests r
JOIN sys.dm_exec_sessions s ON s.session_id = r.session_id
OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t
WHERE r.session_id <> @@SPID AND s.is_user_process = 1
ORDER BY r.total_elapsed_time DESC";

/// The optimizer's missing-index suggestions, most valuable first.
pub const MSSQL_MISSING_INDEXES: &str = "SELECT TOP 20
  DB_NAME(d.database_id) AS [database],
  d.statement AS [table],
  d.equality_columns,
  d.inequality_columns,
  d.included_columns,
  s.user_seeks + s.user_scans AS uses,
  CAST(s.avg_total_user_cost * s.avg_user_impact * (s.user_seeks + s.user_scans) AS decimal(18, 0)) AS improvement,
  s.avg_user_impact AS impact_pct,
  s.last_user_seek
FROM sys.dm_db_missing_index_group_stats s
JOIN sys.dm_db_missing_index_groups g ON g.index_group_handle = s.group_handle
JOIN sys.dm_db_missing_index_details d ON d.index_handle = g.index_handle
ORDER BY improvement DESC";

/// Read and write latency per database file.
pub const MSSQL_FILE_IO: &str = "SELECT DB_NAME(f.database_id) AS [database],
  f.file_id,
  f.num_of_reads AS reads,
  f.num_of_writes AS writes,
  CAST(1.0 * f.io_stall_read_ms / NULLIF(f.num_of_reads, 0) AS decimal(18, 2)) AS avg_read_ms,
  CAST(1.0 * f.io_stall_write_ms / NULLIF(f.num_of_writes, 0) AS decimal(18, 2)) AS avg_write_ms,
  f.num_of_bytes_read / 1048576 AS mb_read,
  f.num_of_bytes_written / 1048576 AS mb_written
FROM sys.dm_io_virtual_file_stats(NULL, NULL) f
ORDER BY f.io_stall_read_ms + f.io_stall_write_ms DESC";

/// Query Store (when enabled in the connected database): the last hour's
/// slowest queries, surviving restarts and plan-cache evictions.
pub const MSSQL_QUERY_STORE: &str = "WITH r AS (
  SELECT p.query_id,
    SUM(rs.count_executions) AS executions,
    SUM(rs.avg_duration * rs.count_executions) AS duration_us,
    SUM(rs.avg_cpu_time * rs.count_executions) AS cpu_us,
    MAX(rs.max_duration) AS max_duration_us,
    COUNT(DISTINCT p.plan_id) AS plans
  FROM sys.query_store_runtime_stats rs
  JOIN sys.query_store_runtime_stats_interval i ON i.runtime_stats_interval_id = rs.runtime_stats_interval_id
  JOIN sys.query_store_plan p ON p.plan_id = rs.plan_id
  JOIN sys.query_store_query q ON q.query_id = p.query_id
  JOIN sys.query_store_query_text qt ON qt.query_text_id = q.query_text_id
  WHERE i.start_time >= DATEADD(hour, -1, SYSUTCDATETIME()) AND qt.query_sql_text NOT LIKE '/* dashr */%'
  GROUP BY p.query_id
)
SELECT TOP 20 LEFT(qt.query_sql_text, 4000) AS query,
  r.executions,
  CAST(r.duration_us / NULLIF(r.executions, 0) / 1000.0 AS decimal(18, 2)) AS avg_duration_ms,
  CAST(r.cpu_us / NULLIF(r.executions, 0) / 1000.0 AS decimal(18, 2)) AS avg_cpu_ms,
  CAST(r.max_duration_us / 1000.0 AS decimal(18, 1)) AS max_duration_ms,
  r.plans
FROM r
JOIN sys.query_store_query q ON q.query_id = r.query_id
JOIN sys.query_store_query_text qt ON qt.query_text_id = q.query_text_id
ORDER BY r.duration_us DESC";

/// Server counters for the collector: `metric`, `label`, `value`, `kind`
/// (`rate` for cumulative per-second counters, `ratio`/`base`, `gauge`).
pub const MSSQL_COLLECT_COUNTERS: &str = "SELECT RTRIM(counter_name) AS metric,
  RTRIM(instance_name) AS label,
  CAST(cntr_value AS float) AS value,
  CASE cntr_type WHEN 272696576 THEN 'rate' WHEN 537003264 THEN 'ratio' WHEN 1073939712 THEN 'base' ELSE 'gauge' END AS kind
FROM sys.dm_os_performance_counters
WHERE (counter_name IN ('Batch Requests/sec', 'SQL Compilations/sec', 'SQL Re-Compilations/sec', 'User Connections',
    'Processes blocked', 'Page life expectancy', 'Page reads/sec', 'Page writes/sec',
    'Buffer cache hit ratio', 'Buffer cache hit ratio base')
  AND (instance_name = '' OR object_name LIKE '%Buffer Manager%'))
  OR (counter_name IN ('Lock Waits/sec', 'Number of Deadlocks/sec') AND instance_name = '_Total')
  OR (counter_name IN ('Transactions/sec', 'Log Flushes/sec') AND instance_name NOT IN ('_Total', 'mssqlsystemresource'))";

/// Cumulative waits for the collector.
pub fn mssql_collect_waits(view: &str) -> String {
    format!(
        "SELECT TOP 12 wait_type AS label, CAST(wait_time_ms AS float) AS value
FROM {view}
WHERE wait_time_ms > 0 AND wait_type NOT IN ({MSSQL_BENIGN_WAITS})
ORDER BY wait_time_ms DESC"
    )
}

/// Cumulative per-database CPU, elapsed and reads for the collector.
pub const MSSQL_COLLECT_DATABASES: &str = "SELECT COALESCE(DB_NAME(CONVERT(int, pa.value)), '-') AS [database],
  CAST(SUM(qs.total_worker_time) / 1000.0 AS float) AS cpu_ms,
  CAST(SUM(qs.total_elapsed_time) / 1000.0 AS float) AS elapsed_ms,
  CAST(SUM(qs.total_logical_reads) AS float) AS logical_reads
FROM sys.dm_exec_query_stats qs
CROSS APPLY (SELECT value FROM sys.dm_exec_plan_attributes(qs.plan_handle) WHERE attribute = N'dbid') pa
CROSS APPLY sys.dm_exec_sql_text(qs.sql_handle) st
WHERE st.text NOT LIKE '/* dashr */%'
GROUP BY COALESCE(DB_NAME(CONVERT(int, pa.value)), '-')";

/// Cumulative per-statement counters for the collector (top by CPU).
pub fn mssql_collect_statements() -> String {
    format!(
        "WITH q AS (
  SELECT TOP 15 qs.query_hash, SUM(qs.execution_count) AS executions, SUM(qs.total_worker_time) AS cpu_us,
    SUM(qs.total_elapsed_time) AS elapsed_us, SUM(qs.total_logical_reads) AS logical_reads
  FROM sys.dm_exec_query_stats qs CROSS APPLY sys.dm_exec_sql_text(qs.sql_handle) st
  WHERE st.text NOT LIKE '/* dashr */%'
  GROUP BY qs.query_hash ORDER BY SUM(qs.total_worker_time) DESC
)
SELECT CONVERT(varchar(18), q.query_hash, 1) AS id,
  sample.[statement] AS query,
  CAST(q.executions AS float) AS calls,
  CAST(q.cpu_us / 1000.0 AS float) AS cpu_ms,
  CAST(q.elapsed_us / 1000.0 AS float) AS time_ms,
  CAST(q.logical_reads AS float) AS logical_reads
FROM q
CROSS APPLY (
  SELECT TOP 1 LEFT({MSSQL_STATEMENT}, 200) AS [statement]
  FROM sys.dm_exec_query_stats qs CROSS APPLY sys.dm_exec_sql_text(qs.sql_handle) st
  WHERE qs.query_hash = q.query_hash
) sample"
    )
}

/// Azure SQL Database's own resource use (15-second samples).
pub const MSSQL_AZURE_RESOURCES: &str =
    "SELECT TOP 1 CAST(avg_cpu_percent AS float) AS cpu_percent,
  CAST(avg_data_io_percent AS float) AS data_io_percent,
  CAST(avg_log_write_percent AS float) AS log_write_percent,
  CAST(avg_memory_usage_percent AS float) AS memory_percent
FROM sys.dm_db_resource_stats ORDER BY end_time DESC";

/// The cached plan of one statement, for `dashr db plan` (open the saved
/// `.sqlplan` in SSMS or Azure Data Studio).
pub fn mssql_plan(query_hash: &str) -> Option<String> {
    let hash = query_hash.trim();
    let valid = hash.len() == 18
        && hash.starts_with("0x")
        && hash[2..].chars().all(|c| c.is_ascii_hexdigit());
    valid.then(|| {
        format!(
            "SELECT TOP 1 CONVERT(nvarchar(max), qp.query_plan) AS query_plan
FROM sys.dm_exec_query_stats qs CROSS APPLY sys.dm_exec_query_plan(qs.plan_handle) qp
WHERE qs.query_hash = CONVERT(binary(8), '{hash}', 1)
ORDER BY qs.total_worker_time DESC"
        )
    })
}

// ================================================================ counters

/// Turns one statement's cumulative counters into rates and a mean over the
/// last interval: `calls_per_second`, `time_ms_per_second`, `mean_ms`.
#[derive(Debug, Default)]
pub struct Deltas {
    previous: BTreeMap<String, (f64, Vec<f64>)>,
}

impl Deltas {
    /// `values` are cumulative; returns their per-second rates since the
    /// previous call for `key`, or `None` the first time or after a reset.
    pub fn rates(&mut self, key: &str, now_secs: f64, values: &[f64]) -> Option<Vec<f64>> {
        let previous = self
            .previous
            .insert(key.to_owned(), (now_secs, values.to_vec()));
        let (then, before) = previous?;
        let elapsed = now_secs - then;
        if elapsed <= 0.0
            || before.len() != values.len()
            || values.iter().zip(&before).any(|(v, b)| v < b)
        {
            return None;
        }
        Some(
            values
                .iter()
                .zip(&before)
                .map(|(v, b)| (v - b) / elapsed)
                .collect(),
        )
    }
}

/// Points for one statement from its rates: calls/s, time ms/s, and the
/// mean time per call over the interval (only when it ran).
pub fn statement_points(
    db: &str,
    database: &str,
    query: &str,
    rates: &[f64],
    names: &[&str],
) -> Vec<Point> {
    let labels = [("db", db), ("database", database), ("query", query)];
    let mut points: Vec<Point> = names
        .iter()
        .zip(rates)
        .map(|(name, rate)| {
            Point::new(&format!("dashr_db_query_{name}_per_second"), &labels, *rate)
        })
        .collect();
    let calls = rates.first().copied().unwrap_or(0.0);
    if calls > 0.0
        && let Some(index) = names.iter().position(|n| *n == "time_ms")
    {
        points.push(Point::new(
            "dashr_db_query_mean_ms",
            &labels,
            rates[index] / calls,
        ));
    }
    if calls > 0.0
        && let Some(index) = names.iter().position(|n| *n == "cpu_ms")
    {
        points.push(Point::new(
            "dashr_db_query_mean_cpu_ms",
            &labels,
            rates[index] / calls,
        ));
    }
    points
}

/// The collector metric name for a Postgres statistic and its label key.
pub fn pg_metric(metric: &str) -> (String, &'static str) {
    let key = match metric {
        "connections" => "state",
        "active_sessions" => "wait",
        _ => "database",
    };
    match metric.strip_suffix("_total") {
        Some(base) => (format!("dashr_db_{base}_per_second"), key),
        None => (format!("dashr_db_{metric}"), key),
    }
}

/// The collector metric name for a SQL Server counter.
pub fn mssql_metric(counter: &str) -> String {
    let slug: String = counter
        .to_ascii_lowercase()
        .replace("/sec", "")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    if counter.ends_with("/sec") {
        format!("dashr_db_{slug}_per_second")
    } else {
        format!("dashr_db_{slug}")
    }
}

// ================================================================ dashboards

#[allow(clippy::too_many_arguments)] // a panel's layout, read at the call site
fn table(
    id: u64,
    title: &str,
    x: u64,
    y: u64,
    w: u64,
    h: u64,
    target: Value,
    description: &str,
) -> Value {
    json!({
        "id": id, "type": "table", "title": title, "description": description,
        "gridPos": {"x": x, "y": y, "w": w, "h": h},
        "datasource": target["datasource"].clone(),
        "targets": [target],
        "options": {"showHeader": true, "cellHeight": "sm"},
        "fieldConfig": {"defaults": {"custom": {"filterable": true}}, "overrides": []}
    })
}

#[allow(clippy::too_many_arguments)]
fn prom(
    id: u64,
    kind: &str,
    title: &str,
    x: u64,
    y: u64,
    w: u64,
    h: u64,
    expr: &str,
    legend: &str,
    unit: &str,
) -> Value {
    let mut panel = json!({
        "id": id, "type": kind, "title": title,
        "gridPos": {"x": x, "y": y, "w": w, "h": h},
        "datasource": {"type": "prometheus", "uid": "prometheus"},
        "targets": [{"refId": "A", "datasource": {"type": "prometheus", "uid": "prometheus"},
                     "expr": expr, "legendFormat": legend, "instant": kind == "stat", "range": kind != "stat"}],
        "fieldConfig": {"defaults": {"unit": unit}, "overrides": []}
    });
    if kind == "stat" {
        panel["options"] = json!({"reduceOptions": {"calcs": ["lastNotNull"]}, "colorMode": "value", "graphMode": "area"});
    }
    panel
}

/// What a database dashboard includes.
#[derive(Debug, Clone, Default)]
pub struct DashboardOptions {
    /// Per-statement statistics are readable.
    pub statements: bool,
    /// Azure SQL Database (database-scoped waits).
    pub azure: bool,
    /// The session has Prometheus, so the collector's trends exist.
    pub live: bool,
    /// Added to every panel id, to merge into an existing dashboard.
    pub id_offset: u64,
    /// Added to every panel's row, to place it below existing panels.
    pub y_offset: u64,
}

/// A ready performance dashboard for a database: live stats and trends from
/// the collector, and live tables straight from the database.
pub fn dashboard(name: &str, engine: Engine, options: &DashboardOptions) -> Value {
    let has_statements = options.statements;
    let db = format!("db=\"{}\"", name.replace('"', ""));
    let e = engine;
    let sql = |ref_id: &str, text: &str| sql_target(name, e, ref_id, text);
    let mut panels = Vec::new();
    match engine {
        Engine::Postgres => {
            panels.push(prom(
                1,
                "stat",
                "Transactions / s",
                0,
                0,
                4,
                4,
                &format!("sum(dashr_db_transactions_per_second{{{db}}})"),
                "",
                "ops",
            ));
            panels.push(prom(
                2,
                "stat",
                "Active sessions",
                4,
                0,
                4,
                4,
                &format!("sum(dashr_db_active_sessions{{{db}}})"),
                "",
                "none",
            ));
            panels.push(prom(
                3,
                "stat",
                "Blocked sessions",
                8,
                0,
                4,
                4,
                &format!("sum(dashr_db_blocked_sessions{{{db}}})"),
                "",
                "none",
            ));
            panels.push(prom(
                4,
                "stat",
                "Longest transaction",
                12,
                0,
                4,
                4,
                &format!("max(dashr_db_longest_transaction_seconds{{{db}}})"),
                "",
                "s",
            ));
            panels.push(prom(5, "stat", "Cache hit ratio", 16, 0, 4, 4, &format!("sum(dashr_db_blocks_hit_per_second{{{db}}}) / clamp_min(sum(dashr_db_blocks_hit_per_second{{{db}}}) + sum(dashr_db_blocks_read_per_second{{{db}}}), 1e-9)"), "", "percentunit"));
            panels.push(prom(
                6,
                "stat",
                "Deadlocks / s",
                20,
                0,
                4,
                4,
                &format!("sum(dashr_db_deadlocks_per_second{{{db}}})"),
                "",
                "none",
            ));
            panels.push(prom(
                7,
                "timeseries",
                "Active sessions by wait",
                0,
                4,
                12,
                8,
                &format!("sum by (wait) (dashr_db_active_sessions{{{db}}})"),
                "{{wait}}",
                "none",
            ));
            panels.push(prom(
                8,
                "timeseries",
                "Transactions and rows / s by database",
                12,
                4,
                12,
                8,
                &format!("sum by (database) (dashr_db_transactions_per_second{{{db}}})"),
                "{{database}}",
                "ops",
            ));
            if has_statements {
                panels.push(prom(
                    9,
                    "timeseries",
                    "Statement time (ms / s) — where the database's time goes",
                    0,
                    12,
                    12,
                    9,
                    &format!("topk(10, dashr_db_query_time_ms_per_second{{{db}}})"),
                    "{{query}}",
                    "ms",
                ));
                panels.push(prom(
                    10,
                    "timeseries",
                    "Mean time per call (ms)",
                    12,
                    12,
                    12,
                    9,
                    &format!("topk(10, dashr_db_query_mean_ms{{{db}}})"),
                    "{{query}}",
                    "ms",
                ));
                panels.push(prom(11, "timeseries", "Database time share", 0, 21, 12, 8, &format!("dashr_db_database_time_ms_per_second{{{db}}} / ignoring(database) group_left sum(dashr_db_database_time_ms_per_second{{{db}}})"), "{{database}}", "percentunit"));
                panels.push(prom(
                    12,
                    "timeseries",
                    "Calls / s",
                    12,
                    21,
                    12,
                    8,
                    &format!("topk(10, dashr_db_query_calls_per_second{{{db}}})"),
                    "{{query}}",
                    "ops",
                ));
                panels.push(table(20, "Top statements by total time", 0, 29, 24, 10, sql("A", &pg_top_by_total_time()), "pg_stat_statements since the last reset: the statements that took the most time in total."));
                panels.push(table(
                    21,
                    "Slowest statements per call",
                    0,
                    39,
                    24,
                    9,
                    sql("A", &pg_top_by_mean_time()),
                    "Highest mean execution time, run at least 5 times.",
                ));
                panels.push(table(
                    22,
                    "Statements reading the most from disk or temp files",
                    0,
                    48,
                    24,
                    9,
                    sql("A", &pg_top_by_io()),
                    "Shared blocks read from disk plus temp blocks written.",
                ));
                panels.push(table(23, "Share per database", 0, 57, 24, 7, sql("A", PG_DATABASE_SHARE), "Each database's share of statement time, disk reads, buffer access, temp writes and rows."));
            } else {
                panels.push(json!({"id": 9, "type": "text", "title": "Per-statement statistics are off",
                    "gridPos": {"x": 0, "y": 12, "w": 24, "h": 4},
                    "options": {"mode": "markdown", "content": "Enable **pg_stat_statements** to see which statements take the time: add it to `shared_preload_libraries` (on RDS/Aurora a parameter group, on Azure `azure.extensions`, on Cloud SQL a flag), restart, then `CREATE EXTENSION pg_stat_statements;`"}}));
            }
            panels.push(table(
                24,
                "Running now (and who blocks whom)",
                0,
                64,
                24,
                8,
                sql("A", PG_ACTIVITY),
                "pg_stat_activity: non-idle client sessions.",
            ));
            panels.push(table(25, "Tables: sequential scans and dead rows", 0, 72, 14, 9, sql("A", PG_TABLES), "Many rows read by sequential scans suggest a missing index; many dead rows suggest vacuum falls behind."));
            panels.push(table(
                26,
                "Unused indexes",
                14,
                72,
                10,
                9,
                sql("A", PG_UNUSED_INDEXES),
                "Never scanned since statistics were reset; they still cost writes.",
            ));
        }
        Engine::Mssql => {
            panels.push(prom(
                1,
                "stat",
                "Batch requests / s",
                0,
                0,
                4,
                4,
                &format!("sum(dashr_db_batch_requests_per_second{{{db}}})"),
                "",
                "ops",
            ));
            panels.push(prom(
                2,
                "stat",
                "Compilations / s",
                4,
                0,
                4,
                4,
                &format!("sum(dashr_db_sql_compilations_per_second{{{db}}})"),
                "",
                "ops",
            ));
            panels.push(prom(
                3,
                "stat",
                "Blocked processes",
                8,
                0,
                4,
                4,
                &format!("sum(dashr_db_processes_blocked{{{db}}})"),
                "",
                "none",
            ));
            panels.push(prom(
                4,
                "stat",
                "Page life expectancy",
                12,
                0,
                4,
                4,
                &format!("min(dashr_db_page_life_expectancy{{{db}}})"),
                "",
                "s",
            ));
            panels.push(prom(
                5,
                "stat",
                "Buffer cache hit ratio",
                16,
                0,
                4,
                4,
                &format!("min(dashr_db_buffer_cache_hit_ratio{{{db}}})"),
                "",
                "percentunit",
            ));
            panels.push(prom(
                6,
                "stat",
                "User connections",
                20,
                0,
                4,
                4,
                &format!("sum(dashr_db_user_connections{{{db}}})"),
                "",
                "none",
            ));
            panels.push(prom(
                7,
                "timeseries",
                "CPU by statement (ms / s)",
                0,
                4,
                12,
                9,
                &format!("topk(10, dashr_db_query_cpu_ms_per_second{{{db}}})"),
                "{{query}}",
                "ms",
            ));
            panels.push(prom(
                8,
                "timeseries",
                "Waits (ms / s) — where SQL Server's time goes",
                12,
                4,
                12,
                9,
                &format!("dashr_db_wait_ms_per_second{{{db}}}"),
                "{{wait}}",
                "ms",
            ));
            panels.push(prom(9, "timeseries", "CPU share by database", 0, 13, 12, 8, &format!("dashr_db_database_cpu_ms_per_second{{{db}}} / ignoring(database) group_left sum(dashr_db_database_cpu_ms_per_second{{{db}}})"), "{{database}}", "percentunit"));
            panels.push(prom(
                10,
                "timeseries",
                "Mean CPU per execution (ms)",
                12,
                13,
                12,
                8,
                &format!("topk(10, dashr_db_query_mean_cpu_ms{{{db}}})"),
                "{{query}}",
                "ms",
            ));
            panels.push(prom(
                11,
                "timeseries",
                "Azure SQL resource use",
                0,
                21,
                24,
                7,
                &format!("dashr_db_azure_resource_percent{{{db}}}"),
                "{{resource}}",
                "percent",
            ));
            panels.push(table(20, "Top 20 statements by CPU", 0, 28, 24, 10, sql("A", &mssql_top_by_cpu()), "sys.dm_exec_query_stats grouped by query_hash. `dashr db plan <db> <query_hash>` saves a statement's plan for SSMS."));
            panels.push(table(
                21,
                "Top 20 statements by elapsed time",
                0,
                38,
                24,
                9,
                sql("A", &mssql_top_by_elapsed()),
                "Slow for the caller, whether from CPU, waits or IO.",
            ));
            panels.push(table(
                22,
                "Top 20 statements by logical reads",
                0,
                47,
                24,
                9,
                sql("A", &mssql_top_by_reads()),
                "Memory and IO pressure.",
            ));
            panels.push(table(
                23,
                "Share per database",
                0,
                56,
                24,
                7,
                sql("A", MSSQL_DATABASE_SHARE),
                "Each database's share of CPU, elapsed time, reads, writes and memory grants.",
            ));
            panels.push(table(24, "Top waits", 0, 63, 12, 9, sql("A", &mssql_waits(if options.azure { "sys.dm_db_wait_stats" } else { "sys.dm_os_wait_stats" })), "Cumulative since start (the database's own on Azure SQL Database); idle waits excluded."));
            panels.push(table(
                25,
                "Read / write latency per file",
                12,
                63,
                12,
                9,
                sql("A", MSSQL_FILE_IO),
                "Average ms per read and write.",
            ));
            panels.push(table(
                26,
                "Running now (and who blocks whom)",
                0,
                72,
                24,
                8,
                sql("A", MSSQL_ACTIVITY),
                "sys.dm_exec_requests for user sessions.",
            ));
            panels.push(table(
                27,
                "Missing index suggestions",
                0,
                80,
                24,
                8,
                sql("A", MSSQL_MISSING_INDEXES),
                "The optimizer's suggestions, most valuable first — review before creating.",
            ));
            panels.push(table(
                28,
                "Query Store: slowest queries, last hour",
                0,
                88,
                24,
                8,
                sql("A", MSSQL_QUERY_STORE),
                "Empty unless Query Store is on in the connected database.",
            ));
        }
    }
    if !options.statements && engine == Engine::Mssql {
        panels.retain(|p| !matches!(p["id"].as_u64(), Some(7 | 9 | 10 | 20 | 21 | 22 | 23)));
    }
    if !options.azure && engine == Engine::Mssql {
        panels.retain(|p| p["id"].as_u64() != Some(11));
    }
    if !options.live {
        panels.retain(|p| p["datasource"]["type"] != "prometheus");
    }
    for panel in &mut panels {
        panel["id"] = json!(panel["id"].as_u64().unwrap_or(0) + options.id_offset);
        if let Some(y) = panel["gridPos"]["y"].as_u64() {
            panel["gridPos"]["y"] = json!(y + options.y_offset);
        }
    }
    json!({"dashboard": {
        "title": format!("{name} — query performance"),
        "refresh": "10s",
        "time": {"from": "now-30m", "to": "now"},
        "panels": panels
    }})
}

/// Every query dashr runs against an engine, by name, so the end-to-end
/// suite can run each one against a real server.
pub fn all_queries(engine: Engine) -> Vec<(&'static str, String)> {
    match engine {
        Engine::Postgres => vec![
            ("has_statements", PG_HAS_STATEMENTS.into()),
            ("top_by_total_time", pg_top_by_total_time()),
            ("top_by_mean_time", pg_top_by_mean_time()),
            ("top_by_io", pg_top_by_io()),
            ("database_share", PG_DATABASE_SHARE.into()),
            ("activity", PG_ACTIVITY.into()),
            ("tables", PG_TABLES.into()),
            ("unused_indexes", PG_UNUSED_INDEXES.into()),
            ("collect", PG_COLLECT.into()),
            ("collect_statements", PG_COLLECT_STATEMENTS.into()),
            ("collect_database_time", PG_COLLECT_DATABASE_TIME.into()),
        ],
        Engine::Mssql => vec![
            ("top_by_cpu", mssql_top_by_cpu()),
            ("top_by_elapsed", mssql_top_by_elapsed()),
            ("top_by_reads", mssql_top_by_reads()),
            ("database_share", MSSQL_DATABASE_SHARE.into()),
            ("waits", mssql_waits("sys.dm_os_wait_stats")),
            ("activity", MSSQL_ACTIVITY.into()),
            ("missing_indexes", MSSQL_MISSING_INDEXES.into()),
            ("file_io", MSSQL_FILE_IO.into()),
            ("query_store", MSSQL_QUERY_STORE.into()),
            ("collect_counters", MSSQL_COLLECT_COUNTERS.into()),
            ("collect_waits", mssql_collect_waits("sys.dm_os_wait_stats")),
            ("collect_databases", MSSQL_COLLECT_DATABASES.into()),
            ("collect_statements", mssql_collect_statements()),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_uris_with_escapes_and_params() {
        let c = parse_connection(
            "postgres://app%40corp:p%40ss%3Aword@db.example.com:6432/orders?sslmode=verify-full",
            None,
        )
        .unwrap();
        assert_eq!(
            (c.engine, c.host.as_str(), c.port),
            (Engine::Postgres, "db.example.com", 6432)
        );
        assert_eq!(
            (c.user.as_str(), c.password.as_str(), c.database.as_str()),
            ("app@corp", "p@ss:word", "orders")
        );
        assert_eq!(c.tls, "verify-full");
        let d = parse_connection("postgresql://u@localhost/app", None).unwrap();
        assert_eq!(
            (d.port, d.tls.as_str(), d.password.as_str()),
            (5432, "require", "")
        );
    }

    #[test]
    fn libpq_key_values() {
        let c = parse_connection(
            "host=10.0.0.5 port=5433 dbname=shop user=reader password='s3cret' sslmode=disable",
            None,
        )
        .unwrap();
        assert_eq!(
            (c.host.as_str(), c.port, c.database.as_str()),
            ("10.0.0.5", 5433, "shop")
        );
        assert_eq!((c.password.as_str(), c.tls.as_str()), ("s3cret", "disable"));
    }

    #[test]
    fn ado_net_strings_for_azure_and_local() {
        let azure = parse_connection("Server=tcp:acme.database.windows.net,1433;Initial Catalog=sales;Persist Security Info=False;User ID=admin;Password=Pa;ss=1;MultipleActiveResultSets=False;Encrypt=True;TrustServerCertificate=False;Connection Timeout=30;", None).unwrap();
        assert_eq!(
            (azure.engine, azure.host.as_str(), azure.port),
            (Engine::Mssql, "acme.database.windows.net", 1433)
        );
        assert_eq!(
            (
                azure.database.as_str(),
                azure.user.as_str(),
                azure.tls.as_str()
            ),
            ("sales", "admin", "true")
        );
        assert!(!azure.trust_server_certificate);
        let local = parse_connection("Data Source=localhost,14330;Database=app;UID=sa;PWD=x;Encrypt=False;TrustServerCertificate=True", None).unwrap();
        assert_eq!(
            (
                local.port,
                local.tls.as_str(),
                local.trust_server_certificate
            ),
            (14330, "false", true)
        );
        assert!(
            parse_connection(
                "Server=host\\SQLEXPRESS;Database=a;User ID=u;Password=p",
                None
            )
            .unwrap_err()
            .contains("port")
        );
        assert!(
            parse_connection("Server=h;Database=a;Integrated Security=SSPI", None)
                .unwrap_err()
                .contains("user")
        );
        let uri =
            parse_connection("sqlserver://sa:pw@db:1444?database=x&encrypt=disable", None).unwrap();
        assert_eq!(
            (
                uri.engine,
                uri.port,
                uri.database.as_str(),
                uri.tls.as_str()
            ),
            (Engine::Mssql, 1444, "x", "disable")
        );
    }

    #[test]
    fn datasources_carry_the_password_only_as_secure_data() {
        let c = parse_connection("postgres://app:pw@127.0.0.1:15432/orders", None).unwrap();
        let ds = datasource("Orders DB", &c);
        assert_eq!(ds["uid"], "db-orders-db");
        assert_eq!(ds["type"], "grafana-postgresql-datasource");
        assert_eq!(
            ds["url"], "host.docker.internal:15432",
            "a tunnel on the host is reached from the container"
        );
        assert_eq!(ds["secureJsonData"]["password"], "pw");
        let target = sql_target("orders", Engine::Postgres, "A", "SELECT 1");
        assert_eq!(
            target["rawSql"], "/* dashr */ SELECT 1",
            "dashr's own queries are tagged"
        );
        assert!(!ds["jsonData"].to_string().contains("pw"));
        let serialized = serde_json::to_string(&c).unwrap();
        assert!(
            !serialized.contains("\"pw\""),
            "a Connection never serializes its password"
        );
    }

    #[test]
    fn deltas_give_rates_and_means() {
        let mut deltas = Deltas::default();
        assert_eq!(deltas.rates("q1", 0.0, &[10.0, 100.0]), None);
        let rates = deltas.rates("q1", 10.0, &[30.0, 500.0]).unwrap();
        assert_eq!(rates, vec![2.0, 40.0]);
        assert_eq!(
            deltas.rates("q1", 20.0, &[5.0, 10.0]),
            None,
            "a reset is not a negative rate"
        );
        let points = statement_points("orders", "app", "SELECT $1", &rates, &["calls", "time_ms"]);
        let mean = points
            .iter()
            .find(|p| p.name == "dashr_db_query_mean_ms")
            .unwrap();
        assert_eq!(mean.value, 20.0);
        assert!(
            points
                .iter()
                .any(|p| p.name == "dashr_db_query_calls_per_second" && p.value == 2.0)
        );
    }

    #[test]
    fn metric_names() {
        assert_eq!(
            pg_metric("transactions_total"),
            ("dashr_db_transactions_per_second".into(), "database")
        );
        assert_eq!(
            pg_metric("active_sessions"),
            ("dashr_db_active_sessions".into(), "wait")
        );
        assert_eq!(
            mssql_metric("Batch Requests/sec"),
            "dashr_db_batch_requests_per_second"
        );
        assert_eq!(
            mssql_metric("SQL Re-Compilations/sec"),
            "dashr_db_sql_re_compilations_per_second"
        );
        assert_eq!(
            mssql_metric("Page life expectancy"),
            "dashr_db_page_life_expectancy"
        );
    }

    /// `DASHR_SQL_DUMP=<dir> cargo test -p dashr-core dump_queries -- --ignored`
    /// writes every query to a file, for running them against real servers.
    #[test]
    #[ignore]
    fn dump_queries() {
        let dir = std::env::var("DASHR_SQL_DUMP").expect("DASHR_SQL_DUMP");
        for (engine, prefix) in [(Engine::Postgres, "pg"), (Engine::Mssql, "mssql")] {
            for (name, sql) in all_queries(engine) {
                std::fs::write(format!("{dir}/{prefix}-{name}.sql"), sql).unwrap();
            }
        }
    }

    #[test]
    fn plans_only_for_well_formed_hashes() {
        assert!(
            mssql_plan("0x1A2B3C4D5E6F7A8B")
                .unwrap()
                .contains("CONVERT(binary(8), '0x1A2B3C4D5E6F7A8B', 1)")
        );
        assert!(mssql_plan("0x1A2B'; DROP TABLE x--").is_none());
        assert!(mssql_plan("1A2B3C4D5E6F7A8B").is_none());
    }

    #[test]
    fn dashboards_are_valid_and_point_at_the_database() {
        let full = DashboardOptions {
            statements: true,
            azure: true,
            live: true,
            ..Default::default()
        };
        for engine in [Engine::Postgres, Engine::Mssql] {
            let board = dashboard("orders", engine, &full);
            let model = &board["dashboard"];
            let panels = model["panels"].as_array().unwrap();
            assert!(panels.len() > 10);
            let ids: std::collections::BTreeSet<u64> =
                panels.iter().map(|p| p["id"].as_u64().unwrap()).collect();
            assert_eq!(ids.len(), panels.len(), "panel ids are unique");
            assert!(panels.iter().any(|p| p["datasource"]["uid"] == "db-orders"));
            assert!(model.to_string().contains("db=\\\"orders\\\""));
            let known = vec!["db-orders".to_owned(), "prometheus".to_owned()];
            let pins = crate::dashboard::Pins {
                uid: "dashr-x",
                refresh: "10s",
                time_from: "now-30m",
                known_datasources: &known,
            };
            let normalized = crate::dashboard::normalize(&board, &pins);
            assert!(normalized.is_ok(), "{normalized:?}");
        }
        let off = dashboard(
            "orders",
            Engine::Postgres,
            &DashboardOptions {
                live: true,
                ..Default::default()
            },
        );
        assert!(
            off.to_string()
                .contains("CREATE EXTENSION pg_stat_statements")
        );
        let tables_only = dashboard(
            "orders",
            Engine::Postgres,
            &DashboardOptions {
                statements: true,
                ..Default::default()
            },
        );
        assert!(
            !tables_only.to_string().contains("prometheus"),
            "no trends without Prometheus"
        );
        let azure = dashboard("orders", Engine::Mssql, &full).to_string();
        assert!(azure.contains("sys.dm_db_wait_stats"));
        let merged = dashboard(
            "orders",
            Engine::Postgres,
            &DashboardOptions {
                statements: true,
                live: true,
                id_offset: 100,
                y_offset: 40,
                ..Default::default()
            },
        );
        let first = &merged["dashboard"]["panels"][0];
        assert_eq!(
            (first["id"].as_u64(), first["gridPos"]["y"].as_u64()),
            (Some(101), Some(40))
        );
    }
}

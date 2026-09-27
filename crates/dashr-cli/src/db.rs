//! `dashr db`: database query performance (DEC-044).
//!
//! `add` connects a PostgreSQL or SQL Server database to the session's
//! Grafana, checks what it can see and starts its collector in the pane;
//! `dashboard` prints its ready-made dashboard; `plan` saves a SQL Server
//! statement's execution plan for the human. The agent only ever sees names,
//! flags and counts: the connection string goes straight to Grafana's
//! encrypted store and nothing a query returns is printed.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use dashr_core::collect::Collector;
use dashr_core::dbperf::{self, Connection, Engine};
use dashr_core::provisioning::DatasourcePolicy;
use dashr_core::session::{SessionRecord, SessionStore};
use dashr_grafana::Client;
use dashr_runtime::Paths;
use dashr_runtime::dbcollect::{self, Relay, Tunnel};
use serde_json::{Value, json};

use crate::Result;
use crate::agent::{COLLECTOR_HEALTH_FILE, print, require};

/// A shell command line as the argv the collectors run.
fn command_argv(command: &str) -> Vec<String> {
    if cfg!(windows) {
        // `shell_argv` prefixes `cmd /c`.
        vec![command.to_owned()]
    } else {
        vec!["sh".into(), "-c".into(), command.to_owned()]
    }
}

fn connection(args: &crate::DbAdd) -> Result<Connection> {
    let engine = match &args.engine {
        Some(text) => Some(
            Engine::parse(text)
                .ok_or_else(|| format!("unknown engine {text}; use postgres or mssql"))?,
        ),
        None => None,
    };
    let url = match (&args.url, &args.url_env) {
        (Some(url), _) if url == "-" => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|error| error.to_string())?;
            Some(text.trim().to_owned())
        }
        (Some(url), _) => Some(url.clone()),
        (None, Some(name)) => Some(
            std::env::var(name).map_err(|_| format!("environment variable {name} is not set"))?,
        ),
        (None, None) => None,
    };
    let mut connection = match url {
        Some(url) => dbperf::parse_connection(&url, engine)?,
        None => {
            let engine = engine.ok_or("give --url, --url-env, or --engine with --host")?;
            Connection {
                engine,
                host: args
                    .host
                    .clone()
                    .ok_or("--host is required without --url")?,
                port: args.port.unwrap_or(engine.default_port()),
                database: args.database.clone().unwrap_or_default(),
                user: args
                    .user
                    .clone()
                    .ok_or("--user is required without --url")?,
                password: String::new(),
                tls: match engine {
                    Engine::Postgres => "require".into(),
                    Engine::Mssql => "true".into(),
                },
                trust_server_certificate: false,
            }
        }
    };
    if let Some(port) = args.port {
        connection.port = port;
    }
    if let Some(host) = &args.host {
        connection.host = host.clone();
    }
    if let Some(tls) = &args.tls {
        connection.tls = tls.clone();
    }
    if args.trust_server_certificate {
        connection.trust_server_certificate = true;
    }
    if let Some(name) = &args.password_env {
        connection.password =
            std::env::var(name).map_err(|_| format!("environment variable {name} is not set"))?;
    }
    if connection.database.is_empty() {
        connection.database = match connection.engine {
            Engine::Postgres => "postgres".into(),
            Engine::Mssql => "master".into(),
        };
    }
    Ok(connection)
}

fn client(record: &SessionRecord) -> Result<Client> {
    let client = Client::local(&record.grafana_url());
    if !client.healthy() {
        return Err("the session's Grafana is not answering; is the dashboard pane open?".into());
    }
    Ok(client)
}

fn value_of(rows: &[dbcollect::Row], key: &str) -> Option<Value> {
    rows.first().and_then(|row| row.get(key)).cloned()
}

fn as_i64(value: Option<Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(i64::from(b)),
        _ => None,
    }
}

/// What the database lets dashr see, as flags and hints; never data.
struct Capabilities {
    statements: bool,
    azure: bool,
    report: Value,
}

fn capabilities(client: &Client, name: &str, engine: Engine) -> Capabilities {
    let query = |sql: &str| dbcollect::rows(client, name, engine, sql);
    match engine {
        Engine::Postgres => {
            let version = as_i64(
                query("SELECT current_setting('server_version_num')::int AS v")
                    .ok()
                    .and_then(|rows| value_of(&rows, "v")),
            );
            let installed = as_i64(
                query(dbperf::PG_HAS_STATEMENTS)
                    .ok()
                    .and_then(|rows| value_of(&rows, "installed")),
            )
            .unwrap_or(0)
                > 0;
            let readable = query("SELECT count(*) AS n FROM pg_stat_statements");
            let preloaded = query(
                "SELECT count(*) AS n FROM pg_settings WHERE name = 'shared_preload_libraries' AND setting LIKE '%pg_stat_statements%'",
            )
            .ok()
            .and_then(|rows| as_i64(value_of(&rows, "n")))
            .unwrap_or(0)
                > 0;
            let all_stats = query(
                "SELECT (pg_has_role(current_user, 'pg_read_all_stats', 'member') OR (SELECT rolsuper FROM pg_roles WHERE rolname = current_user))::int AS ok",
            )
            .ok()
            .and_then(|rows| as_i64(value_of(&rows, "ok")))
            .unwrap_or(0)
                > 0;
            let statements = installed && readable.is_ok();
            let mut hints = Vec::new();
            if version.is_some_and(|v| v < 130000) {
                hints.push(
                    "PostgreSQL older than 13: the statement tables need 13 or later".to_owned(),
                );
            }
            if !statements {
                hints.push(if preloaded {
                    "pg_stat_statements is loaded but not created in this database: ask the human to run `CREATE EXTENSION pg_stat_statements;` (or connect to the database where it exists)".to_owned()
                } else {
                    "pg_stat_statements is not loaded: it must be added to shared_preload_libraries (RDS/Aurora: the parameter group; Azure: azure.extensions and shared_preload_libraries; Cloud SQL: a flag), which restarts the server, then `CREATE EXTENSION pg_stat_statements;`. Until then the dashboard shows sessions, waits, tables and indexes.".to_owned()
                });
            }
            if !all_stats {
                hints.push("the user is not a member of pg_read_all_stats (or pg_monitor): other users' query texts show as <insufficient privilege>; `GRANT pg_monitor TO <user>;` fixes it".to_owned());
            }
            Capabilities {
                statements,
                azure: false,
                report: json!({
                    "server_version_num": version,
                    "pg_stat_statements": statements,
                    "read_all_stats": all_stats,
                    "hints": hints,
                }),
            }
        }
        Engine::Mssql => {
            let props = query(
                "SELECT CAST(SERVERPROPERTY('EngineEdition') AS int) AS edition, CAST(SERVERPROPERTY('ProductMajorVersion') AS int) AS major",
            )
            .unwrap_or_default();
            let edition = as_i64(value_of(&props, "edition"));
            let azure = edition == Some(5);
            let stats = query("SELECT COUNT(*) AS n FROM sys.dm_exec_query_stats");
            let query_store =
                query("SELECT actual_state_desc AS state FROM sys.database_query_store_options")
                    .ok()
                    .and_then(|rows| value_of(&rows, "state"))
                    .and_then(|v| v.as_str().map(str::to_owned));
            let mut hints = Vec::new();
            if let Err(error) = &stats {
                hints.push(format!(
                    "the plan cache is not readable ({}): grant VIEW SERVER STATE (SQL Server 2022: VIEW SERVER PERFORMANCE STATE; Azure SQL Database: VIEW DATABASE STATE)",
                    error.chars().take(160).collect::<String>()
                ));
            }
            if !matches!(query_store.as_deref(), Some("READ_WRITE" | "READ_ONLY")) {
                hints.push("Query Store is off in this database, so its table stays empty: `ALTER DATABASE CURRENT SET QUERY_STORE = ON;` keeps history across restarts".to_owned());
            }
            if azure {
                hints.push(
                    "Azure SQL Database: statistics cover the connected database only".to_owned(),
                );
            }
            Capabilities {
                statements: stats.is_ok(),
                azure,
                report: json!({
                    "engine_edition": edition,
                    "major_version": as_i64(value_of(&props, "major")),
                    "azure_sql_database": azure,
                    "plan_cache": stats.is_ok(),
                    "query_store": query_store,
                    "hints": hints,
                }),
            }
        }
    }
}

pub fn add(paths: &Paths, session: Option<&str>, args: crate::DbAdd) -> Result<()> {
    let store = SessionStore::new(&paths.state_dir);
    let mut record = require(&store, session)?;
    let mut connection = connection(&args)?;
    let client = client(&record)?;
    let name = args.name.clone();
    let uid = dbperf::datasource_uid(&name);
    let password_command = args
        .password_command
        .as_deref()
        .map(command_argv)
        .unwrap_or_default();
    let tunnel_command = args
        .tunnel_command
        .as_deref()
        .map(command_argv)
        .unwrap_or_default();

    // Bring the path up for the checks: the tunnel, the relay, the password.
    let mut tunnel = (!tunnel_command.is_empty()).then(|| Tunnel::new(&tunnel_command));
    if let Some(tunnel) = tunnel.as_mut() {
        tunnel.ensure()?;
        if !dbcollect::wait_for_port(
            &connection.host,
            connection.port,
            Duration::from_secs(60),
            &[],
        ) {
            return Err(format!(
                "the tunnel command did not open {}:{} within 60s",
                connection.host, connection.port
            ));
        }
    }
    let relay_port = Relay::needed(&connection.host).then_some(connection.port);
    let _relay = match relay_port {
        Some(port) => dbcollect::relay_for("127.0.0.1", port)?,
        None => None,
    };
    if !password_command.is_empty() {
        let output = dashr_runtime::collect::run_command(
            &dashr_runtime::collect::shell_argv(&password_command),
            Duration::from_secs(60),
        )
        .map_err(|error| format!("password command: {error}"))?;
        connection.password = output.trim().to_owned();
    }
    client
        .upsert_datasource(&dbperf::datasource(&name, &connection))
        .map_err(|error| error.to_string())?;
    if let Err(error) = client.datasource_health(&uid) {
        let _ = client.delete_datasource(&uid);
        return Err(format!("Grafana cannot connect to {name}: {error}"));
    }
    let caps = capabilities(&client, &name, connection.engine);
    if let Some(mut tunnel) = tunnel {
        tunnel.stop();
    }

    record.datasources.retain(|policy| policy.uid != uid);
    record.datasources.push(DatasourcePolicy {
        uid: uid.clone(),
        name: format!("db {name}"),
        plugin_type: connection.engine.datasource_type().to_owned(),
        // Query texts and rows are data: masked for the agent.
        personal: true,
        allow_fields: Vec::new(),
    });
    store.save(&record).map_err(|error| error.to_string())?;

    let live = record.otlp.is_some();
    if live {
        let collector = Collector::Database {
            name: name.clone(),
            engine: connection.engine,
            password_command,
            refresh_secs: args.refresh_secs,
            tunnel_command,
            statements: caps.statements,
            azure: caps.azure,
            relay_port,
        };
        let mut collectors = store.load_collectors(&record.session_id);
        let id = collector.id();
        collectors.retain(|c| c.id() != id);
        collectors.push(collector);
        store
            .save_collectors(&record.session_id, &collectors)
            .map_err(|error| error.to_string())?;
    }
    print(&json!({
        "added": name,
        "engine": connection.engine,
        "datasource": uid,
        "capabilities": caps.report,
        "live_series": live,
        "next": format!("dashr db dashboard {name} | dashr tool apply_dashboard --args-file -"),
        "note": if live {
            "The pane now samples this database every 30 seconds (and keeps the tunnel and password fresh); the dashboard's tables query it on every refresh."
        } else {
            "This session has no Prometheus, so there are no live trend series; the dashboard's tables still query the database on every refresh. Open the pane with DASHR_OTEL=1 for trends."
        },
    }))
}

fn find_collector(store: &SessionStore, record: &SessionRecord, name: &str) -> Option<Collector> {
    store
        .load_collectors(&record.session_id)
        .into_iter()
        .find(|c| matches!(c, Collector::Database { name: n, .. } if n == name))
}

fn engine_of(record: &SessionRecord, name: &str) -> Result<Engine> {
    let uid = dbperf::datasource_uid(name);
    let policy = record
        .policy(&uid)
        .ok_or_else(|| format!("no database {name}; see `dashr db list`"))?;
    Ok(if policy.plugin_type == Engine::Mssql.datasource_type() {
        Engine::Mssql
    } else {
        Engine::Postgres
    })
}

pub fn dashboard(
    paths: &Paths,
    session: Option<&str>,
    name: &str,
    id_offset: u64,
    y_offset: u64,
) -> Result<()> {
    let store = SessionStore::new(&paths.state_dir);
    let record = require(&store, session)?;
    let engine = engine_of(&record, name)?;
    let (statements, azure) = match find_collector(&store, &record, name) {
        Some(Collector::Database {
            statements, azure, ..
        }) => (statements, azure),
        _ => (true, false),
    };
    let options = dbperf::DashboardOptions {
        statements,
        azure,
        live: record.otlp.is_some(),
        id_offset,
        y_offset,
    };
    print(&dbperf::dashboard(name, engine, &options))
}

pub fn list(paths: &Paths, session: Option<&str>) -> Result<()> {
    let store = SessionStore::new(&paths.state_dir);
    let record = require(&store, session)?;
    let health: Value = std::fs::read(record.runtime_dir.join(COLLECTOR_HEALTH_FILE))
        .ok()
        .and_then(|text| serde_json::from_slice(&text).ok())
        .unwrap_or_else(|| json!({}));
    let list: Vec<Value> = record
        .datasources
        .iter()
        .filter(|policy| policy.uid.starts_with("db-"))
        .map(|policy| {
            let name = policy.name.trim_start_matches("db ").to_owned();
            let id = format!("db:{name}");
            json!({
                "name": name,
                "datasource": policy.uid,
                "type": policy.plugin_type,
                "collector": health.get(&id).cloned().unwrap_or(json!(
                    if find_collector(&store, &record, &name).is_some() { "starting" } else { "none" }
                )),
            })
        })
        .collect();
    print(&json!(list))
}

pub fn remove(paths: &Paths, session: Option<&str>, name: &str) -> Result<()> {
    let store = SessionStore::new(&paths.state_dir);
    let mut record = require(&store, session)?;
    let uid = dbperf::datasource_uid(name);
    engine_of(&record, name)?;
    let mut collectors = store.load_collectors(&record.session_id);
    collectors.retain(|c| c.id() != format!("db:{name}"));
    store
        .save_collectors(&record.session_id, &collectors)
        .map_err(|error| error.to_string())?;
    if let Ok(client) = client(&record) {
        let _ = client.delete_datasource(&uid);
    }
    record.datasources.retain(|policy| policy.uid != uid);
    store.save(&record).map_err(|error| error.to_string())?;
    print(&json!({"removed": name}))
}

pub fn plan(
    paths: &Paths,
    session: Option<&str>,
    name: &str,
    query_hash: &str,
    out: Option<PathBuf>,
) -> Result<()> {
    let store = SessionStore::new(&paths.state_dir);
    let record = require(&store, session)?;
    if engine_of(&record, name)? != Engine::Mssql {
        return Err("plans are saved for SQL Server; for PostgreSQL ask the human to run EXPLAIN (ANALYZE, BUFFERS) on the statement".into());
    }
    let sql = dbperf::mssql_plan(query_hash)
        .ok_or("the query hash looks like 0x1A2B3C4D5E6F7A8B (the query_hash column)")?;
    let client = client(&record)?;
    let rows = dbcollect::rows(&client, name, Engine::Mssql, &sql)?;
    let plan = rows
        .first()
        .and_then(|row| row.get("query_plan"))
        .and_then(Value::as_str)
        .filter(|plan| !plan.is_empty())
        .ok_or("no cached plan for that hash (it may have left the plan cache)")?;
    let path = out.unwrap_or_else(|| PathBuf::from(format!("{}.sqlplan", query_hash.trim())));
    std::fs::write(&path, plan).map_err(|error| format!("{}: {error}", path.display()))?;
    // The plan holds the statement's literals: it is for the human, who
    // opens it in SSMS or Azure Data Studio.
    print(
        &json!({"saved": path.display().to_string(), "bytes": plan.len(), "for": "the human: open it in SSMS or Azure Data Studio"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_args(url: Option<&str>) -> crate::DbAdd {
        crate::DbAdd {
            name: "orders".into(),
            url: url.map(str::to_owned),
            url_env: None,
            engine: None,
            host: None,
            port: None,
            database: None,
            user: None,
            password_env: None,
            password_command: None,
            refresh_secs: 600,
            tunnel_command: None,
            tls: None,
            trust_server_certificate: false,
        }
    }

    #[test]
    fn connections_from_a_url_or_parts() {
        let c = connection(&add_args(Some("postgres://app:pw@db:5432/orders"))).unwrap();
        assert_eq!(
            (c.host.as_str(), c.database.as_str(), c.password.as_str()),
            ("db", "orders", "pw")
        );

        // An RDS instance through an SSM tunnel with an IAM token.
        let mut args = add_args(None);
        args.engine = Some("postgres".into());
        args.host = Some("127.0.0.1".into());
        args.port = Some(15432);
        args.user = Some("dashr_ro".into());
        args.password_command = Some("aws rds generate-db-auth-token ...".into());
        let c = connection(&args).unwrap();
        assert_eq!(
            (c.port, c.database.as_str(), c.tls.as_str()),
            (15432, "postgres", "require")
        );
        assert!(
            c.password.is_empty(),
            "the password comes from the command later"
        );

        let mut missing = add_args(None);
        missing.engine = Some("mssql".into());
        assert!(connection(&missing).unwrap_err().contains("--host"));
        assert!(connection(&add_args(None)).unwrap_err().contains("--url"));
    }

    #[test]
    fn commands_run_through_the_shell() {
        let argv = command_argv("aws rds generate-db-auth-token --hostname h");
        if cfg!(windows) {
            assert_eq!(argv.len(), 1);
        } else {
            assert_eq!(argv[..2], ["sh", "-c"]);
        }
    }
}

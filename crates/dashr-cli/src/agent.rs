//! The commands the human's AI session runs against a session.
//!
//! Every answer is JSON on stdout (the sequence of a trace is text, which
//! reads better), errors go to stderr. Exit codes: 0 done or passed, 1 the
//! flow failed, 4 timed out, 5 any other error, 2 a bad command line.

use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use dashr_runtime::Paths;

use crate::client::{Client, connect};
use crate::jaeger_env;

pub const EXIT_FAILED: u8 = 1;
pub const EXIT_TIMEOUT: u8 = 4;

pub type Outcome = Result<u8, String>;

pub fn print(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

fn read_input(path: &Path) -> Result<Vec<u8>, String> {
    if path == Path::new("-") {
        let mut bytes = Vec::new();
        std::io::stdin()
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        Ok(bytes)
    } else {
        std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

fn encode(value: &str) -> String {
    dashr_runtime::jaeger::encode(value)
}

/// `dashr wait`: waits for a session (by pane id when given) to answer,
/// then prints what the agent needs to start.
pub fn wait(paths: &Paths, session: Option<&str>, timeout: u64) -> Outcome {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let selector = session
        .map(str::to_owned)
        .or_else(|| std::env::var("DASHR_SESSION").ok());
    let registry = dashr_runtime::Registry::new(&paths.state_dir);
    loop {
        if let Ok(record) = registry.find(selector.as_deref(), dashr_runtime::alive)
            && dashr_runtime::alive(&record)
        {
            let env = jaeger_env(&record);
            print(&json!({
                "session": record.session_id,
                "pane": record.pane_id,
                "otlp": {"http": record.otlp_http(), "grpc": record.otlp_grpc()},
                "jaeger_ui": record.jaeger_ui(),
                "env": env,
                "viewer": "the human Ctrl-clicks the viewer link in the dashr pane (sequence diagrams, flow verdicts, every span with its code); Jaeger's UI is linked there too",
                "next": "instrument the feature, `dashr spans set` the spans you added and where, `dashr flow set` the flow the feature should produce, then run it and `dashr flow wait`"
            }));
            return Ok(0);
        }
        if Instant::now() > deadline {
            eprintln!(
                "dashr: no session answered within {timeout}s (the first start pulls the Jaeger image)"
            );
            return Ok(EXIT_TIMEOUT);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

pub fn status(paths: &Paths, session: Option<&str>) -> Outcome {
    print(&connect(paths, session)?.get("status")?);
    Ok(0)
}

pub fn env(paths: &Paths, session: Option<&str>, shell: &str) -> Outcome {
    let client = connect(paths, session)?;
    let env = jaeger_env(&client.record);
    let pairs: Vec<(String, String)> = env
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                .collect()
        })
        .unwrap_or_default();
    match shell {
        "json" => print(&env),
        "powershell" | "pwsh" => pairs.iter().for_each(|(k, v)| println!("$env:{k} = '{v}'")),
        "cmd" => pairs.iter().for_each(|(k, v)| println!("set {k}={v}")),
        _ => pairs.iter().for_each(|(k, v)| println!("export {k}='{v}'")),
    }
    Ok(0)
}

#[allow(clippy::too_many_arguments)]
pub fn traces(
    paths: &Paths,
    session: Option<&str>,
    since: Option<String>,
    service: Option<String>,
    name: Option<String>,
    attrs: Vec<String>,
    errors: bool,
    limit: usize,
) -> Outcome {
    let mut query = vec![format!("limit={limit}")];
    if let Some(since) = since {
        query.push(format!("since={}", encode(&since)));
    }
    if let Some(service) = service {
        query.push(format!("service={}", encode(&service)));
    }
    if let Some(name) = name {
        query.push(format!("name={}", encode(&name)));
    }
    if !attrs.is_empty() {
        query.push(format!("attr={}", encode(&attrs.join(","))));
    }
    if errors {
        query.push("errors=1".into());
    }
    print(&connect(paths, session)?.get(&format!("traces?{}", query.join("&")))?);
    Ok(0)
}

pub fn trace(paths: &Paths, session: Option<&str>, id: &str, json_out: bool) -> Outcome {
    let trace = connect(paths, session)?.get(&format!("traces/{}", encode(id)))?;
    if json_out {
        print(&trace);
    } else {
        let summary = &trace["summary"];
        println!(
            "trace {} · {} spans · {} errors · {} orphans · {:.1} ms · services: {}",
            id,
            summary["spans"],
            summary["errors"],
            summary["orphans"],
            summary["duration_ms"].as_f64().unwrap_or(0.0),
            summary["services"]
                .as_array()
                .map(|s| s
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        );
        print!("{}", trace["sequence"].as_str().unwrap_or_default());
    }
    Ok(0)
}

/// Where the agent runs: the repository the Spans tab's editor opens.
fn cwd_query() -> String {
    std::env::current_dir()
        .map(|dir| format!("?cwd={}", encode(&dir.to_string_lossy())))
        .unwrap_or_default()
}

pub fn flow_set(paths: &Paths, session: Option<&str>, file: &Path) -> Outcome {
    let bytes = read_input(file)?;
    let name = serde_json::from_slice::<Value>(&bytes)
        .map_err(|e| format!("the flow is not JSON: {e}"))?
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or("the flow needs a \"name\"")?;
    let path = format!("flows/{}{}", encode(&name), cwd_query());
    let mut answer = connect(paths, session)?.send("PUT", &path, &bytes)?;
    answer["next"] = Value::String(format!(
        "run the feature (or ask the human to), then `dashr flow wait {name}`"
    ));
    print(&answer);
    Ok(0)
}

/// `dashr spans set`: the spans the agent added and where they are made.
pub fn spans_set(paths: &Paths, session: Option<&str>, file: &Path) -> Outcome {
    let bytes = read_input(file)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "the catalog is not UTF-8")?;
    dashr_core::catalog::Catalog::parse(text)?;
    let mut answer =
        connect(paths, session)?.send("PUT", &format!("catalog{}", cwd_query()), &bytes)?;
    answer["viewer"] = Value::String(
        "the human sees every span in the viewer's Spans tab and can open and edit its code there"
            .into(),
    );
    print(&answer);
    Ok(0)
}

/// `dashr spans`: every span the code has, planned and observed (masked).
pub fn spans_list(paths: &Paths, session: Option<&str>) -> Outcome {
    print(&connect(paths, session)?.get("spans")?);
    Ok(0)
}

pub fn flow_list(paths: &Paths, session: Option<&str>) -> Outcome {
    print(&connect(paths, session)?.get("flows")?);
    Ok(0)
}

pub fn flow_show(paths: &Paths, session: Option<&str>, name: &str) -> Outcome {
    print(&connect(paths, session)?.get(&format!("flows/{}", encode(name)))?);
    Ok(0)
}

pub fn flow_arm(paths: &Paths, session: Option<&str>, name: &str) -> Outcome {
    print(&connect(paths, session)?.send("POST", &format!("flows/{}/arm", encode(name)), b"{}")?);
    Ok(0)
}

pub fn flow_remove(paths: &Paths, session: Option<&str>, name: &str) -> Outcome {
    print(&connect(paths, session)?.delete(&format!("flows/{}", encode(name)))?);
    Ok(0)
}

fn sequence_of(client: &Client, trace_id: Option<&str>) -> Option<String> {
    let trace = client.get(&format!("traces/{}", encode(trace_id?))).ok()?;
    trace["sequence"].as_str().map(str::to_owned)
}

/// Waits for the verdict: a failure, or a pass that has settled.
pub fn flow_wait(paths: &Paths, session: Option<&str>, name: &str, timeout: u64) -> Outcome {
    let client = connect(paths, session)?;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let path = format!("flows/{}", encode(name));
    loop {
        let view = client.get(&path)?;
        let verdict = &view["verdict"];
        let status = verdict["status"].as_str().unwrap_or_default();
        let settled = verdict["settled"].as_bool().unwrap_or(false);
        let decided = status == "fail" || (status == "pass" && settled);
        let timed_out = !decided && Instant::now() > deadline;
        if decided || timed_out {
            let mut out = verdict.clone();
            if timed_out {
                out["timed_out"] = Value::Bool(true);
            }
            if let Some(sequence) = sequence_of(&client, verdict["trace_id"].as_str()) {
                out["sequence"] = Value::String(sequence);
            }
            print(&out);
            return Ok(match (timed_out, status) {
                (true, _) => EXIT_TIMEOUT,
                (false, "pass") => 0,
                _ => EXIT_FAILED,
            });
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}

#[allow(clippy::too_many_arguments)]
pub fn source_add(
    paths: &Paths,
    session: Option<&str>,
    name: &str,
    every: u64,
    format: &str,
    lookback: Option<u64>,
    keep_on_error: bool,
    command: Vec<String>,
) -> Outcome {
    dashr_core::ingest::Format::parse(format)?;
    let body = json!({
        "command": command, "every_secs": every, "format": format,
        "lookback_minutes": lookback, "keep_on_error": keep_on_error,
    });
    print(&connect(paths, session)?.send(
        "PUT",
        &format!("sources/{}", encode(name)),
        body.to_string().as_bytes(),
    )?);
    Ok(0)
}

pub fn source_list(paths: &Paths, session: Option<&str>) -> Outcome {
    print(&connect(paths, session)?.get("sources")?);
    Ok(0)
}

pub fn source_remove(paths: &Paths, session: Option<&str>, name: &str) -> Outcome {
    print(&connect(paths, session)?.delete(&format!("sources/{}", encode(name)))?);
    Ok(0)
}

pub fn ingest(
    paths: &Paths,
    session: Option<&str>,
    file: &Path,
    format: &str,
    source: &str,
) -> Outcome {
    dashr_core::ingest::Format::parse(format)?;
    let bytes = read_input(file)?;
    let path = format!("ingest?format={}&source={}", encode(format), encode(source));
    print(&connect(paths, session)?.send("POST", &path, &bytes)?);
    Ok(0)
}

pub fn sessions(paths: &Paths) -> Outcome {
    let records = dashr_runtime::Registry::new(&paths.state_dir).list();
    print(&Value::Array(
        records
            .iter()
            .map(|r| json!({"session": r.session_id, "pane": r.pane_id, "alive": dashr_runtime::alive(r), "otlp_http": r.otlp_http(), "jaeger_ui": r.jaeger_ui()}))
            .collect(),
    ));
    Ok(0)
}

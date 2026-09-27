//! Commands for the human's own AI session (DEC-039).
//!
//! As in herdr-remote-channel, the human talks only to their AI session. Its
//! skill opens the dashboard pane beside it with `herdr plugin pane open`,
//! waits for it with `dashr wait`, and builds the dashboard with
//! `dashr tool`, which calls the very tools the MCP server serves. The
//! masking is the same code path: nothing here reads a datasource itself.

use std::path::Path;
use std::time::{Duration, Instant};

use dashr_core::ids;
use dashr_core::session::{SessionRecord, SessionStore};
use dashr_herdr::Herdr;
use dashr_mcp::tools::DashrTools;
use dashr_mcp::{ToolOutput, Tools};
use dashr_runtime::Paths;
use serde_json::{Value, json};

use crate::Result;
use crate::herdr_cmds::load_config;
use crate::pane::BRIEF_FILE;

/// The Herdr socket of the calling pane, hashed as session records store it.
fn caller_socket_hash() -> Option<String> {
    std::env::var("HERDR_SOCKET_PATH")
        .ok()
        .filter(|socket| !socket.is_empty())
        .map(|socket| ids::socket_hash(&socket))
}

/// The sessions `key` names: a session id, or the id of its dashboard pane
/// (what `herdr plugin pane open` printed). Pane ids repeat across Herdr
/// servers, so a pane id only matches sessions of the caller's server.
pub fn matching<'a>(
    records: &'a [SessionRecord],
    key: &str,
    socket_hash: Option<&str>,
) -> Vec<&'a SessionRecord> {
    let by_id: Vec<_> = records.iter().filter(|r| r.session_id == key).collect();
    if !by_id.is_empty() {
        return by_id;
    }
    records
        .iter()
        .filter(|r| {
            r.pane_id.as_deref() == Some(key)
                && (socket_hash.is_none() || r.socket_hash.as_deref() == socket_hash)
        })
        .collect()
}

/// Finds the session to act on: `--session`, else `DASHR_SESSION`, else the
/// only session of the caller's Herdr server. `Ok(None)` while it does not
/// exist yet.
fn find(store: &SessionStore, session: Option<&str>) -> Result<Option<SessionRecord>> {
    let key = session
        .map(str::to_owned)
        .or_else(|| std::env::var("DASHR_SESSION").ok())
        .filter(|key| !key.is_empty());
    let socket = caller_socket_hash();
    let records = store.list();
    let found: Vec<&SessionRecord> = match &key {
        Some(key) => matching(&records, key, socket.as_deref()),
        None => records
            .iter()
            .filter(|r| socket.is_none() || r.socket_hash == socket)
            .collect(),
    };
    match found.as_slice() {
        [] => Ok(None),
        [record] => Ok(Some((*record).clone())),
        many => Err(format!(
            "{} dashr sessions are open; name one with --session (a session id or its dashboard pane id): {}",
            many.len(),
            many.iter()
                .map(|r| format!(
                    "{} (pane {})",
                    r.session_id,
                    r.pane_id.as_deref().unwrap_or("-")
                ))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn require(store: &SessionStore, session: Option<&str>) -> Result<SessionRecord> {
    find(store, session)?.ok_or_else(|| match session {
        Some(key) => format!(
            "no dashr session {key}; open the dashboard pane first (herdr plugin pane open --plugin herdr-dashr --entrypoint dashboard) and `dashr wait` for it"
        ),
        None => "no dashr session is open; open the dashboard pane first (herdr plugin pane open --plugin herdr-dashr --entrypoint dashboard) and `dashr wait` for it".to_owned(),
    })
}

/// What `dashr wait` prints once the session is ready.
pub fn ready(record: &SessionRecord, brief: &str) -> Value {
    let mut out = json!({
        "session": record.session_id,
        "pane": record.pane_id,
        "brief": brief,
    });
    if let Some(pipeline) = &record.pipeline {
        out["pipeline"] = json!(pipeline);
    }
    if let Some(otlp) = &record.otlp {
        out["otlp_endpoint"] = json!(otlp.http_endpoint());
    }
    out
}

/// `dashr wait`: blocks until the dashboard pane has its Grafana running and
/// prints the session and its briefing as JSON.
pub fn wait(paths: &Paths, session: Option<&str>, timeout: Duration) -> Result<()> {
    let store = SessionStore::new(&paths.state_dir);
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(record) = find(&store, session)?
            && let Ok(brief) = std::fs::read_to_string(record.runtime_dir.join(BRIEF_FILE))
        {
            return print(&ready(&record, &brief));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the dashboard did not start within {}s; look at the dashboard pane, or run `dashr doctor`",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The arguments of a tool call: `--args`, else `--args-file` (`-` for
/// stdin), else none.
fn arguments(args: Option<&str>, args_file: Option<&Path>) -> Result<Value> {
    let text = match (args, args_file) {
        (Some(text), _) => text.to_owned(),
        (None, Some(path)) if path == Path::new("-") => {
            std::io::read_to_string(std::io::stdin()).map_err(|error| error.to_string())?
        }
        (None, Some(path)) => std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?,
        (None, None) => return Ok(json!({})),
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("the tool arguments are not JSON: {error}"))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err("the tool arguments must be a JSON object".to_owned())
    }
}

/// `dashr tool`: one call of a dashr tool, printed as JSON; without a name,
/// the tool list.
pub fn tool(
    paths: &Paths,
    session: Option<&str>,
    name: Option<&str>,
    args: Option<&str>,
    args_file: Option<&Path>,
) -> Result<()> {
    let config = load_config(paths)?;
    let store = SessionStore::new(&paths.state_dir);
    let herdr = std::env::var("HERDR_BIN_PATH")
        .ok()
        .filter(|bin| !bin.is_empty())
        .map(|bin| Herdr::new(&bin));
    let Some(name) = name else {
        let tools = DashrTools::new(config, store, "", herdr);
        let list: Vec<Value> = tools
            .definitions()
            .into_iter()
            .map(|definition| {
                json!({
                    "name": definition["name"],
                    "description": definition["description"],
                    "arguments": definition["inputSchema"],
                })
            })
            .collect();
        return print(&json!(list));
    };
    let arguments = arguments(args, args_file)?;
    let record = require(&store, session)?;
    // Mask with the configuration the pane loaded, whatever this shell's
    // defaults are: the privacy boundary must not depend on where the
    // command was run from.
    let config = match std::fs::read_to_string(
        record
            .runtime_dir
            .join(dashr_runtime::paths::CONFIG_DIR_FILE),
    ) {
        Ok(dir) => dashr_core::Config::load_from_dir(Path::new(dir.trim()))
            .map_err(|error| error.to_string())?,
        Err(_) => config,
    };
    let mut tools = DashrTools::new(config, store, &record.session_id, herdr);
    match tools.call(name, &arguments) {
        ToolOutput::Json(value) => print(&value),
        ToolOutput::Image { data, mime_type } => {
            let bytes = decode_base64(&data).ok_or("the screenshot is not valid base64")?;
            let extension = if mime_type == "image/png" {
                "png"
            } else {
                "img"
            };
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or_default();
            let path = record
                .runtime_dir
                .join(format!("screenshot-{stamp}.{extension}"));
            std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
            print(&json!({"image": path, "mime_type": mime_type}))
        }
        ToolOutput::Error(message) => Err(message),
    }
}

fn print(value: &Value) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
    );
    Ok(())
}

/// Standard base64, padding optional; `None` on any other character.
pub fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(session: &str, pane: &str, socket: &str) -> SessionRecord {
        serde_json::from_value(json!({
            "session_id": session, "pane_id": pane, "socket_hash": socket,
            "container": "c", "port": 1, "runtime_dir": "/tmp/r",
            "dashboard_uid": "d", "refresh": "5s", "datasources": [], "started_unix": 0
        }))
        .unwrap()
    }

    #[test]
    fn a_session_is_named_by_its_id_or_its_dashboard_pane() {
        let records = [
            record("a-w1-p2", "w1:p2", "s1"),
            record("b-w1-p2", "w1:p2", "s2"),
        ];
        let ids = |found: Vec<&SessionRecord>| {
            found
                .into_iter()
                .map(|r| r.session_id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(matching(&records, "b-w1-p2", Some("s1"))), ["b-w1-p2"]);
        // A pane id only names a session of the caller's Herdr server.
        assert_eq!(ids(matching(&records, "w1:p2", Some("s1"))), ["a-w1-p2"]);
        assert_eq!(matching(&records, "w1:p2", None).len(), 2);
        assert!(matching(&records, "w9:p9", Some("s1")).is_empty());
    }

    #[test]
    fn ready_carries_the_briefing_and_no_grafana_address() {
        let out = ready(&record("a-w1-p2", "w1:p2", "s1"), "brief text");
        assert_eq!(out["session"], "a-w1-p2");
        assert_eq!(out["pane"], "w1:p2");
        assert_eq!(out["brief"], "brief text");
        // The agent never needs Grafana's address; the skill forbids using it.
        assert!(!out.to_string().contains("127.0.0.1"));
    }

    #[test]
    fn arguments_must_be_a_json_object() {
        assert_eq!(arguments(None, None).unwrap(), json!({}));
        assert_eq!(
            arguments(Some(r#"{"expr": "up"}"#), None).unwrap(),
            json!({"expr": "up"})
        );
        assert!(arguments(Some("[1]"), None).is_err());
        assert!(arguments(Some("{"), None).is_err());
    }

    #[test]
    fn base64_round_trips_png_bytes() {
        assert_eq!(decode_base64("iVBORw0KGgo=").unwrap(), b"\x89PNG\r\n\x1a\n");
        assert_eq!(decode_base64("aGk").unwrap(), b"hi");
        assert!(decode_base64("a*b").is_none());
    }
}

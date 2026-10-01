//! The session's HTTP routes (DASHR-SESSION-003).
//!
//! * `/api/...` — for `dashr` commands, with the agent token: everything
//!   that came from a trace is masked.
//! * `/v/...` — for the human's browser, with the viewer token from the
//!   pane's link: raw values, their own data.
//! * `/` — the viewer page itself; `/api/ping` — liveness, no token.

use std::sync::Arc;

use serde_json::{Value, json};

use dashr_core::Flow;
use dashr_core::ingest::Format;
use dashr_core::store::Filter;

use crate::http::{Handler, Request, Response};
use crate::session::now_ms;
use crate::state::{Shared, SourceSpec};

pub const VIEWER_HTML: &str = include_str!("../assets/viewer.html");

/// Constant-time comparison, so a token cannot be guessed byte by byte.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// `10m`, `2h`, `30s`, `1d` as milliseconds.
pub fn duration_ms(text: &str) -> Option<u64> {
    let text = text.trim();
    let (number, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit())?);
    let number: u64 = number.parse().ok()?;
    Some(
        number
            * match unit {
                "s" => 1_000,
                "m" => 60_000,
                "h" => 3_600_000,
                "d" => 86_400_000,
                _ => return None,
            },
    )
}

fn body_json(request: &Request) -> Result<Value, Response> {
    serde_json::from_slice(&request.body)
        .map_err(|e| Response::error(400, &format!("body is not JSON: {e}")))
}

fn filter(request: &Request) -> Result<Filter, Response> {
    let mut filter = Filter {
        limit: Some(20),
        ..Filter::default()
    };
    if let Some(since) = request.query.get("since") {
        let ms =
            duration_ms(since).ok_or_else(|| Response::error(400, "since: like 10m, 2h or 1d"))?;
        filter.since_ns = Some(now_ms().saturating_sub(ms) * 1_000_000);
    }
    filter.service = request
        .query
        .get("service")
        .cloned()
        .filter(|s| !s.is_empty());
    filter.name = request.query.get("name").cloned().filter(|s| !s.is_empty());
    filter.errors_only = request
        .query
        .get("errors")
        .is_some_and(|v| v == "1" || v == "true");
    if let Some(limit) = request.query.get("limit") {
        filter.limit = Some(
            limit
                .parse()
                .map_err(|_| Response::error(400, "limit: a number"))?,
        );
    }
    for pair in request
        .query
        .get("attr")
        .into_iter()
        .flat_map(|a| a.split(','))
    {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| Response::error(400, "attr: key=value[,key=value]"))?;
        filter
            .attributes
            .push((key.to_owned(), Value::String(value.to_owned())));
    }
    Ok(filter)
}

fn agent(shared: &Arc<Shared>, request: &Request, parts: &[&str]) -> Response {
    match (request.method.as_str(), parts) {
        ("GET", ["status"]) => Response::json(200, &shared.status()),
        ("GET", ["traces"]) => match filter(request) {
            Ok(filter) => {
                let summaries = shared.lock().store.summaries(&filter);
                Response::json(
                    200,
                    &Value::Array(summaries.iter().map(|s| shared.mask_summary(s)).collect()),
                )
            }
            Err(response) => response,
        },
        ("GET", ["traces", id]) => match shared.trace_for_agent(id) {
            Some(trace) => Response::json(200, &trace),
            None => Response::error(404, &format!("no trace {id}")),
        },
        ("GET", ["flows"]) => {
            let flows: Vec<Value> = shared
                .flow_names()
                .iter()
                .filter_map(|n| shared.flow_view(n, true))
                .collect();
            Response::json(200, &Value::Array(flows))
        }
        ("PUT", ["flows", name]) => {
            let text = String::from_utf8_lossy(&request.body);
            match Flow::parse(&text) {
                Ok(flow) if flow.name == *name => Response::json(200, &shared.set_flow(flow)),
                Ok(flow) => Response::error(
                    400,
                    &format!("the flow is named {:?}, not {name:?}", flow.name),
                ),
                Err(error) => Response::error(422, &error),
            }
        }
        ("GET", ["flows", name]) => match shared.flow_view(name, false) {
            Some(view) => Response::json(200, &view),
            None => Response::error(404, &format!("no flow {name:?}")),
        },
        ("POST", ["flows", name, "arm"]) => match shared.arm(name) {
            Ok(()) => Response::json(200, &json!({"armed": name})),
            Err(error) => Response::error(404, &error),
        },
        ("DELETE", ["flows", name]) => {
            if shared.remove_flow(name) {
                Response::json(200, &json!({"removed": name}))
            } else {
                Response::error(404, &format!("no flow {name:?}"))
            }
        }
        ("GET", ["sources"]) => Response::json(200, &shared.sources_view()),
        ("PUT", ["sources", name]) => {
            let mut body = match body_json(request) {
                Ok(body) => body,
                Err(response) => return response,
            };
            body["name"] = Value::String((*name).to_owned());
            match serde_json::from_value::<SourceSpec>(body) {
                Ok(spec) => match shared.add_source(spec) {
                    Ok(report) => Response::json(200, &json!({"added": name, "trial": report})),
                    Err(error) => Response::error(422, &error),
                },
                Err(error) => Response::error(400, &format!("invalid source: {error}")),
            }
        }
        ("DELETE", ["sources", name]) => {
            if shared.remove_source(name) {
                Response::json(200, &json!({"removed": name}))
            } else {
                Response::error(404, &format!("no source {name:?}"))
            }
        }
        ("POST", ["ingest"]) => {
            let format = match request
                .query
                .get("format")
                .map(|f| Format::parse(f))
                .transpose()
            {
                Ok(format) => format.unwrap_or_default(),
                Err(error) => return Response::error(400, &error),
            };
            let source = request
                .query
                .get("source")
                .cloned()
                .unwrap_or_else(|| "import".into());
            match shared.ingest(&request.body, format, &source) {
                Ok(report) => {
                    Response::json(200, &serde_json::to_value(report).unwrap_or_default())
                }
                Err(error) => Response::error(422, &error),
            }
        }
        _ => Response::error(404, "no such route"),
    }
}

fn viewer(shared: &Arc<Shared>, request: &Request, parts: &[&str]) -> Response {
    match (request.method.as_str(), parts) {
        ("GET", ["state"]) => Response::json(200, &shared.viewer_state()),
        ("GET", ["trace", id]) => match shared.viewer_trace(id) {
            Some(trace) => Response::json(200, &trace),
            None => Response::error(404, "no such trace"),
        },
        ("POST", ["flows", name, "review"]) => {
            let body = match body_json(request) {
                Ok(body) => body,
                Err(response) => return response,
            };
            let decision = body
                .get("decision")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let comment = body
                .get("comment")
                .and_then(Value::as_str)
                .map(str::to_owned);
            match shared.review(name, decision, comment) {
                Ok(()) => Response::json(200, &json!({"reviewed": name})),
                Err(error) => Response::error(400, &error),
            }
        }
        ("POST", ["flows", name, "arm"]) => match shared.arm(name) {
            Ok(()) => Response::json(200, &json!({"armed": name})),
            Err(error) => Response::error(404, &error),
        },
        _ => Response::error(404, "no such route"),
    }
}

/// The request handler for one session.
pub fn handler(shared: Arc<Shared>, agent_token: String, viewer_token: String) -> Handler {
    Arc::new(move |request: Request| {
        let path = request.path.clone();
        if request.method == "GET" && (path == "/" || path == "/index.html") {
            return Response::text(200, "text/html; charset=utf-8", VIEWER_HTML);
        }
        if path == "/api/ping" {
            return Response::json(200, &json!({"session": shared.info.session_id}));
        }
        let parts: Vec<String> = path
            .trim_matches('/')
            .split('/')
            .map(crate::http::decode_component)
            .collect();
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        match parts.split_first() {
            Some((&"api", rest)) => {
                let presented = request
                    .headers
                    .get("authorization")
                    .and_then(|h| h.strip_prefix("Bearer "))
                    .unwrap_or("");
                if !same(presented, &agent_token) {
                    return Response::error(401, "missing or wrong session token");
                }
                agent(&shared, &request, rest)
            }
            Some((&"v", rest)) => {
                let presented = request
                    .headers
                    .get("x-dashr-viewer")
                    .map(String::as_str)
                    .unwrap_or("");
                if !same(presented, &viewer_token) {
                    return Response::error(401, "open the link from the dashr pane");
                }
                viewer(&shared, &request, rest)
            }
            _ => Response::error(404, "no such page"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_tokens() {
        assert_eq!(duration_ms("10m"), Some(600_000));
        assert_eq!(duration_ms("2h"), Some(7_200_000));
        assert_eq!(duration_ms("5"), None);
        assert_eq!(duration_ms("5w"), None);
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "abcd"));
    }
}

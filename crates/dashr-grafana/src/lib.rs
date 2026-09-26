//! A small blocking client for the parts of Grafana's HTTP API dashr uses.
//!
//! Two Grafanas are spoken to: the pane-owned one on loopback, anonymous and
//! disposable, and — for `promote` only — a persistent one with a
//! service-account token. The token is held in memory and sent as a header;
//! it is never logged or put in a URL (requirement DASHR-PROMO-003).

use std::time::{Duration, Instant};

use dashr_core::dashboard;
use dashr_core::frames::{self, QueryResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
pub enum GrafanaError {
    #[error("Grafana at {base} is unreachable: {message}")]
    Unreachable { base: String, message: String },
    #[error("Grafana answered {status} for {path}: {message}")]
    Status {
        status: u16,
        path: String,
        message: String,
    },
    #[error("Grafana sent an unexpected answer for {path}: {message}")]
    Unexpected { path: String, message: String },
    #[error("Grafana did not become healthy within {0} seconds")]
    Timeout(u64),
}

/// A datasource as the Grafana API lists it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DatasourceInfo {
    pub uid: String,
    pub name: String,
    #[serde(rename = "type")]
    pub plugin_type: String,
}

/// What saving a dashboard returned.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Saved {
    pub uid: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub version: i64,
}

pub struct Client {
    base: String,
    token: Option<String>,
    agent: ureq::Agent,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Client")
            .field("base", &self.base)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl Client {
    /// A client for the anonymous, loopback Grafana of a session.
    pub fn local(base: &str) -> Self {
        Self::build(base, None, Duration::from_secs(30))
    }

    /// A client for a persistent Grafana, authenticated with a token.
    pub fn with_token(base: &str, token: String) -> Self {
        Self::build(base, Some(token), Duration::from_secs(30))
    }

    fn build(base: &str, token: Option<String>, timeout: Duration) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(timeout)
            .build();
        Self {
            base: base.trim_end_matches('/').to_owned(),
            token,
            agent,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn request(&self, method: &str, path: &str) -> ureq::Request {
        let request = self
            .agent
            .request(method, &format!("{}{path}", self.base))
            .set("Accept", "application/json");
        match &self.token {
            Some(token) => request.set("Authorization", &format!("Bearer {token}")),
            None => request,
        }
    }

    fn handle(
        &self,
        path: &str,
        result: Result<ureq::Response, ureq::Error>,
    ) -> Result<Value, GrafanaError> {
        match result {
            Ok(response) => {
                let text = response
                    .into_string()
                    .map_err(|error| GrafanaError::Unexpected {
                        path: path.to_owned(),
                        message: error.to_string(),
                    })?;
                if text.trim().is_empty() {
                    return Ok(Value::Null);
                }
                serde_json::from_str(&text).map_err(|error| GrafanaError::Unexpected {
                    path: path.to_owned(),
                    message: error.to_string(),
                })
            }
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                let message = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("message")
                            .or_else(|| value.get("error"))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| body.chars().take(300).collect());
                Err(GrafanaError::Status {
                    status,
                    path: path.to_owned(),
                    message,
                })
            }
            Err(ureq::Error::Transport(transport)) => Err(GrafanaError::Unreachable {
                base: self.base.clone(),
                message: transport.to_string(),
            }),
        }
    }

    fn get(&self, path: &str) -> Result<Value, GrafanaError> {
        self.handle(path, self.request("GET", path).call())
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, GrafanaError> {
        self.handle(path, self.request("POST", path).send_json(body.clone()))
    }

    /// `GET /api/health`: whether the database is up.
    pub fn healthy(&self) -> bool {
        self.get("/api/health")
            .ok()
            .and_then(|value| {
                value
                    .get("database")
                    .and_then(Value::as_str)
                    .map(|db| db == "ok")
            })
            .unwrap_or(false)
    }

    /// Polls health until it answers or `timeout` passes.
    pub fn wait_healthy(&self, timeout: Duration) -> Result<(), GrafanaError> {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if self.healthy() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err(GrafanaError::Timeout(timeout.as_secs()))
    }

    pub fn datasources(&self) -> Result<Vec<DatasourceInfo>, GrafanaError> {
        let value = self.get("/api/datasources")?;
        serde_json::from_value(value).map_err(|error| GrafanaError::Unexpected {
            path: "/api/datasources".into(),
            message: error.to_string(),
        })
    }

    pub fn datasource_by_name(&self, name: &str) -> Result<DatasourceInfo, GrafanaError> {
        let path = format!("/api/datasources/name/{}", encode_path(name));
        let value = self.get(&path)?;
        serde_json::from_value(value).map_err(|error| GrafanaError::Unexpected {
            path,
            message: error.to_string(),
        })
    }

    /// Saves a dashboard. `overwrite` replaces one with the same uid.
    pub fn save_dashboard(
        &self,
        dashboard: &Value,
        folder_uid: Option<&str>,
        overwrite: bool,
        message: &str,
    ) -> Result<Saved, GrafanaError> {
        let mut body = json!({
            "dashboard": dashboard,
            "overwrite": overwrite,
            "message": message,
        });
        if let Some(folder) = folder_uid {
            body["folderUid"] = json!(folder);
        }
        let value = self.post("/api/dashboards/db", &body)?;
        serde_json::from_value(value).map_err(|error| GrafanaError::Unexpected {
            path: "/api/dashboards/db".into(),
            message: error.to_string(),
        })
    }

    /// The dashboard model for `uid`.
    pub fn dashboard(&self, uid: &str) -> Result<Value, GrafanaError> {
        let path = format!("/api/dashboards/uid/{}", encode_path(uid));
        let value = self.get(&path)?;
        value
            .get("dashboard")
            .cloned()
            .ok_or_else(|| GrafanaError::Unexpected {
                path,
                message: "no dashboard in the answer".into(),
            })
    }

    /// The uid of the folder titled `title`, creating it when missing.
    pub fn ensure_folder(&self, title: &str) -> Result<String, GrafanaError> {
        let folders = self.get("/api/folders?limit=1000")?;
        if let Some(uid) = folders.as_array().and_then(|folders| {
            folders
                .iter()
                .find(|folder| folder.get("title").and_then(Value::as_str) == Some(title))
                .and_then(|folder| folder.get("uid").and_then(Value::as_str))
        }) {
            return Ok(uid.to_owned());
        }
        let created = self.post("/api/folders", &json!({"title": title}))?;
        created
            .get("uid")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| GrafanaError::Unexpected {
                path: "/api/folders".into(),
                message: "no uid in the answer".into(),
            })
    }

    /// Runs queries through `POST /api/ds/query`, one request per query.
    ///
    /// One request per query because Grafana fails the whole request when
    /// any single query names a datasource it cannot find, which would turn
    /// one broken target into a whole panel of errors (decision DEC-013).
    pub fn query(&self, queries: &[Value], from: &str, to: &str) -> Vec<QueryResult> {
        queries
            .iter()
            .map(|query| {
                let ref_id = query
                    .get("refId")
                    .and_then(Value::as_str)
                    .unwrap_or("A")
                    .to_owned();
                if dashboard::query_datasource_uid(query).is_some_and(|uid| uid.starts_with('$')) {
                    return QueryResult::failed(&ref_id, "query uses a datasource variable");
                }
                let mut query = query.clone();
                if let Some(object) = query.as_object_mut() {
                    object.entry("maxDataPoints").or_insert_with(|| json!(500));
                    object.entry("intervalMs").or_insert_with(|| json!(30_000));
                }
                let body = json!({"from": from, "to": to, "queries": [query]});
                match self.post("/api/ds/query", &body) {
                    Ok(value) => frames::parse_response(&value)
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| QueryResult::failed(&ref_id, "no result")),
                    // A failing query answers with a status but still carries
                    // a per-query result body; the error text is kept either
                    // way.
                    Err(GrafanaError::Status { message, .. }) => {
                        QueryResult::failed(&ref_id, message)
                    }
                    Err(error) => QueryResult::failed(&ref_id, error.to_string()),
                }
            })
            .collect()
    }
}

/// The dashboard's time range, defaulting to the last hour.
pub fn time_range(dashboard: &Value) -> (String, String) {
    let from = dashboard
        .pointer("/time/from")
        .and_then(Value::as_str)
        .unwrap_or("now-1h")
        .to_owned();
    let to = dashboard
        .pointer("/time/to")
        .and_then(Value::as_str)
        .unwrap_or("now")
        .to_owned();
    (from, to)
}

/// Percent-encodes one path segment.
pub fn encode_path(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests;

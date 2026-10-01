//! `dashr` commands talking to a running session's API.

use std::time::Duration;

use serde_json::Value;

use dashr_runtime::{Paths, Registry, SessionRecord};

pub struct Client {
    agent: ureq::Agent,
    pub record: SessionRecord,
}

/// The session to talk to: `--session` (an id or a pane id), else
/// `DASHR_SESSION`, else the only live one, else the newest live one.
pub fn connect(paths: &Paths, session: Option<&str>) -> Result<Client, String> {
    let selector = session
        .map(str::to_owned)
        .or_else(|| std::env::var("DASHR_SESSION").ok());
    let record = Registry::new(&paths.state_dir).find(selector.as_deref(), dashr_runtime::alive)?;
    Ok(Client::new(record))
}

impl Client {
    pub fn new(record: SessionRecord) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(180)))
            .http_status_as_error(false)
            .build()
            .into();
        Self { agent, record }
    }

    fn handle(
        result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<Value, String> {
        let mut response =
            result.map_err(|error| format!("the session did not answer: {error}"))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        if status / 100 == 2 {
            Ok(value)
        } else {
            Err(value
                .get("error")
                .and_then(Value::as_str)
                .map_or_else(|| value.to_string(), str::to_owned))
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/{path}", self.record.api_url())
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.record.agent_token)
    }

    pub fn get(&self, path: &str) -> Result<Value, String> {
        Self::handle(
            self.agent
                .get(self.url(path))
                .header("authorization", &self.auth())
                .call(),
        )
    }

    pub fn delete(&self, path: &str) -> Result<Value, String> {
        Self::handle(
            self.agent
                .delete(self.url(path))
                .header("authorization", &self.auth())
                .call(),
        )
    }

    pub fn send(&self, method: &str, path: &str, body: &[u8]) -> Result<Value, String> {
        let request = match method {
            "PUT" => self.agent.put(self.url(path)),
            _ => self.agent.post(self.url(path)),
        };
        Self::handle(
            request
                .header("authorization", &self.auth())
                .header("content-type", "application/json")
                .send(body),
        )
    }
}

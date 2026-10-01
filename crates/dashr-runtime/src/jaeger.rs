//! Talking to the session's Jaeger: reading traces from its query API and
//! sending pulled spans to its OTLP receiver.

use std::time::Duration;

use dashr_core::Span;
use dashr_core::ingest::{self, Format};

pub struct Jaeger {
    agent: ureq::Agent,
    ui: String,
    otlp_http: String,
}

impl Jaeger {
    pub fn new(ui_port: u16, otlp_http_port: u16) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            agent,
            ui: format!("http://127.0.0.1:{ui_port}"),
            otlp_http: format!("http://127.0.0.1:{otlp_http_port}"),
        }
    }

    fn get(&self, path: &str) -> Result<String, String> {
        let mut response = self
            .agent
            .get(format!("{}{path}", self.ui))
            .call()
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        if status == 200 {
            Ok(body)
        } else {
            Err(format!(
                "Jaeger answered {status}: {}",
                body.chars().take(200).collect::<String>()
            ))
        }
    }

    /// Whether the query API answers.
    pub fn healthy(&self) -> bool {
        self.get("/api/services").is_ok()
    }

    pub fn services(&self) -> Result<Vec<String>, String> {
        let body: serde_json::Value =
            serde_json::from_str(&self.get("/api/services")?).map_err(|e| e.to_string())?;
        Ok(body
            .get("data")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// The traces a service took part in between `start_us` and `end_us`.
    pub fn traces(
        &self,
        service: &str,
        start_us: u64,
        end_us: u64,
        limit: usize,
    ) -> Result<Vec<Span>, String> {
        let path = format!(
            "/api/traces?service={}&start={start_us}&end={end_us}&limit={limit}",
            encode(service)
        );
        let body = self.get(&path)?;
        ingest::parse(body.as_bytes(), Format::Jaeger, "otlp")
    }

    /// Sends spans to Jaeger's OTLP/HTTP receiver.
    pub fn send(&self, spans: &[Span]) -> Result<(), String> {
        if spans.is_empty() {
            return Ok(());
        }
        let body = ingest::encode_otlp(spans).to_string();
        let mut response = self
            .agent
            .post(format!("{}/v1/traces", self.otlp_http))
            .header("content-type", "application/json")
            .send(body.as_bytes())
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        if status / 100 == 2 {
            Ok(())
        } else {
            let text = response.body_mut().read_to_string().unwrap_or_default();
            Err(format!(
                "Jaeger refused the spans ({status}): {}",
                text.chars().take(200).collect::<String>()
            ))
        }
    }
}

/// Percent-encodes a query value.
pub fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn query_values_are_encoded() {
        assert_eq!(super::encode("orders api/v2"), "orders%20api%2Fv2");
    }
}

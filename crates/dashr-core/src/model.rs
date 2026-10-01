//! One span, whatever system it came from.
//!
//! Every source — an OpenTelemetry SDK exporting to the session, AWS X-Ray,
//! Zipkin, Jaeger, Azure Application Insights, Google Cloud Trace — is
//! converted into this one shape, so a trace that crosses systems is one
//! trace (DASHR-TRACE-002). Trace ids are 32 lowercase hex characters and
//! span ids 16, the W3C Trace Context form, which X-Ray ids also map onto.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    #[default]
    Internal,
    Server,
    Client,
    Producer,
    Consumer,
}

impl SpanKind {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.to_ascii_lowercase();
        let text = text.trim_start_matches("span_kind_");
        Some(match text {
            "internal" | "unspecified" | "" => Self::Internal,
            "server" | "rpc_server" => Self::Server,
            "client" | "rpc_client" => Self::Client,
            "producer" => Self::Producer,
            "consumer" => Self::Consumer,
            _ => return None,
        })
    }

    /// The OTLP enum value (0 unspecified, 1 internal, 2 server, ...).
    pub fn from_otlp(value: i64) -> Self {
        match value {
            2 => Self::Server,
            3 => Self::Client,
            4 => Self::Producer,
            5 => Self::Consumer,
            _ => Self::Internal,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Server => "server",
            Self::Client => "client",
            Self::Producer => "producer",
            Self::Consumer => "consumer",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Unset,
    Ok,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpanEvent {
    pub name: String,
    pub time_ns: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub trace_id: String,
    pub span_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub name: String,
    /// `service.name`, or the system's equivalent (X-Ray segment name,
    /// Application Insights role name, ...).
    pub service: String,
    #[serde(default)]
    pub kind: SpanKind,
    pub start_ns: u64,
    pub end_ns: u64,
    #[serde(default)]
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resource: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<SpanEvent>,
    /// Linked spans as `(trace_id, span_id)`: a batch consumer, a fan-in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<(String, String)>,
    /// Where the span came from: `otlp`, or the pull source's name.
    pub source: String,
}

impl Span {
    pub fn duration_ns(&self) -> u64 {
        self.end_ns.saturating_sub(self.start_ns)
    }

    pub fn duration_ms(&self) -> f64 {
        self.duration_ns() as f64 / 1e6
    }

    pub fn is_error(&self) -> bool {
        self.status == Status::Error
    }

    /// A span attribute, else a resource attribute.
    pub fn attribute(&self, key: &str) -> Option<&Value> {
        self.attributes.get(key).or_else(|| self.resource.get(key))
    }

    fn text(&self, key: &str) -> Option<String> {
        match self.attribute(key)? {
            Value::String(text) if !text.is_empty() => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        }
    }

    /// What a client or producer span talks to, by the OpenTelemetry
    /// semantic conventions: a named peer service, a database, a queue,
    /// an RPC service, else the server address.
    pub fn peer(&self) -> Option<String> {
        if let Some(peer) = self.text("peer.service") {
            return Some(peer);
        }
        if let Some(system) = self.text("db.system").or_else(|| self.text("db.system.name")) {
            return Some(match self.text("db.namespace").or_else(|| self.text("db.name")) {
                Some(name) => format!("{system}:{name}"),
                None => system,
            });
        }
        if let Some(destination) = self
            .text("messaging.destination.name")
            .or_else(|| self.text("messaging.destination"))
        {
            return Some(destination);
        }
        if let Some(system) = self.text("messaging.system") {
            return Some(system);
        }
        if let Some(service) = self.text("rpc.service") {
            return Some(service);
        }
        self.text("server.address")
            .or_else(|| self.text("net.peer.name"))
            .or_else(|| self.text("http.host"))
            .or_else(|| self.text("url.full").and_then(|url| host_of(&url)))
            .or_else(|| self.text("http.url").and_then(|url| host_of(&url)))
    }
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    (!host.is_empty()).then(|| host.to_owned())
}

/// 64-bit FNV-1a: stable across platforms and releases.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

fn is_hex(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// An id as `len` lowercase hex characters.
///
/// Hex ids are lowercased and left-padded (Zipkin's 64-bit trace ids,
/// X-Ray's dashed trace ids with the dashes removed). Anything else — the
/// hierarchical ids of old Application Insights SDKs — is hashed, so the
/// same input always gives the same id.
pub fn normalize_id(raw: &str, len: usize) -> String {
    let trimmed = raw.trim();
    // X-Ray: 1-5759e988-bd862e3fe1be46a994272793.
    let compact: String = match trimmed.strip_prefix("1-") {
        Some(rest) if rest.len() == 33 && rest.as_bytes()[8] == b'-' => rest.replace('-', ""),
        _ => trimmed.to_owned(),
    };
    if is_hex(&compact) && compact.len() <= len {
        return format!("{:0>len$}", compact.to_ascii_lowercase());
    }
    let mut out = String::new();
    let mut seed = 0u64;
    while out.len() < len {
        let mut bytes = compact.as_bytes().to_vec();
        bytes.extend_from_slice(&seed.to_le_bytes());
        out.push_str(&format!("{:016x}", fnv1a64(&bytes)));
        seed += 1;
    }
    out.truncate(len);
    out
}

pub fn trace_id(raw: &str) -> String {
    normalize_id(raw, 32)
}

pub fn span_id(raw: &str) -> String {
    normalize_id(raw, 16)
}

/// Bytes (OTLP protobuf ids) as lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decodes standard or URL-safe base64; `None` when it is not base64.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn span(attributes: Value) -> Span {
        Span {
            trace_id: "0".repeat(32),
            span_id: "1".repeat(16),
            parent_id: None,
            name: "call".into(),
            service: "api".into(),
            kind: SpanKind::Client,
            start_ns: 0,
            end_ns: 1,
            status: Status::Unset,
            status_message: None,
            attributes: serde_json::from_value(attributes).unwrap(),
            resource: BTreeMap::new(),
            events: vec![],
            links: vec![],
            source: "otlp".into(),
        }
    }

    #[test]
    fn ids_from_every_system_become_w3c_hex() {
        assert_eq!(
            trace_id("1-5759e988-bd862e3fe1be46a994272793"),
            "5759e988bd862e3fe1be46a994272793"
        );
        assert_eq!(trace_id("463AC35C9F6413AD"), "0000000000000000463ac35c9f6413ad");
        assert_eq!(span_id("53995c3f42cd8ad8"), "53995c3f42cd8ad8");
        let hashed = span_id("|abc.123.");
        assert_eq!(hashed.len(), 16);
        assert_eq!(hashed, span_id("|abc.123."), "stable");
        assert!(is_hex(&hashed));
    }

    #[test]
    fn base64_ids_decode() {
        assert_eq!(hex(&base64_decode("W47/KD8ruQhQ0hs8SZ1Oag==").unwrap()), "5b8eff283f2bb90850d21b3c499d4e6a");
        assert!(base64_decode("not base64!").is_none());
    }

    #[test]
    fn peers_follow_the_semantic_conventions() {
        assert_eq!(span(json!({"peer.service": "stock"})).peer().as_deref(), Some("stock"));
        assert_eq!(
            span(json!({"db.system": "postgresql", "db.name": "shop"})).peer().as_deref(),
            Some("postgresql:shop")
        );
        assert_eq!(
            span(json!({"messaging.system": "aws_sqs", "messaging.destination.name": "orders"}))
                .peer()
                .as_deref(),
            Some("orders")
        );
        assert_eq!(
            span(json!({"url.full": "https://user@pay.example.com:443/charge?x=1"})).peer().as_deref(),
            Some("pay.example.com:443")
        );
        assert_eq!(span(json!({})).peer(), None);
    }
}

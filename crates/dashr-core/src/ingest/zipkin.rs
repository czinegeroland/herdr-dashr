//! Zipkin v2 JSON: a list of spans, or `/api/v2/traces`' list of lists.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{number_u64, text};
use crate::model::{Span, SpanEvent, SpanKind, Status, span_id, trace_id};

fn one(span: &Value, source: &str) -> Option<Span> {
    let trace = trace_id(&text(span.get("traceId"))?);
    let own = span_id(&text(span.get("id"))?);
    let service = span
        .pointer("/localEndpoint/serviceName")
        .and_then(Value::as_str)
        .unwrap_or("unknown_service")
        .to_owned();
    let mut attributes: BTreeMap<String, Value> = span
        .get("tags")
        .and_then(Value::as_object)
        .map(|tags| tags.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    if let Some(remote) = span
        .pointer("/remoteEndpoint/serviceName")
        .and_then(Value::as_str)
    {
        attributes
            .entry("peer.service".into())
            .or_insert_with(|| Value::String(remote.to_owned()));
    }
    let error = attributes.get("error").map(|value| match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    });
    let start_ns = span.get("timestamp").and_then(number_u64).unwrap_or(0) * 1_000;
    let duration_ns = span.get("duration").and_then(number_u64).unwrap_or(0) * 1_000;
    let mut resource = BTreeMap::new();
    resource.insert("service.name".to_owned(), Value::String(service.clone()));
    let events = span
        .get("annotations")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| SpanEvent {
                    name: text(item.get("value")).unwrap_or_default(),
                    time_ns: item.get("timestamp").and_then(number_u64).unwrap_or(0) * 1_000,
                    attributes: BTreeMap::new(),
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Span {
        trace_id: trace,
        span_id: own,
        parent_id: text(span.get("parentId")).map(|p| span_id(&p)),
        name: text(span.get("name")).unwrap_or_default(),
        service,
        kind: span
            .get("kind")
            .and_then(Value::as_str)
            .and_then(SpanKind::parse)
            .unwrap_or_default(),
        start_ns,
        end_ns: start_ns + duration_ns,
        status: if error.is_some() {
            Status::Error
        } else {
            Status::Unset
        },
        status_message: error.filter(|e| !e.is_empty() && e != "true"),
        attributes,
        resource,
        events,
        links: Vec::new(),
        source: source.to_owned(),
    })
}

pub fn parse(document: &Value, source: &str) -> Result<Vec<Span>, String> {
    let mut out = Vec::new();
    let items: Vec<&Value> = match document {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    for item in items {
        match item {
            Value::Array(trace) => out.extend(trace.iter().filter_map(|s| one(s, source))),
            other => out.extend(one(other, source)),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn zipkin_spans() {
        let spans = parse(
            &json!([[{
                "traceId": "463ac35c9f6413ad", "id": "a2fb4a1d1a96d312", "parentId": "463ac35c9f6413ad",
                "name": "get /stock", "kind": "CLIENT", "timestamp": 1_000, "duration": 250,
                "localEndpoint": {"serviceName": "orders"}, "remoteEndpoint": {"serviceName": "stock"},
                "tags": {"http.method": "GET", "error": "timeout"}
            }]]),
            "zipkin",
        )
        .unwrap();
        let span = &spans[0];
        assert_eq!(span.trace_id, "0000000000000000463ac35c9f6413ad");
        assert_eq!(span.parent_id.as_deref(), Some("463ac35c9f6413ad"));
        assert_eq!((span.kind, span.duration_ns()), (SpanKind::Client, 250_000));
        assert_eq!(span.peer().as_deref(), Some("stock"));
        assert_eq!(
            (span.status, span.status_message.as_deref()),
            (Status::Error, Some("timeout"))
        );
    }
}

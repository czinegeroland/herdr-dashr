//! Jaeger query API JSON (`/api/traces`, `/api/traces/{id}`); Grafana
//! Tempo's Jaeger-compatible endpoints answer in the same shape.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{number_u64, text};
use crate::model::{Span, SpanEvent, SpanKind, Status, span_id, trace_id};

fn tags(items: Option<&Value>) -> BTreeMap<String, Value> {
    items
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|tag| Some((text(tag.get("key"))?, tag.get("value")?.clone())))
                .collect()
        })
        .unwrap_or_default()
}

fn trace(document: &Value, source: &str, out: &mut Vec<Span>) {
    let processes = document.get("processes").and_then(Value::as_object);
    for span in document.get("spans").and_then(Value::as_array).into_iter().flatten() {
        let (Some(raw_trace), Some(raw_span)) = (text(span.get("traceID")), text(span.get("spanID"))) else {
            continue;
        };
        let process = text(span.get("processID")).and_then(|id| processes?.get(&id).cloned());
        let service = process
            .as_ref()
            .and_then(|p| text(p.get("serviceName")))
            .unwrap_or_else(|| "unknown_service".into());
        let mut resource = tags(process.as_ref().and_then(|p| p.get("tags")));
        resource.insert("service.name".into(), Value::String(service.clone()));
        let mut attributes = tags(span.get("tags"));
        let kind = attributes
            .remove("span.kind")
            .and_then(|k| k.as_str().and_then(SpanKind::parse))
            .unwrap_or_default();
        let failed = attributes.get("error").is_some_and(|e| e == &Value::Bool(true) || e == "true")
            || attributes.get("otel.status_code").is_some_and(|c| c == "ERROR");
        let message = text(attributes.get("otel.status_description"));
        let parent = span
            .get("references")
            .and_then(Value::as_array)
            .and_then(|refs| refs.iter().find(|r| text(r.get("refType")).as_deref() == Some("CHILD_OF")).or(refs.first()))
            .and_then(|r| text(r.get("spanID")))
            .map(|p| span_id(&p));
        let start_ns = span.get("startTime").and_then(number_u64).unwrap_or(0) * 1_000;
        let events = span
            .get("logs")
            .and_then(Value::as_array)
            .map(|logs| {
                logs.iter()
                    .map(|log| {
                        let fields = tags(log.get("fields"));
                        SpanEvent {
                            name: text(fields.get("event")).unwrap_or_else(|| "log".into()),
                            time_ns: log.get("timestamp").and_then(number_u64).unwrap_or(0) * 1_000,
                            attributes: fields,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(Span {
            trace_id: trace_id(&raw_trace),
            span_id: span_id(&raw_span),
            parent_id: parent,
            name: text(span.get("operationName")).unwrap_or_default(),
            service,
            kind,
            start_ns,
            end_ns: start_ns + span.get("duration").and_then(number_u64).unwrap_or(0) * 1_000,
            status: if failed { Status::Error } else { Status::Unset },
            status_message: if failed { message } else { None },
            attributes,
            resource,
            events,
            links: Vec::new(),
            source: source.to_owned(),
        });
    }
}

pub fn parse(document: &Value, source: &str) -> Result<Vec<Span>, String> {
    let mut out = Vec::new();
    match document.get("data") {
        Some(Value::Array(traces)) => traces.iter().for_each(|t| trace(t, source, &mut out)),
        Some(single @ Value::Object(_)) => trace(single, source, &mut out),
        _ => trace(document, source, &mut out),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn jaeger_traces() {
        let spans = parse(
            &json!({"data": [{
                "traceID": "6f1a",
                "spans": [{
                    "traceID": "6f1a", "spanID": "b1", "operationName": "charge",
                    "references": [{"refType": "CHILD_OF", "traceID": "6f1a", "spanID": "a1"}],
                    "startTime": 10, "duration": 5, "processID": "p1",
                    "tags": [{"key": "span.kind", "type": "string", "value": "server"}, {"key": "error", "type": "bool", "value": true}]
                }],
                "processes": {"p1": {"serviceName": "payments", "tags": []}}
            }]}),
            "jaeger",
        )
        .unwrap();
        let span = &spans[0];
        assert_eq!((span.service.as_str(), span.kind, span.status), ("payments", SpanKind::Server, Status::Error));
        assert_eq!(span.parent_id.as_deref(), Some("00000000000000a1"));
        assert_eq!(span.duration_ns(), 5_000);
    }
}

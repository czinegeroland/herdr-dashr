//! OTLP/JSON (`ExportTraceServiceRequest` in the protobuf JSON mapping).
//!
//! Ids are hex in OTLP/JSON; some encoders emit the canonical protobuf JSON
//! base64 instead, which is accepted too. Enum fields may be numbers or
//! names, and 64-bit integers numbers or strings.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::number_u64;
use crate::model::{Span, SpanEvent, SpanKind, Status, base64_decode, hex, span_id, trace_id};

fn field<'a>(object: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    object.get(camel).or_else(|| object.get(snake))
}

fn list<'a>(object: &'a Value, camel: &str, snake: &str) -> &'a [Value] {
    field(object, camel, snake)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn id(value: Option<&Value>, len: usize) -> Option<String> {
    let text = value?.as_str()?;
    if text.is_empty() {
        return None;
    }
    if text.len() == len && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(text.to_ascii_lowercase());
    }
    let decoded = base64_decode(text).filter(|bytes| bytes.len() * 2 == len);
    match decoded {
        Some(bytes) if bytes.iter().all(|b| *b == 0) => None,
        Some(bytes) => Some(hex(&bytes)),
        None if len == 32 => Some(trace_id(text)),
        None => Some(span_id(text)),
    }
}

/// An OTLP `AnyValue` as plain JSON.
pub(crate) fn any_value(value: &Value) -> Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    if let Some(text) = object
        .get("stringValue")
        .or_else(|| object.get("string_value"))
    {
        return text.clone();
    }
    if let Some(flag) = object.get("boolValue").or_else(|| object.get("bool_value")) {
        return flag.clone();
    }
    if let Some(number) = object.get("intValue").or_else(|| object.get("int_value")) {
        return match number {
            Value::String(text) => text
                .parse::<i64>()
                .map(Value::from)
                .unwrap_or_else(|_| number.clone()),
            other => other.clone(),
        };
    }
    if let Some(number) = object
        .get("doubleValue")
        .or_else(|| object.get("double_value"))
    {
        return number.clone();
    }
    if let Some(array) = object
        .get("arrayValue")
        .or_else(|| object.get("array_value"))
    {
        return Value::Array(
            list(array, "values", "values")
                .iter()
                .map(any_value)
                .collect(),
        );
    }
    if let Some(kvlist) = object
        .get("kvlistValue")
        .or_else(|| object.get("kvlist_value"))
    {
        return Value::Object(
            attributes(list(kvlist, "values", "values"))
                .into_iter()
                .collect::<Map<_, _>>(),
        );
    }
    if let Some(bytes) = object
        .get("bytesValue")
        .or_else(|| object.get("bytes_value"))
    {
        return bytes.clone();
    }
    Value::Null
}

pub(crate) fn attributes(items: &[Value]) -> BTreeMap<String, Value> {
    items
        .iter()
        .filter_map(|item| {
            let key = item.get("key")?.as_str()?.to_owned();
            let value = item.get("value").map(any_value).unwrap_or(Value::Null);
            Some((key, value))
        })
        .collect()
}

fn kind(value: Option<&Value>) -> SpanKind {
    match value {
        Some(Value::Number(number)) => SpanKind::from_otlp(number.as_i64().unwrap_or(0)),
        Some(Value::String(text)) => SpanKind::parse(text).unwrap_or_default(),
        _ => SpanKind::Internal,
    }
}

fn status(value: Option<&Value>) -> (Status, Option<String>) {
    let Some(value) = value else {
        return (Status::Unset, None);
    };
    let code = match value.get("code") {
        Some(Value::Number(number)) => match number.as_i64() {
            Some(1) => Status::Ok,
            Some(2) => Status::Error,
            _ => Status::Unset,
        },
        Some(Value::String(text)) => {
            match text.to_ascii_uppercase().trim_start_matches("STATUS_CODE_") {
                "OK" => Status::Ok,
                "ERROR" => Status::Error,
                _ => Status::Unset,
            }
        }
        _ => Status::Unset,
    };
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .map(str::to_owned);
    (code, message)
}

pub fn parse(document: &Value, source: &str) -> Result<Vec<Span>, String> {
    let mut out = Vec::new();
    for resource_spans in list(document, "resourceSpans", "resource_spans") {
        let resource = resource_spans
            .get("resource")
            .map(|r| attributes(list(r, "attributes", "attributes")))
            .unwrap_or_default();
        let service = resource
            .get("service.name")
            .and_then(Value::as_str)
            .unwrap_or("unknown_service")
            .to_owned();
        let scopes = field(resource_spans, "scopeSpans", "scope_spans")
            .or_else(|| {
                field(
                    resource_spans,
                    "instrumentationLibrarySpans",
                    "instrumentation_library_spans",
                )
            })
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for scope in scopes {
            for span in list(scope, "spans", "spans") {
                let Some(trace) = id(field(span, "traceId", "trace_id"), 32) else {
                    continue;
                };
                let Some(own) = id(field(span, "spanId", "span_id"), 16) else {
                    continue;
                };
                let (status, status_message) = status(span.get("status"));
                let events = list(span, "events", "events")
                    .iter()
                    .map(|event| SpanEvent {
                        name: event
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        time_ns: field(event, "timeUnixNano", "time_unix_nano")
                            .and_then(number_u64)
                            .unwrap_or(0),
                        attributes: attributes(list(event, "attributes", "attributes")),
                    })
                    .collect();
                let links = list(span, "links", "links")
                    .iter()
                    .filter_map(|link| {
                        Some((
                            id(field(link, "traceId", "trace_id"), 32)?,
                            id(field(link, "spanId", "span_id"), 16)?,
                        ))
                    })
                    .collect();
                let start_ns = field(span, "startTimeUnixNano", "start_time_unix_nano")
                    .and_then(number_u64)
                    .unwrap_or(0);
                out.push(Span {
                    trace_id: trace,
                    span_id: own,
                    parent_id: id(field(span, "parentSpanId", "parent_span_id"), 16),
                    name: span
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    service: service.clone(),
                    kind: kind(span.get("kind")),
                    start_ns,
                    end_ns: field(span, "endTimeUnixNano", "end_time_unix_nano")
                        .and_then(number_u64)
                        .unwrap_or(start_ns),
                    status,
                    status_message,
                    attributes: attributes(list(span, "attributes", "attributes")),
                    resource: resource.clone(),
                    events,
                    links,
                    source: source.to_owned(),
                });
            }
        }
    }
    Ok(out)
}

fn to_any(value: &Value) -> Value {
    match value {
        Value::String(text) => serde_json::json!({"stringValue": text}),
        Value::Bool(flag) => serde_json::json!({"boolValue": flag}),
        Value::Number(number) if number.is_i64() || number.is_u64() => {
            serde_json::json!({"intValue": number.to_string()})
        }
        Value::Number(number) => serde_json::json!({"doubleValue": number}),
        Value::Array(items) => {
            serde_json::json!({"arrayValue": {"values": items.iter().map(to_any).collect::<Vec<_>>()}})
        }
        Value::Object(map) => {
            serde_json::json!({"kvlistValue": {"values": key_values(map.iter())}})
        }
        Value::Null => serde_json::json!({}),
    }
}

fn key_values<'a>(items: impl Iterator<Item = (&'a String, &'a Value)>) -> Vec<Value> {
    items
        .map(|(key, value)| serde_json::json!({"key": key, "value": to_any(value)}))
        .collect()
}

/// Spans as an OTLP/JSON export request, grouped by service, so a pulled
/// trace can be sent on to Jaeger (or any OTLP receiver).
pub fn encode(spans: &[Span]) -> Value {
    let mut groups: BTreeMap<String, (BTreeMap<String, Value>, Vec<Value>)> = BTreeMap::new();
    for span in spans {
        let entry = groups.entry(span.service.clone()).or_insert_with(|| {
            let mut resource = span.resource.clone();
            resource.insert("service.name".into(), Value::String(span.service.clone()));
            resource.insert("dashr.source".into(), Value::String(span.source.clone()));
            (resource, Vec::new())
        });
        let kind = match span.kind {
            SpanKind::Internal => 1,
            SpanKind::Server => 2,
            SpanKind::Client => 3,
            SpanKind::Producer => 4,
            SpanKind::Consumer => 5,
        };
        let code = match span.status {
            Status::Unset => 0,
            Status::Ok => 1,
            Status::Error => 2,
        };
        let mut status = serde_json::json!({"code": code});
        if let Some(message) = &span.status_message {
            status["message"] = Value::String(message.clone());
        }
        entry.1.push(serde_json::json!({
            "traceId": span.trace_id,
            "spanId": span.span_id,
            "parentSpanId": span.parent_id.clone().unwrap_or_default(),
            "name": span.name,
            "kind": kind,
            "startTimeUnixNano": span.start_ns.to_string(),
            "endTimeUnixNano": span.end_ns.to_string(),
            "attributes": key_values(span.attributes.iter()),
            "events": span.events.iter().map(|e| serde_json::json!({
                "name": e.name, "timeUnixNano": e.time_ns.to_string(), "attributes": key_values(e.attributes.iter())
            })).collect::<Vec<_>>(),
            "links": span.links.iter().map(|(t, s)| serde_json::json!({"traceId": t, "spanId": s})).collect::<Vec<_>>(),
            "status": status,
        }));
    }
    serde_json::json!({"resourceSpans": groups.into_values().map(|(resource, spans)| serde_json::json!({
        "resource": {"attributes": key_values(resource.iter())},
        "scopeSpans": [{"scope": {"name": "dashr"}, "spans": spans}],
    })).collect::<Vec<_>>()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn otlp_json_spans() {
        let document = json!({"resourceSpans": [{
            "resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "orders"}}]},
            "scopeSpans": [{"scope": {"name": "x"}, "spans": [{
                "traceId": "5B8EFFF798038103D269B633813FC60C",
                "spanId": "EEE19B7EC3C1B174",
                "parentSpanId": "",
                "name": "POST /orders",
                "kind": 2,
                "startTimeUnixNano": "1544712660000000000",
                "endTimeUnixNano": "1544712661000000000",
                "attributes": [
                    {"key": "order.items", "value": {"intValue": "3"}},
                    {"key": "tags", "value": {"arrayValue": {"values": [{"stringValue": "a"}]}}}
                ],
                "events": [{"timeUnixNano": "1544712660500000000", "name": "validated"}],
                "status": {"code": 2, "message": "boom"}
            }, {
                "traceId": "W47/KD8ruQhQ0hs8SZ1Oag==",
                "spanId": "AAAAAAAAAAE=",
                "name": "child", "kind": "SPAN_KIND_CLIENT",
                "startTimeUnixNano": 5, "endTimeUnixNano": 9,
                "status": {"code": "STATUS_CODE_OK"}
            }]}]
        }]});
        let spans = parse(&document, "otlp").unwrap();
        assert_eq!(spans.len(), 2);
        let root = &spans[0];
        assert_eq!(root.trace_id, "5b8efff798038103d269b633813fc60c");
        assert_eq!(root.parent_id, None);
        assert_eq!(root.service, "orders");
        assert_eq!(root.kind, SpanKind::Server);
        assert_eq!(root.duration_ms(), 1000.0);
        assert_eq!(root.attributes["order.items"], json!(3));
        assert_eq!(root.attributes["tags"], json!(["a"]));
        assert_eq!(root.status, Status::Error);
        assert_eq!(root.status_message.as_deref(), Some("boom"));
        assert_eq!(root.events[0].name, "validated");
        assert_eq!(spans[1].trace_id, "5b8eff283f2bb90850d21b3c499d4e6a");
        assert_eq!(spans[1].span_id, "0000000000000001");
        assert_eq!(spans[1].kind, SpanKind::Client);
        assert_eq!(spans[1].status, Status::Ok);
    }

    #[test]
    fn encoding_round_trips() {
        let original = parse(&json!({"resourceSpans": [{
            "resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "orders"}}]},
            "scopeSpans": [{"spans": [{
                "traceId": "5b8efff798038103d269b633813fc60c", "spanId": "eee19b7ec3c1b174", "parentSpanId": "aaaaaaaaaaaaaaaa",
                "name": "reserve", "kind": 3, "startTimeUnixNano": "10", "endTimeUnixNano": "20",
                "attributes": [{"key": "n", "value": {"intValue": "3"}}, {"key": "ok", "value": {"boolValue": true}},
                               {"key": "ratio", "value": {"doubleValue": 0.5}}, {"key": "m", "value": {"kvlistValue": {"values": [{"key": "a", "value": {"stringValue": "b"}}]}}}],
                "status": {"code": 2, "message": "x"}
            }]}]
        }]}), "xray").unwrap();
        let again = parse(&encode(&original), "xray").unwrap();
        let mut expected = original.clone();
        expected[0]
            .resource
            .insert("dashr.source".into(), json!("xray"));
        assert_eq!(again, expected);
    }
}

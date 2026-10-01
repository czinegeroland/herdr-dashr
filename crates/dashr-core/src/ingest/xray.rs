//! AWS X-Ray segment documents.
//!
//! Accepts `aws xray batch-get-traces` output (`{"Traces": [...]}`, each
//! segment's `Document` a JSON string), bare segment documents, or lists of
//! either. A segment is a span of the service it names; its subsegments
//! are spans of the same service, nested under it. A subsegment in the
//! `aws` or `remote` namespace is a call to another system: a client span
//! whose `peer.service` is the subsegment's name (`DynamoDB`, `Lambda`), so
//! the sequence shows the call even when the callee sends no trace of its
//! own. Step Functions, Lambda and ECS segments of one execution share the
//! trace id, which becomes the W3C id OpenTelemetry uses.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{number_f64, text};
use crate::model::{Span, SpanKind, Status, span_id, trace_id};

fn seconds_ns(value: Option<&Value>) -> Option<u64> {
    number_f64(value?).map(|secs| (secs * 1e9).round() as u64)
}

fn flatten(prefix: &str, value: &Value, out: &mut BTreeMap<String, Value>) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                flatten(&format!("{prefix}.{key}"), inner, out);
            }
        }
        Value::Null => {}
        other => {
            out.insert(prefix.to_owned(), other.clone());
        }
    }
}

fn path_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = rest.find('/').map_or("/", |index| &rest[index..]);
    path.split(['?', '#']).next().unwrap_or("/").to_owned()
}

struct Context<'a> {
    trace: &'a str,
    service: &'a str,
    resource: &'a BTreeMap<String, Value>,
    source: &'a str,
}

fn convert(doc: &Value, parent: Option<String>, segment: bool, cx: &Context, out: &mut Vec<Span>) {
    let Some(raw_id) = text(doc.get("id")) else {
        return;
    };
    let own = span_id(&raw_id);
    let name = text(doc.get("name")).unwrap_or_else(|| "segment".into());
    let namespace = text(doc.get("namespace")).unwrap_or_default();
    let mut attributes = BTreeMap::new();
    if let Some(annotations) = doc.get("annotations").and_then(Value::as_object) {
        for (key, value) in annotations {
            attributes.insert(key.clone(), value.clone());
        }
    }
    if let Some(metadata) = doc.get("metadata") {
        flatten("metadata", metadata, &mut attributes);
    }
    if let Some(aws) = doc.get("aws").and_then(Value::as_object) {
        for (key, value) in aws {
            if key == "xray" {
                continue;
            }
            flatten(&format!("aws.{key}"), value, &mut attributes);
        }
    }
    if let Some(sql) = doc.get("sql") {
        flatten("sql", sql, &mut attributes);
    }
    let request = doc.pointer("/http/request");
    let method = request.and_then(|r| text(r.get("method")));
    let url = request.and_then(|r| text(r.get("url")));
    if let Some(method) = &method {
        attributes.insert("http.request.method".into(), Value::String(method.clone()));
    }
    if let Some(url) = &url {
        attributes.insert("url.full".into(), Value::String(url.clone()));
    }
    if let Some(status) = doc.pointer("/http/response/status") {
        attributes.insert("http.response.status_code".into(), status.clone());
    }
    if let Some(origin) = text(doc.get("origin")) {
        attributes.insert("aws.xray.origin".into(), Value::String(origin));
    }
    let outbound = !segment && (namespace == "aws" || namespace == "remote");
    let kind = if segment {
        SpanKind::Server
    } else if outbound {
        attributes.insert("peer.service".into(), Value::String(name.clone()));
        SpanKind::Client
    } else {
        SpanKind::Internal
    };
    let operation = attributes.get("aws.operation").and_then(Value::as_str).map(str::to_owned);
    let span_name = match (&method, &url, &operation) {
        (Some(method), Some(url), _) if segment || namespace == "remote" => format!("{method} {}", path_of(url)),
        (_, _, Some(operation)) if outbound => operation.clone(),
        _ => name.clone(),
    };
    let start_ns = seconds_ns(doc.get("start_time")).unwrap_or(0);
    let in_progress = doc.get("in_progress").and_then(Value::as_bool).unwrap_or(false);
    if in_progress {
        attributes.insert("xray.in_progress".into(), Value::Bool(true));
    }
    let end_ns = seconds_ns(doc.get("end_time")).unwrap_or(start_ns);
    let failed = ["fault", "error", "throttle"]
        .iter()
        .any(|flag| doc.get(*flag).and_then(Value::as_bool).unwrap_or(false));
    let message = doc
        .pointer("/cause/exceptions/0/message")
        .and_then(Value::as_str)
        .or_else(|| doc.pointer("/cause/message").and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| {
            failed.then(|| match doc.pointer("/http/response/status") {
                Some(status) => format!("HTTP {status}"),
                None => "fault".to_owned(),
            })
        });
    out.push(Span {
        trace_id: cx.trace.to_owned(),
        span_id: own.clone(),
        parent_id: parent,
        name: span_name,
        service: cx.service.to_owned(),
        kind,
        start_ns,
        end_ns: end_ns.max(start_ns),
        status: if failed { Status::Error } else { Status::Unset },
        status_message: if failed { message } else { None },
        attributes,
        resource: cx.resource.clone(),
        events: Vec::new(),
        links: Vec::new(),
        source: cx.source.to_owned(),
    });
    if let Some(children) = doc.get("subsegments").and_then(Value::as_array) {
        for child in children {
            convert(child, Some(own.clone()), false, cx, out);
        }
    }
}

fn segment(doc: &Value, source: &str, out: &mut Vec<Span>) {
    let Some(raw_trace) = text(doc.get("trace_id")) else {
        return;
    };
    let trace = trace_id(&raw_trace);
    let service = text(doc.get("name")).unwrap_or_else(|| "unknown_service".into());
    let mut resource = BTreeMap::new();
    resource.insert("service.name".to_owned(), Value::String(service.clone()));
    resource.insert("cloud.provider".to_owned(), Value::String("aws".into()));
    if let Some(origin) = text(doc.get("origin")) {
        resource.insert("aws.xray.origin".to_owned(), Value::String(origin));
    }
    let parent = text(doc.get("parent_id")).map(|p| span_id(&p));
    let cx = Context { trace: &trace, service: &service, resource: &resource, source };
    // An independent subsegment (`"type": "subsegment"`) is still a span of
    // its own service, nested under the segment it names as parent.
    convert(doc, parent, true, &cx, out);
}

fn walk(value: &Value, source: &str, out: &mut Vec<Span>) -> Result<(), String> {
    match value {
        Value::Array(items) => {
            for item in items {
                walk(item, source, out)?;
            }
        }
        Value::Object(map) => {
            if let Some(traces) = map.get("Traces") {
                walk(traces, source, out)?;
            } else if let Some(segments) = map.get("Segments") {
                walk(segments, source, out)?;
            } else if let Some(document) = map.get("Document") {
                let parsed: Value = match document {
                    Value::String(text) => serde_json::from_str(text).map_err(|error| format!("bad X-Ray segment document: {error}"))?,
                    other => other.clone(),
                };
                segment(&parsed, source, out);
            } else if map.contains_key("trace_id") {
                segment(value, source, out);
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn parse(document: &Value, source: &str) -> Result<Vec<Span>, String> {
    let mut out = Vec::new();
    walk(document, source, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(value: Value) -> Value {
        json!({"Id": value["id"], "Document": value.to_string()})
    }

    /// A Step Functions execution invoking a Lambda, as batch-get-traces
    /// returns it.
    fn step_function() -> Value {
        let sfn = json!({
            "id": "1111111111111111", "name": "checkout-machine",
            "trace_id": "1-66f7a1b2-0123456789abcdef01234567",
            "start_time": 1_727_000_000.0, "end_time": 1_727_000_003.5,
            "origin": "AWS::StepFunctions::StateMachine",
            "aws": {"execution_arn": "arn:aws:states:eu-west-1:1:execution:checkout:abc"},
            "subsegments": [{
                "id": "2222222222222222", "name": "ValidateOrder",
                "start_time": 1_727_000_000.1, "end_time": 1_727_000_001.0,
                "subsegments": [{
                    "id": "3333333333333333", "name": "Lambda", "namespace": "aws",
                    "start_time": 1_727_000_000.2, "end_time": 1_727_000_000.9,
                    "aws": {"operation": "Invoke", "function_name": "validate"}
                }]
            }]
        });
        let lambda = json!({
            "id": "4444444444444444", "name": "validate",
            "trace_id": "1-66f7a1b2-0123456789abcdef01234567",
            "parent_id": "3333333333333333",
            "start_time": 1_727_000_000.25, "end_time": 1_727_000_000.85,
            "origin": "AWS::Lambda::Function",
            "annotations": {"order_id": "o-42", "test_run": "r-1"},
            "fault": true,
            "cause": {"exceptions": [{"message": "stock service timed out", "type": "Timeout"}]},
            "subsegments": [{
                "id": "5555555555555555", "name": "DynamoDB", "namespace": "aws",
                "start_time": 1_727_000_000.3, "end_time": 1_727_000_000.4,
                "aws": {"operation": "GetItem", "table_name": "orders"}
            }]
        });
        json!({"Traces": [{"Id": "1-66f7a1b2-0123456789abcdef01234567", "Segments": [doc(sfn), doc(lambda)]}], "UnprocessedTraceIds": []})
    }

    #[test]
    fn a_step_function_execution_becomes_one_trace() {
        let spans = parse(&step_function(), "xray").unwrap();
        assert_eq!(spans.len(), 5);
        assert!(spans.iter().all(|s| s.trace_id == "66f7a1b20123456789abcdef01234567"));
        let by = |id: &str| spans.iter().find(|s| s.span_id == id).unwrap();
        let machine = by("1111111111111111");
        assert_eq!((machine.service.as_str(), machine.kind, machine.parent_id.as_deref()), ("checkout-machine", SpanKind::Server, None));
        let state = by("2222222222222222");
        assert_eq!((state.name.as_str(), state.kind, state.parent_id.as_deref()), ("ValidateOrder", SpanKind::Internal, Some("1111111111111111")));
        let invoke = by("3333333333333333");
        assert_eq!((invoke.name.as_str(), invoke.kind), ("Invoke", SpanKind::Client));
        assert_eq!(invoke.peer().as_deref(), Some("Lambda"));
        let function = by("4444444444444444");
        assert_eq!(function.service, "validate");
        assert_eq!(function.parent_id.as_deref(), Some("3333333333333333"));
        assert_eq!(function.attributes["order_id"], json!("o-42"));
        assert_eq!(function.status, Status::Error);
        assert_eq!(function.status_message.as_deref(), Some("stock service timed out"));
        assert!((function.duration_ms() - 600.0).abs() < 0.01);
        let dynamo = by("5555555555555555");
        assert_eq!((dynamo.service.as_str(), dynamo.name.as_str()), ("validate", "GetItem"));
        assert_eq!(dynamo.attributes["aws.table_name"], json!("orders"));
    }

    #[test]
    fn bare_segments_and_http_names() {
        let segment = json!({
            "id": "6666666666666666", "name": "orders-api", "trace_id": "1-66f7a1b2-0123456789abcdef01234567",
            "start_time": 1.0, "in_progress": true,
            "http": {"request": {"method": "POST", "url": "https://api.example.com/orders?x=1"}}
        });
        let spans = parse(&json!([segment]), "xray").unwrap();
        assert_eq!(spans[0].name, "POST /orders");
        assert_eq!(spans[0].attributes["xray.in_progress"], json!(true));
        assert_eq!(spans[0].end_ns, spans[0].start_ns);
    }
}

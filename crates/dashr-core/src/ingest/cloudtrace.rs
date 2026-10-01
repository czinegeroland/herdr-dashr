//! Google Cloud Trace API v1 (`projects.traces.list` with `view=COMPLETE`,
//! or `projects.traces.get`). Labels become attributes; the service is the
//! `service.name` label OpenTelemetry's exporter writes, else App Engine's
//! module, else the project.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{rfc3339_ns, text};
use crate::model::{Span, SpanKind, Status, span_id, trace_id};

/// Cloud Trace v1 span ids are decimal 64-bit integers.
fn gcp_id(raw: &str) -> String {
    raw.parse::<u64>().map(|n| format!("{n:016x}")).unwrap_or_else(|_| span_id(raw))
}

fn trace(document: &Value, source: &str, out: &mut Vec<Span>) {
    let Some(raw_trace) = text(document.get("traceId")) else { return };
    let project = text(document.get("projectId")).unwrap_or_else(|| "gcp".into());
    for span in document.get("spans").and_then(Value::as_array).into_iter().flatten() {
        let Some(raw_span) = text(span.get("spanId")) else { continue };
        let labels: BTreeMap<String, Value> = span
            .get("labels")
            .and_then(Value::as_object)
            .map(|l| l.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        let label = |key: &str| labels.get(key).and_then(Value::as_str).map(str::to_owned);
        let service = label("service.name")
            .or_else(|| label("g.co/gae/app/module"))
            .or_else(|| label("g.co/r/cloud_run_revision/service_name"))
            .unwrap_or_else(|| project.clone());
        let code = label("/http/status_code").and_then(|c| c.parse::<u16>().ok());
        let failed = label("error").is_some() || label("/error/message").is_some() || code.is_some_and(|c| c >= 500);
        let start_ns = text(span.get("startTime")).and_then(|t| rfc3339_ns(&t)).unwrap_or(0);
        let mut resource = BTreeMap::new();
        resource.insert("service.name".to_owned(), Value::String(service.clone()));
        resource.insert("cloud.provider".to_owned(), Value::String("gcp".into()));
        out.push(Span {
            trace_id: trace_id(&raw_trace),
            span_id: gcp_id(&raw_span),
            parent_id: text(span.get("parentSpanId")).filter(|p| p != "0" && !p.is_empty()).map(|p| gcp_id(&p)),
            name: text(span.get("name")).unwrap_or_default(),
            service,
            kind: span.get("kind").and_then(Value::as_str).and_then(SpanKind::parse).unwrap_or_default(),
            start_ns,
            end_ns: text(span.get("endTime")).and_then(|t| rfc3339_ns(&t)).unwrap_or(start_ns),
            status: if failed { Status::Error } else { Status::Unset },
            status_message: if failed { label("/error/message").or_else(|| code.map(|c| format!("HTTP {c}"))) } else { None },
            attributes: labels,
            resource,
            events: Vec::new(),
            links: Vec::new(),
            source: source.to_owned(),
        });
    }
}

pub fn parse(document: &Value, source: &str) -> Result<Vec<Span>, String> {
    let mut out = Vec::new();
    match document.get("traces") {
        Some(Value::Array(traces)) => traces.iter().for_each(|t| trace(t, source, &mut out)),
        _ => trace(document, source, &mut out),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cloud_trace_v1() {
        let spans = parse(
            &json!({"traces": [{"projectId": "shop", "traceId": "4bf92f3577b34da6a3ce929d0e0e4736", "spans": [
                {"spanId": "123", "kind": "RPC_SERVER", "name": "/orders", "startTime": "2026-09-28T08:00:00Z",
                 "endTime": "2026-09-28T08:00:00.2Z", "labels": {"service.name": "orders", "/http/status_code": "503"}},
                {"spanId": "456", "parentSpanId": "123", "kind": "RPC_CLIENT", "name": "stock",
                 "startTime": "2026-09-28T08:00:00.05Z", "endTime": "2026-09-28T08:00:00.1Z", "labels": {}}
            ]}]}),
            "gcp",
        )
        .unwrap();
        assert_eq!(spans[0].service, "orders");
        assert_eq!(spans[0].kind, SpanKind::Server);
        assert_eq!(spans[0].status, Status::Error);
        assert_eq!(spans[0].span_id, "000000000000007b", "decimal span ids become hex");
        assert_eq!(spans[1].parent_id.as_deref(), Some("000000000000007b"));
        assert_eq!(spans[1].service, "shop");
        assert!((spans[0].duration_ms() - 200.0).abs() < 0.001);
    }
}

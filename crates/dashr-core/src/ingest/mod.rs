//! Spans from any tracing system, in one shape (DASHR-TRACE-002).
//!
//! dashr knows the wire and export formats of the common tracing systems
//! and nothing about how to reach them: the agent writes the command that
//! fetches traces (an `aws`, `az`, `gcloud` or `curl` call) and names the
//! format of what it prints. A command may print several JSON documents in
//! a row (one per `batch-get-traces` call, say) or JSON lines.

mod appinsights;
mod cloudtrace;
mod jaeger;
mod otlp_json;

pub use otlp_json::encode as encode_otlp;
pub mod otlp_proto;
mod xray;
mod zipkin;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    /// Recognise the format from the document itself.
    #[default]
    Auto,
    /// OTLP/JSON: `{"resourceSpans": [...]}`.
    Otlp,
    /// OTLP/protobuf: an `ExportTraceServiceRequest`.
    OtlpProto,
    /// AWS X-Ray: `aws xray batch-get-traces` output, or segment documents.
    Xray,
    /// Zipkin v2 JSON: a list of spans.
    Zipkin,
    /// Jaeger query API JSON: `{"data": [trace, ...]}`.
    Jaeger,
    /// Azure Application Insights or Log Analytics query results
    /// (`az monitor app-insights query`, `az monitor log-analytics query`).
    Appinsights,
    /// Google Cloud Trace API v1: `{"traces": [...]}` or one trace.
    Cloudtrace,
}

impl Format {
    pub const NAMES: &'static [&'static str] = &[
        "auto",
        "otlp",
        "otlp-proto",
        "xray",
        "zipkin",
        "jaeger",
        "appinsights",
        "cloudtrace",
    ];

    pub fn parse(text: &str) -> Result<Self, String> {
        serde_json::from_value(Value::String(text.to_ascii_lowercase()))
            .map_err(|_| format!("unknown format {text:?}; one of {}", Self::NAMES.join(", ")))
    }
}

/// Converts what a source printed (or an exporter sent) into spans.
/// `source` labels every span (`otlp`, or the pull source's name).
pub fn parse(bytes: &[u8], format: Format, source: &str) -> Result<Vec<Span>, String> {
    if format == Format::OtlpProto {
        return otlp_proto::parse(bytes, source);
    }
    let documents = documents(bytes)?;
    let mut spans = Vec::new();
    for document in documents {
        let format = match format {
            Format::Auto => detect(&document).ok_or_else(|| {
                "could not recognise the trace format; name it with --format".to_owned()
            })?,
            other => other,
        };
        spans.extend(match format {
            Format::Otlp => otlp_json::parse(&document, source)?,
            Format::Xray => xray::parse(&document, source)?,
            Format::Zipkin => zipkin::parse(&document, source)?,
            Format::Jaeger => jaeger::parse(&document, source)?,
            Format::Appinsights => appinsights::parse(&document, source)?,
            Format::Cloudtrace => cloudtrace::parse(&document, source)?,
            Format::Auto | Format::OtlpProto => unreachable!("handled above"),
        });
    }
    Ok(spans)
}

/// Every JSON value in `bytes`: one document, several in a row, or lines.
fn documents(bytes: &[u8]) -> Result<Vec<Value>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "output is not UTF-8 text".to_owned())?;
    let mut out = Vec::new();
    let stream = serde_json::Deserializer::from_str(text).into_iter::<Value>();
    for value in stream {
        out.push(value.map_err(|error| format!("not JSON: {error}"))?);
    }
    Ok(out)
}

/// Recognises a document's format by its shape.
pub fn detect(document: &Value) -> Option<Format> {
    let has = |key: &str| document.get(key).is_some();
    if has("resourceSpans") || has("resource_spans") || has("batches") {
        return Some(Format::Otlp);
    }
    if has("Traces") || (has("trace_id") && has("id") && has("start_time")) {
        return Some(Format::Xray);
    }
    if document.get("Segments").is_some() {
        return Some(Format::Xray);
    }
    if document.get("data").and_then(Value::as_array).is_some()
        || (has("spans") && has("processes"))
    {
        return Some(Format::Jaeger);
    }
    if has("tables") {
        return Some(Format::Appinsights);
    }
    if has("traces") || (has("projectId") && has("spans")) {
        return Some(Format::Cloudtrace);
    }
    let first = match document {
        Value::Array(items) => items.first()?,
        other => other,
    };
    if let Some(items) = first.as_array() {
        // Zipkin's /api/v2/traces: a list of traces, each a list of spans.
        return items
            .first()
            .and_then(|span| span.get("traceId"))
            .map(|_| Format::Zipkin);
    }
    if first.get("traceId").is_some() && first.get("id").is_some() {
        return Some(Format::Zipkin);
    }
    if first.get("Document").is_some() {
        return Some(Format::Xray);
    }
    if first.get("trace_id").is_some() && first.get("start_time").is_some() {
        return Some(Format::Xray);
    }
    if first.get("OperationId").is_some() || first.get("operation_Id").is_some() {
        return Some(Format::Appinsights);
    }
    None
}

/// A JSON number or numeric string as `u64`.
pub(crate) fn number_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_f64().map(|f| f as u64)),
        Value::String(text) => text
            .parse::<u64>()
            .ok()
            .or_else(|| text.parse::<f64>().ok().map(|f| f as u64)),
        _ => None,
    }
}

pub(crate) fn number_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

pub(crate) fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// An RFC 3339 timestamp as Unix nanoseconds (UTC offsets honoured).
pub fn rfc3339_ns(text: &str) -> Option<u64> {
    let text = text.trim();
    let (date, rest) = text.split_once(['T', ' '])?;
    let mut parts = date.split('-');
    let (year, month, day): (i64, i64, i64) = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    let (clock, offset_secs) =
        if let Some(clock) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
            (clock, 0i64)
        } else if let Some(index) = rest.rfind(['+', '-']).filter(|i| *i >= 5) {
            let (clock, offset) = rest.split_at(index);
            let sign = if offset.starts_with('-') { -1 } else { 1 };
            let mut hm = offset[1..].split(':');
            let hours: i64 = hm.next()?.parse().ok()?;
            let minutes: i64 = hm.next().unwrap_or("0").parse().ok()?;
            (clock, sign * (hours * 3600 + minutes * 60))
        } else {
            (rest, 0)
        };
    let mut hms = clock.split(':');
    let hour: i64 = hms.next()?.parse().ok()?;
    let minute: i64 = hms.next()?.parse().ok()?;
    let second_text = hms.next().unwrap_or("0");
    let (whole, fraction) = second_text.split_once('.').unwrap_or((second_text, ""));
    let second: i64 = whole.parse().ok()?;
    let nanos: i64 = format!("{:0<9}", &fraction[..fraction.len().min(9)])
        .parse()
        .ok()?;
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    u64::try_from(secs)
        .ok()
        .map(|s| s * 1_000_000_000 + nanos as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_are_recognised_by_shape() {
        assert_eq!(detect(&json!({"resourceSpans": []})), Some(Format::Otlp));
        assert_eq!(
            detect(&json!({"Traces": [], "UnprocessedTraceIds": []})),
            Some(Format::Xray)
        );
        assert_eq!(
            detect(&json!([{"traceId": "a", "id": "b"}])),
            Some(Format::Zipkin)
        );
        assert_eq!(
            detect(&json!([[{"traceId": "a", "id": "b"}]])),
            Some(Format::Zipkin)
        );
        assert_eq!(
            detect(&json!({"data": [{"traceID": "a"}]})),
            Some(Format::Jaeger)
        );
        assert_eq!(detect(&json!({"tables": []})), Some(Format::Appinsights));
        assert_eq!(detect(&json!({"traces": []})), Some(Format::Cloudtrace));
        assert_eq!(detect(&json!({"batches": []})), Some(Format::Otlp), "Tempo");
        assert_eq!(
            detect(&json!([{"OperationId": "a"}])),
            Some(Format::Appinsights)
        );
        assert_eq!(detect(&json!({"hello": 1})), None);
    }

    #[test]
    fn several_documents_in_a_row() {
        let docs = documents(b"{\"a\":1}\n{\"b\":2} {\"c\":3}").unwrap();
        assert_eq!(docs.len(), 3);
        assert!(documents(b"{nope").is_err());
    }

    #[test]
    fn rfc3339_with_offsets_and_fractions() {
        assert_eq!(rfc3339_ns("1970-01-01T00:00:01Z"), Some(1_000_000_000));
        assert_eq!(
            rfc3339_ns("2026-09-28T10:00:00.5+02:00"),
            rfc3339_ns("2026-09-28T08:00:00.500Z")
        );
        assert_eq!(
            rfc3339_ns("2026-09-28T08:00:00.123456789Z").map(|n| n % 1_000_000_000),
            Some(123_456_789)
        );
        assert_eq!(rfc3339_ns("garbage"), None);
    }

    #[test]
    fn unknown_formats_are_refused() {
        assert!(Format::parse("datadog").unwrap_err().contains("xray"));
        assert_eq!(Format::parse("OTLP-PROTO"), Ok(Format::OtlpProto));
    }
}

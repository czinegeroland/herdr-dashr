//! Azure Monitor query results: `az monitor app-insights query` (classic
//! `requests` / `dependencies` tables) and `az monitor log-analytics query`
//! (workspace `AppRequests` / `AppDependencies`), as `{"tables": [...]}`
//! with columns and rows. Requests are server spans, dependencies client
//! spans; the operation id is the trace id.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{number_f64, rfc3339_ns, text};
use crate::model::{Span, SpanKind, Status, span_id, trace_id};

fn pick<'a>(row: &'a BTreeMap<String, Value>, names: &[&str]) -> Option<&'a Value> {
    names
        .iter()
        .find_map(|name| row.get(*name))
        .filter(|v| !v.is_null())
}

fn one(row: &BTreeMap<String, Value>, table: &str, source: &str) -> Option<Span> {
    let operation = text(pick(row, &["operation_Id", "OperationId"]))?;
    let id = text(pick(row, &["id", "Id"]))?;
    let item_type = text(pick(row, &["itemType", "Type"]))
        .unwrap_or_else(|| table.to_owned())
        .to_ascii_lowercase();
    let kind = if item_type.contains("request") {
        SpanKind::Server
    } else if item_type.contains("dependenc") {
        SpanKind::Client
    } else {
        SpanKind::Internal
    };
    let parent = text(pick(row, &["operation_ParentId", "ParentId"]))
        .filter(|p| p != &operation && !p.is_empty());
    let start_ns = text(pick(row, &["timestamp", "TimeGenerated"]))
        .and_then(|t| rfc3339_ns(&t))
        .unwrap_or(0);
    let duration_ms = pick(row, &["duration", "DurationMs"])
        .and_then(number_f64)
        .unwrap_or(0.0);
    let success = match pick(row, &["success", "Success"]) {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => !text.eq_ignore_ascii_case("false"),
        _ => true,
    };
    let service = text(pick(row, &["cloud_RoleName", "AppRoleName"]))
        .unwrap_or_else(|| "unknown_service".into());
    let mut attributes = BTreeMap::new();
    let custom = pick(row, &["customDimensions", "Properties"]).cloned();
    match custom {
        Some(Value::Object(map)) => attributes.extend(map),
        Some(Value::String(text)) => {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) {
                attributes.extend(map);
            }
        }
        _ => {}
    }
    if let Some(target) = text(pick(row, &["target", "Target"])) {
        attributes.insert("peer.service".into(), Value::String(target));
    }
    for (column, key) in [
        (["resultCode", "ResultCode"], "result.code"),
        (["type", "DependencyType"], "dependency.type"),
        (["url", "Url"], "url.full"),
        (["data", "Data"], "dependency.data"),
    ] {
        if let Some(value) = pick(row, &column) {
            attributes.insert(key.to_owned(), value.clone());
        }
    }
    let mut resource = BTreeMap::new();
    resource.insert("service.name".to_owned(), Value::String(service.clone()));
    resource.insert("cloud.provider".to_owned(), Value::String("azure".into()));
    Some(Span {
        trace_id: trace_id(&operation),
        span_id: span_id(&id),
        parent_id: parent.map(|p| span_id(&p)),
        name: text(pick(row, &["name", "Name"])).unwrap_or_default(),
        service,
        kind,
        start_ns,
        end_ns: start_ns + (duration_ms * 1e6) as u64,
        status: if success {
            Status::Unset
        } else {
            Status::Error
        },
        status_message: (!success).then(|| {
            text(pick(row, &["resultCode", "ResultCode"])).unwrap_or_else(|| "failed".into())
        }),
        attributes,
        resource,
        events: Vec::new(),
        links: Vec::new(),
        source: source.to_owned(),
    })
}

pub fn parse(document: &Value, source: &str) -> Result<Vec<Span>, String> {
    let mut out = Vec::new();
    for table in document
        .get("tables")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = text(table.get("name"))
            .or_else(|| text(table.get("TableName")))
            .unwrap_or_default();
        let columns: Vec<String> = table
            .get("columns")
            .and_then(Value::as_array)
            .map(|cols| {
                cols.iter()
                    .filter_map(|c| text(c.get("name")).or_else(|| text(c.get("ColumnName"))))
                    .collect()
            })
            .unwrap_or_default();
        for row in table
            .get("rows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(cells) = row.as_array() else {
                continue;
            };
            let row: BTreeMap<String, Value> =
                columns.iter().cloned().zip(cells.iter().cloned()).collect();
            out.extend(one(&row, &name, source));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_and_dependencies() {
        let document = json!({"tables": [{"name": "PrimaryResult",
        "columns": [{"name": "itemType"}, {"name": "operation_Id"}, {"name": "id"}, {"name": "operation_ParentId"},
                    {"name": "name"}, {"name": "timestamp"}, {"name": "duration"}, {"name": "success"},
                    {"name": "cloud_RoleName"}, {"name": "target"}, {"name": "customDimensions"}],
        "rows": [
            ["request", "4bf92f3577b34da6a3ce929d0e0e4736", "00f067aa0ba902b7", "4bf92f3577b34da6a3ce929d0e0e4736",
             "POST /orders", "2026-09-28T08:00:00Z", 120.5, "True", "orders-api", null, "{\"order.id\":\"o-1\"}"],
            ["dependency", "4bf92f3577b34da6a3ce929d0e0e4736", "b7ad6b7169203331", "00f067aa0ba902b7",
             "PUT stock", "2026-09-28T08:00:00.01Z", 30, "False", "orders-api", "stock-api", ""]
        ]}]});
        let spans = parse(&document, "azure").unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(
            (spans[0].kind, spans[0].parent_id.as_deref()),
            (SpanKind::Server, None)
        );
        assert_eq!(spans[0].attributes["order.id"], json!("o-1"));
        assert!((spans[0].duration_ms() - 120.5).abs() < 0.001);
        assert_eq!(spans[1].parent_id.as_deref(), Some("00f067aa0ba902b7"));
        assert_eq!(spans[1].peer().as_deref(), Some("stock-api"));
        assert_eq!(spans[1].status, Status::Error);
    }
}

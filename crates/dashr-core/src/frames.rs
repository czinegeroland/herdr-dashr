//! Grafana data frames, as returned by `POST /api/ds/query`.
//!
//! Frames are column-oriented: `schema.fields` describes each column and
//! `data.values` holds one array per column. This module parses them and
//! produces two very different things from them:
//!
//! * a [`TargetStatus`], which carries counts, field names and errors but
//!   never a value, for `panel_status` (requirement DASHR-MCP-004);
//! * rows, which only ever leave this crate through the masker.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::masking::FieldInput;

/// One column-oriented frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub name: Option<String>,
    pub fields: Vec<FieldInput>,
    pub columns: Vec<Vec<Value>>,
}

impl Frame {
    pub fn row_count(&self) -> usize {
        self.columns.iter().map(Vec::len).max().unwrap_or(0)
    }

    /// The frame as rows, padding short columns with nulls.
    pub fn rows(&self) -> Vec<Vec<Value>> {
        (0..self.row_count())
            .map(|row| {
                self.columns
                    .iter()
                    .map(|column| column.get(row).cloned().unwrap_or(Value::Null))
                    .collect()
            })
            .collect()
    }

    /// Every numeric column's values, in order, skipping nulls.
    pub fn numeric_series(&self) -> Vec<Vec<f64>> {
        self.fields
            .iter()
            .zip(&self.columns)
            .filter(|(field, _)| field.field_type == "number")
            .map(|(_, column)| column.iter().filter_map(Value::as_f64).collect())
            .collect()
    }
}

/// The result of one query (one `refId`).
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub ref_id: String,
    pub status: Option<u64>,
    pub error: Option<String>,
    pub frames: Vec<Frame>,
}

impl QueryResult {
    /// A result that failed before Grafana answered for it.
    pub fn failed(ref_id: &str, error: impl Into<String>) -> Self {
        Self {
            ref_id: ref_id.to_owned(),
            status: None,
            error: Some(error.into()),
            frames: Vec::new(),
        }
    }

    pub fn row_count(&self) -> usize {
        self.frames.iter().map(Frame::row_count).sum()
    }
}

fn parse_field(field: &Value) -> FieldInput {
    let name = field
        .pointer("/config/displayNameFromDS")
        .or_else(|| field.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let labels: BTreeMap<String, String> = field
        .get("labels")
        .and_then(Value::as_object)
        .map(|labels| {
            labels
                .iter()
                .map(|(key, value)| {
                    let value = value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string());
                    (key.clone(), value)
                })
                .collect()
        })
        .unwrap_or_default();
    FieldInput {
        name,
        field_type: field
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("other")
            .to_owned(),
        labels,
    }
}

fn parse_frame(frame: &Value) -> Frame {
    let fields: Vec<FieldInput> = frame
        .pointer("/schema/fields")
        .and_then(Value::as_array)
        .map(|fields| fields.iter().map(parse_field).collect())
        .unwrap_or_default();
    let columns: Vec<Vec<Value>> = frame
        .pointer("/data/values")
        .and_then(Value::as_array)
        .map(|columns| {
            columns
                .iter()
                .map(|column| column.as_array().cloned().unwrap_or_default())
                .collect()
        })
        .unwrap_or_default();
    Frame {
        name: frame
            .pointer("/schema/name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        fields,
        columns,
    }
}

/// Parses a `/api/ds/query` response body into one result per `refId`.
pub fn parse_response(body: &Value) -> Vec<QueryResult> {
    let Some(results) = body.get("results").and_then(Value::as_object) else {
        return Vec::new();
    };
    results
        .iter()
        .map(|(ref_id, result)| QueryResult {
            ref_id: ref_id.clone(),
            status: result.get("status").and_then(Value::as_u64),
            error: result
                .get("error")
                .and_then(Value::as_str)
                .filter(|error| !error.is_empty())
                .map(str::to_owned),
            frames: result
                .get("frames")
                .and_then(Value::as_array)
                .map(|frames| frames.iter().map(parse_frame).collect())
                .unwrap_or_default(),
        })
        .collect()
}

/// A field as `panel_status` reports it: a name and a type, no values.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldShape {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: String,
}

/// What one query returned, without any value.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TargetStatus {
    pub ref_id: String,
    pub frames: usize,
    pub rows: usize,
    pub fields: Vec<FieldShape>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Summarises a result. `mask` is applied to the error text, which can echo
/// data back.
pub fn summarize(result: &QueryResult, mask: &dyn Fn(&str) -> String) -> TargetStatus {
    let mut fields: Vec<FieldShape> = Vec::new();
    for frame in &result.frames {
        for field in &frame.fields {
            let shape = FieldShape {
                name: field.name.clone(),
                field_type: field.field_type.clone(),
            };
            if !fields.contains(&shape) {
                fields.push(shape);
            }
        }
    }
    TargetStatus {
        ref_id: result.ref_id.clone(),
        frames: result.frames.len(),
        rows: result.row_count(),
        fields,
        error: result.error.as_deref().map(mask),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "results": {
                "A": {
                    "status": 200,
                    "frames": [{
                        "schema": {
                            "refId": "A",
                            "fields": [
                                {"name": "time", "type": "time"},
                                {"name": "A-series", "type": "number", "labels": {"job": "api"}}
                            ]
                        },
                        "data": {"values": [[1, 2, 3], [0.5, null, 1.5]]}
                    }]
                },
                "B": {"status": 400, "error": "parse error near ann@example.com", "frames": []}
            }
        })
    }

    #[test]
    fn parses_frames_and_errors() {
        let results = parse_response(&sample());
        assert_eq!(results.len(), 2);
        let a = results.iter().find(|r| r.ref_id == "A").unwrap();
        assert_eq!(a.row_count(), 3);
        assert_eq!(a.frames[0].fields[1].labels["job"], "api");
        assert_eq!(a.frames[0].rows()[1], vec![json!(2), Value::Null]);
        assert_eq!(a.frames[0].numeric_series(), vec![vec![0.5, 1.5]]);
        let b = results.iter().find(|r| r.ref_id == "B").unwrap();
        assert!(b.error.is_some());
    }

    #[test]
    fn summaries_carry_no_values_and_mask_errors() {
        let results = parse_response(&sample());
        let mask = |text: &str| text.replace("ann@example.com", "<email#1>");
        let summaries: Vec<TargetStatus> = results.iter().map(|r| summarize(r, &mask)).collect();
        let dump = serde_json::to_string(&summaries).unwrap();
        assert!(!dump.contains("0.5"));
        assert!(!dump.contains("ann@example.com"));
        assert!(dump.contains("A-series"));
        assert!(dump.contains("<email#1>"));
    }

    #[test]
    fn ragged_columns_are_padded() {
        let frame = Frame {
            name: None,
            fields: vec![],
            columns: vec![vec![json!(1)], vec![json!(1), json!(2)]],
        };
        assert_eq!(frame.rows()[1], vec![Value::Null, json!(2)]);
    }

    #[test]
    fn missing_results_parse_to_nothing() {
        assert!(parse_response(&json!({"message": "nope"})).is_empty());
    }
}

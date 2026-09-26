//! Dashboard JSON: validation, normalisation and panel traversal.
//!
//! The agent writes standard Grafana dashboard JSON (docs/DESIGN.md step 3).
//! Before it reaches Grafana, [`normalize`] checks the parts that would
//! otherwise fail silently — a panel pointing at a datasource that does not
//! exist renders as an empty box, not an error — and pins the fields the
//! session owns: the uid, the refresh interval and the `dashr` tag
//! (requirement DASHR-MCP-003).

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

/// Panel types Grafana ships. Anything else is a warning, not an error: the
/// custom image may carry more.
const KNOWN_PANEL_TYPES: &[&str] = &[
    "timeseries",
    "stat",
    "gauge",
    "bargauge",
    "barchart",
    "table",
    "logs",
    "text",
    "piechart",
    "heatmap",
    "histogram",
    "state-timeline",
    "status-history",
    "traces",
    "nodeGraph",
    "xychart",
    "trend",
    "row",
    "flamegraph",
    "canvas",
    "geomap",
    "dashlist",
    "news",
    "alertlist",
    "annolist",
];

/// Datasource uids Grafana resolves itself.
const SPECIAL_DATASOURCES: &[&str] = &["-- Mixed --", "-- Dashboard --", "grafana", "__expr__"];

/// The most panels one dashboard may carry. A dashboard of hundreds of
/// panels is a runaway loop, not a design.
pub const MAX_PANELS: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DashboardError {
    #[error("dashboard must be a JSON object")]
    NotAnObject,
    #[error("dashboard needs a non-empty title")]
    NoTitle,
    #[error("dashboard.panels must be an array")]
    PanelsNotArray,
    #[error("dashboard has {0} panels; the limit is {MAX_PANELS}")]
    TooManyPanels(usize),
    #[error("panel {index} is not an object")]
    PanelNotObject { index: usize },
    #[error("panel {title:?} has no type")]
    PanelNoType { title: String },
    #[error("panel {title:?} uses datasource {uid:?}, which is not provisioned; known: {known}")]
    UnknownDatasource {
        title: String,
        uid: String,
        known: String,
    },
    #[error("panel {title:?}: targets must be an array")]
    TargetsNotArray { title: String },
}

/// The fields a session pins on every dashboard it pushes.
#[derive(Debug, Clone)]
pub struct Pins<'a> {
    pub uid: &'a str,
    pub refresh: &'a str,
    pub time_from: &'a str,
    pub known_datasources: &'a [String],
}

/// A normalised dashboard and what was changed or doubtful about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Normalized {
    pub dashboard: Value,
    pub warnings: Vec<String>,
    pub panel_count: usize,
}

/// Accepts either a bare dashboard or `{"dashboard": {...}}`.
pub fn unwrap_envelope(input: &Value) -> &Value {
    match input.get("dashboard") {
        Some(inner) if inner.is_object() => inner,
        _ => input,
    }
}

fn datasource_uid(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => map.get("uid").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

fn panel_title(panel: &Map<String, Value>) -> String {
    panel
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// Every panel, rows' collapsed children included, in document order.
pub fn flat_panels(dashboard: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    if let Some(panels) = dashboard.get("panels").and_then(Value::as_array) {
        for panel in panels {
            out.push(panel);
            if let Some(children) = panel.get("panels").and_then(Value::as_array) {
                out.extend(children.iter());
            }
        }
    }
    out
}

/// A panel's queries, each with its effective datasource filled in.
///
/// A target without a datasource inherits the panel's, which is how Grafana
/// resolves it. Hidden targets are skipped: Grafana does not run them.
pub fn panel_queries(panel: &Value) -> Vec<Value> {
    let panel_datasource = panel.get("datasource").cloned();
    panel
        .get("targets")
        .and_then(Value::as_array)
        .map(|targets| {
            targets
                .iter()
                .filter(|target| target.get("hide").and_then(Value::as_bool) != Some(true))
                .enumerate()
                .map(|(index, target)| {
                    let mut query = target.clone();
                    if let Some(object) = query.as_object_mut() {
                        let missing = object
                            .get("datasource")
                            .map(|value| value.is_null())
                            .unwrap_or(true);
                        if missing && let Some(datasource) = &panel_datasource {
                            object.insert("datasource".into(), datasource.clone());
                        }
                        if !object.contains_key("refId") {
                            let ref_id = char::from(b'A' + (index % 26) as u8).to_string();
                            object.insert("refId".into(), json!(ref_id));
                        }
                    }
                    query
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The uid of a query's datasource, if it has one.
pub fn query_datasource_uid(query: &Value) -> Option<String> {
    query.get("datasource").and_then(datasource_uid)
}

fn check_datasource(
    uid: &str,
    title: &str,
    known: &[String],
    warnings: &mut Vec<String>,
) -> Result<(), DashboardError> {
    if uid.starts_with('$') {
        warnings.push(format!(
            "panel {title:?} uses datasource variable {uid}; it cannot be checked or monitored"
        ));
        return Ok(());
    }
    if SPECIAL_DATASOURCES.contains(&uid) || known.iter().any(|k| k == uid) {
        return Ok(());
    }
    Err(DashboardError::UnknownDatasource {
        title: title.to_owned(),
        uid: uid.to_owned(),
        known: known.join(", "),
    })
}

fn normalize_panel(
    panel: &mut Map<String, Value>,
    pins: &Pins<'_>,
    ids: &mut BTreeSet<i64>,
    next_id: &mut i64,
    cursor: &mut (i64, i64),
    warnings: &mut Vec<String>,
) -> Result<(), DashboardError> {
    let title = panel_title(panel);
    let panel_type = panel
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty())
        .ok_or_else(|| DashboardError::PanelNoType {
            title: title.clone(),
        })?
        .to_owned();
    if !KNOWN_PANEL_TYPES.contains(&panel_type.as_str()) {
        warnings.push(format!(
            "panel {title:?} has type {panel_type:?}, which the stock image may not render"
        ));
    }

    // Unique numeric ids; panel_status and watches refer to panels by id.
    let id = panel.get("id").and_then(Value::as_i64);
    let id = match id {
        Some(id) if !ids.contains(&id) => id,
        _ => {
            while ids.contains(next_id) {
                *next_id += 1;
            }
            *next_id
        }
    };
    ids.insert(id);
    panel.insert("id".into(), json!(id));

    // Flow layout for panels without a position: two per row.
    if !panel.get("gridPos").is_some_and(Value::is_object) {
        let (width, height) = if panel_type == "row" {
            (24, 1)
        } else {
            (12, 8)
        };
        let (x, y) = *cursor;
        let (x, y) = if x + width > 24 { (0, y + 8) } else { (x, y) };
        panel.insert(
            "gridPos".into(),
            json!({"x": x, "y": y, "w": width, "h": height}),
        );
        *cursor = if x + width >= 24 {
            (0, y + height)
        } else {
            (x + width, y)
        };
    }

    if let Some(datasource) = panel.get("datasource")
        && let Some(uid) = datasource_uid(datasource)
    {
        check_datasource(&uid, &title, pins.known_datasources, warnings)?;
    }
    match panel.get("targets") {
        None | Some(Value::Null) => {}
        Some(Value::Array(targets)) => {
            for target in targets {
                if let Some(uid) = target.get("datasource").and_then(datasource_uid) {
                    check_datasource(&uid, &title, pins.known_datasources, warnings)?;
                }
            }
        }
        Some(_) => return Err(DashboardError::TargetsNotArray { title }),
    }
    Ok(())
}

/// Validates a dashboard and pins the session's fields on it.
pub fn normalize(input: &Value, pins: &Pins<'_>) -> Result<Normalized, DashboardError> {
    let mut dashboard = unwrap_envelope(input).clone();
    let object = dashboard
        .as_object_mut()
        .ok_or(DashboardError::NotAnObject)?;
    let title = object
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .ok_or(DashboardError::NoTitle)?
        .to_owned();

    let mut warnings = Vec::new();
    let mut panel_count = 0;
    let mut ids = BTreeSet::new();
    let mut next_id: i64 = 1;
    let mut cursor = (0, 0);

    // First collect ids that are already valid, so assigned ids avoid them.
    match object.get("panels") {
        None | Some(Value::Null) => {
            object.insert("panels".into(), json!([]));
        }
        Some(Value::Array(_)) => {}
        Some(_) => return Err(DashboardError::PanelsNotArray),
    }
    let mut panels = object
        .get_mut("panels")
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
        .unwrap_or_default();

    let total = panels.len()
        + panels
            .iter()
            .filter_map(|panel| panel.get("panels").and_then(Value::as_array))
            .map(Vec::len)
            .sum::<usize>();
    if total > MAX_PANELS {
        return Err(DashboardError::TooManyPanels(total));
    }

    for (index, panel) in panels.iter_mut().enumerate() {
        let panel_object = panel
            .as_object_mut()
            .ok_or(DashboardError::PanelNotObject { index })?;
        normalize_panel(
            panel_object,
            pins,
            &mut ids,
            &mut next_id,
            &mut cursor,
            &mut warnings,
        )?;
        panel_count += 1;
        if let Some(children) = panel_object.get_mut("panels").and_then(Value::as_array_mut) {
            for (child_index, child) in children.iter_mut().enumerate() {
                let child_object = child
                    .as_object_mut()
                    .ok_or(DashboardError::PanelNotObject { index: child_index })?;
                let mut child_cursor = (0, 0);
                normalize_panel(
                    child_object,
                    pins,
                    &mut ids,
                    &mut next_id,
                    &mut child_cursor,
                    &mut warnings,
                )?;
                panel_count += 1;
            }
        }
    }
    object.insert("panels".into(), Value::Array(panels));

    object.insert("title".into(), json!(title));
    object.insert("uid".into(), json!(pins.uid));
    object.remove("id");
    object.remove("version");
    object.insert("refresh".into(), json!(pins.refresh));
    if !object.get("time").is_some_and(Value::is_object) {
        object.insert("time".into(), json!({"from": pins.time_from, "to": "now"}));
    }
    object.entry("schemaVersion").or_insert_with(|| json!(39));
    let mut tags: Vec<Value> = object
        .get("tags")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !tags.iter().any(|tag| tag == "dashr") {
        tags.push(json!("dashr"));
    }
    object.insert("tags".into(), Value::Array(tags));
    if panel_count == 0 {
        warnings.push("dashboard has no panels".to_owned());
    }

    Ok(Normalized {
        dashboard,
        warnings,
        panel_count,
    })
}

/// A starting dashboard, shown before the agent has built anything.
///
/// It uses the TestData datasource only, so it renders on every machine and
/// shows the human that the pane is alive.
pub fn welcome(title: &str, note: &str) -> Value {
    json!({
        "title": title,
        "panels": [
            {
                "id": 1,
                "type": "text",
                "title": "herdr-dashr",
                "gridPos": {"x": 0, "y": 0, "w": 24, "h": 6},
                "options": {"mode": "markdown", "content": note}
            },
            {
                "id": 2,
                "type": "timeseries",
                "title": "Heartbeat (TestData)",
                "gridPos": {"x": 0, "y": 6, "w": 24, "h": 8},
                "datasource": {"type": "grafana-testdata-datasource", "uid": crate::provisioning::TESTDATA_UID},
                "targets": [{"refId": "A", "scenarioId": "random_walk", "seriesCount": 1}]
            }
        ]
    })
}

/// The first dashboard of an OpenTelemetry session: where to send data, a
/// live trail of every log line and the latest traces (DASHR-OTEL-005).
pub fn otel_welcome(http_endpoint: &str, grpc_endpoint: &str) -> Value {
    let note = format!(
        "**OpenTelemetry endpoint of this pane** — nothing is stored; it stops when the pane closes.\n\n\
         `OTEL_EXPORTER_OTLP_ENDPOINT={http_endpoint}` (OTLP/HTTP) · gRPC `{grpc_endpoint}`\n\n\
         Ship any command's output: `dashr tail -- <command>` · Ask the agent below to watch for the log messages you expect."
    );
    let loki = json!({"type": "loki", "uid": "loki"});
    let tempo = json!({"type": "tempo", "uid": "tempo"});
    json!({
        "title": "OpenTelemetry",
        "time": {"from": "now-15m", "to": "now"},
        "panels": [
            {
                "id": 1,
                "type": "text",
                "title": "herdr-dashr",
                "gridPos": {"x": 0, "y": 0, "w": 24, "h": 4},
                "options": {"mode": "markdown", "content": note}
            },
            {
                "id": 2,
                "type": "logs",
                "title": "Logs (all services)",
                "gridPos": {"x": 0, "y": 4, "w": 24, "h": 14},
                "datasource": loki,
                "targets": [{"refId": "A", "datasource": loki,
                             "expr": crate::logx::DEFAULT_SELECTOR, "queryType": "range"}],
                "options": {"showTime": true, "sortOrder": "Descending", "wrapLogMessage": true,
                            "enableLogDetails": true, "dedupStrategy": "none"}
            },
            {
                "id": 3,
                "type": "table",
                "title": "Recent traces",
                "gridPos": {"x": 0, "y": 18, "w": 24, "h": 8},
                "datasource": tempo,
                "targets": [{"refId": "A", "datasource": tempo, "queryType": "traceql",
                             "query": "{}", "limit": 20, "tableType": "traces"}]
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otel_welcome_validates_against_the_otel_datasources() {
        let known = vec![
            "dashr-testdata".to_owned(),
            "loki".to_owned(),
            "tempo".to_owned(),
        ];
        let welcome = otel_welcome("http://127.0.0.1:4318", "http://127.0.0.1:4317");
        let normalized = normalize(&welcome, &pins(&known)).unwrap();
        assert!(normalized.warnings.is_empty(), "{:?}", normalized.warnings);
        assert!(
            welcome
                .to_string()
                .contains("OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318")
        );
    }

    fn known() -> Vec<String> {
        vec!["dashr-testdata".to_owned(), "prom".to_owned()]
    }

    fn pins(known: &[String]) -> Pins<'_> {
        Pins {
            uid: "dashr-abc-w1-p1",
            refresh: "5s",
            time_from: "now-1h",
            known_datasources: known,
        }
    }

    #[test]
    fn pins_session_fields_and_assigns_ids_and_layout() {
        let known = known();
        let input = json!({"dashboard": {
            "title": " Errors ",
            "uid": "agent-chose-this",
            "id": 7,
            "version": 3,
            "tags": ["x"],
            "panels": [
                {"type": "timeseries", "title": "a", "datasource": {"uid": "prom"}, "targets": [{"expr": "up"}]},
                {"type": "stat", "title": "b", "id": 1},
                {"type": "stat", "title": "c", "id": 1}
            ]
        }});
        let normalized = normalize(&input, &pins(&known)).unwrap();
        let dashboard = &normalized.dashboard;
        assert_eq!(dashboard["uid"], "dashr-abc-w1-p1");
        assert_eq!(dashboard["title"], "Errors");
        assert!(dashboard.get("id").is_none());
        assert!(dashboard.get("version").is_none());
        assert_eq!(dashboard["refresh"], "5s");
        assert_eq!(dashboard["time"]["from"], "now-1h");
        assert_eq!(dashboard["tags"], json!(["x", "dashr"]));
        let ids: Vec<i64> = flat_panels(dashboard)
            .iter()
            .map(|p| p["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids.len(), 3);
        let unique: BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), 3, "{ids:?}");
        assert_eq!(dashboard["panels"][0]["gridPos"]["x"], 0);
        assert_eq!(dashboard["panels"][1]["gridPos"]["x"], 12);
        assert_eq!(dashboard["panels"][2]["gridPos"]["y"], 8);
        assert_eq!(normalized.panel_count, 3);
    }

    #[test]
    fn rejects_unknown_datasources_with_the_known_list() {
        let known = known();
        let input = json!({"title": "t", "panels": [
            {"type": "timeseries", "title": "a", "targets": [{"datasource": {"uid": "nope"}}]}
        ]});
        let error = normalize(&input, &pins(&known)).unwrap_err().to_string();
        assert!(error.contains("nope"), "{error}");
        assert!(error.contains("dashr-testdata"), "{error}");
    }

    #[test]
    fn accepts_special_and_variable_datasources_with_warnings() {
        let known = known();
        let input = json!({"title": "t", "panels": [
            {"type": "timeseries", "title": "a", "datasource": "-- Mixed --", "targets": [{"datasource": {"uid": "prom"}}]},
            {"type": "timeseries", "title": "b", "datasource": {"uid": "${ds}"}},
            {"type": "weird-plugin", "title": "c"}
        ]});
        let normalized = normalize(&input, &pins(&known)).unwrap();
        assert_eq!(normalized.warnings.len(), 2, "{:?}", normalized.warnings);
    }

    #[test]
    fn structural_errors() {
        let known = known();
        let p = pins(&known);
        assert_eq!(normalize(&json!([]), &p), Err(DashboardError::NotAnObject));
        assert_eq!(
            normalize(&json!({"title": ""}), &p),
            Err(DashboardError::NoTitle)
        );
        assert_eq!(
            normalize(&json!({"title": "t", "panels": {}}), &p),
            Err(DashboardError::PanelsNotArray)
        );
        assert!(matches!(
            normalize(&json!({"title": "t", "panels": [{"title": "x"}]}), &p),
            Err(DashboardError::PanelNoType { .. })
        ));
        let many: Vec<Value> = (0..61).map(|_| json!({"type": "stat"})).collect();
        assert_eq!(
            normalize(&json!({"title": "t", "panels": many}), &p),
            Err(DashboardError::TooManyPanels(61))
        );
    }

    #[test]
    fn rows_children_are_validated_and_flattened() {
        let known = known();
        let input = json!({"title": "t", "panels": [
            {"type": "row", "title": "r", "collapsed": true, "panels": [
                {"type": "stat", "title": "inner", "targets": [{"datasource": {"uid": "prom"}}]}
            ]}
        ]});
        let normalized = normalize(&input, &pins(&known)).unwrap();
        assert_eq!(normalized.panel_count, 2);
        assert_eq!(flat_panels(&normalized.dashboard).len(), 2);
        let bad = json!({"title": "t", "panels": [
            {"type": "row", "panels": [{"type": "stat", "datasource": {"uid": "zzz"}}]}
        ]});
        assert!(normalize(&bad, &pins(&known)).is_err());
    }

    #[test]
    fn queries_inherit_panel_datasource_and_skip_hidden() {
        let panel = json!({
            "datasource": {"uid": "prom", "type": "prometheus"},
            "targets": [
                {"expr": "up"},
                {"expr": "down", "hide": true},
                {"refId": "Z", "datasource": {"uid": "dashr-testdata"}}
            ]
        });
        let queries = panel_queries(&panel);
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0]["datasource"]["uid"], "prom");
        assert_eq!(queries[0]["refId"], "A");
        assert_eq!(query_datasource_uid(&queries[1]).unwrap(), "dashr-testdata");
    }

    #[test]
    fn welcome_dashboard_is_valid() {
        let known = known();
        let normalized = normalize(&welcome("hello", "note"), &pins(&known)).unwrap();
        assert!(normalized.warnings.is_empty());
        assert_eq!(normalized.panel_count, 2);
    }
}

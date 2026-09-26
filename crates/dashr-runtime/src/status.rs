//! Panel health without values (requirement DASHR-MCP-004).
//!
//! The agent verifies its dashboard by asking which panels returned rows,
//! which are empty and which errored — never by looking at the numbers.

use dashr_core::dashboard;
use dashr_core::frames::{self, QueryResult, TargetStatus};
use dashr_core::masking::Masker;
use dashr_grafana::Client;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelState {
    Ok,
    Empty,
    Error,
    /// Text panels, rows: nothing to query.
    NoQueries,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PanelStatus {
    pub panel_id: i64,
    pub title: String,
    #[serde(rename = "type")]
    pub panel_type: String,
    pub state: PanelState,
    pub rows: usize,
    pub targets: Vec<TargetStatus>,
}

/// Counts for the sidebar token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub ok: usize,
    pub empty: usize,
    pub error: usize,
}

impl Summary {
    pub fn of(statuses: &[PanelStatus]) -> Self {
        let mut summary = Self::default();
        for status in statuses {
            match status.state {
                PanelState::Ok => summary.ok += 1,
                PanelState::Empty => summary.empty += 1,
                PanelState::Error => summary.error += 1,
                PanelState::NoQueries => {}
            }
        }
        summary
    }

    /// The sidebar token text, e.g. `6 ok · 1 err`.
    pub fn token(&self) -> String {
        let mut parts = vec![format!("{} ok", self.ok)];
        if self.empty > 0 {
            parts.push(format!("{} empty", self.empty));
        }
        if self.error > 0 {
            parts.push(format!("{} err", self.error));
        }
        parts.join(" · ")
    }
}

/// Runs one panel's queries.
pub fn panel_results(client: &Client, dashboard_model: &Value, panel: &Value) -> Vec<QueryResult> {
    let (from, to) = dashr_grafana::time_range(dashboard_model);
    let queries = dashboard::panel_queries(panel);
    client.query(&queries, &from, &to)
}

/// Classifies one panel from its results.
pub fn classify(panel: &Value, results: &[QueryResult], masker: &Masker) -> PanelStatus {
    let mask = |text: &str| masker.mask_text(text);
    let targets: Vec<TargetStatus> = results
        .iter()
        .map(|result| frames::summarize(result, &mask))
        .collect();
    let rows = targets.iter().map(|target| target.rows).sum();
    let state = if targets.is_empty() {
        PanelState::NoQueries
    } else if targets.iter().any(|target| target.error.is_some()) {
        PanelState::Error
    } else if rows == 0 {
        PanelState::Empty
    } else {
        PanelState::Ok
    };
    PanelStatus {
        panel_id: panel.get("id").and_then(Value::as_i64).unwrap_or(0),
        title: panel
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        panel_type: panel
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        state,
        rows,
        targets,
    }
}

/// Status of every queryable panel on a dashboard.
pub fn dashboard_status(
    client: &Client,
    dashboard_model: &Value,
    masker: &Masker,
) -> Vec<PanelStatus> {
    dashboard::flat_panels(dashboard_model)
        .into_iter()
        .filter(|panel| panel.get("type").and_then(Value::as_str) != Some("row"))
        .map(|panel| {
            let results = panel_results(client, dashboard_model, panel);
            classify(panel, &results, masker)
        })
        .collect()
}

/// Finds a panel by id.
pub fn find_panel(dashboard_model: &Value, panel_id: i64) -> Option<&Value> {
    dashboard::flat_panels(dashboard_model)
        .into_iter()
        .find(|panel| panel.get("id").and_then(Value::as_i64) == Some(panel_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashr_core::config::MaskingConfig;
    use serde_json::json;

    fn result(rows: usize, error: Option<&str>) -> QueryResult {
        let body = json!({"results": {"A": {
            "status": if error.is_some() { 400 } else { 200 },
            "error": error,
            "frames": [{"schema": {"fields": [{"name": "v", "type": "number"}]},
                         "data": {"values": [(0..rows).map(|i| json!(i)).collect::<Vec<_>>()]}}]
        }}});
        frames::parse_response(&body).remove(0)
    }

    #[test]
    fn classification() {
        let masker = Masker::new(&MaskingConfig::default());
        let panel = json!({"id": 4, "title": "t", "type": "stat"});
        assert_eq!(
            classify(&panel, &[result(3, None)], &masker).state,
            PanelState::Ok
        );
        assert_eq!(
            classify(&panel, &[result(0, None)], &masker).state,
            PanelState::Empty
        );
        let errored = classify(
            &panel,
            &[result(3, None), result(0, Some("bad ann@example.com"))],
            &masker,
        );
        assert_eq!(errored.state, PanelState::Error);
        assert_eq!(errored.targets[1].error.as_deref(), Some("bad <email#1>"));
        assert_eq!(classify(&panel, &[], &masker).state, PanelState::NoQueries);
        assert_eq!(errored.panel_id, 4);
    }

    #[test]
    fn summary_token() {
        let masker = Masker::new(&MaskingConfig::default());
        let panel = json!({"id": 1, "type": "stat"});
        let statuses = vec![
            classify(&panel, &[result(1, None)], &masker),
            classify(&panel, &[result(1, None)], &masker),
            classify(&panel, &[result(0, None)], &masker),
            classify(&panel, &[result(0, Some("x"))], &masker),
            classify(&panel, &[], &masker),
        ];
        let summary = Summary::of(&statuses);
        assert_eq!(
            summary,
            Summary {
                ok: 2,
                empty: 1,
                error: 1
            }
        );
        assert_eq!(summary.token(), "2 ok · 1 empty · 1 err");
        assert_eq!(
            Summary {
                ok: 3,
                ..Summary::default()
            }
            .token(),
            "3 ok"
        );
    }

    #[test]
    fn finds_panels_inside_rows() {
        let model =
            json!({"panels": [{"id": 1, "type": "row", "panels": [{"id": 7, "type": "stat"}]}]});
        assert!(find_panel(&model, 7).is_some());
        assert!(find_panel(&model, 8).is_none());
    }
}

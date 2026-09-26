//! The tools the agent gets, and the privacy rules they enforce.
//!
//! Every value that came from a datasource leaves through the masker; the
//! rest of what the tools return is schema (field names and types), counts,
//! errors (masked too), and the dashboard JSON the agent itself wrote.

use base64::Engine;
use dashr_aws::cli::AwsCli;
use dashr_core::config::Config;
use dashr_core::dashboard;
use dashr_core::frames::QueryResult;
use dashr_core::masking::Masker;
use dashr_core::provisioning::{self, DatasourcePolicy};
use dashr_core::session::{SessionRecord, SessionStore};
use dashr_core::watch::{Comparison, Reducer, WatchRule};
use dashr_grafana::Client;
use dashr_herdr::Herdr;
use dashr_runtime::browser::Browser;
use dashr_runtime::{apply, promote, status};
use serde_json::{Value, json};

use crate::protocol::{ToolOutput, Tools};

/// What the agent is told when it connects.
pub const INSTRUCTIONS: &str = "\
herdr-dashr gives you a disposable Grafana shown live in the pane above you. \
Build dashboards for the problem the human describes.

Privacy rules, enforced by these tools:
- You never see real data values. Design from schemas: list_datasources, probe_query and \
panel_data_sample return field names and types with MASKED sample rows (<email#1>, <ipv4#2>...). \
Pseudonyms are stable within one answer, so repetition and cardinality are visible.
- Verify your work with panel_status: rows, empty panels and query errors, never values.
- Do not try to read values another way (screenshots of personal data, curl to Grafana, \
reading the pane). The screenshot tool refuses dashboards that touch personal datasources.

Workflow: list_datasources -> probe_query to learn the shape of data -> apply_dashboard \
(standard Grafana dashboard JSON; uid, refresh and tags are set for you) -> panel_status -> fix. \
Use watch_panel to alert the human when a panel crosses a threshold; the pane evaluates it locally. \
promote copies a useful dashboard to the team's persistent Grafana.";

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({"name": name, "description": description, "inputSchema": schema})
}

/// The strictest policy, for data whose datasource cannot be identified.
fn strict_policy(uid: &str) -> DatasourcePolicy {
    DatasourcePolicy {
        uid: uid.to_owned(),
        name: uid.to_owned(),
        plugin_type: "unknown".to_owned(),
        personal: true,
        allow_fields: Vec::new(),
    }
}

pub struct DashrTools {
    config: Config,
    store: SessionStore,
    session_id: String,
    masker: Masker,
    browser: Option<Browser>,
    aws: AwsCli,
    herdr: Option<Herdr>,
}

impl DashrTools {
    pub fn new(
        config: Config,
        store: SessionStore,
        session_id: &str,
        herdr: Option<Herdr>,
    ) -> Self {
        let masker = Masker::new(&config.masking);
        let browser = config
            .browser
            .enabled
            .then(|| Browser::new(&config.browser.command));
        let aws = AwsCli::new(&config.aws.cli, config.aws.profile.as_deref());
        Self {
            config,
            store,
            session_id: session_id.to_owned(),
            masker,
            browser,
            aws,
            herdr,
        }
    }

    fn record(&self) -> Result<SessionRecord, String> {
        self.store
            .load(&self.session_id)
            .map_err(|error| error.to_string())
    }

    fn client(record: &SessionRecord) -> Client {
        Client::local(&record.grafana_url())
    }

    fn current_dashboard(&self, record: &SessionRecord) -> Result<Value, String> {
        Self::client(record)
            .dashboard(&record.dashboard_uid)
            .map_err(|error| error.to_string())
    }

    /// Masks the results of queries, per the policy of each query's datasource.
    fn masked_results(
        &self,
        masker: &Masker,
        record: &SessionRecord,
        queries: &[Value],
        results: &[QueryResult],
    ) -> Vec<Value> {
        results
            .iter()
            .map(|result| {
                let query = queries.iter().find(|query| {
                    query.get("refId").and_then(Value::as_str) == Some(&result.ref_id)
                });
                let uid = query
                    .and_then(dashboard::query_datasource_uid)
                    .unwrap_or_else(|| "unknown".to_owned());
                let policy = record
                    .policy(&uid)
                    .cloned()
                    .unwrap_or_else(|| strict_policy(&uid));
                let frames: Vec<Value> = result
                    .frames
                    .iter()
                    .take(3)
                    .map(|frame| {
                        let table = masker.mask_table(&frame.fields, &frame.rows(), &policy);
                        json!({
                            "name": frame.name.as_deref().map(|name| masker.mask_text(name)),
                            "fields": table.fields,
                            "rows": table.rows,
                            "total_rows": table.total_rows,
                            "replaced": table.replaced,
                        })
                    })
                    .collect();
                json!({
                    "ref_id": result.ref_id,
                    "datasource": policy.name,
                    "personal": policy.personal,
                    "error": result.error.as_deref().map(|error| masker.mask_text(error)),
                    "frames_total": result.frames.len(),
                    "frames": frames,
                })
            })
            .collect()
    }

    fn list_datasources(&self) -> ToolOutput {
        match self.record() {
            Ok(record) => ToolOutput::Json(json!({"datasources": record.datasources})),
            Err(error) => ToolOutput::Error(error),
        }
    }

    fn get_dashboard(&self) -> ToolOutput {
        let result = self
            .record()
            .and_then(|record| self.current_dashboard(&record));
        match result {
            Ok(model) => ToolOutput::Json(json!({"dashboard": model})),
            Err(error) => ToolOutput::Error(error),
        }
    }

    fn apply_dashboard(&self, arguments: &Value) -> ToolOutput {
        let record = match self.record() {
            Ok(record) => record,
            Err(error) => return ToolOutput::Error(error),
        };
        let Some(input) = arguments.get("dashboard") else {
            return ToolOutput::Error("apply_dashboard needs a `dashboard` object".into());
        };
        let message = arguments
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("applied by agent");
        match apply::apply(
            &record,
            &Self::client(&record),
            self.browser.as_ref(),
            &self.config.grafana.time_from,
            input,
            message,
        ) {
            Ok(outcome) => ToolOutput::Json(json!({
                "applied": true,
                "uid": outcome.uid,
                "version": outcome.version,
                "panel_count": outcome.panel_count,
                "warnings": outcome.warnings,
                "browser_reloaded": outcome.reloaded,
                "next": "call panel_status to check every panel returns data"
            })),
            Err(error) => ToolOutput::Error(format!("dashboard not applied: {error}")),
        }
    }

    fn panel_status(&self, arguments: &Value) -> ToolOutput {
        let record = match self.record() {
            Ok(record) => record,
            Err(error) => return ToolOutput::Error(error),
        };
        let model = match self.current_dashboard(&record) {
            Ok(model) => model,
            Err(error) => return ToolOutput::Error(error),
        };
        let client = Self::client(&record);
        let statuses = match arguments.get("panel_id").and_then(Value::as_i64) {
            Some(id) => match status::find_panel(&model, id) {
                Some(panel) => {
                    let results = status::panel_results(&client, &model, panel);
                    vec![status::classify(panel, &results, &self.masker)]
                }
                None => return ToolOutput::Error(format!("no panel with id {id}")),
            },
            None => status::dashboard_status(&client, &model, &self.masker),
        };
        let summary = status::Summary::of(&statuses);
        ToolOutput::Json(json!({"summary": summary, "panels": statuses}))
    }

    fn max_rows(&self, arguments: &Value) -> usize {
        arguments
            .get("max_rows")
            .and_then(Value::as_u64)
            .map(|rows| rows as usize)
            .unwrap_or(self.config.masking.max_rows)
            .clamp(1, self.config.masking.max_rows)
    }

    fn panel_data_sample(&self, arguments: &Value) -> ToolOutput {
        let record = match self.record() {
            Ok(record) => record,
            Err(error) => return ToolOutput::Error(error),
        };
        let Some(panel_id) = arguments.get("panel_id").and_then(Value::as_i64) else {
            return ToolOutput::Error("panel_data_sample needs panel_id".into());
        };
        let model = match self.current_dashboard(&record) {
            Ok(model) => model,
            Err(error) => return ToolOutput::Error(error),
        };
        let Some(panel) = status::find_panel(&model, panel_id) else {
            return ToolOutput::Error(format!("no panel with id {panel_id}"));
        };
        let mut queries = dashboard::panel_queries(panel);
        if let Some(ref_id) = arguments.get("ref_id").and_then(Value::as_str) {
            queries.retain(|query| query.get("refId").and_then(Value::as_str) == Some(ref_id));
        }
        let (from, to) = dashr_grafana::time_range(&model);
        let results = Self::client(&record).query(&queries, &from, &to);
        let masker = self.masker_with_rows(self.max_rows(arguments));
        ToolOutput::Json(json!({
            "panel_id": panel_id,
            "title": panel.get("title"),
            "targets": self.masked_results(&masker, &record, &queries, &results),
        }))
    }

    /// A masker with a smaller row cap, never a larger one.
    fn masker_with_rows(&self, rows: usize) -> Masker {
        let mut config = self.config.masking.clone();
        config.max_rows = rows;
        Masker::new(&config)
    }

    fn probe_query(&self, arguments: &Value) -> ToolOutput {
        let record = match self.record() {
            Ok(record) => record,
            Err(error) => return ToolOutput::Error(error),
        };
        let Some(uid) = arguments.get("datasource_uid").and_then(Value::as_str) else {
            return ToolOutput::Error("probe_query needs datasource_uid".into());
        };
        let Some(policy) = record.policy(uid) else {
            return ToolOutput::Error(format!(
                "unknown datasource {uid}; known: {}",
                record.datasource_uids().join(", ")
            ));
        };
        let Some(query) = arguments.get("query").and_then(Value::as_object) else {
            return ToolOutput::Error(
                "probe_query needs a `query` object in the datasource's query model".into(),
            );
        };
        let mut query = Value::Object(query.clone());
        query["refId"] = json!("A");
        query["datasource"] = json!({"uid": uid, "type": policy.plugin_type});
        let from = arguments
            .get("from")
            .and_then(Value::as_str)
            .unwrap_or(&self.config.grafana.time_from);
        let to = arguments.get("to").and_then(Value::as_str).unwrap_or("now");
        let queries = vec![query];
        let results = Self::client(&record).query(&queries, from, to);
        let masker = self.masker_with_rows(self.max_rows(arguments));
        ToolOutput::Json(
            json!({"results": self.masked_results(&masker, &record, &queries, &results)}),
        )
    }

    fn watch_panel(&self, arguments: &Value) -> ToolOutput {
        let record = match self.record() {
            Ok(record) => record,
            Err(error) => return ToolOutput::Error(error),
        };
        let Some(panel_id) = arguments.get("panel_id").and_then(Value::as_i64) else {
            return ToolOutput::Error("watch_panel needs panel_id".into());
        };
        let model = match self.current_dashboard(&record) {
            Ok(model) => model,
            Err(error) => return ToolOutput::Error(error),
        };
        if status::find_panel(&model, panel_id).is_none() {
            return ToolOutput::Error(format!("no panel with id {panel_id}"));
        }
        let reducer: Reducer = match serde_json::from_value(
            arguments.get("reducer").cloned().unwrap_or(json!("last")),
        ) {
            Ok(reducer) => reducer,
            Err(_) => {
                return ToolOutput::Error(
                    "reducer must be one of last, max, min, mean, sum, count".into(),
                );
            }
        };
        let op: Comparison =
            match serde_json::from_value(arguments.get("op").cloned().unwrap_or(json!(">"))) {
                Ok(op) => op,
                Err(_) => {
                    return ToolOutput::Error("op must be one of >, >=, <, <=, ==, !=".into());
                }
            };
        let Some(threshold) = arguments.get("threshold").and_then(Value::as_f64) else {
            return ToolOutput::Error("watch_panel needs a numeric threshold".into());
        };
        let mut state = self.store.load_watches(&record.session_id);
        let mut number = state.rules.len() + 1;
        while state
            .rules
            .iter()
            .any(|rule| rule.id == format!("w{number}"))
        {
            number += 1;
        }
        let rule = WatchRule {
            id: format!("w{number}"),
            panel_id,
            reducer,
            op,
            threshold,
            label: arguments
                .get("label")
                .and_then(Value::as_str)
                .map(|label| label.chars().take(60).collect()),
        };
        state.rules.push(rule.clone());
        if let Err(error) = self.store.save_watches(&record.session_id, &state) {
            return ToolOutput::Error(error.to_string());
        }
        ToolOutput::Json(json!({
            "watch": rule,
            "evaluated_every_secs": self.config.monitor.interval_secs,
            "note": "the pane evaluates this locally; you will not see values, the human gets a Herdr notification"
        }))
    }

    fn list_watches(&self) -> ToolOutput {
        let state = self.store.load_watches(&self.session_id);
        ToolOutput::Json(json!({"watches": state.rules, "breached": state.breached}))
    }

    fn remove_watch(&self, arguments: &Value) -> ToolOutput {
        let Some(id) = arguments.get("id").and_then(Value::as_str) else {
            return ToolOutput::Error("remove_watch needs id".into());
        };
        let mut state = self.store.load_watches(&self.session_id);
        let before = state.rules.len();
        state.rules.retain(|rule| rule.id != id);
        state.breached.retain(|breached| breached != id);
        if state.rules.len() == before {
            return ToolOutput::Error(format!("no watch {id}"));
        }
        match self.store.save_watches(&self.session_id, &state) {
            Ok(()) => ToolOutput::Json(json!({"removed": id})),
            Err(error) => ToolOutput::Error(error.to_string()),
        }
    }

    fn screenshot(&self) -> ToolOutput {
        let record = match self.record() {
            Ok(record) => record,
            Err(error) => return ToolOutput::Error(error),
        };
        let model = match self.current_dashboard(&record) {
            Ok(model) => model,
            Err(error) => return ToolOutput::Error(error),
        };
        if let Err(reason) = screenshot_allowed(&record, &model) {
            return ToolOutput::Error(format!(
                "screenshot refused: {reason}. Use panel_status to verify panels instead."
            ));
        }
        let Some(browser) = &self.browser else {
            return ToolOutput::Error("the browser pane is disabled in configuration".into());
        };
        let path = record.runtime_dir.join("screenshot.png");
        let fragment = format!("127.0.0.1:{}/d/{}", record.port, record.dashboard_uid);
        if let Err(error) = browser.screenshot(&fragment, &path) {
            return ToolOutput::Error(error.to_string());
        }
        let bytes = std::fs::read(&path);
        let _ = std::fs::remove_file(&path);
        match bytes {
            Ok(bytes) => ToolOutput::Image {
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
                mime_type: "image/png".into(),
            },
            Err(error) => ToolOutput::Error(format!("screenshot not readable: {error}")),
        }
    }

    fn open_for_pipeline(&self, arguments: &Value) -> ToolOutput {
        let Some(url) = arguments.get("url").and_then(Value::as_str) else {
            return ToolOutput::Error("open_for_pipeline needs url".into());
        };
        let pipeline = match dashr_aws::url::parse(url) {
            Ok(pipeline) => pipeline,
            Err(error) => return ToolOutput::Error(error.to_string()),
        };
        let record = match self.record() {
            Ok(record) => record,
            Err(error) => return ToolOutput::Error(error),
        };
        let datasource = provisioning::cloudwatch_uid(&pipeline.region);
        if record.policy(&datasource).is_none() {
            // CloudWatch credentials reach Grafana only at container start,
            // so another region means another pane.
            return match &self.herdr {
                Some(herdr) => match herdr.plugin_pane_open(
                    dashr_herdr::manifest::PLUGIN_ID,
                    "dashboard",
                    "tab",
                    &[("DASHR_PIPELINE_URL".to_owned(), url.to_owned())],
                ) {
                    Ok(pane) => ToolOutput::Json(json!({
                        "opened": "new dashboard tab",
                        "pane": pane,
                        "reason": format!("this session has no CloudWatch datasource for {}", pipeline.region)
                    })),
                    Err(error) => ToolOutput::Error(error.to_string()),
                },
                None => ToolOutput::Error(format!(
                    "this session has no CloudWatch datasource for {}; open the pipeline with `dashr herdr action pipeline`",
                    pipeline.region
                )),
            };
        }
        let inventory = match self.aws.discover(&pipeline) {
            Ok(inventory) => inventory,
            Err(error) => return ToolOutput::Error(error.to_string()),
        };
        let proposal = dashr_aws::propose::propose(&inventory, &datasource);
        let applied = apply::apply(
            &record,
            &Self::client(&record),
            self.browser.as_ref(),
            &self.config.grafana.time_from,
            &proposal,
            "pipeline bootstrap",
        );
        ToolOutput::Json(json!({
            "inventory": inventory,
            "applied": applied.as_ref().map(|outcome| outcome.panel_count).ok(),
            "error": applied.err().map(|error| error.to_string()),
        }))
    }

    fn promote(&self, arguments: &Value) -> ToolOutput {
        let record = match self.record() {
            Ok(record) => record,
            Err(error) => return ToolOutput::Error(error),
        };
        match promote::promote(
            &record,
            &Self::client(&record),
            self.config.promote.as_ref(),
            arguments.get("title").and_then(Value::as_str),
        ) {
            Ok(promoted) => ToolOutput::Json(serde_json::to_value(promoted).unwrap_or_default()),
            Err(error) => ToolOutput::Error(error.to_string()),
        }
    }
}

/// Whether a screenshot may be taken: every datasource the dashboard uses
/// must be known and flagged non-personal (DASHR-MCP-010, DASHR-SEC-006).
pub fn screenshot_allowed(record: &SessionRecord, model: &Value) -> Result<(), String> {
    let mut used = Vec::new();
    promote::referenced_uids(model, &mut used);
    for uid in used {
        if uid == "__expr__" || uid == "-- Mixed --" {
            continue; // Mixed panels are judged by their targets.
        }
        match record.policy(&uid) {
            Some(policy) if !policy.personal => {}
            Some(policy) => {
                return Err(format!(
                    "datasource {} may contain personal data",
                    policy.name
                ));
            }
            None => return Err(format!("datasource {uid} cannot be checked")),
        }
    }
    Ok(())
}

impl Tools for DashrTools {
    fn definitions(&self) -> Vec<Value> {
        let empty = json!({"type": "object", "properties": {}, "additionalProperties": false});
        vec![
            tool(
                "list_datasources",
                "Datasources in this session's Grafana: uid, name, type, and whether it may hold personal data.",
                empty.clone(),
            ),
            tool(
                "get_dashboard",
                "The current dashboard JSON (your own design; contains no data).",
                empty.clone(),
            ),
            tool(
                "apply_dashboard",
                "Validate and push a Grafana dashboard JSON, then reload the browser pane. uid, refresh and the dashr tag are set for you; datasource uids must come from list_datasources.",
                json!({"type": "object", "required": ["dashboard"], "properties": {
                    "dashboard": {"type": "object", "description": "Standard Grafana dashboard JSON with title and panels."},
                    "message": {"type": "string", "description": "Version message."}
                }}),
            ),
            tool(
                "panel_status",
                "Per panel: ok/empty/error, row counts, field names and masked errors. Never values.",
                json!({"type": "object", "properties": {"panel_id": {"type": "integer"}}}),
            ),
            tool(
                "panel_data_sample",
                "Masked sample rows of one panel's queries, with real field names and types.",
                json!({"type": "object", "required": ["panel_id"], "properties": {
                    "panel_id": {"type": "integer"},
                    "ref_id": {"type": "string"},
                    "max_rows": {"type": "integer", "minimum": 1}
                }}),
            ),
            tool(
                "probe_query",
                "Run one query against a datasource and return its schema with masked sample rows, to learn the shape of data before building a panel. `query` is the datasource's query model, e.g. {\"expr\":\"up\"} for Prometheus or {\"expr\":\"{app=\\\"api\\\"}\",\"queryType\":\"range\"} for Loki.",
                json!({"type": "object", "required": ["datasource_uid", "query"], "properties": {
                    "datasource_uid": {"type": "string"},
                    "query": {"type": "object"},
                    "from": {"type": "string", "description": "e.g. now-1h"},
                    "to": {"type": "string"},
                    "max_rows": {"type": "integer", "minimum": 1}
                }}),
            ),
            tool(
                "watch_panel",
                "Alert the human when a panel crosses a threshold. Evaluated locally by the pane; you never see the values.",
                json!({"type": "object", "required": ["panel_id", "threshold"], "properties": {
                    "panel_id": {"type": "integer"},
                    "reducer": {"type": "string", "enum": ["last", "max", "min", "mean", "sum", "count"]},
                    "op": {"type": "string", "enum": [">", ">=", "<", "<=", "==", "!="]},
                    "threshold": {"type": "number"},
                    "label": {"type": "string", "description": "Short notification text, e.g. 'DLQ not empty'."}
                }}),
            ),
            tool(
                "list_watches",
                "Current watch rules and which are breached.",
                empty.clone(),
            ),
            tool(
                "remove_watch",
                "Remove a watch rule.",
                json!({"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}}}),
            ),
            tool(
                "screenshot",
                "PNG of the browser pane, for layout checks. Refused when any panel uses a datasource that may hold personal data.",
                empty.clone(),
            ),
            tool(
                "open_for_pipeline",
                "Inspect an AWS CodePipeline (console URL) and apply a first dashboard of its log groups, queues, state machines and Lambdas. Opens a new dashboard tab when this session lacks CloudWatch for the pipeline's region.",
                json!({"type": "object", "required": ["url"], "properties": {"url": {"type": "string"}}}),
            ),
            tool(
                "promote",
                "Copy the current dashboard to the persistent Grafana configured by the human, remapping datasources by name.",
                json!({"type": "object", "properties": {"title": {"type": "string"}}}),
            ),
        ]
    }

    fn call(&mut self, name: &str, arguments: &Value) -> ToolOutput {
        match name {
            "list_datasources" => self.list_datasources(),
            "get_dashboard" => self.get_dashboard(),
            "apply_dashboard" => self.apply_dashboard(arguments),
            "panel_status" => self.panel_status(arguments),
            "panel_data_sample" => self.panel_data_sample(arguments),
            "probe_query" => self.probe_query(arguments),
            "watch_panel" => self.watch_panel(arguments),
            "list_watches" => self.list_watches(),
            "remove_watch" => self.remove_watch(arguments),
            "screenshot" => self.screenshot(),
            "open_for_pipeline" => self.open_for_pipeline(arguments),
            "promote" => self.promote(arguments),
            other => ToolOutput::Error(format!("unknown tool {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(personal_prom: bool) -> SessionRecord {
        serde_json::from_value(json!({
            "session_id": "s-test", "pane_id": null, "socket_hash": null, "container": "c",
            "port": 1, "dashboard_uid": "u", "runtime_dir": "/nonexistent", "refresh": "5s",
            "datasources": [
                {"uid": "dashr-testdata", "name": "TestData", "type": "grafana-testdata-datasource", "personal": false},
                {"uid": "prom", "name": "Prometheus", "type": "prometheus", "personal": personal_prom},
                {"uid": "loki", "name": "Loki", "type": "loki", "personal": true}
            ],
            "started_unix": 0
        }))
        .unwrap()
    }

    #[test]
    fn screenshots_only_for_non_personal_dashboards() {
        let safe = json!({"panels": [
            {"type": "text"},
            {"datasource": {"uid": "prom"}, "targets": [{"datasource": {"uid": "dashr-testdata"}}]},
            {"datasource": "-- Mixed --", "targets": [{"datasource": {"uid": "prom"}}]}
        ]});
        assert!(screenshot_allowed(&record(false), &safe).is_ok());
        assert!(
            screenshot_allowed(&record(true), &safe)
                .unwrap_err()
                .contains("Prometheus")
        );
        let loki = json!({"panels": [{"targets": [{"datasource": {"uid": "loki"}}]}]});
        assert!(screenshot_allowed(&record(false), &loki).is_err());
        let variable = json!({"panels": [{"datasource": {"uid": "${ds}"}}]});
        assert!(
            screenshot_allowed(&record(false), &variable)
                .unwrap_err()
                .contains("cannot be checked")
        );
    }

    #[test]
    fn every_defined_tool_is_dispatched_and_reports_a_missing_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = DashrTools::new(
            Config::default(),
            SessionStore::new(dir.path()),
            "absent",
            None,
        );
        for definition in tools.definitions() {
            let name = definition["name"].as_str().unwrap().to_owned();
            assert!(definition["inputSchema"]["type"] == "object", "{name}");
            let output = tools.call(&name, &json!({"panel_id": 1, "id": "w1", "url": "https://x", "dashboard": {}, "datasource_uid": "a", "query": {}, "threshold": 1}));
            match output {
                ToolOutput::Error(message) => {
                    assert!(!message.contains("unknown tool"), "{name}: {message}")
                }
                ToolOutput::Json(_) => {} // list_watches works without a session
                ToolOutput::Image { .. } => panic!("{name} produced an image without a session"),
            }
        }
    }

    #[test]
    fn watches_are_managed_through_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        store
            .save_watches(
                "s",
                &dashr_core::session::WatchState {
                    rules: vec![WatchRule {
                        id: "w1".into(),
                        panel_id: 1,
                        reducer: Reducer::Max,
                        op: Comparison::Gt,
                        threshold: 0.0,
                        label: None,
                    }],
                    breached: vec!["w1".into()],
                },
            )
            .unwrap();
        let mut tools = DashrTools::new(Config::default(), store.clone(), "s", None);
        assert!(matches!(
            tools.call("remove_watch", &json!({"id": "w9"})),
            ToolOutput::Error(_)
        ));
        assert!(matches!(
            tools.call("remove_watch", &json!({"id": "w1"})),
            ToolOutput::Json(_)
        ));
        let state = store.load_watches("s");
        assert!(state.rules.is_empty() && state.breached.is_empty());
    }
}

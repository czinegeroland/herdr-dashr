//! Arming, checking and clearing log expectations (DASHR-LOGX-001..004).
//!
//! Shared by the MCP tools and `dashr expect`. The dashboard section itself
//! is built by [`dashr_core::logx`]; this module stores the armed spec,
//! merges the section into the live dashboard, keeps one watch per
//! expectation and reads back counts. Only counts leave: never a line.

use std::time::{SystemTime, UNIX_EPOCH};

use dashr_core::logx::{self, Expectation, LogxError, LogxSpec, Outcome, Presence};
use dashr_core::masking::Masker;
use dashr_core::session::{SessionRecord, SessionStore};
use dashr_grafana::{Client, GrafanaError};
use serde::Serialize;
use serde_json::Value;

use crate::apply::{self, ApplyError, ApplyOutcome};
use crate::browser::Browser;
use crate::status;

#[derive(Debug, thiserror::Error)]
pub enum ExpectError {
    #[error("{0}")]
    Invalid(#[from] LogxError),
    #[error(
        "this session has no Loki datasource; open the OpenTelemetry dashboard (`[otel] enabled = true` or the \"Open live logs and traces\" action)"
    )]
    NoLoki,
    #[error("datasource {0} is not a Loki datasource of this session")]
    NotLoki(String),
    #[error("no log expectations are armed; arm some with expect_logs")]
    NotArmed,
    #[error("{0}")]
    Grafana(#[from] GrafanaError),
    #[error("{0}")]
    Apply(#[from] ApplyError),
    #[error("{0}")]
    Session(#[from] dashr_core::session::SessionError),
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// The Loki datasource to query: the one asked for, else `loki` (the
/// OpenTelemetry image's), else the only configured Loki.
pub fn loki_uid(record: &SessionRecord, asked: Option<&str>) -> Result<String, ExpectError> {
    let lokis: Vec<&str> = record
        .datasources
        .iter()
        .filter(|policy| policy.plugin_type == "loki")
        .map(|policy| policy.uid.as_str())
        .collect();
    match asked {
        Some(uid) if lokis.contains(&uid) => Ok(uid.to_owned()),
        Some(uid) => Err(ExpectError::NotLoki(uid.to_owned())),
        None if lokis.contains(&"loki") => Ok("loki".to_owned()),
        None => lokis
            .first()
            .map(|uid| (*uid).to_owned())
            .ok_or(ExpectError::NoLoki),
    }
}

/// What arming did.
#[derive(Debug, Serialize)]
pub struct Armed {
    pub expectations: usize,
    pub armed_at_ms: i64,
    pub selector: String,
    pub datasource_uid: String,
    pub dashboard: ApplyOutcome,
    pub notes: Vec<String>,
}

/// Shows `url` in the browser pane, falling back to a reload.
fn show(record: &SessionRecord, browser: Option<&Browser>, from: &str, notes: &mut Vec<String>) {
    let Some(browser) = browser.filter(|browser| browser.installed()) else {
        return;
    };
    let fragment = format!("127.0.0.1:{}/d/{}", record.port, record.dashboard_uid);
    let url = format!("{}&from={from}&to=now", record.kiosk_url());
    if let Err(error) = browser.navigate(&fragment, &url) {
        notes.push(format!("browser not updated: {error}"));
    }
}

/// Arms `expectations`, replacing any armed before. Counting starts now.
#[allow(clippy::too_many_arguments)]
pub fn arm(
    record: &SessionRecord,
    client: &Client,
    browser: Option<&Browser>,
    store: &SessionStore,
    time_from: &str,
    expectations: Vec<Expectation>,
    selector: Option<&str>,
    datasource: Option<&str>,
) -> Result<Armed, ExpectError> {
    let spec = LogxSpec {
        expectations,
        selector: selector
            .map(str::trim)
            .filter(|selector| !selector.is_empty())
            .unwrap_or(logx::DEFAULT_SELECTOR)
            .to_owned(),
        datasource_uid: loki_uid(record, datasource)?,
        armed_at_ms: now_ms(),
    };
    logx::validate(&spec)?;
    let current = client.dashboard(&record.dashboard_uid)?;
    let merged = logx::merge(&current, &spec);
    let outcome = apply::apply(
        record,
        client,
        None,
        time_from,
        &merged,
        "log expectations armed",
    )?;
    store.save_logx(&record.session_id, &spec)?;

    // One watch per expectation, replacing the previous set's. Breach state
    // is left to the pane's monitor, as for remove_watch: it must see a
    // breached watch go to return the pane to idle.
    let mut state = store.load_watches(&record.session_id);
    state
        .rules
        .retain(|rule| !rule.id.starts_with(logx::WATCH_PREFIX));
    state.rules.extend(logx::watch_rules(&spec));
    store.save_watches(&record.session_id, &state)?;

    let mut notes = outcome.warnings.clone();
    show(record, browser, &logx::window_start(&spec), &mut notes);
    Ok(Armed {
        expectations: spec.expectations.len(),
        armed_at_ms: spec.armed_at_ms,
        selector: spec.selector,
        datasource_uid: spec.datasource_uid,
        dashboard: outcome,
        notes,
    })
}

/// One expectation's state. Counts only.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ExpectationState {
    pub name: String,
    pub pattern: String,
    pub expect: Presence,
    /// Matching lines since arming; `None` when the count could not be read.
    pub count: Option<u64>,
    pub outcome: Option<Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Report {
    pub armed_at_ms: i64,
    pub selector: String,
    pub expectations: Vec<ExpectationState>,
    /// Every expected message seen and no forbidden one.
    pub passed: bool,
}

/// Reads each expectation's count from its tile's query.
pub fn check(
    record: &SessionRecord,
    client: &Client,
    store: &SessionStore,
    masker: &Masker,
) -> Result<Report, ExpectError> {
    let spec = store
        .load_logx(&record.session_id)
        .ok_or(ExpectError::NotArmed)?;
    let model = client.dashboard(&record.dashboard_uid)?;
    let mut states = Vec::new();
    for (index, expectation) in spec.expectations.iter().enumerate() {
        let panel = status::find_panel(&model, logx::ID_FIRST + index as i64);
        let (count, error) = match panel {
            None => (
                None,
                Some("its tile was removed from the dashboard".to_owned()),
            ),
            Some(panel) => {
                let results = status::panel_results(client, &model, panel);
                if let Some(error) = results.iter().find_map(|r| r.error.clone()) {
                    let error = masker.mask_text(&error);
                    (None, Some(error.chars().take(200).collect()))
                } else {
                    let total = results
                        .iter()
                        .flat_map(|result| &result.frames)
                        .flat_map(|frame| frame.numeric_series())
                        .filter_map(|series| series.last().copied())
                        .fold(0.0, |sum, value| sum + value);
                    (Some(total.max(0.0).round() as u64), None)
                }
            }
        };
        states.push(ExpectationState {
            name: expectation.name.clone(),
            pattern: expectation.pattern.clone(),
            expect: expectation.expect,
            count,
            outcome: count.map(|count| logx::outcome(expectation.expect, count)),
            error,
        });
    }
    let passed = states
        .iter()
        .all(|state| matches!(state.outcome, Some(Outcome::Seen | Outcome::Clear)));
    Ok(Report {
        armed_at_ms: spec.armed_at_ms,
        selector: spec.selector,
        expectations: states,
        passed,
    })
}

/// Removes the section, its watches and the stored spec. Returns whether
/// anything was armed.
pub fn clear(
    record: &SessionRecord,
    client: &Client,
    browser: Option<&Browser>,
    store: &SessionStore,
    time_from: &str,
) -> Result<bool, ExpectError> {
    let armed = store.load_logx(&record.session_id).is_some();
    let current = client.dashboard(&record.dashboard_uid)?;
    let mut cleared = logx::remove(&current);
    cleared["time"] = serde_json::json!({"from": time_from, "to": "now"});
    let mut notes = Vec::new();
    if cleared != current {
        apply::apply(
            record,
            client,
            None,
            time_from,
            &cleared,
            "log expectations cleared",
        )?;
        show(record, browser, time_from, &mut notes);
    }
    store.clear_logx(&record.session_id);
    // Breach state stays with the monitor (see `arm`).
    let mut state = store.load_watches(&record.session_id);
    state
        .rules
        .retain(|rule| !rule.id.starts_with(logx::WATCH_PREFIX));
    store.save_watches(&record.session_id, &state)?;
    Ok(armed)
}

/// Parses `NAME = PATTERN` (spaces around `=`) or a bare pattern, which
/// then names itself. A bare `=` stays in the pattern: `id=\d+`.
pub fn parse_expectation(text: &str, expect: Presence) -> Expectation {
    let (name, pattern) = match text.split_once(" = ") {
        Some((name, pattern)) if !name.trim().is_empty() && !pattern.trim().is_empty() => {
            (name.trim().to_owned(), pattern.trim().to_owned())
        }
        _ => (
            text.trim().chars().take(60).collect(),
            text.trim().to_owned(),
        ),
    };
    Expectation {
        name,
        pattern,
        expect,
    }
}

/// Whether a value is a JSON list of expectation objects.
pub fn expectations_from_json(value: &Value) -> Result<Vec<Expectation>, String> {
    serde_json::from_value(value.clone()).map_err(|error| {
        format!("expectations must be a list of {{name, pattern, expect: present|absent}}: {error}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashr_core::provisioning::DatasourcePolicy;

    fn record(types: &[(&str, &str)]) -> SessionRecord {
        SessionRecord {
            session_id: "s".into(),
            pane_id: None,
            socket_hash: None,
            container: "c".into(),
            port: 1,
            dashboard_uid: "u".into(),
            chat_pane: None,
            runtime_dir: "/tmp/x".into(),
            refresh: "2s".into(),
            datasources: types
                .iter()
                .map(|(uid, plugin_type)| DatasourcePolicy {
                    uid: (*uid).into(),
                    name: (*uid).into(),
                    plugin_type: (*plugin_type).into(),
                    personal: true,
                    allow_fields: vec![],
                })
                .collect(),
            pipeline: None,
            otlp: None,
            started_unix: 0,
        }
    }

    #[test]
    fn picks_the_loki_datasource() {
        let otel = record(&[
            ("dashr-testdata", "grafana-testdata-datasource"),
            ("app-logs", "loki"),
            ("loki", "loki"),
        ]);
        assert_eq!(loki_uid(&otel, None).unwrap(), "loki");
        assert_eq!(loki_uid(&otel, Some("app-logs")).unwrap(), "app-logs");
        assert!(matches!(
            loki_uid(&otel, Some("dashr-testdata")),
            Err(ExpectError::NotLoki(_))
        ));
        let configured = record(&[("app-logs", "loki")]);
        assert_eq!(loki_uid(&configured, None).unwrap(), "app-logs");
        let none = record(&[("dashr-testdata", "grafana-testdata-datasource")]);
        assert!(matches!(loki_uid(&none, None), Err(ExpectError::NoLoki)));
    }

    #[test]
    fn parses_named_and_bare_expectations() {
        let named = parse_expectation("order created = order \\d+ created", Presence::Present);
        assert_eq!(named.name, "order created");
        assert_eq!(named.pattern, "order \\d+ created");
        let bare = parse_expectation("payment captured", Presence::Absent);
        assert_eq!(bare.name, "payment captured");
        assert_eq!(bare.expect, Presence::Absent);
        // An `=` inside the pattern is not a name.
        let inner = parse_expectation("id=\\d+", Presence::Present);
        assert_eq!(inner.pattern, "id=\\d+");
    }
}

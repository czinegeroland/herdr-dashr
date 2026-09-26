//! Pushing a dashboard into the session's Grafana (DASHR-MCP-003).

use dashr_core::dashboard::{self, DashboardError, Pins};
use dashr_core::session::SessionRecord;
use dashr_grafana::{Client, GrafanaError};
use serde::Serialize;
use serde_json::Value;

use crate::browser::Browser;

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("{0}")]
    Invalid(#[from] DashboardError),
    #[error("{0}")]
    Grafana(#[from] GrafanaError),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApplyOutcome {
    pub uid: String,
    pub version: i64,
    pub panel_count: usize,
    pub warnings: Vec<String>,
    /// Whether the browser pane was reloaded; `false` in the text view.
    pub reloaded: bool,
}

/// Validates, pins and saves a dashboard, then reloads the browser pane.
pub fn apply(
    record: &SessionRecord,
    client: &Client,
    browser: Option<&Browser>,
    time_from: &str,
    input: &Value,
    message: &str,
) -> Result<ApplyOutcome, ApplyError> {
    let known = record.datasource_uids();
    let pins = Pins {
        uid: &record.dashboard_uid,
        refresh: &record.refresh,
        time_from,
        known_datasources: &known,
    };
    let normalized = dashboard::normalize(input, &pins)?;
    let saved = client.save_dashboard(&normalized.dashboard, None, true, message)?;
    let mut warnings = normalized.warnings;
    let reloaded = match browser {
        Some(browser) if browser.installed() => {
            let fragment = format!("127.0.0.1:{}/d/{}", record.port, record.dashboard_uid);
            match browser.reload(&fragment) {
                Ok(()) => true,
                Err(error) => {
                    warnings.push(format!("browser not reloaded: {error}"));
                    false
                }
            }
        }
        _ => false,
    };
    Ok(ApplyOutcome {
        uid: saved.uid,
        version: saved.version,
        panel_count: normalized.panel_count,
        warnings,
        reloaded,
    })
}

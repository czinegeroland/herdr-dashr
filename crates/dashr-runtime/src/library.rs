//! Saving a session's dashboard to the local library and loading one back
//! (DASHR-LIB-001..004). Shared by the MCP tools and `dashr dashboards`.

use dashr_core::library::{self, Library, LibraryError, Summary};
use dashr_core::session::{SessionRecord, SessionStore};
use dashr_grafana::{Client, GrafanaError};
use serde::Serialize;

use crate::apply::{self, ApplyError, ApplyOutcome};
use crate::browser::Browser;

#[derive(Debug, thiserror::Error)]
pub enum LibraryOpError {
    #[error("{0}")]
    Library(#[from] LibraryError),
    #[error("{0}")]
    Grafana(#[from] GrafanaError),
    #[error("{0}")]
    Apply(#[from] ApplyError),
    #[error(
        "{name:?} uses datasources this session does not have: {missing}. Open a session that has them (for example the OpenTelemetry dashboard, or the pipeline's) or add them to the configuration."
    )]
    MissingDatasources { name: String, missing: String },
}

#[derive(Debug, Serialize)]
pub struct SaveOutcome {
    pub saved: Summary,
    pub notes: Vec<String>,
}

/// Saves the session's current dashboard under `name`.
pub fn save(
    record: &SessionRecord,
    client: &Client,
    library: &Library,
    name: &str,
    overwrite: bool,
) -> Result<SaveOutcome, LibraryOpError> {
    let current = client.dashboard(&record.dashboard_uid)?;
    let (saved, notes) = library.save(
        name,
        &current,
        record.pipeline.as_ref().map(|p| format!("pipeline {p}")),
        overwrite,
        crate::session::now_unix(),
    )?;
    Ok(SaveOutcome { saved, notes })
}

#[derive(Debug, Serialize)]
pub struct LoadOutcome {
    pub name: String,
    pub dashboard: ApplyOutcome,
}

/// Replaces the session's dashboard with a saved one. Refuses, changing
/// nothing, when the session lacks a datasource the dashboard uses.
pub fn load(
    record: &SessionRecord,
    client: &Client,
    browser: Option<&Browser>,
    store: &SessionStore,
    library: &Library,
    name: &str,
    time_from: &str,
) -> Result<LoadOutcome, LibraryOpError> {
    let saved = library.load(name)?;
    let missing = library::missing_datasources(&saved, &record.datasource_uids());
    if !missing.is_empty() {
        return Err(LibraryOpError::MissingDatasources {
            name: saved.name,
            missing: missing.join(", "),
        });
    }
    let outcome = apply::apply(
        record,
        client,
        browser,
        time_from,
        &saved.dashboard,
        &format!("loaded saved dashboard {}", saved.name),
    )?;
    // Expectations armed on the replaced dashboard no longer have tiles.
    // Breach state stays with the monitor (DEC-032).
    store.clear_logx(&record.session_id);
    let mut watches = store.load_watches(&record.session_id);
    let before = watches.rules.len();
    watches
        .rules
        .retain(|rule| !rule.id.starts_with(dashr_core::logx::WATCH_PREFIX));
    if watches.rules.len() != before {
        let _ = store.save_watches(&record.session_id, &watches);
    }
    Ok(LoadOutcome {
        name: saved.name,
        dashboard: outcome,
    })
}

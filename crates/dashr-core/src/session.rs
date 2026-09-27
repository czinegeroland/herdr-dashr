//! The session record: how the MCP server, the hooks and the pane find each
//! other.
//!
//! One JSON file per dashboard pane under
//! `<state dir>/sessions/`. It holds identifiers and a loopback
//! port, never a secret or a data value (requirement DASHR-GRAF-008), and it
//! is deleted when the pane stops. Watches live in a sibling file because
//! two processes write them: the MCP server adds rules and the pane records
//! breach state.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::provisioning::DatasourcePolicy;
use crate::watch::WatchRule;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionRecord {
    pub session_id: String,
    /// The Herdr pane that owns the Grafana, if it runs under Herdr.
    pub pane_id: Option<String>,
    pub socket_hash: Option<String>,
    pub container: String,
    pub port: u16,
    pub dashboard_uid: String,
    /// The chat pane split off below the dashboard.
    #[serde(default)]
    pub chat_pane: Option<String>,
    /// Directory on tmpfs holding provisioning, the MCP config and the
    /// browser profile.
    pub runtime_dir: PathBuf,
    pub refresh: String,
    pub datasources: Vec<DatasourcePolicy>,
    /// The CodePipeline this session was opened for.
    #[serde(default)]
    pub pipeline: Option<String>,
    /// Loopback OTLP ports, when the session runs the OpenTelemetry image.
    #[serde(default)]
    pub otlp: Option<Otlp>,
    pub started_unix: u64,
}

/// Where a session receives OpenTelemetry data.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Otlp {
    pub grpc_port: u16,
    pub http_port: u16,
}

impl Otlp {
    /// The value for `OTEL_EXPORTER_OTLP_ENDPOINT` (OTLP/HTTP).
    pub fn http_endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.http_port)
    }

    pub fn grpc_endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.grpc_port)
    }
}

impl SessionRecord {
    pub fn grafana_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The link the dashboard pane shows, to open in the human's own
    /// browser. Short enough to fit the narrow pane on one line; the
    /// dashboard itself carries its refresh interval (DEC-040).
    pub fn dashboard_url(&self) -> String {
        format!("{}/d/{}", self.grafana_url(), self.dashboard_uid)
    }

    /// The URL the browser pane shows: kiosk mode hides Grafana's chrome,
    /// `refresh` keeps data live with no agent involved (DASHR-VIEW-001).
    pub fn kiosk_url(&self) -> String {
        format!(
            "{}/d/{}?orgId=1&kiosk&refresh={}",
            self.grafana_url(),
            self.dashboard_uid,
            self.refresh
        )
    }

    pub fn policy(&self, uid: &str) -> Option<&DatasourcePolicy> {
        self.datasources.iter().find(|policy| policy.uid == uid)
    }

    pub fn datasource_uids(&self) -> Vec<String> {
        self.datasources
            .iter()
            .map(|policy| policy.uid.clone())
            .collect()
    }
}

/// Watch rules plus which are currently breached.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WatchState {
    pub rules: Vec<WatchRule>,
    #[serde(default)]
    pub breached: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("no session {0}; is its dashboard pane still open?")]
    NotFound(String),
    #[error("session file {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("session file {path} is corrupt: {source}")]
    Corrupt {
        path: String,
        source: serde_json::Error,
    },
}

/// Where session files live.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("sessions"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn record_path(&self, session_id: &str) -> PathBuf {
        self.dir
            .join(format!("{}.json", crate::ids::sanitize(session_id)))
    }

    fn watch_path(&self, session_id: &str) -> PathBuf {
        self.dir
            .join(format!("{}.watches.json", crate::ids::sanitize(session_id)))
    }

    fn logx_path(&self, session_id: &str) -> PathBuf {
        self.dir
            .join(format!("{}.logx.json", crate::ids::sanitize(session_id)))
    }

    fn io(path: &Path) -> impl FnOnce(std::io::Error) -> SessionError + '_ {
        move |source| SessionError::Io {
            path: path.display().to_string(),
            source,
        }
    }

    /// Writes atomically: a reader never sees half a file.
    fn write_json<T: Serialize>(&self, path: &Path, value: &T) -> Result<(), SessionError> {
        std::fs::create_dir_all(&self.dir).map_err(Self::io(&self.dir))?;
        let temporary = path.with_extension("tmp");
        let text = serde_json::to_vec_pretty(value).map_err(|source| SessionError::Corrupt {
            path: path.display().to_string(),
            source,
        })?;
        std::fs::write(&temporary, text).map_err(Self::io(&temporary))?;
        std::fs::rename(&temporary, path).map_err(Self::io(path))
    }

    pub fn save(&self, record: &SessionRecord) -> Result<(), SessionError> {
        self.write_json(&self.record_path(&record.session_id), record)
    }

    pub fn load(&self, session_id: &str) -> Result<SessionRecord, SessionError> {
        let path = self.record_path(session_id);
        let text = match std::fs::read(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(SessionError::NotFound(session_id.to_owned()));
            }
            Err(source) => return Err(Self::io(&path)(source)),
        };
        serde_json::from_slice(&text).map_err(|source| SessionError::Corrupt {
            path: path.display().to_string(),
            source,
        })
    }

    /// Every readable session record. Corrupt files are skipped.
    pub fn list(&self) -> Vec<SessionRecord> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut records: Vec<SessionRecord> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "json")
                    && !path.to_string_lossy().ends_with(".watches.json")
                    && !path.to_string_lossy().ends_with(".logx.json")
            })
            .filter_map(|path| std::fs::read(path).ok())
            .filter_map(|text| serde_json::from_slice(&text).ok())
            .collect();
        records.sort_by(|a: &SessionRecord, b| a.session_id.cmp(&b.session_id));
        records
    }

    /// The session owned by a pane, when one exists.
    pub fn find_by_pane(&self, socket_hash: &str, pane_id: &str) -> Option<SessionRecord> {
        self.list().into_iter().find(|record| {
            record.pane_id.as_deref() == Some(pane_id)
                && record.socket_hash.as_deref() == Some(socket_hash)
        })
    }

    pub fn remove(&self, session_id: &str) {
        let _ = std::fs::remove_file(self.record_path(session_id));
        let _ = std::fs::remove_file(self.watch_path(session_id));
        let _ = std::fs::remove_file(self.logx_path(session_id));
    }

    /// The armed log expectations, when there are any.
    pub fn load_logx(&self, session_id: &str) -> Option<crate::logx::LogxSpec> {
        std::fs::read(self.logx_path(session_id))
            .ok()
            .and_then(|text| serde_json::from_slice(&text).ok())
    }

    pub fn save_logx(
        &self,
        session_id: &str,
        spec: &crate::logx::LogxSpec,
    ) -> Result<(), SessionError> {
        self.write_json(&self.logx_path(session_id), spec)
    }

    pub fn clear_logx(&self, session_id: &str) {
        let _ = std::fs::remove_file(self.logx_path(session_id));
    }

    pub fn load_watches(&self, session_id: &str) -> WatchState {
        std::fs::read(self.watch_path(session_id))
            .ok()
            .and_then(|text| serde_json::from_slice(&text).ok())
            .unwrap_or_default()
    }

    pub fn save_watches(&self, session_id: &str, state: &WatchState) -> Result<(), SessionError> {
        self.write_json(&self.watch_path(session_id), state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::{Comparison, Reducer};

    fn record(id: &str) -> SessionRecord {
        SessionRecord {
            session_id: id.into(),
            pane_id: Some("w1:p1".into()),
            socket_hash: Some("abcd1234".into()),
            container: format!("herdr-grafana-{id}"),
            port: 32768,
            dashboard_uid: format!("dashr-{id}"),
            chat_pane: None,
            runtime_dir: "/dev/shm/x".into(),
            refresh: "5s".into(),
            datasources: vec![crate::provisioning::DatasourcePolicy {
                uid: "dashr-testdata".into(),
                name: "TestData".into(),
                plugin_type: "grafana-testdata-datasource".into(),
                personal: false,
                allow_fields: vec![],
            }],
            pipeline: None,
            otlp: None,
            started_unix: 1,
        }
    }

    #[test]
    fn round_trips_lists_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        assert!(matches!(store.load("s1"), Err(SessionError::NotFound(_))));
        store.save(&record("s1")).unwrap();
        store.save(&record("s2")).unwrap();
        store
            .save_watches(
                "s1",
                &WatchState {
                    rules: vec![WatchRule {
                        id: "w".into(),
                        panel_id: 1,
                        reducer: Reducer::Max,
                        op: Comparison::Gt,
                        threshold: 0.0,
                        label: None,
                        severity: Default::default(),
                    }],
                    breached: vec![],
                },
            )
            .unwrap();
        assert_eq!(store.load("s1").unwrap(), record("s1"));
        assert_eq!(store.list().len(), 2, "watch files are not sessions");
        assert_eq!(store.load_watches("s1").rules.len(), 1);
        assert!(store.find_by_pane("abcd1234", "w1:p1").is_some());
        assert!(store.find_by_pane("ffffffff", "w1:p1").is_none());
        store.remove("s1");
        assert!(store.load("s1").is_err());
        assert!(store.load_watches("s1").rules.is_empty());
    }

    #[test]
    fn urls() {
        let record = record("s1");
        assert_eq!(record.grafana_url(), "http://127.0.0.1:32768");
        assert_eq!(
            record.kiosk_url(),
            "http://127.0.0.1:32768/d/dashr-s1?orgId=1&kiosk&refresh=5s"
        );
        assert!(record.policy("dashr-testdata").is_some());
    }

    #[test]
    fn records_hold_no_secret_shaped_fields() {
        let text = serde_json::to_string(&record("s1")).unwrap();
        for forbidden in ["password", "token", "secret", "url\":\"http"] {
            assert!(!text.contains(forbidden), "{forbidden} in {text}");
        }
    }
}

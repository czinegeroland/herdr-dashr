//! Running sessions, as records in the state directory (DASHR-SESSION-002).
//!
//! A session is one pane (or one `dashr serve`) with its Jaeger container
//! and its API. The record tells `dashr` commands — run by the human's AI
//! session from any pane — where the API is and the token it needs. It is
//! written owner-only and removed when the session ends.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::docker::Ports;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    /// The Herdr pane, when the session runs in one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    pub pid: u32,
    pub api_port: u16,
    /// Sent by `dashr` commands; the viewer has a token of its own.
    pub agent_token: String,
    pub container: String,
    pub ports: Ports,
    /// Where Jaeger's ports are published.
    pub bind: String,
    pub started_ms: u64,
}

impl SessionRecord {
    pub fn api_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.api_port)
    }

    fn host(&self) -> &str {
        if self.bind == "0.0.0.0" || self.bind == "::" { "127.0.0.1" } else { &self.bind }
    }

    pub fn otlp_http(&self) -> String {
        format!("http://{}:{}", self.host(), self.ports.otlp_http)
    }

    pub fn otlp_grpc(&self) -> String {
        format!("http://{}:{}", self.host(), self.ports.otlp_grpc)
    }

    pub fn jaeger_ui(&self) -> String {
        format!("http://127.0.0.1:{}", self.ports.ui)
    }
}

pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    pub fn new(state_dir: &Path) -> Self {
        Self { dir: state_dir.join("sessions") }
    }

    fn path(&self, session_id: &str) -> PathBuf {
        let safe: String = session_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect();
        self.dir.join(format!("{safe}.json"))
    }

    pub fn save(&self, record: &SessionRecord) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path(&record.session_id);
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(record).unwrap_or_default())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(temporary, path)
    }

    pub fn remove(&self, session_id: &str) {
        let _ = std::fs::remove_file(self.path(session_id));
    }

    /// Every record, newest first.
    pub fn list(&self) -> Vec<SessionRecord> {
        let mut out: Vec<SessionRecord> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
            .filter_map(|entry| serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok())
            .collect();
        out.sort_by(|a: &SessionRecord, b| b.started_ms.cmp(&a.started_ms));
        out
    }

    /// The session `selector` names (its id or its pane id); without one,
    /// the only session `alive` accepts, else the newest.
    pub fn find(&self, selector: Option<&str>, alive: impl Fn(&SessionRecord) -> bool) -> Result<SessionRecord, String> {
        let records = self.list();
        if let Some(selector) = selector.filter(|s| !s.is_empty()) {
            return records
                .into_iter()
                .find(|r| r.session_id == selector || r.pane_id.as_deref() == Some(selector))
                .ok_or_else(|| format!("no dashr session {selector:?}; `dashr sessions` lists them"));
        }
        let live: Vec<SessionRecord> = records.into_iter().filter(|r| alive(r)).collect();
        live.into_iter()
            .next()
            .ok_or_else(|| "no dashr session is running: open the trace pane first (see `dashr --help`)".to_owned())
    }
}

/// `n` random bytes as hex, from the operating system where it offers them.
pub fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut bytes))
        .is_ok();
    if !filled {
        // Windows: no /dev/urandom. Mix the clock, the process and an
        // address; tokens only guard loopback ports against other local
        // users, and are never reused across sessions.
        let seed = format!(
            "{:?}{}{:p}",
            std::time::SystemTime::now(),
            std::process::id(),
            &bytes
        );
        let mut state = dashr_core::model::fnv1a64(seed.as_bytes());
        for byte in &mut bytes {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
    }
    dashr_core::model::hex(&bytes)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, pane: Option<&str>, started: u64) -> SessionRecord {
        SessionRecord {
            session_id: id.into(),
            pane_id: pane.map(str::to_owned),
            pid: 1,
            api_port: 1234,
            agent_token: "t".into(),
            container: format!("dashr-{id}"),
            ports: Ports { otlp_grpc: 4317, otlp_http: 4318, ui: 16686 },
            bind: "127.0.0.1".into(),
            started_ms: started,
        }
    }

    #[test]
    fn records_are_found_by_id_pane_or_liveness() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path());
        registry.save(&record("a", Some("w1:p2"), 1)).unwrap();
        registry.save(&record("b", None, 2)).unwrap();
        assert_eq!(registry.find(Some("w1:p2"), |_| true).unwrap().session_id, "a");
        assert_eq!(registry.find(None, |_| true).unwrap().session_id, "b", "newest first");
        assert_eq!(registry.find(None, |r| r.session_id == "a").unwrap().session_id, "a");
        assert!(registry.find(None, |_| false).unwrap_err().contains("no dashr session"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("sessions/a.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        registry.remove("a");
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn tokens_are_random_hex() {
        let (a, b) = (random_hex(16), random_hex(16));
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }

    #[test]
    fn endpoints() {
        let mut r = record("a", None, 1);
        assert_eq!(r.otlp_http(), "http://127.0.0.1:4318");
        r.bind = "0.0.0.0".into();
        assert_eq!(r.otlp_grpc(), "http://127.0.0.1:4317");
    }
}

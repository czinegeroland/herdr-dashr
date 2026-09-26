//! Saved dashboards: name the current dashboard, keep it on this machine and
//! load it into any later session (requirements DASHR-LIB-001..004).
//!
//! A saved dashboard is the dashboard JSON the agent (or the pipeline
//! bootstrap) wrote: panels, queries and layout, never a data value. What
//! belongs to one session is dropped before saving: the log-expectation
//! section, whose counts start at the moment it was armed, and the
//! session-pinned uid, version and id.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Longest dashboard name.
pub const MAX_NAME_CHARS: usize = 60;

#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error(
        "a saved dashboard name is 1-{MAX_NAME_CHARS} characters of letters, digits, spaces, '-', '_' or '.': {0:?}"
    )]
    Name(String),
    #[error(
        "a dashboard named {0:?} is already saved; save with overwrite (`--force` on the command line) to replace it"
    )]
    Exists(String),
    #[error("no saved dashboard named {0:?}; saved: {1}")]
    NotFound(String, String),
    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("{path} is not a saved dashboard: {source}")]
    Corrupt {
        path: String,
        source: serde_json::Error,
    },
}

/// One saved dashboard as it is stored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Saved {
    pub name: String,
    /// Epoch seconds.
    pub saved_unix: u64,
    /// Where it came from, e.g. "pipeline checkout (eu-west-1)"; informative.
    #[serde(default)]
    pub origin: Option<String>,
    /// Datasource uids its panels use, so a load can say what is missing.
    #[serde(default)]
    pub datasources: Vec<String>,
    pub dashboard: Value,
}

/// A listing entry: everything but the dashboard.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Summary {
    pub name: String,
    pub saved_unix: u64,
    pub origin: Option<String>,
    pub title: String,
    pub panels: usize,
    pub datasources: Vec<String>,
}

/// Checks a name and returns it trimmed.
pub fn validate_name(name: &str) -> Result<String, LibraryError> {
    let trimmed = name.trim();
    let ok = !trimmed.is_empty()
        && trimmed.chars().count() <= MAX_NAME_CHARS
        && trimmed
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        && !trimmed.starts_with('.');
    if ok {
        Ok(trimmed.to_owned())
    } else {
        Err(LibraryError::Name(name.to_owned()))
    }
}

/// The file stem for a name: lowercase, so names differing only in case are
/// the same dashboard on every file system.
pub fn file_stem(name: &str) -> String {
    crate::ids::sanitize(&name.trim().to_lowercase())
}

/// Every datasource uid the dashboard's panels and targets reference.
pub fn datasource_uids(dashboard: &Value) -> Vec<String> {
    let mut uids = Vec::new();
    for panel in crate::dashboard::flat_panels(dashboard) {
        let mut add = |value: Option<&Value>| {
            if let Some(uid) = value.and_then(|ds| ds.get("uid")).and_then(Value::as_str)
                && !uid.starts_with('$')
                && !uid.starts_with("-- ")
                && !uids.iter().any(|known| known == uid)
            {
                uids.push(uid.to_owned());
            }
        };
        add(panel.get("datasource"));
        for target in panel
            .get("targets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            add(target.get("datasource"));
        }
    }
    uids
}

/// The dashboard as it should be saved, and notes on what was left out.
pub fn prepare(dashboard: &Value) -> (Value, Vec<String>) {
    let mut notes = Vec::new();
    let had_section = crate::dashboard::flat_panels(dashboard)
        .iter()
        .any(|panel| {
            panel
                .get("id")
                .and_then(Value::as_i64)
                .is_some_and(|id| (crate::logx::ID_FIRST..=crate::logx::ID_LAST).contains(&id))
        });
    let mut out = if had_section {
        notes.push(
            "the log-expectation section was not saved: it belongs to this session; arm expectations again after loading"
                .to_owned(),
        );
        let mut removed = crate::logx::remove(dashboard);
        // Its time range started when the expectations were armed.
        if let Some(object) = removed.as_object_mut() {
            object.remove("time");
        }
        removed
    } else {
        dashboard.clone()
    };
    if let Some(object) = out.as_object_mut() {
        for key in ["uid", "id", "version"] {
            object.remove(key);
        }
    }
    (out, notes)
}

/// The dashboards saved on this machine.
#[derive(Debug, Clone)]
pub struct Library {
    dir: PathBuf,
}

impl Library {
    /// The library under the plugin state directory.
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("dashboards"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{}.json", file_stem(name)))
    }

    fn io(path: &Path) -> impl FnOnce(std::io::Error) -> LibraryError + '_ {
        move |source| LibraryError::Io {
            path: path.display().to_string(),
            source,
        }
    }

    /// Saves a dashboard under a name. Refuses to replace an existing one
    /// unless `overwrite`. Returns the entry and notes on what was left out.
    pub fn save(
        &self,
        name: &str,
        dashboard: &Value,
        origin: Option<String>,
        overwrite: bool,
        now_unix: u64,
    ) -> Result<(Summary, Vec<String>), LibraryError> {
        let name = validate_name(name)?;
        let path = self.path(&name);
        if path.exists() && !overwrite {
            let existing = self.load(&name).map(|saved| saved.name).unwrap_or(name);
            return Err(LibraryError::Exists(existing));
        }
        let (dashboard, notes) = prepare(dashboard);
        let saved = Saved {
            name,
            saved_unix: now_unix,
            origin,
            datasources: datasource_uids(&dashboard),
            dashboard,
        };
        std::fs::create_dir_all(&self.dir).map_err(Self::io(&self.dir))?;
        let temporary = path.with_extension("tmp");
        let text = serde_json::to_vec_pretty(&saved).map_err(|source| LibraryError::Corrupt {
            path: path.display().to_string(),
            source,
        })?;
        std::fs::write(&temporary, text).map_err(Self::io(&temporary))?;
        std::fs::rename(&temporary, &path).map_err(Self::io(&path))?;
        Ok((summary(&saved), notes))
    }

    pub fn load(&self, name: &str) -> Result<Saved, LibraryError> {
        let path = self.path(name);
        let text = match std::fs::read(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let names: Vec<String> = self.list().into_iter().map(|s| s.name).collect();
                return Err(LibraryError::NotFound(
                    name.trim().to_owned(),
                    if names.is_empty() {
                        "none".to_owned()
                    } else {
                        names.join(", ")
                    },
                ));
            }
            Err(source) => return Err(Self::io(&path)(source)),
        };
        serde_json::from_slice(&text).map_err(|source| LibraryError::Corrupt {
            path: path.display().to_string(),
            source,
        })
    }

    /// Every readable saved dashboard, newest first.
    pub fn list(&self) -> Vec<Summary> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut list: Vec<Summary> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .filter_map(|path| std::fs::read(path).ok())
            .filter_map(|text| serde_json::from_slice::<Saved>(&text).ok())
            .map(|saved| summary(&saved))
            .collect();
        list.sort_by(|a, b| {
            b.saved_unix
                .cmp(&a.saved_unix)
                .then_with(|| a.name.cmp(&b.name))
        });
        list
    }

    /// Deletes a saved dashboard; `Ok(false)` when there was none.
    pub fn delete(&self, name: &str) -> Result<bool, LibraryError> {
        let path = self.path(name);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(Self::io(&path)(source)),
        }
    }
}

fn summary(saved: &Saved) -> Summary {
    Summary {
        name: saved.name.clone(),
        saved_unix: saved.saved_unix,
        origin: saved.origin.clone(),
        title: saved
            .dashboard
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        panels: crate::dashboard::flat_panels(&saved.dashboard)
            .iter()
            .filter(|panel| panel.get("type").and_then(Value::as_str) != Some("row"))
            .count(),
        datasources: saved.datasources.clone(),
    }
}

/// Uids a saved dashboard needs that `available` lacks.
pub fn missing_datasources(saved: &Saved, available: &[String]) -> Vec<String> {
    saved
        .datasources
        .iter()
        .filter(|uid| !available.contains(uid))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dashboard() -> Value {
        json!({
            "uid": "dashr-abc-w1-p1", "id": 7, "version": 3, "title": "Checkout",
            "time": {"from": "now-1h", "to": "now"},
            "panels": [
                {"id": 1, "type": "timeseries", "title": "Errors", "datasource": {"uid": "loki"},
                 "targets": [{"refId": "A", "datasource": {"uid": "loki"}}]},
                {"id": 2, "type": "row", "title": "More", "panels": [
                    {"id": 3, "type": "stat", "title": "DLQ", "datasource": {"uid": "dashr-cloudwatch-eu-west-1"}}]},
                {"id": 4, "type": "text", "title": "Note"},
                {"id": 5, "type": "stat", "title": "Var", "datasource": {"uid": "${ds}"}}
            ]
        })
    }

    #[test]
    fn names_are_checked_and_case_insensitive() {
        assert_eq!(
            validate_name("  checkout debug ").unwrap(),
            "checkout debug"
        );
        for bad in ["", "   ", "a/b", "../x", ".hidden", &"x".repeat(61)] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        assert_eq!(file_stem("Checkout Debug"), file_stem("checkout debug"));
        assert_eq!(file_stem("Checkout Debug"), "checkout-debug");
    }

    #[test]
    fn saving_drops_what_belongs_to_the_session() {
        let spec = crate::logx::LogxSpec {
            expectations: vec![crate::logx::Expectation {
                name: "order".into(),
                pattern: "order".into(),
                expect: crate::logx::Presence::Present,
            }],
            selector: crate::logx::DEFAULT_SELECTOR.into(),
            datasource_uid: "loki".into(),
            armed_at_ms: 1_790_000_000_000,
        };
        let armed = crate::logx::merge(&dashboard(), &spec);
        let (saved, notes) = prepare(&armed);
        assert!(
            saved.get("uid").is_none()
                && saved.get("id").is_none()
                && saved.get("version").is_none()
        );
        assert!(
            saved.get("time").is_none(),
            "the armed time range is not kept"
        );
        let ids: Vec<i64> = saved["panels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![1, 2, 4, 5]);
        assert_eq!(
            saved["panels"][0]["gridPos"]["y"],
            json!(null),
            "layout restored as it was"
        );
        assert_eq!(notes.len(), 1);
        let (plain, notes) = prepare(&dashboard());
        assert!(notes.is_empty());
        assert_eq!(
            plain["time"]["from"], "now-1h",
            "an ordinary time range is kept"
        );
    }

    #[test]
    fn datasource_uids_skip_variables() {
        assert_eq!(
            datasource_uids(&dashboard()),
            vec!["loki", "dashr-cloudwatch-eu-west-1"]
        );
    }

    #[test]
    fn save_list_load_delete() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::new(dir.path());
        assert!(library.list().is_empty());
        let (summary, _) = library
            .save(
                "Checkout debug",
                &dashboard(),
                Some("pipeline checkout".into()),
                false,
                100,
            )
            .unwrap();
        assert_eq!(summary.panels, 4, "rows are not counted");
        assert_eq!(summary.title, "Checkout");
        assert!(matches!(
            library.save("checkout DEBUG", &dashboard(), None, false, 101),
            Err(LibraryError::Exists(name)) if name == "Checkout debug"
        ));
        library
            .save("checkout DEBUG", &dashboard(), None, true, 102)
            .unwrap();
        library
            .save(
                "other",
                &json!({"title": "O", "panels": []}),
                None,
                false,
                103,
            )
            .unwrap();
        let names: Vec<String> = library.list().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["other", "checkout DEBUG"], "newest first");
        let loaded = library.load("Checkout Debug").unwrap();
        assert_eq!(loaded.dashboard["title"], "Checkout");
        assert_eq!(
            missing_datasources(&loaded, &["loki".into()]),
            vec!["dashr-cloudwatch-eu-west-1"]
        );
        match library.load("nope") {
            Err(LibraryError::NotFound(name, saved)) => {
                assert_eq!(name, "nope");
                assert!(saved.contains("other"));
            }
            other => panic!("{other:?}"),
        }
        assert!(library.delete("OTHER").unwrap());
        assert!(!library.delete("other").unwrap());
        assert_eq!(library.list().len(), 1);
    }
}

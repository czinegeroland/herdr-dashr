//! Promoting a session dashboard to a persistent Grafana (DASHR-PROMO-*).
//!
//! Datasource uids differ between the disposable Grafana and the real one,
//! so references are remapped by datasource *name*: a promoted dashboard
//! works only where datasources of the same names exist, and the error says
//! which are missing rather than saving a dashboard of empty panels.

use std::collections::BTreeMap;

use dashr_core::config::PromoteConfig;
use dashr_core::session::SessionRecord;
use dashr_grafana::{Client, GrafanaError};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum PromoteError {
    #[error(
        "promotion is not configured; add a [promote] section with url and token_env to dashr.toml"
    )]
    NotConfigured,
    #[error("{0} is not set; export a Grafana service-account token in it")]
    NoToken(String),
    #[error("the target Grafana has no datasource named {0}")]
    MissingDatasources(String),
    #[error("{0}")]
    Grafana(#[from] GrafanaError),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Promoted {
    pub uid: String,
    pub url: String,
    pub folder: String,
}

/// Rewrites every `datasource` reference whose uid is in `map`.
pub fn remap_datasources(value: &mut Value, map: &BTreeMap<String, (String, String)>) {
    match value {
        Value::Object(object) => {
            if let Some(datasource) = object.get_mut("datasource") {
                let uid = match datasource {
                    Value::String(uid) => Some(uid.clone()),
                    Value::Object(inner) => {
                        inner.get("uid").and_then(Value::as_str).map(str::to_owned)
                    }
                    _ => None,
                };
                if let Some((new_uid, new_type)) = uid.and_then(|uid| map.get(&uid)) {
                    *datasource = serde_json::json!({"uid": new_uid, "type": new_type});
                }
            }
            for (key, child) in object.iter_mut() {
                if key != "datasource" {
                    remap_datasources(child, map);
                }
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| remap_datasources(item, map)),
        _ => {}
    }
}

/// Datasource uids referenced anywhere in a dashboard.
pub fn referenced_uids(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            if let Some(datasource) = object.get("datasource") {
                let uid = match datasource {
                    Value::String(uid) => Some(uid.clone()),
                    Value::Object(inner) => {
                        inner.get("uid").and_then(Value::as_str).map(str::to_owned)
                    }
                    _ => None,
                };
                if let Some(uid) = uid
                    && !out.contains(&uid)
                {
                    out.push(uid);
                }
            }
            object
                .values()
                .for_each(|child| referenced_uids(child, out));
        }
        Value::Array(items) => items.iter().for_each(|item| referenced_uids(item, out)),
        _ => {}
    }
}

/// Promotes the session's current dashboard.
pub fn promote(
    record: &SessionRecord,
    local: &Client,
    config: Option<&PromoteConfig>,
    title: Option<&str>,
) -> Result<Promoted, PromoteError> {
    let config = config.ok_or(PromoteError::NotConfigured)?;
    let token = std::env::var(&config.token_env)
        .ok()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| PromoteError::NoToken(config.token_env.clone()))?;
    let target = Client::with_token(&config.url, token);

    let mut dashboard = local.dashboard(&record.dashboard_uid)?;
    let mut used = Vec::new();
    referenced_uids(&dashboard, &mut used);

    let mut map = BTreeMap::new();
    let mut missing = Vec::new();
    for uid in used {
        let Some(policy) = record.policy(&uid) else {
            continue; // special or variable datasources pass through
        };
        match target.datasource_by_name(&policy.name) {
            Ok(found) => {
                map.insert(uid, (found.uid, found.plugin_type));
            }
            Err(GrafanaError::Status { status: 404, .. }) => missing.push(policy.name.clone()),
            Err(error) => return Err(error.into()),
        }
    }
    if !missing.is_empty() {
        return Err(PromoteError::MissingDatasources(missing.join(", ")));
    }
    remap_datasources(&mut dashboard, &map);

    if let Some(object) = dashboard.as_object_mut() {
        object.remove("uid");
        object.remove("id");
        object.remove("version");
        if let Some(title) = title.filter(|t| !t.trim().is_empty()) {
            object.insert("title".into(), Value::String(title.trim().to_owned()));
        }
    }
    let folder_uid = target.ensure_folder(&config.folder)?;
    let saved = target.save_dashboard(
        &dashboard,
        Some(&folder_uid),
        false,
        "promoted from herdr-dashr",
    )?;
    Ok(Promoted {
        url: format!("{}{}", target.base(), saved.url),
        uid: saved.uid,
        folder: config.folder.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn remaps_every_reference_and_leaves_others() {
        let mut dashboard = json!({"panels": [
            {"datasource": {"uid": "loki-local", "type": "loki"}, "targets": [
                {"datasource": {"uid": "loki-local"}}, {"datasource": "-- Mixed --"}
            ]},
            {"type": "row", "panels": [{"datasource": "prom-local"}]}
        ]});
        let mut map = BTreeMap::new();
        map.insert(
            "loki-local".to_owned(),
            ("LOKI1".to_owned(), "loki".to_owned()),
        );
        map.insert(
            "prom-local".to_owned(),
            ("PROM1".to_owned(), "prometheus".to_owned()),
        );
        let mut before = Vec::new();
        referenced_uids(&dashboard, &mut before);
        assert_eq!(before, vec!["loki-local", "-- Mixed --", "prom-local"]);
        remap_datasources(&mut dashboard, &map);
        assert_eq!(dashboard["panels"][0]["datasource"]["uid"], "LOKI1");
        assert_eq!(
            dashboard["panels"][0]["targets"][0]["datasource"]["uid"],
            "LOKI1"
        );
        assert_eq!(
            dashboard["panels"][0]["targets"][1]["datasource"],
            "-- Mixed --"
        );
        assert_eq!(
            dashboard["panels"][1]["panels"][0]["datasource"]["type"],
            "prometheus"
        );
    }

    #[test]
    fn unconfigured_or_tokenless_promotion_is_refused_before_any_request() {
        let record: SessionRecord = serde_json::from_value(json!({
            "session_id": "s", "pane_id": null, "socket_hash": null, "container": "c",
            "port": 1, "dashboard_uid": "u", "runtime_dir": "/x", "refresh": "5s",
            "datasources": [], "started_unix": 0
        }))
        .unwrap();
        let local = Client::local("http://127.0.0.1:1");
        assert!(matches!(
            promote(&record, &local, None, None),
            Err(PromoteError::NotConfigured)
        ));
        let config = PromoteConfig {
            url: "https://g".into(),
            token_env: "DASHR_TEST_SURELY_UNSET_TOKEN".into(),
            folder: "dashr".into(),
        };
        assert!(matches!(
            promote(&record, &local, Some(&config), None),
            Err(PromoteError::NoToken(_))
        ));
    }
}

//! The environment Herdr injects into plugin processes.

use std::path::PathBuf;

use serde_json::Value;

/// What a plugin process knows about where it runs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginEnv {
    pub bin: Option<String>,
    pub socket: Option<String>,
    pub pane_id: Option<String>,
    pub tab_id: Option<String>,
    pub workspace_id: Option<String>,
    pub plugin_id: Option<String>,
    pub plugin_root: Option<PathBuf>,
    pub config_dir: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub context: Option<Value>,
    pub event: Option<Value>,
    pub clicked_url: Option<String>,
}

impl PluginEnv {
    /// Reads the process environment.
    pub fn from_process() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads through a lookup function, for tests.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |name: &str| lookup(name).filter(|value| !value.is_empty());
        let json = |name: &str| get(name).and_then(|text| serde_json::from_str(&text).ok());
        let context: Option<Value> = json("HERDR_PLUGIN_CONTEXT_JSON");
        let clicked_url = get("HERDR_PLUGIN_CLICKED_URL").or_else(|| {
            context
                .as_ref()
                .and_then(|context| context.get("clicked_url"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        Self {
            bin: get("HERDR_BIN_PATH"),
            socket: get("HERDR_SOCKET_PATH"),
            pane_id: get("HERDR_PANE_ID"),
            tab_id: get("HERDR_TAB_ID"),
            workspace_id: get("HERDR_WORKSPACE_ID"),
            plugin_id: get("HERDR_PLUGIN_ID"),
            plugin_root: get("HERDR_PLUGIN_ROOT").map(PathBuf::from),
            config_dir: get("HERDR_PLUGIN_CONFIG_DIR").map(PathBuf::from),
            state_dir: get("HERDR_PLUGIN_STATE_DIR").map(PathBuf::from),
            context,
            event: json("HERDR_PLUGIN_EVENT_JSON"),
            clicked_url,
        }
    }

    /// The pane id an event is about.
    ///
    /// Herdr sends `{"event":"pane_closed","data":{"pane_id":"w1:p3",...}}`,
    /// observed against Herdr 0.9.1.
    pub fn event_pane_id(&self) -> Option<String> {
        let event = self.event.as_ref()?;
        event
            .pointer("/data/pane_id")
            .or_else(|| event.get("pane_id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    /// The pane the user was looking at when an action ran.
    pub fn focused_pane(&self) -> Option<String> {
        self.context
            .as_ref()
            .and_then(|context| context.get("focused_pane_id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| self.pane_id.clone())
    }

    /// The working directory of the focused pane, for the chat pane.
    pub fn focused_cwd(&self) -> Option<String> {
        let context = self.context.as_ref()?;
        context
            .get("focused_pane_cwd")
            .or_else(|| context.get("workspace_cwd"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    /// The herdr executable: `HERDR_BIN_PATH`, else `herdr` on `PATH`.
    pub fn herdr_bin(&self) -> String {
        self.bin.clone().unwrap_or_else(|| "herdr".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> PluginEnv {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        PluginEnv::from_lookup(|name| map.get(name).cloned())
    }

    #[test]
    fn reads_observed_herdr_shapes() {
        let env = env(&[
            ("HERDR_PANE_ID", "w1:p3"),
            (
                "HERDR_SOCKET_PATH",
                "/root/.config/herdr/sessions/x/herdr.sock",
            ),
            (
                "HERDR_PLUGIN_STATE_DIR",
                "/root/.local/state/herdr/plugins/herdr-dashr",
            ),
            (
                "HERDR_PLUGIN_CONTEXT_JSON",
                r#"{"workspace_id":"w1","focused_pane_id":"w1:p1","focused_pane_cwd":"/src/app","clicked_url":"https://x"}"#,
            ),
            (
                "HERDR_PLUGIN_EVENT_JSON",
                r#"{"event":"pane_closed","data":{"type":"pane_closed","pane_id":"w1:p3","workspace_id":"w1"}}"#,
            ),
        ]);
        assert_eq!(env.event_pane_id().as_deref(), Some("w1:p3"));
        assert_eq!(env.focused_pane().as_deref(), Some("w1:p1"));
        assert_eq!(env.focused_cwd().as_deref(), Some("/src/app"));
        assert_eq!(env.clicked_url.as_deref(), Some("https://x"));
        assert_eq!(env.herdr_bin(), "herdr");
    }

    #[test]
    fn empty_and_invalid_values_are_absent() {
        let env = env(&[
            ("HERDR_PANE_ID", ""),
            ("HERDR_PLUGIN_CONTEXT_JSON", "{not json"),
        ]);
        assert_eq!(env, PluginEnv::default());
    }

    #[test]
    fn explicit_clicked_url_wins() {
        let env = env(&[
            ("HERDR_PLUGIN_CLICKED_URL", "https://a"),
            (
                "HERDR_PLUGIN_CONTEXT_JSON",
                r#"{"clicked_url":"https://b"}"#,
            ),
        ]);
        assert_eq!(env.clicked_url.as_deref(), Some("https://a"));
    }
}

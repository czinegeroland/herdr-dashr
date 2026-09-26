//! A thin wrapper over the `herdr` CLI.
//!
//! Every method builds an argv with a pure function (tested below) and runs
//! it; responses are the CLI's JSON envelope `{"id":..,"result":..}` or
//! `{"id":..,"error":{"code":..,"message":..}}`.

use std::process::{Command, Stdio};

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum HerdrError {
    #[error("could not run {bin}: {message}")]
    Spawn { bin: String, message: String },
    #[error("herdr {command} failed: {message}")]
    Failed { command: String, message: String },
    #[error("herdr {command} answered in an unexpected shape")]
    Unexpected { command: String },
}

/// Agent states Herdr accepts from `pane report-agent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Unknown => "unknown",
        }
    }
}

/// The `--source` dashr reports under. Herdr keeps lifecycle authority per
/// source, so a single stable value lets `release-agent` undo exactly what
/// dashr reported.
pub const SOURCE: &str = "custom:dashr";
/// The agent label shown for dashboard panes.
pub const AGENT_LABEL: &str = "dashr";
/// The sidebar token dashr owns: `$dashr` in Herdr's agent rows.
pub const TOKEN: &str = "dashr";

/// Argv builders, kept pure for testing.
pub mod argv {
    use super::*;

    fn owned(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    pub fn pane_list() -> Vec<String> {
        owned(&["pane", "list"])
    }

    pub fn pane_get(pane: &str) -> Vec<String> {
        owned(&["pane", "get", pane])
    }

    pub fn pane_split(
        pane: &str,
        direction: &str,
        ratio: f32,
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Vec<String> {
        let mut args = owned(&["pane", "split", pane, "--direction", direction]);
        args.push("--ratio".into());
        args.push(format!("{ratio:.2}"));
        if let Some(cwd) = cwd {
            args.push("--cwd".into());
            args.push(cwd.to_owned());
        }
        for (key, value) in env {
            args.push("--env".into());
            args.push(format!("{key}={value}"));
        }
        args.push("--no-focus".into());
        args
    }

    pub fn pane_run(pane: &str, command: &str) -> Vec<String> {
        owned(&["pane", "run", pane, command])
    }

    pub fn pane_close(pane: &str) -> Vec<String> {
        owned(&["pane", "close", pane])
    }

    pub fn pane_rename(pane: &str, label: &str) -> Vec<String> {
        owned(&["pane", "rename", pane, label])
    }

    pub fn report_agent(pane: &str, state: AgentState, message: Option<&str>) -> Vec<String> {
        let mut args = owned(&[
            "pane",
            "report-agent",
            pane,
            "--source",
            SOURCE,
            "--agent",
            AGENT_LABEL,
            "--state",
            state.as_str(),
        ]);
        if let Some(message) = message {
            args.push("--message".into());
            args.push(message.to_owned());
        }
        args
    }

    pub fn release_agent(pane: &str) -> Vec<String> {
        owned(&[
            "pane",
            "release-agent",
            pane,
            "--source",
            SOURCE,
            "--agent",
            AGENT_LABEL,
        ])
    }

    pub fn report_token(pane: &str, value: &str, ttl_ms: Option<u64>) -> Vec<String> {
        let mut args = owned(&["pane", "report-metadata", pane, "--source", SOURCE]);
        args.push("--token".into());
        args.push(format!("{TOKEN}={value}"));
        if let Some(ttl) = ttl_ms {
            args.push("--ttl-ms".into());
            args.push(ttl.to_string());
        }
        args
    }

    pub fn notify(title: &str, body: Option<&str>, urgent: bool) -> Vec<String> {
        let mut args = owned(&["notification", "show", title]);
        if let Some(body) = body {
            args.push("--body".into());
            args.push(body.to_owned());
        }
        args.push("--sound".into());
        args.push(if urgent { "request" } else { "none" }.into());
        args
    }

    pub fn plugin_pane_open(
        plugin: &str,
        entrypoint: &str,
        placement: &str,
        env: &[(String, String)],
    ) -> Vec<String> {
        let mut args = owned(&[
            "plugin",
            "pane",
            "open",
            "--plugin",
            plugin,
            "--entrypoint",
            entrypoint,
            "--placement",
            placement,
        ]);
        for (key, value) in env {
            args.push("--env".into());
            args.push(format!("{key}={value}"));
        }
        args.push("--focus".into());
        args
    }
}

/// Pulls `result.pane.pane_id` or `result.plugin_pane.pane.pane_id`.
pub fn created_pane_id(response: &Value) -> Option<String> {
    let result = response.get("result")?;
    result
        .pointer("/pane/pane_id")
        .or_else(|| result.pointer("/plugin_pane/pane/pane_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Pane ids in a `pane list` answer.
pub fn pane_ids(response: &Value) -> Vec<String> {
    response
        .pointer("/result/panes")
        .and_then(Value::as_array)
        .map(|panes| {
            panes
                .iter()
                .filter_map(|pane| pane.get("pane_id").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Normalises a notification or message text to Herdr's 80-character cap.
pub fn cap(text: &str) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    let trimmed = clean.trim();
    if trimmed.chars().count() <= 80 {
        trimmed.to_owned()
    } else {
        format!("{}…", trimmed.chars().take(79).collect::<String>())
    }
}

/// The `herdr` executable.
#[derive(Debug, Clone)]
pub struct Herdr {
    bin: String,
}

impl Herdr {
    pub fn new(bin: &str) -> Self {
        Self {
            bin: bin.to_owned(),
        }
    }

    /// Runs one command and returns its JSON answer.
    pub fn call(&self, args: &[String]) -> Result<Value, HerdrError> {
        let command = args.iter().take(2).cloned().collect::<Vec<_>>().join(" ");
        let output = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| HerdrError::Spawn {
                bin: self.bin.clone(),
                message: error.to_string(),
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: Option<Value> = stdout
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str(line.trim()).ok());
        if let Some(error) = parsed.as_ref().and_then(|value| value.get("error")) {
            let message = error
                .get("message")
                .or_else(|| error.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_owned();
            return Err(HerdrError::Failed { command, message });
        }
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(HerdrError::Failed {
                command,
                message: stderr.trim().chars().take(400).collect(),
            });
        }
        Ok(parsed.unwrap_or(Value::Null))
    }

    pub fn pane_list(&self) -> Result<Vec<String>, HerdrError> {
        self.call(&argv::pane_list()).map(|value| pane_ids(&value))
    }

    pub fn pane_exists(&self, pane: &str) -> bool {
        self.call(&argv::pane_get(pane)).is_ok()
    }

    pub fn pane_split(
        &self,
        pane: &str,
        direction: &str,
        ratio: f32,
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<String, HerdrError> {
        let value = self.call(&argv::pane_split(pane, direction, ratio, cwd, env))?;
        created_pane_id(&value).ok_or(HerdrError::Unexpected {
            command: "pane split".into(),
        })
    }

    pub fn pane_run(&self, pane: &str, command: &str) -> Result<(), HerdrError> {
        self.call(&argv::pane_run(pane, command)).map(|_| ())
    }

    pub fn pane_close(&self, pane: &str) -> Result<(), HerdrError> {
        self.call(&argv::pane_close(pane)).map(|_| ())
    }

    pub fn pane_rename(&self, pane: &str, label: &str) -> Result<(), HerdrError> {
        self.call(&argv::pane_rename(pane, label)).map(|_| ())
    }

    pub fn report_agent(
        &self,
        pane: &str,
        state: AgentState,
        message: Option<&str>,
    ) -> Result<(), HerdrError> {
        let message = message.map(cap);
        self.call(&argv::report_agent(pane, state, message.as_deref()))
            .map(|_| ())
    }

    pub fn release_agent(&self, pane: &str) -> Result<(), HerdrError> {
        self.call(&argv::release_agent(pane)).map(|_| ())
    }

    pub fn report_token(
        &self,
        pane: &str,
        value: &str,
        ttl_ms: Option<u64>,
    ) -> Result<(), HerdrError> {
        self.call(&argv::report_token(pane, &cap(value), ttl_ms))
            .map(|_| ())
    }

    pub fn notify(&self, title: &str, body: Option<&str>, urgent: bool) -> Result<(), HerdrError> {
        let body = body.map(cap);
        self.call(&argv::notify(&cap(title), body.as_deref(), urgent))
            .map(|_| ())
    }

    pub fn plugin_pane_open(
        &self,
        plugin: &str,
        entrypoint: &str,
        placement: &str,
        env: &[(String, String)],
    ) -> Result<String, HerdrError> {
        let value = self.call(&argv::plugin_pane_open(plugin, entrypoint, placement, env))?;
        Ok(created_pane_id(&value).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn split_argv() {
        let args = argv::pane_split(
            "w1:p3",
            "down",
            0.62,
            Some("/src"),
            &[("DASHR_SESSION".into(), "abc-w1-p3".into())],
        );
        assert_eq!(
            args.join(" "),
            "pane split w1:p3 --direction down --ratio 0.62 --cwd /src --env DASHR_SESSION=abc-w1-p3 --no-focus"
        );
    }

    #[test]
    fn report_argvs_use_the_dashr_source() {
        assert_eq!(
            argv::report_agent("w1:p1", AgentState::Blocked, Some("DLQ not empty")).join(" "),
            "pane report-agent w1:p1 --source custom:dashr --agent dashr --state blocked --message DLQ not empty"
        );
        assert_eq!(
            argv::report_token("w1:p1", "6 ok", Some(60_000)).join(" "),
            "pane report-metadata w1:p1 --source custom:dashr --token dashr=6 ok --ttl-ms 60000"
        );
        assert_eq!(
            argv::release_agent("w1:p1").join(" "),
            "pane release-agent w1:p1 --source custom:dashr --agent dashr"
        );
        assert_eq!(
            argv::notify("t", None, true).join(" "),
            "notification show t --sound request"
        );
    }

    #[test]
    fn plugin_pane_open_argv() {
        let args = argv::plugin_pane_open(
            "herdr-dashr",
            "dashboard",
            "tab",
            &[("DASHR_PIPELINE_URL".into(), "https://x".into())],
        );
        assert_eq!(
            args.join(" "),
            "plugin pane open --plugin herdr-dashr --entrypoint dashboard --placement tab --env DASHR_PIPELINE_URL=https://x --focus"
        );
    }

    #[test]
    fn parses_observed_responses() {
        let split =
            json!({"id":"cli:pane:split","result":{"pane":{"pane_id":"w1:p2"},"type":"pane_info"}});
        assert_eq!(created_pane_id(&split).as_deref(), Some("w1:p2"));
        let opened = json!({"result":{"plugin_pane":{"entrypoint":"dash","pane":{"pane_id":"w1:p3"}},"type":"plugin_pane_opened"}});
        assert_eq!(created_pane_id(&opened).as_deref(), Some("w1:p3"));
        let list = json!({"result":{"panes":[{"pane_id":"w1:p1"},{"pane_id":"w1:p2"}],"type":"pane_list"}});
        assert_eq!(pane_ids(&list), vec!["w1:p1", "w1:p2"]);
    }

    #[test]
    fn cap_strips_control_characters_and_truncates() {
        assert_eq!(cap(" a\u{1b}[31mb "), "a[31mb");
        let long = "x".repeat(100);
        assert_eq!(cap(&long).chars().count(), 80);
    }

    #[cfg(unix)] // uses `sh`
    #[test]
    fn errors_are_reported_from_the_envelope() {
        // `sh -c` stands in for herdr: prints an error envelope, exits 1.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-herdr");
        // Copied into place by `cp` so no write handle to the executable is
        // ever open here to leak into a parallel test's fork (ETXTBSY).
        let source = dir.path().join("fake-herdr.sh");
        std::fs::write(
            &source,
            "#!/bin/sh\necho '{\"id\":\"x\",\"error\":{\"code\":\"pane_not_found\",\"message\":\"pane w9:p9 not found\"}}'\nexit 1\n",
        )
        .unwrap();
        let copied = std::process::Command::new("cp")
            .arg(&source)
            .arg(&script)
            .status()
            .unwrap();
        assert!(copied.success());
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let herdr = Herdr::new(script.to_str().unwrap());
            let error = herdr.pane_close("w9:p9").unwrap_err().to_string();
            assert!(error.contains("pane w9:p9 not found"), "{error}");
            assert!(!herdr.pane_exists("w9:p9"));
        }
    }
}

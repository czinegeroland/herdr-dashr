//! terminal-browser: the pane that shows the real Grafana UI.
//!
//! terminal-browser renders Chromium through the kitty graphics protocol and
//! ships an agent-browser compatible `action` command. dashr uses three of
//! its surfaces: `open <url>` in the dashboard pane, `ls --all --json` to find
//! which browser shows this session's dashboard, and `action --browser <key>`
//! to reload it after a dashboard change (DASHR-VIEW-004) or take a gated
//! screenshot (DASHR-MCP-010).
//!
//! The browser profile is pointed at the session's runtime directory with
//! `TERMINAL_BROWSER_APPDATA`, which terminal-browser reads when it claims a
//! profile, so cookies, cache and local storage live in memory and are
//! deleted with the session (DASHR-VIEW-002).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

/// The variable terminal-browser reads for its profile location.
pub const PROFILE_ENV: &str = "TERMINAL_BROWSER_APPDATA";

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("terminal-browser ({0}) is not installed")]
    Missing(String),
    #[error("no terminal-browser is showing {0}")]
    NotShowing(String),
    #[error("terminal-browser {action} failed: {message}")]
    Failed { action: String, message: String },
}

/// Whether a command can be found on `PATH` (or is an existing path).
pub fn on_path(command: &str) -> bool {
    if command.contains('/') {
        return Path::new(command).is_file();
    }
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(command).is_file()))
        .unwrap_or(false)
}

/// Picks the browser key whose tabs show `url_fragment`, from
/// `terminal-browser ls --all --json` output.
pub fn key_showing(ls_json: &Value, url_fragment: &str) -> Option<String> {
    let browsers = ls_json
        .as_array()
        .or_else(|| ls_json.get("browsers").and_then(Value::as_array))?;
    browsers
        .iter()
        .find(|browser| {
            browser
                .get("tabs")
                .and_then(Value::as_array)
                .is_some_and(|tabs| {
                    tabs.iter().any(|tab| {
                        tab.get("url")
                            .and_then(Value::as_str)
                            .is_some_and(|url| url.contains(url_fragment))
                    })
                })
        })
        .and_then(|browser| browser.get("key").and_then(Value::as_str))
        .map(str::to_owned)
}

#[derive(Debug, Clone)]
pub struct Browser {
    command: String,
}

impl Browser {
    pub fn new(command: &str) -> Self {
        Self {
            command: command.to_owned(),
        }
    }

    pub fn installed(&self) -> bool {
        on_path(&self.command)
    }

    /// The profile directory for a session.
    pub fn profile_dir(runtime_dir: &Path) -> PathBuf {
        runtime_dir.join("browser")
    }

    /// A command that opens `url` in the current terminal.
    pub fn open_command(&self, url: &str, runtime_dir: &Path) -> Command {
        let mut command = Command::new(&self.command);
        command
            .args(["open", url])
            .env(PROFILE_ENV, Self::profile_dir(runtime_dir));
        command
    }

    fn output(&self, args: &[&str]) -> Result<String, BrowserError> {
        let output = Command::new(&self.command)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => BrowserError::Missing(self.command.clone()),
                _ => BrowserError::Failed {
                    action: args.first().copied().unwrap_or("").to_owned(),
                    message: error.to_string(),
                },
            })?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(BrowserError::Failed {
                action: args.first().copied().unwrap_or("").to_owned(),
                message: String::from_utf8_lossy(&output.stderr)
                    .trim()
                    .chars()
                    .take(300)
                    .collect(),
            })
        }
    }

    /// The key of the browser showing `url_fragment`.
    pub fn find(&self, url_fragment: &str) -> Result<String, BrowserError> {
        if !self.installed() {
            return Err(BrowserError::Missing(self.command.clone()));
        }
        let text = self.output(&["ls", "--all", "--json"])?;
        let value: Value = serde_json::from_str(&text).map_err(|error| BrowserError::Failed {
            action: "ls".into(),
            message: error.to_string(),
        })?;
        key_showing(&value, url_fragment)
            .ok_or_else(|| BrowserError::NotShowing(url_fragment.to_owned()))
    }

    /// Reloads the browser showing `url_fragment`.
    pub fn reload(&self, url_fragment: &str) -> Result<(), BrowserError> {
        let key = self.find(url_fragment)?;
        self.output(&["action", "--browser", &key, "--", "reload"])
            .map(|_| ())
    }

    /// Saves a screenshot of the browser showing `url_fragment` to `path`.
    pub fn screenshot(&self, url_fragment: &str, path: &Path) -> Result<(), BrowserError> {
        let key = self.find(url_fragment)?;
        let target = path.display().to_string();
        self.output(&["action", "--browser", &key, "--", "screenshot", &target])
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_the_browser_showing_the_session() {
        let listing = json!([
            {"key": "a1", "tabs": [{"url": "https://example.com"}]},
            {"key": "b2", "tabs": [{"url": "about:blank"}, {"url": "http://127.0.0.1:32768/d/dashr-x?kiosk"}]}
        ]);
        assert_eq!(
            key_showing(&listing, "127.0.0.1:32768/d/dashr-x").as_deref(),
            Some("b2")
        );
        assert_eq!(key_showing(&listing, "127.0.0.1:1/d/y"), None);
        assert_eq!(key_showing(&json!({"browsers": []}), "x"), None);
    }

    #[test]
    fn open_command_points_the_profile_at_the_runtime_dir() {
        let browser = Browser::new("terminal-browser");
        let command = browser.open_command(
            "http://127.0.0.1:1/d/u?kiosk",
            Path::new("/dev/shm/herdr-dashr/s"),
        );
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["open", "http://127.0.0.1:1/d/u?kiosk"]);
        let env: Vec<_> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            env,
            vec![(
                PROFILE_ENV.to_owned(),
                Some("/dev/shm/herdr-dashr/s/browser".to_owned())
            )]
        );
    }

    #[test]
    fn missing_browser() {
        let browser = Browser::new("definitely-not-a-browser-xyz");
        assert!(!browser.installed());
        assert!(matches!(browser.reload("x"), Err(BrowserError::Missing(_))));
        assert!(on_path("sh"));
    }
}

//! terminal-browser: the pane that shows the real Grafana UI.
//!
//! terminal-browser renders Chromium through the kitty graphics protocol.
//! dashr uses it in two ways:
//!
//! * `terminal-browser open <url>` runs in the dashboard pane's terminal
//!   (DASHR-VIEW-001), with `TERMINAL_BROWSER_APPDATA` pointing the profile at
//!   the session's runtime directory, so cookies, cache and local storage live
//!   in memory and are deleted with the session (DASHR-VIEW-002).
//! * `terminal-browser ls --all --json` names the loopback DevTools port of
//!   each browser. dashr finds the page showing this session's dashboard on
//!   that port and drives it over the Chrome DevTools Protocol: `Page.reload`
//!   after a dashboard change (DASHR-VIEW-004) and `Page.captureScreenshot`
//!   for the gated screenshot tool (DASHR-MCP-010).
//!
//! Direct CDP rather than `terminal-browser action`: the latter goes through
//! agent-browser, which attaches for one command and then, on the next, looks
//! for a Chrome of its own to launch instead of reusing the connection
//! (observed with terminal-browser 0.11.1 / agent-browser 0.33.0). Talking
//! to the page directly has no such chain and returns screenshot bytes
//! without a temporary file (decision DEC-024).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

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

fn failed(action: &str, message: impl std::fmt::Display) -> BrowserError {
    BrowserError::Failed {
        action: action.to_owned(),
        message: message.to_string(),
    }
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

fn browsers(ls_json: &Value) -> &[Value] {
    ls_json
        .as_array()
        .or_else(|| ls_json.get("browsers").and_then(Value::as_array))
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn shows(browser: &Value, url_fragment: &str) -> bool {
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
}

/// The key of the browser whose tabs show `url_fragment`, from
/// `terminal-browser ls --all --json` output.
pub fn key_showing(ls_json: &Value, url_fragment: &str) -> Option<String> {
    browsers(ls_json)
        .iter()
        .find(|browser| shows(browser, url_fragment))
        .and_then(|browser| browser.get("key").and_then(Value::as_str))
        .map(str::to_owned)
}

/// The DevTools port of the browser showing `url_fragment`.
pub fn cdp_port_showing(ls_json: &Value, url_fragment: &str) -> Option<u16> {
    browsers(ls_json)
        .iter()
        .find(|browser| shows(browser, url_fragment))
        .and_then(|browser| browser.get("cdpPort").and_then(Value::as_u64))
        .and_then(|port| u16::try_from(port).ok())
}

/// The websocket URL of the page showing `url_fragment`, from a DevTools
/// `/json/list` answer. Only loopback URLs are accepted.
pub fn page_websocket(list: &Value, url_fragment: &str) -> Option<String> {
    list.as_array()?
        .iter()
        .filter(|target| target.get("type").and_then(Value::as_str) == Some("page"))
        .find(|target| {
            target
                .get("url")
                .and_then(Value::as_str)
                .is_some_and(|url| url.contains(url_fragment))
        })
        .and_then(|target| target.get("webSocketDebuggerUrl").and_then(Value::as_str))
        .filter(|ws| ws.starts_with("ws://127.0.0.1:") || ws.starts_with("ws://localhost:"))
        .map(str::to_owned)
}

/// Sends one DevTools command and waits for its answer, skipping events.
pub fn cdp_call(websocket: &str, method: &str, params: Value) -> Result<Value, BrowserError> {
    let (mut socket, _) = tungstenite::connect(websocket).map_err(|error| failed(method, error))?;
    if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_ref() {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
    }
    let request = json!({"id": 1, "method": method, "params": params});
    socket
        .send(tungstenite::Message::text(request.to_string()))
        .map_err(|error| failed(method, error))?;
    loop {
        let message = socket.read().map_err(|error| failed(method, error))?;
        let tungstenite::Message::Text(text) = message else {
            continue;
        };
        let value: Value =
            serde_json::from_str(text.as_str()).map_err(|error| failed(method, error))?;
        if value.get("id").and_then(Value::as_i64) != Some(1) {
            continue; // an event
        }
        let _ = socket.close(None);
        if let Some(error) = value.get("error") {
            return Err(failed(method, error));
        }
        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
    }
}

/// The locale to give the browser when the environment has none it can use.
///
/// Chromium takes its language from `LC_ALL`/`LANG`. Under the C or POSIX
/// locale, or none at all, it reports its language as `c`, and Grafana's
/// date formatting then throws `RangeError: Invalid language tag: c` and
/// replaces the whole dashboard with "An unexpected error happened" (found
/// by the end-to-end browser scenario, DEC-025).
pub fn browser_locale(lc_all: Option<&str>, lang: Option<&str>) -> Option<&'static str> {
    let effective = lc_all
        .filter(|v| !v.is_empty())
        .or(lang.filter(|v| !v.is_empty()));
    match effective {
        None => Some("en_US.UTF-8"),
        Some(value) if value == "C" || value == "POSIX" || value.starts_with("C.") => {
            Some("en_US.UTF-8")
        }
        Some(_) => None,
    }
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
        let lc_all = std::env::var("LC_ALL").ok();
        let lang = std::env::var("LANG").ok();
        if let Some(locale) = browser_locale(lc_all.as_deref(), lang.as_deref()) {
            command.env("LANG", locale).env_remove("LC_ALL");
        }
        command
    }

    fn list(&self) -> Result<Value, BrowserError> {
        if !self.installed() {
            return Err(BrowserError::Missing(self.command.clone()));
        }
        let output = Command::new(&self.command)
            .args(["ls", "--all", "--json"])
            .stdin(Stdio::null())
            .output()
            .map_err(|error| failed("ls", error))?;
        if !output.status.success() {
            return Err(failed("ls", String::from_utf8_lossy(&output.stderr).trim()));
        }
        serde_json::from_slice(&output.stdout).map_err(|error| failed("ls", error))
    }

    /// The key of the browser showing `url_fragment`.
    pub fn find(&self, url_fragment: &str) -> Result<String, BrowserError> {
        key_showing(&self.list()?, url_fragment)
            .ok_or_else(|| BrowserError::NotShowing(url_fragment.to_owned()))
    }

    /// The DevTools websocket of the page showing `url_fragment`.
    fn page(&self, url_fragment: &str) -> Result<String, BrowserError> {
        let port = cdp_port_showing(&self.list()?, url_fragment)
            .ok_or_else(|| BrowserError::NotShowing(url_fragment.to_owned()))?;
        let list: Value = ureq::get(format!("http://127.0.0.1:{port}/json/list"))
            .call()
            .map_err(|error| failed("json/list", error))?
            .body_mut()
            .read_json()
            .map_err(|error| failed("json/list", error))?;
        page_websocket(&list, url_fragment)
            .ok_or_else(|| BrowserError::NotShowing(url_fragment.to_owned()))
    }

    /// Reloads the page showing `url_fragment`, so a changed dashboard
    /// model is picked up (kiosk refresh only re-runs queries).
    pub fn reload(&self, url_fragment: &str) -> Result<(), BrowserError> {
        let page = self.page(url_fragment)?;
        cdp_call(&page, "Page.reload", json!({"ignoreCache": true})).map(|_| ())
    }

    /// A PNG of the page showing `url_fragment`, base64 encoded.
    pub fn screenshot(&self, url_fragment: &str) -> Result<String, BrowserError> {
        let page = self.page(url_fragment)?;
        let result = cdp_call(&page, "Page.captureScreenshot", json!({"format": "png"}))?;
        result
            .get("data")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| failed("Page.captureScreenshot", "no image data"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn finds_the_browser_and_page_showing_the_session() {
        // Shapes observed from terminal-browser 0.11.1 and Chromium's /json/list.
        let listing = json!({"self": null, "browsers": [
            {"key": "a1", "cdpPort": 1111, "tabs": [{"url": "https://example.com"}]},
            {"key": "b2", "cdpPort": 39293, "tabs": [{"url": "http://127.0.0.1:32768/d/dashr-x/dashr?orgId=1&kiosk="}]}
        ]});
        let fragment = "127.0.0.1:32768/d/dashr-x";
        assert_eq!(key_showing(&listing, fragment).as_deref(), Some("b2"));
        assert_eq!(cdp_port_showing(&listing, fragment), Some(39293));
        assert_eq!(key_showing(&listing, "127.0.0.1:1/d/y"), None);
        assert_eq!(key_showing(&json!([]), "x"), None);

        let targets = json!([
            {"type": "service_worker", "url": "http://127.0.0.1:32768/d/dashr-x/sw", "webSocketDebuggerUrl": "ws://127.0.0.1:39293/devtools/sw"},
            {"type": "page", "url": "http://127.0.0.1:32768/d/dashr-x/dashr", "webSocketDebuggerUrl": "ws://127.0.0.1:39293/devtools/page/AB"}
        ]);
        assert_eq!(
            page_websocket(&targets, fragment).as_deref(),
            Some("ws://127.0.0.1:39293/devtools/page/AB")
        );
        let remote = json!([{"type": "page", "url": fragment, "webSocketDebuggerUrl": "ws://evil.example/devtools/page/AB"}]);
        assert_eq!(page_websocket(&remote, fragment), None, "only loopback");
    }

    #[test]
    fn cdp_call_skips_events_and_returns_the_result() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let request: Value =
                serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "Page.captureScreenshot");
            socket
                .send(tungstenite::Message::text(
                    r#"{"method":"Page.frameNavigated","params":{}}"#,
                ))
                .unwrap();
            socket
                .send(tungstenite::Message::text(
                    r#"{"id":1,"result":{"data":"iVBORw0KGgo="}}"#,
                ))
                .unwrap();
            let _ = socket.read();
        });
        let result = cdp_call(
            &format!("ws://{address}/devtools/page/X"),
            "Page.captureScreenshot",
            json!({"format": "png"}),
        )
        .unwrap();
        assert_eq!(result["data"], "iVBORw0KGgo=");
        server.join().unwrap();
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
        assert!(env.contains(&(
            PROFILE_ENV.to_owned(),
            Some("/dev/shm/herdr-dashr/s/browser".to_owned())
        )));
    }

    #[test]
    fn a_usable_locale_is_supplied_only_when_missing() {
        assert_eq!(browser_locale(None, None), Some("en_US.UTF-8"));
        assert_eq!(browser_locale(None, Some("C")), Some("en_US.UTF-8"));
        assert_eq!(
            browser_locale(Some("C.UTF-8"), Some("hu_HU.UTF-8")),
            Some("en_US.UTF-8")
        );
        assert_eq!(browser_locale(Some("POSIX"), None), Some("en_US.UTF-8"));
        assert_eq!(browser_locale(None, Some("hu_HU.UTF-8")), None);
        assert_eq!(browser_locale(Some(""), Some("de_DE.UTF-8")), None);
    }

    #[test]
    fn missing_browser() {
        let browser = Browser::new("definitely-not-a-browser-xyz");
        assert!(!browser.installed());
        assert!(matches!(browser.reload("x"), Err(BrowserError::Missing(_))));
        assert!(on_path("sh"));
    }
}

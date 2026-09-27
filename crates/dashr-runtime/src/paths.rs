//! Where configuration, state and runtime files live.
//!
//! State — session records, watches, saved dashboards — lives in dashr's own
//! directory, not Herdr's plugin state directory, as herdr-remote-channel
//! keeps its own home (DEC-039): the dashboard pane, the human's AI session
//! running `dashr wait` / `dashr tool`, and dashr run by hand must all find
//! the same sessions, and only the pane has Herdr's plugin variables.
//! `DASHR_STATE_DIR` overrides it; otherwise it follows the platform
//! convention.
//!
//! Configuration comes from `HERDR_PLUGIN_CONFIG_DIR` under Herdr (where
//! `herdr plugin config-dir herdr-dashr` points), else the platform default.
//! A pane records the directory it used, so the AI session's `dashr tool`
//! masks with the same configuration (see `CONFIG_DIR_FILE`).
//!
//! Runtime files — provisioning, the MCP config, the browser profile — go
//! to a memory-backed directory when the platform has one
//! (requirement DASHR-GRAF-004).

use std::path::{Path, PathBuf};

/// Overrides the state directory.
pub const STATE_ENV: &str = "DASHR_STATE_DIR";
/// In a session's runtime directory: the configuration directory the pane
/// loaded, so commands run outside Herdr use the same masking rules.
pub const CONFIG_DIR_FILE: &str = "config-dir";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The platform's configuration directory for dashr.
fn default_config_dir() -> PathBuf {
    // Windows first: a Windows host may also define HOME.
    if cfg!(windows)
        && let Some(appdata) = env_path("APPDATA")
    {
        return appdata.join("herdr-dashr");
    }
    env_path("XDG_CONFIG_HOME")
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("herdr-dashr"))
        .unwrap_or_else(|| home().join(".config").join("herdr-dashr"))
}

/// The platform's state directory for dashr.
fn default_state_dir() -> PathBuf {
    if cfg!(windows)
        && let Some(appdata) = env_path("APPDATA")
    {
        return appdata.join("herdr-dashr").join("state");
    }
    if let Some(xdg) = env_path("XDG_STATE_HOME").filter(|dir| dir.is_absolute()) {
        return xdg.join("herdr-dashr");
    }
    if cfg!(target_os = "macos") {
        return home().join("Library/Application Support/herdr-dashr");
    }
    home().join(".local").join("state").join("herdr-dashr")
}

impl Paths {
    /// Resolves paths: explicit values first, then `HERDR_PLUGIN_CONFIG_DIR`
    /// (configuration) and `DASHR_STATE_DIR` (state), then the platform
    /// defaults.
    pub fn resolve(config_dir: Option<PathBuf>, state_dir: Option<PathBuf>) -> Self {
        Self {
            config_dir: config_dir
                .or_else(|| env_path("HERDR_PLUGIN_CONFIG_DIR"))
                .unwrap_or_else(default_config_dir),
            state_dir: state_dir
                .or_else(|| env_path(STATE_ENV))
                .unwrap_or_else(default_state_dir),
        }
    }
}

/// A place runtime directories can go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeBase {
    pub path: PathBuf,
    pub in_memory: bool,
    /// Shared with other users of the machine.
    pub shared: bool,
}

/// Candidate bases, best first: memory-backed and private, memory-backed
/// and shared, then the temp directory.
pub fn runtime_bases() -> Vec<RuntimeBase> {
    let mut bases = Vec::new();
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        bases.push(RuntimeBase {
            path: PathBuf::from(dir),
            in_memory: true,
            shared: false,
        });
    }
    if Path::new("/dev/shm").is_dir() {
        bases.push(RuntimeBase {
            path: PathBuf::from("/dev/shm"),
            in_memory: true,
            shared: true,
        });
    }
    // macOS has no user tmpfs; its per-user temp directory is private to the
    // user but disk-backed. Recorded as a known limitation (OQ-002).
    bases.push(RuntimeBase {
        path: std::env::temp_dir(),
        in_memory: false,
        shared: true,
    });
    bases
}

/// The directory name under a base.
///
/// `/dev/shm` and `/tmp` are shared by every user on the machine; the
/// first user to create a plain `herdr-dashr` there makes it 0700 and locks
/// everyone else out, pushing them to a disk-backed fallback (found by the
/// end-to-end suite running as a second user). Under a shared base the name
/// carries the user. `XDG_RUNTIME_DIR` is already per-user.
fn base_name(shared: bool) -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .map(|user| dashr_core::ids::sanitize(&user))
        .unwrap_or_default();
    if shared && !user.is_empty() {
        format!("herdr-dashr-{user}")
    } else {
        "herdr-dashr".to_owned()
    }
}

/// Creates `<base>/herdr-dashr[-<user>]/<session>` with mode 0700 and says whether
/// it is memory-backed.
pub fn create_runtime_dir(session_id: &str) -> std::io::Result<(PathBuf, bool)> {
    let mut last_error = None;
    for RuntimeBase {
        path: base,
        in_memory,
        shared,
    } in runtime_bases()
    {
        let dir = base
            .join(base_name(shared))
            .join(dashr_core::ids::sanitize(session_id));
        match create_private(&dir) {
            Ok(()) => return Ok((dir, in_memory)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| std::io::Error::other("no runtime directory available")))
}

fn create_private(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(parent) = dir.parent() {
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Removes a runtime directory, refusing anything outside `herdr-dashr/`.
pub fn remove_runtime_dir(dir: &Path) {
    let inside = dir
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "herdr-dashr" || name.starts_with("herdr-dashr-"));
    if inside {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_paths_win() {
        let paths = Paths::resolve(Some("/c".into()), Some("/s".into()));
        assert_eq!(paths.config_dir, PathBuf::from("/c"));
        assert_eq!(paths.state_dir, PathBuf::from("/s"));
    }

    #[test]
    fn runtime_dir_is_private_and_removable() {
        let (dir, _) = create_runtime_dir("test-session-xyz").unwrap();
        assert!(dir.ends_with("test-session-xyz"));
        let parent = dir
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(parent.starts_with("herdr-dashr"), "{parent}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
        remove_runtime_dir(&dir);
        assert!(!dir.exists());
    }

    #[test]
    fn shared_bases_get_a_per_user_name() {
        // USER is set in every environment the suite runs in.
        if let Ok(user) = std::env::var("USER") {
            assert_eq!(
                base_name(true),
                format!("herdr-dashr-{}", dashr_core::ids::sanitize(&user))
            );
        }
        assert_eq!(base_name(false), "herdr-dashr");
    }

    #[test]
    fn refuses_to_remove_foreign_directories() {
        let foreign = tempfile::tempdir().unwrap();
        remove_runtime_dir(foreign.path());
        assert!(foreign.path().exists());
    }
}

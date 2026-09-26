//! Where configuration, state and runtime files live.
//!
//! Under Herdr the plugin directories come from `HERDR_PLUGIN_CONFIG_DIR`
//! and `HERDR_PLUGIN_STATE_DIR`. The chat pane is an ordinary pane without
//! those variables, so the paths are passed to `dashr mcp` explicitly; run
//! by hand, dashr falls back to XDG locations.
//!
//! Runtime files — provisioning, the MCP config, the browser profile — go
//! to a memory-backed directory when the platform has one
//! (requirement DASHR-GRAF-004).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

impl Paths {
    /// Resolves paths from explicit values, Herdr's plugin variables, then
    /// XDG defaults.
    pub fn resolve(config_dir: Option<PathBuf>, state_dir: Option<PathBuf>) -> Self {
        let env_path = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let config_dir = config_dir
            .or_else(|| env_path("HERDR_PLUGIN_CONFIG_DIR"))
            .or_else(|| env_path("XDG_CONFIG_HOME").map(|dir| dir.join("herdr-dashr")))
            .unwrap_or_else(|| home().join(".config").join("herdr-dashr"));
        let state_dir = state_dir
            .or_else(|| env_path("HERDR_PLUGIN_STATE_DIR"))
            .or_else(|| env_path("XDG_STATE_HOME").map(|dir| dir.join("herdr-dashr")))
            .unwrap_or_else(|| home().join(".local").join("state").join("herdr-dashr"));
        Self {
            config_dir,
            state_dir,
        }
    }
}

/// Candidate memory-backed bases, best first.
pub fn runtime_bases() -> Vec<(PathBuf, bool)> {
    let mut bases = Vec::new();
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        bases.push((PathBuf::from(dir), true));
    }
    if Path::new("/dev/shm").is_dir() {
        bases.push((PathBuf::from("/dev/shm"), true));
    }
    // macOS has no user tmpfs; its per-user temp directory is private to the
    // user but disk-backed. Recorded as a known limitation (OQ-002).
    bases.push((std::env::temp_dir(), false));
    bases
}

/// Creates `<base>/herdr-dashr/<session>` with mode 0700 and says whether
/// it is memory-backed.
pub fn create_runtime_dir(session_id: &str) -> std::io::Result<(PathBuf, bool)> {
    let mut last_error = None;
    for (base, in_memory) in runtime_bases() {
        let dir = base
            .join("herdr-dashr")
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
        .is_some_and(|name| name == "herdr-dashr");
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
        assert!(dir.ends_with("herdr-dashr/test-session-xyz"));
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
    fn refuses_to_remove_foreign_directories() {
        let foreign = tempfile::tempdir().unwrap();
        remove_runtime_dir(foreign.path());
        assert!(foreign.path().exists());
    }
}

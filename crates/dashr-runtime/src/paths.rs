//! Where configuration and state live.
//!
//! State — the records of running sessions — lives in dashr's own
//! directory, so the pane, the human's AI session running `dashr`, and
//! dashr run by hand all find the same sessions (DEC-039).
//! `DASHR_STATE_DIR` overrides it. Configuration comes from
//! `HERDR_PLUGIN_CONFIG_DIR` under Herdr, else the platform default.

use std::path::PathBuf;

/// Overrides the state directory.
pub const STATE_ENV: &str = "DASHR_STATE_DIR";

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_paths_win() {
        let paths = Paths::resolve(Some("/c".into()), Some("/s".into()));
        assert_eq!(paths.config_dir, PathBuf::from("/c"));
        assert_eq!(paths.state_dir, PathBuf::from("/s"));
    }
}

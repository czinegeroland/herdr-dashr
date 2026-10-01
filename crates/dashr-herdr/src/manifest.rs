//! The plugin manifest, `herdr-plugin.toml`, generated from code.
//!
//! The checked-in manifest is output of [`render`]; a test in `dashr-cli`
//! fails when the two disagree (requirement DASHR-HERDR-001). Generating it
//! means an action id, a pane id and the command that answers them cannot
//! drift apart.

use serde::Serialize;

pub const PLUGIN_ID: &str = "herdr-dashr";
pub const MIN_HERDR_VERSION: &str = "0.9.0";
/// The npm package the build step installs.
pub const PACKAGE: &str = "herdr-dashr";
/// The launcher the build step installs, relative to the plugin root that
/// Herdr uses as the working directory (DEC-038, as herdr-remote-channel).
///
/// Entry points run it as `node <this>` rather than executing a binary
/// directly: `node` is `node.exe` on Windows, so it resolves however Herdr
/// spawns a command, while `node_modules/.bin/dashr` would be `dashr.cmd`
/// there, which a bare `CreateProcess` does not find. The launcher picks the
/// platform package npm installed and runs the real executable, passing
/// arguments, signals and the exit code through.
pub const LAUNCHER: &str = "node_modules/herdr-dashr/bin.js";

#[derive(Debug, Serialize)]
struct Manifest {
    id: &'static str,
    name: &'static str,
    version: &'static str,
    min_herdr_version: &'static str,
    description: &'static str,
    platforms: Vec<&'static str>,
    build: Vec<Build>,
    startup: Vec<Hook>,
    actions: Vec<Action>,
    events: Vec<Event>,
    panes: Vec<Pane>,
}

#[derive(Debug, Serialize)]
struct Build {
    command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    platforms: Option<Vec<&'static str>>,
}

#[derive(Debug, Serialize)]
struct Hook {
    command: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Action {
    id: &'static str,
    title: &'static str,
    description: &'static str,
    contexts: Vec<&'static str>,
    command: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Event {
    on: &'static str,
    command: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Pane {
    id: &'static str,
    title: &'static str,
    placement: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<&'static str>,
    command: Vec<String>,
}

fn dashr(args: &[&str]) -> Vec<String> {
    ["node", LAUNCHER]
        .into_iter()
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect()
}

/// The argv that installs this version's executable from npm.
///
/// On Windows `npm` is `npm.cmd`, which `CreateProcess` does not find, so
/// the step goes through `cmd /c`. The version is pinned to this build's, so
/// the manifest Herdr read and the executable it installs are one release.
/// `--no-save` because the plugin root is not a package, and `--prefix .`
/// for the same reason: without a `package.json` npm walks up to the first
/// parent that has one (often the home directory) and installs there, where
/// `node node_modules/herdr-dashr/bin.js` does not look.
fn npm_install(prefix: &[&str], version: &str) -> Vec<String> {
    prefix
        .iter()
        .copied()
        .chain([
            "npm",
            "install",
            "--prefix",
            ".",
            "--no-save",
            "--no-audit",
            "--no-fund",
        ])
        .map(str::to_owned)
        .chain(std::iter::once(format!("{PACKAGE}@{version}")))
        .collect()
}

/// The repository the agent skill is installed from.
pub const REPOSITORY: &str = "czinegeroland/herdr-dashr";

/// The argv that installs the agent skill, globally, for the coding agents
/// found on the machine (`skills` detects them: Claude Code, Codex, ...).
///
/// `skills add` reads `.agents/skills/herdr-dashr/` from the repository's
/// default branch; it takes no tag or commit, so this step is not pinned to
/// the manifest version (herdr-remote-channel's DEC-079). `npx` is
/// `npx.cmd` on Windows, hence `cmd /c` there.
fn skills_add(prefix: &[&str]) -> Vec<String> {
    prefix
        .iter()
        .copied()
        .chain([
            "npx", "--yes", "skills", "add", REPOSITORY, "--skill", PLUGIN_ID, "--global", "--yes",
        ])
        .map(str::to_owned)
        .collect()
}

/// The actions the manifest declares: `(id, title, description)`.
pub const ACTIONS: &[(&str, &str, &str)] = &[
    (
        "open",
        "Open trace session",
        "Open a live trace session (Jaeger, sequence diagrams, flow checks) beside this pane.",
    ),
    (
        "doctor",
        "Check dashr prerequisites",
        "Check Docker, Herdr and the other tools dashr uses.",
    ),
];

/// The panes the manifest declares: `(id, title)`.
pub const PANES: &[(&str, &str)] = &[("traces", "dashr"), ("doctor", "dashr doctor")];

/// The one-line description: manifest, npm package and repository.
pub const DESCRIPTION: &str = "End-to-end testing by traces: your AI agent instruments a feature with OpenTelemetry, you browse and edit the code behind every span, and every service's spans — local, AWS X-Ray, Azure, Google Cloud, Jaeger, Zipkin — meet in one live sequence diagram the agent checks against the expected flow.";

fn manifest(version: &'static str) -> Manifest {
    Manifest {
        id: PLUGIN_ID,
        name: "herdr-dashr",
        version,
        min_herdr_version: MIN_HERDR_VERSION,
        description: DESCRIPTION,
        platforms: vec!["linux", "macos", "windows"],
        // No compiler and no shell: the executable comes from npm, one
        // package per platform with its own binary, as herdr-remote-channel
        // does (DEC-038). `sh scripts/install.sh` was skipped by Herdr on
        // Windows, which left the plugin installed with nothing to run.
        build: vec![
            Build {
                command: npm_install(&["cmd", "/c"], version),
                platforms: Some(vec!["windows"]),
            },
            Build {
                command: npm_install(&[], version),
                platforms: Some(vec!["linux", "macos"]),
            },
            // The `dashr` command on the human's PATH, for their AI session;
            // never fails the install (DASHR-HERDR-011).
            Build {
                command: dashr(&["global", "install", "--best-effort"]),
                platforms: None,
            },
            // The agent skill, installed for every coding agent with the `skills`
            // CLI straight from this repository, as herdr-remote-channel
            // does (DEC-039). It teaches the human's own AI session to open
            // the trace pane and drive it with `dashr`.
            Build {
                command: skills_add(&["cmd", "/c"]),
                platforms: Some(vec!["windows"]),
            },
            Build {
                command: skills_add(&[]),
                platforms: Some(vec!["linux", "macos"]),
            },
        ],
        startup: vec![Hook {
            command: dashr(&["herdr", "startup"]),
        }],
        actions: ACTIONS
            .iter()
            .map(|(id, title, description)| Action {
                id,
                title,
                description,
                contexts: vec!["workspace", "pane"],
                command: dashr(&["herdr", "action", id]),
            })
            .collect(),
        events: vec![Event {
            on: "pane.closed",
            command: dashr(&["herdr", "event", "pane-closed"]),
        }],
        panes: vec![
            Pane {
                id: "traces",
                title: "dashr",
                placement: "split",
                width: None,
                height: None,
                command: dashr(&["herdr", "pane", "traces"]),
            },
            Pane {
                id: "doctor",
                title: "dashr doctor",
                placement: "popup",
                width: Some("80%"),
                height: Some("70%"),
                command: dashr(&["herdr", "pane", "doctor"]),
            },
        ],
    }
}

const HEADER: &str = "\
# dashr: end-to-end testing by traces, beside your AI agent in Herdr.
# Generated by `dashr herdr manifest` from crates/dashr-herdr/src/manifest.rs.

";

/// The manifest text for `version`.
pub fn render(version: &'static str) -> String {
    let body = toml::to_string(&manifest(version)).expect("the manifest serializes");
    format!("{HEADER}{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_every_entrypoint() {
        let text = render("9.9.9");
        let parsed: toml::Value = toml::from_str(&text).expect("manifest is valid TOML");
        assert_eq!(parsed["id"].as_str(), Some(PLUGIN_ID));
        assert_eq!(parsed["version"].as_str(), Some("9.9.9"));
        assert_eq!(parsed["actions"].as_array().unwrap().len(), ACTIONS.len());
        assert_eq!(parsed["panes"].as_array().unwrap().len(), PANES.len());
        assert_eq!(parsed["events"][0]["on"].as_str(), Some("pane.closed"));
        assert!(parsed.get("link_handlers").is_none());
        assert_eq!(parsed["panes"][0]["id"].as_str(), Some("traces"));
        assert!(text.starts_with("# dashr: end-to-end testing by traces"));
    }

    #[test]
    fn installs_from_npm_on_every_platform_including_windows() {
        let text = render("9.9.9");
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        let platforms: Vec<&str> = parsed["platforms"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        assert_eq!(platforms, ["linux", "macos", "windows"]);
        let build = parsed["build"].as_array().unwrap();
        let argv = |i: usize| -> Vec<&str> {
            build[i]["command"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(toml::Value::as_str)
                .collect()
        };
        assert_eq!(
            argv(0),
            [
                "cmd",
                "/c",
                "npm",
                "install",
                "--prefix",
                ".",
                "--no-save",
                "--no-audit",
                "--no-fund",
                "herdr-dashr@9.9.9"
            ]
        );
        assert_eq!(build[0]["platforms"][0].as_str(), Some("windows"));
        assert_eq!(argv(1)[0], "npm");
        let skills = [
            "npx",
            "--yes",
            "skills",
            "add",
            "czinegeroland/herdr-dashr",
            "--skill",
            "herdr-dashr",
            "--global",
            "--yes",
        ];
        assert_eq!(
            argv(2),
            ["node", LAUNCHER, "global", "install", "--best-effort"]
        );
        assert!(build[2].get("platforms").is_none());
        assert_eq!(argv(3)[..2], ["cmd", "/c"]);
        assert_eq!(argv(3)[2..], skills);
        assert_eq!(build[3]["platforms"][0].as_str(), Some("windows"));
        assert_eq!(argv(4), skills);
        assert_eq!(build.len(), 5);
        // Every entry point runs the launcher with node: no shell, no .cmd.
        assert!(!text.contains("\"sh\""));
        assert!(!text.contains("bin/dashr"));
        assert_eq!(parsed["startup"][0]["command"][0].as_str(), Some("node"),);
    }

    #[test]
    fn ids_follow_herdr_rules() {
        // Local ids: ASCII letters, digits, colon, underscore, hyphen; no dots.
        let ok = |id: &str| {
            !id.is_empty()
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || ":_-".contains(c))
        };
        for id in ACTIONS.iter().map(|a| a.0).chain(PANES.iter().map(|p| p.0)) {
            assert!(ok(id), "{id}");
        }
    }
}

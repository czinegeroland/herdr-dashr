//! The agent skill that teaches dashboard building, shipped in the binary.
//!
//! The skill's files live in `.agents/skills/herdr-dashr/` and are embedded
//! at compile time, so the installed binary always carries the guidance that
//! matches its tools (requirement DASHR-SKILL-001). They reach the agent two
//! ways:
//!
//! * installed as a Claude Code skill (`~/.claude/skills/herdr-dashr/`) by
//!   the plugin's build step and refreshed by the dashboard pane when the
//!   binary is newer (DASHR-SKILL-002);
//! * served as MCP resources, for agents without skill support
//!   (DASHR-SKILL-003).
//!
//! dashr only ever overwrites or removes a skill directory it wrote itself:
//! the installed `SKILL.md` must carry the `generated-by: herdr-dashr`
//! marker, so a skill the user wrote or edited under the same name is left
//! alone unless they pass `--force`.

use std::path::{Path, PathBuf};

use dashr_core::ids::fnv1a64;

/// The skill's directory name.
pub const NAME: &str = "herdr-dashr";
/// The line that marks a skill directory as dashr's own.
pub const MARKER: &str = "generated-by: herdr-dashr";
/// The file recording which content is installed.
pub const STAMP: &str = ".dashr-skill";

/// Every file of the skill: path relative to the skill directory, contents.
pub const FILES: &[(&str, &str)] = &[
    (
        "SKILL.md",
        include_str!("../../../.agents/skills/herdr-dashr/SKILL.md"),
    ),
    (
        "reference/dashboard-json.md",
        include_str!("../../../.agents/skills/herdr-dashr/reference/dashboard-json.md"),
    ),
    (
        "reference/datasources.md",
        include_str!("../../../.agents/skills/herdr-dashr/reference/datasources.md"),
    ),
    (
        "reference/recipes.md",
        include_str!("../../../.agents/skills/herdr-dashr/reference/recipes.md"),
    ),
    (
        "reference/otel-and-logs.md",
        include_str!("../../../.agents/skills/herdr-dashr/reference/otel-and-logs.md"),
    ),
];

/// A fingerprint of the embedded content, written to [`STAMP`].
pub fn fingerprint() -> String {
    let mut bytes = Vec::new();
    for (path, contents) in FILES {
        bytes.extend_from_slice(path.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(contents.as_bytes());
        bytes.push(0);
    }
    format!("{:016x}", fnv1a64(&bytes))
}

/// What an install did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Installed(PathBuf),
    Updated(PathBuf),
    UpToDate(PathBuf),
    /// A skill of the same name that dashr did not write.
    SkippedForeign(PathBuf),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Installed(dir) => write!(
                formatter,
                "installed the herdr-dashr skill in {}",
                dir.display()
            ),
            Outcome::Updated(dir) => write!(
                formatter,
                "updated the herdr-dashr skill in {}",
                dir.display()
            ),
            Outcome::UpToDate(dir) => write!(
                formatter,
                "the herdr-dashr skill in {} is up to date",
                dir.display()
            ),
            Outcome::SkippedForeign(dir) => write!(
                formatter,
                "left {} alone: it was not written by dashr (use --force to replace it)",
                dir.display()
            ),
        }
    }
}

fn owned_by_dashr(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("SKILL.md"))
        .map(|text| text.contains(MARKER))
        .unwrap_or(false)
}

/// Installs or refreshes the skill in `<skills_dir>/herdr-dashr`.
pub fn install(skills_dir: &Path, force: bool) -> std::io::Result<Outcome> {
    let dir = skills_dir.join(NAME);
    let exists = dir.join("SKILL.md").exists();
    if exists && !force && !owned_by_dashr(&dir) {
        return Ok(Outcome::SkippedForeign(dir));
    }
    let stamp = fingerprint();
    if exists
        && std::fs::read_to_string(dir.join(STAMP)).is_ok_and(|installed| installed.trim() == stamp)
    {
        return Ok(Outcome::UpToDate(dir));
    }
    for (path, contents) in FILES {
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, contents)?;
    }
    std::fs::write(dir.join(STAMP), format!("{stamp}\n"))?;
    Ok(if exists {
        Outcome::Updated(dir)
    } else {
        Outcome::Installed(dir)
    })
}

/// Removes the skill, only when dashr wrote it. Returns whether it did.
pub fn uninstall(skills_dir: &Path) -> std::io::Result<bool> {
    let dir = skills_dir.join(NAME);
    if !owned_by_dashr(&dir) {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(true)
}

/// Expands a leading `~/` against `HOME`.
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// The MCP resource URI of a skill file.
pub fn resource_uri(path: &str) -> String {
    format!("dashr://guide/{path}")
}

/// The skill file behind a resource URI.
pub fn resource(uri: &str) -> Option<&'static str> {
    let path = uri.strip_prefix("dashr://guide/")?;
    FILES
        .iter()
        .find(|(name, _)| *name == path)
        .map(|(_, contents)| *contents)
}

/// The JSON blocks of a markdown file whose info string is `json dashr-example`.
pub fn examples(markdown: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = markdown;
    while let Some(start) = rest.find("```json dashr-example\n") {
        let body = &rest[start + "```json dashr-example\n".len()..];
        let Some(end) = body.find("\n```") else { break };
        out.push(&body[..end]);
        rest = &body[end..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashr_core::dashboard::{Pins, normalize};
    use serde_json::Value;

    #[test]
    fn skill_carries_the_marker_and_frontmatter() {
        let (name, skill) = FILES[0];
        assert_eq!(name, "SKILL.md");
        assert!(skill.starts_with("---\nname: herdr-dashr\n"));
        assert!(skill.contains(MARKER));
        assert!(skill.contains("description:"));
        for (path, _) in &FILES[1..] {
            assert!(skill.contains(path), "SKILL.md does not point at {path}");
        }
    }

    #[test]
    fn every_dashboard_example_is_valid() {
        let known: Vec<String> = ["dashr-testdata", "prometheus", "loki"]
            .map(String::from)
            .to_vec();
        let pins = Pins {
            uid: "u",
            refresh: "5s",
            time_from: "now-1h",
            known_datasources: &known,
        };
        let mut count = 0;
        for (path, contents) in FILES {
            for example in examples(contents) {
                let value: Value = serde_json::from_str(example)
                    .unwrap_or_else(|error| panic!("{path}: example is not JSON: {error}"));
                let normalized = normalize(&value, &pins)
                    .unwrap_or_else(|error| panic!("{path}: example rejected: {error}"));
                assert!(
                    normalized.warnings.is_empty(),
                    "{path}: {:?}",
                    normalized.warnings
                );
                count += 1;
            }
        }
        assert!(count >= 2, "found {count} examples");
    }

    #[test]
    fn every_query_model_snippet_is_json() {
        let (_, datasources) = FILES
            .iter()
            .find(|(path, _)| *path == "reference/datasources.md")
            .unwrap();
        let mut rest = *datasources;
        let mut values = 0;
        while let Some(start) = rest.find("```json\n") {
            let body = &rest[start + 8..];
            let end = body.find("\n```").unwrap();
            for value in serde_json::Deserializer::from_str(&body[..end]).into_iter::<Value>() {
                let value = value
                    .unwrap_or_else(|error| panic!("bad query model: {error}\n{}", &body[..end]));
                assert!(value.is_object());
                values += 1;
            }
            rest = &body[end..];
        }
        assert!(values >= 15, "found {values} query models");
    }

    #[test]
    fn install_refresh_and_uninstall_respect_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let first = install(dir.path(), false).unwrap();
        assert!(matches!(first, Outcome::Installed(_)));
        let installed = dir.path().join(NAME);
        for (path, contents) in FILES {
            assert_eq!(
                std::fs::read_to_string(installed.join(path)).unwrap(),
                *contents
            );
        }
        assert!(matches!(
            install(dir.path(), false).unwrap(),
            Outcome::UpToDate(_)
        ));
        // An older dashr's content is refreshed.
        std::fs::write(installed.join(STAMP), "0000\n").unwrap();
        assert!(matches!(
            install(dir.path(), false).unwrap(),
            Outcome::Updated(_)
        ));
        assert!(uninstall(dir.path()).unwrap());
        assert!(!installed.exists());

        // A skill the user wrote is never touched without --force.
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(
            installed.join("SKILL.md"),
            "---\nname: herdr-dashr\n---\nmine",
        )
        .unwrap();
        assert!(matches!(
            install(dir.path(), false).unwrap(),
            Outcome::SkippedForeign(_)
        ));
        assert!(!uninstall(dir.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(installed.join("SKILL.md")).unwrap(),
            "---\nname: herdr-dashr\n---\nmine"
        );
        assert!(matches!(
            install(dir.path(), true).unwrap(),
            Outcome::Updated(_)
        ));
        assert!(owned_by_dashr(&installed));
    }

    #[test]
    fn resources_map_to_files() {
        assert_eq!(resource(&resource_uri("SKILL.md")), Some(FILES[0].1));
        assert!(resource("dashr://guide/../etc/passwd").is_none());
        assert!(resource("file:///SKILL.md").is_none());
    }

    #[test]
    fn home_expansion() {
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                expand_home("~/.claude/skills"),
                Path::new(&home).join(".claude/skills")
            );
        }
        assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
    }
}

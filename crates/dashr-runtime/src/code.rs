//! The human's code editor behind the Spans tab (DASHR-VIEW-006).
//!
//! The viewer reads and saves source files under the session's code root:
//! the repository the agent works in, named by the span catalog or by the
//! directory `dashr spans set` / `dashr flow set` ran in. Nothing outside
//! that root is readable or writable (DASHR-SEC-006): paths are resolved,
//! symlinks followed, and the result must still be inside the root.
//! Saves are atomic and refused when the file changed since it was opened.

use std::path::{Component, Path, PathBuf};

use serde::Serialize;

/// Files larger than this are not opened in the browser.
pub const MAX_FILE: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct File {
    /// Relative to the root, with `/`.
    pub path: String,
    /// The absolute path, for "open in VS Code".
    pub absolute: String,
    pub content: String,
    /// Changes whenever the content does.
    pub version: String,
    pub language: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    NoRoot,
    NotFound(String),
    Outside,
    TooLarge,
    NotText,
    /// The file changed since the given version; carries the current one.
    Conflict(String),
    Io(String),
}

impl Error {
    pub fn status(&self) -> u16 {
        match self {
            Error::NoRoot | Error::NotFound(_) => 404,
            Error::Outside => 403,
            Error::TooLarge => 413,
            Error::NotText => 415,
            Error::Conflict(_) => 409,
            Error::Io(_) => 500,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Error::NoRoot => "no code root yet: the agent sets it with `dashr spans set`".into(),
            Error::NotFound(path) => format!("{path}: not found under the code root"),
            Error::Outside => "only files under the code root can be opened".into(),
            Error::TooLarge => format!("larger than {} MB", MAX_FILE / 1024 / 1024),
            Error::NotText => "not a UTF-8 text file".into(),
            Error::Conflict(_) => "the file changed on disk since you opened it; reload it".into(),
            Error::Io(error) => error.clone(),
        }
    }
}

/// FNV-1a over the bytes: a version tag, not a security measure.
pub fn version(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}-{}", bytes.len())
}

pub fn language(path: &str) -> &'static str {
    let extension = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match extension.as_str() {
        "cs" => "csharp",
        "fs" | "fsx" => "fsharp",
        "py" => "python",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "go" => "go",
        "rs" => "rust",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "scala" => "scala",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "ini",
        "xml" | "csproj" | "props" => "xml",
        "sh" | "bash" => "shell",
        "sql" => "sql",
        "md" => "markdown",
        _ => "plaintext",
    }
}

/// Lexically removes `.` and `..` (never above the start).
fn tidy(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// The file `requested` names under `root` (canonical). An absolute path
/// from another machine (`/src/app/Program.cs` in a container's
/// `code.filepath`) falls back to its longest suffix that exists here.
pub fn resolve(root: &Path, requested: &str) -> Result<PathBuf, Error> {
    let requested = requested.trim();
    if requested.is_empty() || requested.contains('\0') {
        return Err(Error::NotFound(requested.to_owned()));
    }
    let given = Path::new(requested);
    let mut tries: Vec<PathBuf> = Vec::new();
    // `has_root`: on Windows `/app/x.cs` is rooted but not absolute.
    if given.has_root() {
        tries.push(given.to_path_buf());
        let parts: Vec<_> = given
            .components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .collect();
        for start in 0..parts.len() {
            tries.push(root.join(parts[start..].iter().collect::<PathBuf>()));
        }
    } else {
        tries.push(root.join(tidy(given).ok_or(Error::Outside)?));
    }
    let mut outside = false;
    for candidate in tries {
        let Ok(real) = candidate.canonicalize() else {
            continue;
        };
        if !real.starts_with(root) {
            outside = true;
            continue;
        }
        if real.is_file() {
            return Ok(real);
        }
    }
    Err(if outside {
        Error::Outside
    } else {
        Error::NotFound(requested.to_owned())
    })
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn read(root: Option<&Path>, requested: &str) -> Result<File, Error> {
    let root = root.ok_or(Error::NoRoot)?;
    let path = resolve(root, requested)?;
    let bytes = std::fs::read(&path).map_err(|e| Error::Io(e.to_string()))?;
    if bytes.len() > MAX_FILE {
        return Err(Error::TooLarge);
    }
    let version = version(&bytes);
    let content = String::from_utf8(bytes).map_err(|_| Error::NotText)?;
    let rel = relative(root, &path);
    Ok(File {
        language: language(&rel),
        path: rel,
        // Windows canonical paths carry `\\?\`; editors want `C:\...`.
        absolute: path
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_owned(),
        content,
        version,
    })
}

/// Saves `content` over the file if it is still at `base_version`; returns
/// the new version and how many lines changed.
pub fn write(
    root: Option<&Path>,
    requested: &str,
    content: &str,
    base_version: &str,
) -> Result<(String, String, usize), Error> {
    let root = root.ok_or(Error::NoRoot)?;
    let path = resolve(root, requested)?;
    if content.len() > MAX_FILE {
        return Err(Error::TooLarge);
    }
    let current = std::fs::read(&path).map_err(|e| Error::Io(e.to_string()))?;
    let current_version = version(&current);
    if current_version != base_version {
        return Err(Error::Conflict(current_version));
    }
    let old = String::from_utf8_lossy(&current);
    let changed = changed_lines(&old, content);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.dashr-{}", std::process::id()));
    let permissions = std::fs::metadata(&path).map(|m| m.permissions()).ok();
    std::fs::write(&temporary, content.as_bytes())
        .and_then(|()| {
            if let Some(permissions) = permissions {
                std::fs::set_permissions(&temporary, permissions)?;
            }
            std::fs::rename(&temporary, &path)
        })
        .map_err(|e| {
            let _ = std::fs::remove_file(&temporary);
            Error::Io(format!("cannot save: {e}"))
        })?;
    Ok((relative(root, &path), version(content.as_bytes()), changed))
}

/// Lines that differ, after the common head and tail.
fn changed_lines(old: &str, new: &str) -> usize {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let head = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let tail = a[head..]
        .iter()
        .rev()
        .zip(b[head..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    (a.len() - head - tail).max(b.len() - head - tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/api")).unwrap();
        std::fs::write(root.join("src/api/Orders.cs"), "class Orders {\n}\n").unwrap();
        (dir, root)
    }

    #[test]
    fn files_are_read_relative_to_the_root() {
        let (_dir, root) = repo();
        let file = read(Some(&root), "src/api/Orders.cs").unwrap();
        assert_eq!(file.path, "src/api/Orders.cs");
        assert_eq!(file.language, "csharp");
        assert_eq!(file.version, version(b"class Orders {\n}\n"));
        let elsewhere = read(Some(&root), "/app/src/api/Orders.cs").unwrap();
        assert_eq!(
            elsewhere.path, "src/api/Orders.cs",
            "a container path falls back to its suffix"
        );
        assert_eq!(read(None, "x.cs").unwrap_err(), Error::NoRoot);
        assert!(matches!(
            read(Some(&root), "src/nope.cs"),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn nothing_outside_the_root() {
        let (dir, root) = repo();
        let secret = dir
            .path()
            .parent()
            .unwrap()
            .join(format!("dashr-secret-{}", std::process::id()));
        std::fs::write(&secret, "secret").unwrap();
        let name = secret.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            read(Some(&root), &format!("../{name}")).unwrap_err(),
            Error::Outside
        );
        assert_eq!(
            read(Some(&root), "src/../../x").unwrap_err(),
            Error::Outside
        );
        let absolute = secret
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(read(Some(&root), &absolute).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&secret, root.join("link.txt")).unwrap();
            assert_eq!(read(Some(&root), "link.txt").unwrap_err(), Error::Outside);
        }
        std::fs::remove_file(&secret).unwrap();
    }

    #[test]
    fn saves_are_atomic_and_refuse_stale_versions() {
        let (_dir, root) = repo();
        let file = read(Some(&root), "src/api/Orders.cs").unwrap();
        let (path, new_version, changed) = write(
            Some(&root),
            "src/api/Orders.cs",
            "class Orders {\n  int x;\n}\n",
            &file.version,
        )
        .unwrap();
        assert_eq!((path.as_str(), changed), ("src/api/Orders.cs", 1));
        assert_eq!(
            std::fs::read_to_string(root.join("src/api/Orders.cs")).unwrap(),
            "class Orders {\n  int x;\n}\n"
        );
        assert_eq!(new_version, version(b"class Orders {\n  int x;\n}\n"));
        let stale = write(Some(&root), "src/api/Orders.cs", "x", &file.version).unwrap_err();
        assert_eq!(stale, Error::Conflict(new_version));
        let leftovers: Vec<_> = std::fs::read_dir(root.join("src/api")).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temporary file left behind");
    }

    #[test]
    fn changed_line_counts() {
        assert_eq!(changed_lines("a\nb\nc", "a\nB\nc"), 1);
        assert_eq!(changed_lines("a\nc", "a\nb\nb2\nc"), 2);
        assert_eq!(changed_lines("same", "same"), 0);
    }
}

//! Scoped one-level directory browser. Gateway (`api_browse.rs`), CLI
//! (`src/browse.rs`), and the future TUI directory picker all reach the
//! same canonical implementation here.
//!
//! Hard-scoped to `<install>/shared/` — the only place skills, knowledge
//! bundles, and other host-wide content live. `..` traversal that escapes
//! the root is rejected before any I/O.

use std::path::PathBuf;

use serde::Serialize;

use zeroclaw_config::paths::{RootEscapeError, resolve_under};
use zeroclaw_config::schema::Config;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrowseEntry {
    pub name: String,
    /// `"dir"` or `"file"`. Symlinks resolve through their target.
    pub kind: &'static str,
    /// File size in bytes. `None` for directories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct BrowseResult {
    /// Path relative to `<install>/shared/` that the result describes.
    /// Useful for breadcrumb rendering.
    pub path: String,
    pub entries: Vec<BrowseEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum BrowseError {
    #[error(transparent)]
    Escape(#[from] RootEscapeError),
    #[error("path '{0}' does not exist")]
    NotFound(String),
    #[error("path '{0}' is not a directory")]
    NotADirectory(String),
    #[error("'{0}' is a system directory and cannot be removed via the dashboard")]
    Protected(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Browse one level of `<install>/shared/<raw>`. Returns entries sorted by
/// (kind, name) — directories first, then files, alphabetical within each.
pub fn list_directory(config: &Config, raw: &str) -> Result<BrowseResult, BrowseError> {
    let shared = config.shared_workspace_dir();
    let resolved: PathBuf = resolve_under(&shared, raw)?;

    let metadata = match std::fs::metadata(&resolved) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(BrowseError::NotFound(raw.to_string()));
        }
        Err(err) => return Err(err.into()),
    };
    if !metadata.is_dir() {
        return Err(BrowseError::NotADirectory(raw.to_string()));
    }

    let mut entries: Vec<BrowseEntry> = Vec::new();
    for child in std::fs::read_dir(&resolved)?.flatten() {
        let Ok(file_type) = child.file_type() else {
            continue;
        };
        let name = child.file_name().to_string_lossy().into_owned();
        if file_type.is_dir() {
            entries.push(BrowseEntry {
                name,
                kind: "dir",
                size: None,
            });
        } else if file_type.is_file() {
            let size = child.metadata().ok().map(|m| m.len());
            entries.push(BrowseEntry {
                name,
                kind: "file",
                size,
            });
        }
    }
    entries.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));

    Ok(BrowseResult {
        path: raw.trim_matches('/').to_string(),
        entries,
    })
}

/// Top-level shared/ entries that the runtime owns and the operator must
/// not be able to remove via the dashboard. Backend-enforced so a
/// compromised or buggy frontend cannot bypass this. Names match what
/// the install scaffolds via `migrate_legacy_workspace_to_default_agent`
/// and the `<install>/shared/` initializer.
const PROTECTED_SHARED_TOP_LEVEL: &[&str] = &["skills", "skill-bundles", "knowledge"];

/// Create a new directory at `<install>/shared/<raw>`. Idempotent — if the
/// path already exists as a directory, returns Ok without re-creating.
/// Rejects path traversal and refuses to create over an existing file.
pub fn make_directory(config: &Config, raw: &str) -> Result<(), BrowseError> {
    let shared = config.shared_workspace_dir();
    let resolved: PathBuf = resolve_under(&shared, raw)?;
    if let Ok(meta) = std::fs::metadata(&resolved) {
        if meta.is_dir() {
            return Ok(());
        }
        return Err(BrowseError::NotADirectory(raw.to_string()));
    }
    std::fs::create_dir_all(&resolved)?;
    Ok(())
}

/// Delete the directory at `<install>/shared/<raw>` recursively. Refuses
/// to remove protected top-level entries (skills/, skill-bundles/,
/// knowledge/) or the shared root itself. Rejects path traversal.
pub fn remove_directory(config: &Config, raw: &str) -> Result<(), BrowseError> {
    let trimmed = raw.trim_matches('/');
    if trimmed.is_empty() {
        return Err(BrowseError::Protected("shared".to_string()));
    }
    let top = trimmed.split('/').next().unwrap_or("");
    if PROTECTED_SHARED_TOP_LEVEL.contains(&top) && !trimmed.contains('/') {
        return Err(BrowseError::Protected(format!("shared/{top}")));
    }
    let shared = config.shared_workspace_dir();
    let resolved: PathBuf = resolve_under(&shared, raw)?;
    let metadata = match std::fs::metadata(&resolved) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(BrowseError::NotFound(raw.to_string()));
        }
        Err(err) => return Err(err.into()),
    };
    if !metadata.is_dir() {
        return Err(BrowseError::NotADirectory(raw.to_string()));
    }
    std::fs::remove_dir_all(&resolved)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("shared/skills/alpha")).unwrap();
        std::fs::create_dir_all(dir.path().join("shared/skills/beta")).unwrap();
        std::fs::write(dir.path().join("shared/readme.txt"), b"hi").unwrap();

        let cfg = Config {
            config_path: dir.path().join("config.toml"),
            ..Config::default()
        };
        (dir, cfg)
    }

    #[test]
    fn lists_shared_root_when_path_empty() {
        let (_dir, cfg) = fixture();
        let result = list_directory(&cfg, "").unwrap();
        assert_eq!(result.entries.len(), 2);
        assert_eq!(result.entries[0].name, "skills");
        assert_eq!(result.entries[0].kind, "dir");
        assert_eq!(result.entries[1].name, "readme.txt");
        assert_eq!(result.entries[1].kind, "file");
    }

    #[test]
    fn descends_one_level() {
        let (_dir, cfg) = fixture();
        let result = list_directory(&cfg, "skills").unwrap();
        let names: Vec<_> = result.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn rejects_escape() {
        let (_dir, cfg) = fixture();
        let err = list_directory(&cfg, "../etc").unwrap_err();
        assert!(matches!(err, BrowseError::Escape(_)));
    }

    #[test]
    fn errors_on_missing_path() {
        let (_dir, cfg) = fixture();
        let err = list_directory(&cfg, "ghost").unwrap_err();
        assert!(matches!(err, BrowseError::NotFound(_)));
    }

    #[test]
    fn errors_when_path_is_a_file() {
        let (_dir, cfg) = fixture();
        let err = list_directory(&cfg, "readme.txt").unwrap_err();
        assert!(matches!(err, BrowseError::NotADirectory(_)));
    }

    #[test]
    fn make_directory_creates_nested_path() {
        let (dir, cfg) = fixture();
        make_directory(&cfg, "skills/gamma/sub").unwrap();
        assert!(dir.path().join("shared/skills/gamma/sub").is_dir());
    }

    #[test]
    fn make_directory_is_idempotent() {
        let (_dir, cfg) = fixture();
        make_directory(&cfg, "skills/alpha").unwrap();
        make_directory(&cfg, "skills/alpha").unwrap();
    }

    #[test]
    fn make_directory_rejects_escape() {
        let (_dir, cfg) = fixture();
        let err = make_directory(&cfg, "../etc").unwrap_err();
        assert!(matches!(err, BrowseError::Escape(_)));
    }

    #[test]
    fn make_directory_refuses_over_existing_file() {
        let (_dir, cfg) = fixture();
        let err = make_directory(&cfg, "readme.txt").unwrap_err();
        assert!(matches!(err, BrowseError::NotADirectory(_)));
    }

    #[test]
    fn remove_directory_recursively_drops_subtree() {
        let (dir, cfg) = fixture();
        make_directory(&cfg, "skills/alpha/nested/deep").unwrap();
        remove_directory(&cfg, "skills/alpha").unwrap();
        assert!(!dir.path().join("shared/skills/alpha").exists());
        // sibling not touched
        assert!(dir.path().join("shared/skills/beta").is_dir());
    }

    #[test]
    fn remove_directory_refuses_protected_top_level() {
        let (_dir, cfg) = fixture();
        for name in ["skills", "skill-bundles", "knowledge"] {
            let err = remove_directory(&cfg, name).unwrap_err();
            assert!(
                matches!(err, BrowseError::Protected(_)),
                "must refuse to remove protected top-level '{name}', got {err:?}"
            );
        }
    }

    #[test]
    fn remove_directory_refuses_empty_path() {
        let (_dir, cfg) = fixture();
        let err = remove_directory(&cfg, "").unwrap_err();
        assert!(matches!(err, BrowseError::Protected(_)));
    }

    #[test]
    fn remove_directory_rejects_escape() {
        let (_dir, cfg) = fixture();
        let err = remove_directory(&cfg, "../etc").unwrap_err();
        assert!(matches!(err, BrowseError::Escape(_)));
    }

    #[test]
    fn remove_directory_errors_on_missing() {
        let (_dir, cfg) = fixture();
        let err = remove_directory(&cfg, "skills/ghost").unwrap_err();
        assert!(matches!(err, BrowseError::NotFound(_)));
    }

    #[test]
    fn remove_directory_allows_nested_under_protected_top_level() {
        // skills/ is protected, but skills/alpha is operator-owned.
        let (dir, cfg) = fixture();
        remove_directory(&cfg, "skills/alpha").unwrap();
        assert!(!dir.path().join("shared/skills/alpha").exists());
        assert!(dir.path().join("shared/skills").is_dir());
    }
}

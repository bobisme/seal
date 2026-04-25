//! Data format version detection and enforcement.
//!
//! Crit data format versions:
//! - v1: Single `.seal/events.jsonl` file for all reviews
//! - v2: Per-review event logs at `.seal/reviews/{review_id}/events.jsonl`
//!
//! Version is stored in `.seal/version` file.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

/// Current data format version.
pub const CURRENT_VERSION: u32 = 2;

/// Data format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataVersion {
    /// v1: Single events.jsonl file
    V1,
    /// v2: Per-review event logs
    V2,
}

impl DataVersion {
    #[must_use]
    pub const fn as_u32(&self) -> u32 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
        }
    }
}

/// Path to the version file within .seal/
fn version_file_path(seal_root: &Path) -> std::path::PathBuf {
    seal_root.join(".seal").join("version")
}

/// Path to the legacy events.jsonl file
fn legacy_events_path(seal_root: &Path) -> std::path::PathBuf {
    seal_root.join(".seal").join("events.jsonl")
}

/// Path to the reviews directory (v2)
fn reviews_dir_path(seal_root: &Path) -> std::path::PathBuf {
    seal_root.join(".seal").join("reviews")
}

/// Detect the data format version of a seal repository.
///
/// Detection logic:
/// - If .seal/version exists, read it
/// - If .seal/version missing but .seal/events.jsonl exists (non-empty) -> v1
/// - If .seal/version missing and no events.jsonl (or empty) -> new repo, treat as v2
pub fn detect_version(seal_root: &Path) -> Result<Option<DataVersion>> {
    let version_path = version_file_path(seal_root);
    let legacy_path = legacy_events_path(seal_root);
    let seal_dir = seal_root.join(".seal");

    // No .seal/ directory means not initialized
    if !seal_dir.exists() {
        return Ok(None);
    }

    // Check for explicit version file
    if version_path.exists() {
        let content = fs::read_to_string(&version_path)
            .with_context(|| format!("Failed to read version file: {}", version_path.display()))?;
        let version_num: u32 = content
            .trim()
            .parse()
            .with_context(|| format!("Invalid version number in {}", version_path.display()))?;
        return match version_num {
            1 => Ok(Some(DataVersion::V1)),
            2 => Ok(Some(DataVersion::V2)),
            _ => bail!("Unknown data format version: {version_num}"),
        };
    }

    // No version file - check for legacy events.jsonl
    if legacy_path.exists() {
        // Check if it has any content
        let metadata = fs::metadata(&legacy_path)
            .with_context(|| format!("Failed to read metadata: {}", legacy_path.display()))?;
        if metadata.len() > 0 {
            // Non-empty events.jsonl without version file = v1
            return Ok(Some(DataVersion::V1));
        }
    }

    // Check if reviews directory exists with content (v2 structure)
    let reviews_dir = reviews_dir_path(seal_root);
    if reviews_dir.exists() && reviews_dir.is_dir() {
        // Has reviews directory - assume v2 even without version file
        return Ok(Some(DataVersion::V2));
    }

    // Empty or new repo - will be v2 when initialized
    Ok(None)
}

/// Check that the repository is using v2 format, or fail with migration instructions.
///
/// Call this at the start of any command that reads/writes events.
pub fn require_v2(seal_root: &Path) -> Result<()> {
    match detect_version(seal_root)? {
        Some(DataVersion::V1) => {
            bail!(
                "This repository uses seal data format v1 (single events.jsonl).\n\
                 Run 'seal migrate' to upgrade to v2 (per-review event logs).\n\
                 \n\
                 Why migrate?\n\
                 - v2 eliminates merge conflicts between concurrent reviews\n\
                 - v2 works correctly with jj workspaces and maw\n\
                 - v2 prevents data loss during workspace operations"
            );
        }
        Some(DataVersion::V2) => Ok(()),
        None => {
            // Not initialized yet, or empty - v2 will be used on first write
            Ok(())
        }
    }
}

/// Write the version file to mark a repository as using v2 format.
pub fn write_version_file(seal_root: &Path, version: DataVersion) -> Result<()> {
    let version_path = version_file_path(seal_root);

    // Ensure .seal/ directory exists
    if let Some(parent) = version_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    fs::write(&version_path, format!("{}\n", version.as_u32()))
        .with_context(|| format!("Failed to write version file: {}", version_path.display()))?;

    Ok(())
}

/// Check if migration is needed (v1 -> v2).
pub fn needs_migration(seal_root: &Path) -> Result<bool> {
    Ok(detect_version(seal_root)? == Some(DataVersion::V1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_detect_version_no_seal_dir() {
        let dir = tempdir().unwrap();
        assert_eq!(detect_version(dir.path()).unwrap(), None);
    }

    #[test]
    fn test_detect_version_empty_seal_dir() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join(".seal")).unwrap();
        assert_eq!(detect_version(dir.path()).unwrap(), None);
    }

    #[test]
    fn test_detect_version_v1_legacy_events() {
        let dir = tempdir().unwrap();
        let seal_dir = dir.path().join(".seal");
        fs::create_dir(&seal_dir).unwrap();
        fs::write(seal_dir.join("events.jsonl"), "some content\n").unwrap();

        assert_eq!(detect_version(dir.path()).unwrap(), Some(DataVersion::V1));
    }

    #[test]
    fn test_detect_version_v1_empty_events_not_v1() {
        let dir = tempdir().unwrap();
        let seal_dir = dir.path().join(".seal");
        fs::create_dir(&seal_dir).unwrap();
        fs::write(seal_dir.join("events.jsonl"), "").unwrap();

        // Empty events.jsonl is not considered v1
        assert_eq!(detect_version(dir.path()).unwrap(), None);
    }

    #[test]
    fn test_detect_version_explicit_v1() {
        let dir = tempdir().unwrap();
        let seal_dir = dir.path().join(".seal");
        fs::create_dir(&seal_dir).unwrap();
        fs::write(seal_dir.join("version"), "1\n").unwrap();

        assert_eq!(detect_version(dir.path()).unwrap(), Some(DataVersion::V1));
    }

    #[test]
    fn test_detect_version_explicit_v2() {
        let dir = tempdir().unwrap();
        let seal_dir = dir.path().join(".seal");
        fs::create_dir(&seal_dir).unwrap();
        fs::write(seal_dir.join("version"), "2\n").unwrap();

        assert_eq!(detect_version(dir.path()).unwrap(), Some(DataVersion::V2));
    }

    #[test]
    fn test_detect_version_reviews_dir() {
        let dir = tempdir().unwrap();
        let seal_dir = dir.path().join(".seal");
        fs::create_dir_all(seal_dir.join("reviews")).unwrap();

        assert_eq!(detect_version(dir.path()).unwrap(), Some(DataVersion::V2));
    }

    #[test]
    fn test_require_v2_fails_on_v1() {
        let dir = tempdir().unwrap();
        let seal_dir = dir.path().join(".seal");
        fs::create_dir(&seal_dir).unwrap();
        fs::write(seal_dir.join("events.jsonl"), "some content\n").unwrap();

        let result = require_v2(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("seal migrate"));
    }

    #[test]
    fn test_require_v2_ok_on_v2() {
        let dir = tempdir().unwrap();
        let seal_dir = dir.path().join(".seal");
        fs::create_dir(&seal_dir).unwrap();
        fs::write(seal_dir.join("version"), "2\n").unwrap();

        assert!(require_v2(dir.path()).is_ok());
    }

    #[test]
    fn test_require_v2_ok_on_new_repo() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join(".seal")).unwrap();

        assert!(require_v2(dir.path()).is_ok());
    }

    #[test]
    fn test_write_version_file() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join(".seal")).unwrap();

        write_version_file(dir.path(), DataVersion::V2).unwrap();

        let content = fs::read_to_string(dir.path().join(".seal").join("version")).unwrap();
        assert_eq!(content.trim(), "2");
    }

    #[test]
    fn test_needs_migration() {
        let dir = tempdir().unwrap();
        let seal_dir = dir.path().join(".seal");
        fs::create_dir(&seal_dir).unwrap();

        // Empty repo doesn't need migration
        assert!(!needs_migration(dir.path()).unwrap());

        // v1 repo needs migration
        fs::write(seal_dir.join("events.jsonl"), "content\n").unwrap();
        assert!(needs_migration(dir.path()).unwrap());

        // After writing v2 version file, no migration needed
        fs::write(seal_dir.join("version"), "2\n").unwrap();
        assert!(!needs_migration(dir.path()).unwrap());
    }
}

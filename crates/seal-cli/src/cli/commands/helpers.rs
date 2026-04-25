//! Shared helpers for CLI commands.
//!
//! Provides centralized, version-aware database operations for all commands.

use anyhow::{bail, Result};
use std::path::Path;

use crate::cli::commands::init::{index_path, is_initialized, SEAL_DIR};
use seal_core::core::{CoreContext, SealServices};
use seal_core::projection::{sync_from_review_logs, ProjectionDb, ReviewDetail, ThreadDetail};
use seal_core::scm::ScmRepo;
use seal_core::version::{detect_version, require_v2, DataVersion};

/// Auto-migrate from legacy `.crit/` directory to `.seal/`.
///
/// If `.crit/` exists but `.seal/` does not, renames it and removes
/// the stale projection cache so it gets rebuilt with the new paths.
/// Also renames `.critignore` to `.sealignore` if present.
pub fn auto_migrate_crit_to_seal(repo_root: &Path) -> Result<()> {
    let old_dir = repo_root.join(".crit");
    let new_dir = repo_root.join(SEAL_DIR);

    if old_dir.exists() && !new_dir.exists() {
        std::fs::rename(&old_dir, &new_dir)?;
        eprintln!("Migrated .crit/ → .seal/ in {}", repo_root.display());

        // Remove stale projection cache
        let index = new_dir.join("index.db");
        if index.exists() {
            std::fs::remove_file(&index).ok();
        }
        let journal = new_dir.join("index.db-journal");
        if journal.exists() {
            std::fs::remove_file(&journal).ok();
        }
    }

    let old_ignore = repo_root.join(".critignore");
    let new_ignore = repo_root.join(".sealignore");
    if old_ignore.exists() && !new_ignore.exists() {
        std::fs::rename(&old_ignore, &new_ignore)?;
        eprintln!("Migrated .critignore → .sealignore");
    }

    Ok(())
}

/// Ensure seal is initialized in the given directory.
///
/// Automatically migrates `.crit/` → `.seal/` if needed.
pub fn ensure_initialized(repo_root: &Path) -> Result<()> {
    auto_migrate_crit_to_seal(repo_root)?;
    if !is_initialized(repo_root) {
        bail!("Not a seal repository. Run 'seal --agent <your-name> init' first.");
    }
    Ok(())
}

/// Get the path to the .seal directory.
#[must_use]
pub fn seal_dir(repo_root: &Path) -> std::path::PathBuf {
    repo_root.join(SEAL_DIR)
}

/// Open the projection database and sync from event logs (version-aware).
///
/// For v2 repos: Uses `sync_from_review_logs()` for timestamp-based sync
/// from per-review event logs.
///
/// For v1 repos: Fails with migration instructions.
///
/// This is the recommended way to get a synced projection database in commands.
pub fn open_and_sync(repo_root: &Path) -> Result<ProjectionDb> {
    ensure_initialized(repo_root)?;

    // Enforce v2 format
    require_v2(repo_root)?;

    // Open database and initialize schema
    let db = ProjectionDb::open(&index_path(repo_root))?;
    db.init_schema()?;

    // Sync from per-review event logs (v2)
    sync_from_review_logs(&db, repo_root)?;

    Ok(db)
}

/// Create a `SealServices` instance for the given repository root.
///
/// This is the recommended way to get service access in CLI commands.
/// Validates initialization, enforces v2 format, opens and syncs the projection.
pub fn open_services(repo_root: &Path) -> Result<SealServices> {
    ensure_initialized(repo_root)?;
    let ctx = CoreContext::new(repo_root, &index_path(repo_root))?;
    Ok(ctx.services()?)
}

/// Open the projection database and sync, allowing v1 format (for read-only operations).
///
/// Use this only for commands that need to read v1 data before migration.
/// Most commands should use `open_and_sync()` which enforces v2.
pub fn open_and_sync_any_version(repo_root: &Path) -> Result<ProjectionDb> {
    ensure_initialized(repo_root)?;

    let db = ProjectionDb::open(&index_path(repo_root))?;
    db.init_schema()?;

    match detect_version(repo_root)? {
        Some(DataVersion::V1) => {
            // v1: Use legacy sync
            use crate::cli::commands::init::events_path;
            use seal_core::log::open_or_create;
            use seal_core::projection::sync_from_log_with_backup;

            let log = open_or_create(&events_path(repo_root))?;
            let seal_dir = repo_root.join(SEAL_DIR);
            sync_from_log_with_backup(&db, &log, Some(&seal_dir))?;
        }
        Some(DataVersion::V2) | None => {
            // v2 or new: Use per-review sync
            sync_from_review_logs(&db, repo_root)?;
        }
    }

    Ok(db)
}

/// Get a review by ID, returning an error if not found.
pub fn get_review(seal_root: &Path, review_id: &str) -> Result<ReviewDetail> {
    let services = open_services(seal_root)?;
    services.reviews().get(review_id).map_err(|e| {
        if matches!(e, seal_core::core::CoreError::ReviewNotFound { .. }) {
            anyhow::anyhow!(
                "Review not found: {review_id}\n  To fix: seal --agent <your-name> reviews list"
            )
        } else {
            e.into()
        }
    })
}

/// Require a review to exist (for operations that need the review).
///
/// Returns the review if found, or an error with helpful message if not.
pub fn require_local_review(seal_root: &Path, review_id: &str) -> Result<ReviewDetail> {
    get_review(seal_root, review_id)
}

/// Get a thread by ID, returning an error if not found.
pub fn get_thread(seal_root: &Path, thread_id: &str) -> Result<ThreadDetail> {
    let services = open_services(seal_root)?;
    services.threads().get(thread_id).map_err(|e| {
        if matches!(e, seal_core::core::CoreError::ThreadNotFound { .. }) {
            anyhow::anyhow!(
                "Thread not found: {thread_id}\n  To fix: seal --agent <your-name> threads list <review_id>"
            )
        } else {
            e.into()
        }
    })
}

/// Resolve the best commit hash to anchor new review thread creation.
///
/// Priority order:
/// 1. `final_commit` if present
/// 2. Resolved commit for `scm_anchor`
/// 3. Resolved commit for legacy `jj_change_id`
/// 4. `initial_commit`
pub fn resolve_review_thread_commit(scm: &dyn ScmRepo, review: &ReviewDetail) -> String {
    review
        .final_commit
        .clone()
        .or_else(|| scm.commit_for_anchor(&review.scm_anchor).ok())
        .or_else(|| scm.commit_for_anchor(&review.jj_change_id).ok())
        .unwrap_or_else(|| review.initial_commit.clone())
}

/// Create a "review not found" error.
#[must_use]
pub fn review_not_found_error(_seal_root: &Path, review_id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "Review not found: {review_id}\n  To fix: seal --agent <your-name> reviews list"
    )
}

/// Create a "thread not found" error.
#[must_use]
pub fn thread_not_found_error(_seal_root: &Path, thread_id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "Thread not found: {thread_id}\n  To fix: seal --agent <your-name> threads list <review_id>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_ensure_initialized_fails_on_empty_dir() {
        let dir = tempdir().unwrap();
        let result = ensure_initialized(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("init"));
    }

    #[test]
    fn test_ensure_initialized_v2() {
        let dir = tempdir().unwrap();
        let seal = dir.path().join(".seal");
        fs::create_dir(&seal).unwrap();
        fs::write(seal.join("version"), "2\n").unwrap();
        fs::create_dir(seal.join("reviews")).unwrap();

        assert!(ensure_initialized(dir.path()).is_ok());
    }

    #[test]
    fn test_open_and_sync_rejects_v1() {
        let dir = tempdir().unwrap();
        let seal = dir.path().join(".seal");
        fs::create_dir(&seal).unwrap();
        fs::write(seal.join("events.jsonl"), "some content\n").unwrap();

        let result = open_and_sync(dir.path());
        match result {
            Err(e) => assert!(e.to_string().contains("seal migrate")),
            Ok(_) => panic!("Expected error for v1 repo"),
        }
    }

    #[test]
    fn test_open_and_sync_v2() {
        let dir = tempdir().unwrap();
        let seal = dir.path().join(".seal");
        fs::create_dir(&seal).unwrap();
        fs::write(seal.join("version"), "2\n").unwrap();
        fs::create_dir(seal.join("reviews")).unwrap();

        let result = open_and_sync(dir.path());
        assert!(result.is_ok());
    }

    #[test]
    fn test_open_and_sync_any_version_v1() {
        let dir = tempdir().unwrap();
        let seal = dir.path().join(".seal");
        fs::create_dir(&seal).unwrap();
        fs::write(seal.join("events.jsonl"), "").unwrap();

        // v1 should work with open_and_sync_any_version
        let result = open_and_sync_any_version(dir.path());
        assert!(result.is_ok());
    }

    #[test]
    fn test_auto_migrate_crit_to_seal() {
        let dir = tempdir().unwrap();
        let old = dir.path().join(".crit");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("version"), "2\n").unwrap();
        fs::create_dir(old.join("reviews")).unwrap();
        fs::write(old.join("index.db"), "stale").unwrap();

        // Also create .critignore
        fs::write(dir.path().join(".critignore"), ".seal/\n").unwrap();

        auto_migrate_crit_to_seal(dir.path()).unwrap();

        // .crit/ should be gone, .seal/ should exist
        assert!(!dir.path().join(".crit").exists());
        assert!(dir.path().join(".seal").exists());
        assert!(dir.path().join(".seal").join("version").exists());
        assert!(dir.path().join(".seal").join("reviews").exists());
        // stale index.db should be removed
        assert!(!dir.path().join(".seal").join("index.db").exists());
        // .critignore -> .sealignore
        assert!(!dir.path().join(".critignore").exists());
        assert!(dir.path().join(".sealignore").exists());
    }

    #[test]
    fn test_auto_migrate_noop_when_seal_exists() {
        let dir = tempdir().unwrap();
        // Both exist — should not touch either
        let old = dir.path().join(".crit");
        let new = dir.path().join(".seal");
        fs::create_dir(&old).unwrap();
        fs::create_dir(&new).unwrap();
        fs::write(new.join("version"), "2\n").unwrap();
        fs::create_dir(new.join("reviews")).unwrap();

        auto_migrate_crit_to_seal(dir.path()).unwrap();

        // Both should still exist
        assert!(dir.path().join(".crit").exists());
        assert!(dir.path().join(".seal").exists());
    }

    #[test]
    fn test_ensure_initialized_auto_migrates() {
        let dir = tempdir().unwrap();
        let old = dir.path().join(".crit");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("version"), "2\n").unwrap();
        fs::create_dir(old.join("reviews")).unwrap();

        // ensure_initialized should auto-migrate and succeed
        assert!(ensure_initialized(dir.path()).is_ok());
        assert!(!dir.path().join(".crit").exists());
        assert!(dir.path().join(".seal").exists());
    }
}

//! Implementation of `seal sync` command.

use anyhow::{bail, Result};
use serde::Serialize;
use std::path::Path;

use crate::cli::commands::helpers::{ensure_initialized, open_services};
use crate::output::{Formatter, OutputFormat};
use seal_core::projection::SyncReport;

/// Serializable output for the sync command.
#[derive(Serialize)]
struct SyncOutput {
    action: String,
    events_applied: usize,
    files_synced: usize,
    files_skipped: usize,
    anomalies: Vec<AnomalyOutput>,
}

/// Serializable anomaly output.
#[derive(Serialize)]
struct AnomalyOutput {
    review_id: String,
    kind: String,
    detail: String,
}

impl SyncOutput {
    fn from_report(action: &str, report: &SyncReport) -> Self {
        Self {
            action: action.to_string(),
            events_applied: report.applied,
            files_synced: report.files_synced,
            files_skipped: report.files_skipped,
            anomalies: report
                .anomalies
                .iter()
                .map(|a| AnomalyOutput {
                    review_id: a.review_id.clone(),
                    kind: format!("{:?}", a.kind),
                    detail: a.detail.clone(),
                })
                .collect(),
        }
    }
}

/// Serializable output for rebuild mode (includes rebuild event count + sync report).
#[derive(Serialize)]
struct RebuildOutput {
    action: String,
    events_rebuilt: usize,
    events_applied: usize,
    files_synced: usize,
    files_skipped: usize,
    anomalies: Vec<AnomalyOutput>,
}

/// Run the sync command.
pub fn run_sync(
    seal_root: &Path,
    rebuild: bool,
    accept_regression: Option<String>,
    format: OutputFormat,
) -> Result<()> {
    if rebuild && accept_regression.is_some() {
        bail!("Cannot use --rebuild and --accept-regression together.\n  Use --rebuild for full rebuild, or --accept-regression <review-id> for a single file.");
    }

    ensure_initialized(seal_root)?;

    let services = open_services(seal_root)?;
    let formatter = Formatter::new(format);

    if rebuild {
        // Full destructive rebuild
        let rebuild_result = services.sync().rebuild()?;

        let output = RebuildOutput {
            action: "rebuild".to_string(),
            events_rebuilt: rebuild_result.events_rebuilt,
            events_applied: rebuild_result.sync_report.applied,
            files_synced: rebuild_result.sync_report.files_synced,
            files_skipped: rebuild_result.sync_report.files_skipped,
            anomalies: rebuild_result
                .sync_report
                .anomalies
                .iter()
                .map(|a| AnomalyOutput {
                    review_id: a.review_id.clone(),
                    kind: format!("{:?}", a.kind),
                    detail: a.detail.clone(),
                })
                .collect(),
        };
        formatter.print(&output)?;
    } else if let Some(review_id) = accept_regression {
        // Re-baseline a single review file
        let report = services.sync().accept_regression(&review_id)?;

        let output = SyncOutput::from_report("accept-regression", &report);
        formatter.print(&output)?;
    } else {
        // Normal sync
        let report = services.sync().sync()?;

        let output = SyncOutput::from_report("sync", &report);
        formatter.print(&output)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands::init::index_path;
    use seal_core::events::{CodeSelection, Event, EventEnvelope, ReviewCreated, ThreadCreated};
    use seal_core::log::{AppendLog, ReviewLog};
    use seal_core::projection::{sync_from_review_logs, ProjectionDb};
    use tempfile::tempdir;

    /// Set up a v2 seal repository with one review containing events.
    fn setup_v2_repo_with_review(seal_root: &Path, review_id: &str) -> ProjectionDb {
        let seal_dir = seal_root.join(".seal");
        std::fs::create_dir_all(seal_dir.join("reviews")).unwrap();
        std::fs::write(seal_dir.join("version"), "2\n").unwrap();

        // Write review events
        let log = ReviewLog::new(seal_root, review_id).unwrap();
        log.append(&EventEnvelope::new(
            "test-author",
            Event::ReviewCreated(ReviewCreated {
                review_id: review_id.to_string(),
                jj_change_id: "change123".to_string(),
                scm_kind: Some("jj".to_string()),
                scm_anchor: Some("change123".to_string()),
                initial_commit: "commit456".to_string(),
                base_commit: None,
                title: format!("Review {review_id}"),
                description: Some("Test description".to_string()),
            }),
        ))
        .unwrap();
        log.append(&EventEnvelope::new(
            "test-author",
            Event::ThreadCreated(ThreadCreated {
                thread_id: format!("{review_id}-th1"),
                review_id: review_id.to_string(),
                file_path: "src/main.rs".to_string(),
                selection: CodeSelection::range(10, 20),
                commit_hash: "abc123".to_string(),
            }),
        ))
        .unwrap();

        let db = ProjectionDb::open(&index_path(seal_root)).unwrap();
        db.init_schema().unwrap();
        db
    }

    #[test]
    fn test_run_sync_basic() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();
        let _db = setup_v2_repo_with_review(seal_root, "cr-sync1");

        // Run sync command
        let result = run_sync(seal_root, false, None, OutputFormat::Text);
        assert!(result.is_ok(), "sync should succeed: {:?}", result.err());

        // Verify data was synced by opening the db again
        let db = ProjectionDb::open(&index_path(seal_root)).unwrap();
        db.init_schema().unwrap();
        let review_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
            .unwrap();
        assert_eq!(review_count, 1, "should have 1 review after sync");

        let thread_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(thread_count, 1, "should have 1 thread after sync");
    }

    #[test]
    fn test_run_sync_rebuild() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();
        let db = setup_v2_repo_with_review(seal_root, "cr-rebuild1");

        // Initial sync to populate data
        sync_from_review_logs(&db, seal_root).unwrap();

        // Verify data exists
        let review_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
            .unwrap();
        assert_eq!(review_count, 1);

        // Drop db handle so rebuild can open fresh
        drop(db);

        // Run rebuild
        let result = run_sync(seal_root, true, None, OutputFormat::Text);
        assert!(result.is_ok(), "rebuild should succeed: {:?}", result.err());

        // Verify data still exists after rebuild
        let db = ProjectionDb::open(&index_path(seal_root)).unwrap();
        db.init_schema().unwrap();
        let review_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
            .unwrap();
        assert_eq!(review_count, 1, "should still have 1 review after rebuild");

        // Verify review_file_state was re-populated
        let state_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM review_file_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            state_count, 1,
            "review_file_state should have 1 row after rebuild+sync"
        );
    }

    #[test]
    fn test_run_sync_accept_regression() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();
        let db = setup_v2_repo_with_review(seal_root, "cr-regress1");

        // Initial sync
        sync_from_review_logs(&db, seal_root).unwrap();

        // Verify review_file_state exists
        let state_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM review_file_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state_count, 1, "should have file state after sync");

        // Simulate a regression by corrupting review_file_state
        // Set a higher line_count than actual file so sync will see it as "shrunk"
        db.conn()
            .execute(
                "UPDATE review_file_state SET line_count = 999 WHERE review_id = ?",
                rusqlite::params!["cr-regress1"],
            )
            .unwrap();

        // Drop db so accept_regression can open fresh
        drop(db);

        // Run accept-regression
        let result = run_sync(
            seal_root,
            false,
            Some("cr-regress1".to_string()),
            OutputFormat::Text,
        );
        assert!(
            result.is_ok(),
            "accept-regression should succeed: {:?}",
            result.err()
        );

        // Verify review_file_state was re-populated with correct values
        let db = ProjectionDb::open(&index_path(seal_root)).unwrap();
        db.init_schema().unwrap();
        let line_count: i64 = db
            .conn()
            .query_row(
                "SELECT line_count FROM review_file_state WHERE review_id = ?",
                rusqlite::params!["cr-regress1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            line_count, 2,
            "should have 2 lines (not 999) after re-baseline"
        );
    }

    #[test]
    fn test_run_sync_rebuild_and_accept_regression_mutually_exclusive() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();
        let _db = setup_v2_repo_with_review(seal_root, "cr-both");

        let result = run_sync(
            seal_root,
            true,
            Some("cr-both".to_string()),
            OutputFormat::Text,
        );
        assert!(result.is_err(), "should error when both flags provided");
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Cannot use --rebuild and --accept-regression together"),);
    }
}

//! Projection engine for botseal.
//!
//! Projects events from the append-only log into a queryable database.
//! The database is ephemeral and can be rebuilt from the event log at any time.

#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::doc_markdown)]

mod query;

pub use query::{
    Comment, InboxSummary, OpenThreadOnMyReview, ReviewAwaitingVote, ReviewDetail, ReviewSummary,
    ReviewerVote, ThreadDetail, ThreadSummary, ThreadWithNewResponses,
};

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::events::{
    CodeSelection, CommentAdded, Event, EventEnvelope, ReviewAbandoned, ReviewApproved,
    ReviewCreated, ReviewMerged, ReviewerVoted, ReviewersRequested, ThreadCreated, ThreadReopened,
    ThreadResolved,
};
use crate::log::{list_review_ids, read_all_reviews, AppendLog, ReviewLog};
use crate::scm::BackendDetection;

static EMITTED_WARNING_KEYS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HistoryBackend {
    Git,
    Jj,
    Unknown,
}

fn history_backend(seal_root: Option<&Path>) -> HistoryBackend {
    let Some(seal_root) = seal_root else {
        return HistoryBackend::Unknown;
    };

    let detection = BackendDetection::detect(seal_root);
    if detection.jj_root.is_some() {
        HistoryBackend::Jj
    } else if detection.git_root.is_some() {
        HistoryBackend::Git
    } else {
        HistoryBackend::Unknown
    }
}

fn history_lookup_command(seal_root: Option<&Path>, path: &str) -> Option<String> {
    match history_backend(seal_root) {
        HistoryBackend::Git => Some(format!("git log --follow -p -- {path}")),
        HistoryBackend::Jj => Some(format!("jj file annotate {path}")),
        HistoryBackend::Unknown => None,
    }
}

fn review_log_history_path(review_id: Option<&str>) -> String {
    match review_id {
        Some(review_id) => format!(".seal/reviews/{review_id}/events.jsonl"),
        None => ".seal/reviews/<review_id>/events.jsonl".to_string(),
    }
}

fn rebuild_recovery_hint(seal_root: Option<&Path>) -> String {
    let path = review_log_history_path(None);
    if let Some(command) = history_lookup_command(seal_root, &path) {
        format!(
            "These reviews were in index.db but not in the restored review event logs. Check repository history for older versions using: {command}"
        )
    } else {
        format!(
            "These reviews were in index.db but not in the restored review event logs. Check repository history for older versions of {path}."
        )
    }
}

fn should_emit_warning_once(key: String) -> bool {
    let emitted = EMITTED_WARNING_KEYS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut emitted = emitted
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    emitted.insert(key)
}

fn emit_orphaned_reviews_warning(seal_root: Option<&Path>, review_ids: &[String]) {
    if review_ids.is_empty() {
        return;
    }

    let repo_key = seal_root.map_or_else(
        || "<unknown>".to_string(),
        |path| path.display().to_string(),
    );
    let key = format!("orphaned-reviews:{repo_key}");
    if !should_emit_warning_once(key) {
        return;
    }

    eprintln!("WARNING: review log(s) have events but no ReviewCreated - skipping orphaned events");
    for review_id in review_ids.iter().take(3) {
        eprintln!("  {review_id}");
    }
    if review_ids.len() > 3 {
        eprintln!("  ... and {} more", review_ids.len() - 3);
    }
    let path = review_log_history_path(review_ids.first().map(String::as_str));
    if let Some(command) = history_lookup_command(seal_root, &path) {
        eprintln!("  Use `{command}` to find lost reviews in history");
    }
}

fn emit_stale_review_logs_warning(seal_root: &Path, anomalies: &[SyncAnomaly]) {
    if anomalies.is_empty() {
        return;
    }

    let mut anomaly_lines: Vec<String> = anomalies
        .iter()
        .map(|anomaly| format!("{}: {}", anomaly.review_id, anomaly.detail))
        .collect();
    anomaly_lines.sort();

    let key = format!(
        "stale-review-logs:{}:{}",
        seal_root.display(),
        anomaly_lines.join("|")
    );
    if !should_emit_warning_once(key) {
        return;
    }

    let header = match history_backend(Some(seal_root)) {
        HistoryBackend::Jj => "review event file(s) appear stale (likely jj workspace sync)",
        HistoryBackend::Git | HistoryBackend::Unknown => "review event file(s) appear stale",
    };
    tracing::warn!("{header}");
    for line in anomaly_lines {
        tracing::warn!("  {line}");
    }
    tracing::warn!("Projection data preserved. To investigate:");
    let history_path = if anomalies.len() == 1 {
        review_log_history_path(Some(&anomalies[0].review_id))
    } else {
        review_log_history_path(None)
    };
    if let Some(command) = history_lookup_command(Some(seal_root), &history_path) {
        tracing::warn!("  {command}");
    } else {
        tracing::warn!("  Inspect repository history for .seal/reviews/<review_id>/events.jsonl");
    }
    tracing::warn!("To force rebuild from current files:");
    tracing::warn!("  seal sync --rebuild");
}

// ============================================================================
// Sync report types
// ============================================================================

/// Result of a per-file monotonic sync operation.
#[derive(Debug, Serialize)]
pub struct SyncReport {
    /// Number of events applied to the projection.
    pub applied: usize,
    /// Number of review files that were synced (new or grew).
    pub files_synced: usize,
    /// Number of review files skipped (unchanged).
    pub files_skipped: usize,
    /// Anomalies detected during sync.
    pub anomalies: Vec<SyncAnomaly>,
}

/// An anomaly detected during per-file sync.
#[derive(Debug, Serialize)]
pub struct SyncAnomaly {
    /// The review ID of the affected file.
    pub review_id: String,
    /// What kind of anomaly was detected.
    pub kind: AnomalyKind,
    /// Human-readable detail about the anomaly.
    pub detail: String,
}

/// The kind of anomaly detected during sync.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub enum AnomalyKind {
    /// File has fewer lines than previously synced.
    Shrunk,
    /// Prefix hash of previously synced lines no longer matches.
    HashMismatch,
    /// File disappeared from disk but projection data exists.
    Missing,
    /// File could not be parsed or applying events failed.
    ParseError,
}

/// Database for projected state from events.
pub struct ProjectionDb {
    conn: Connection,
}

impl ProjectionDb {
    /// Open or create a projection database at the given path.
    ///
    /// Creates parent directories if they don't exist.
    pub fn open(path: &Path) -> Result<Self> {
        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("Failed to create parent directories: {}", parent.display())
                })?;
            }
        }

        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open database: {}", path.display()))?;

        // Enable foreign keys for integrity (optional, see schema notes)
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .context("Failed to enable foreign keys")?;

        Ok(Self { conn })
    }

    /// Create an in-memory projection database.
    ///
    /// Used for temporary merged projections (e.g., `inbox --all-workspaces`)
    /// and in tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("Failed to open in-memory database")?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .context("Failed to enable foreign keys")?;
        Ok(Self { conn })
    }

    /// Initialize the database schema.
    ///
    /// Creates all tables, indexes, and views if they don't exist.
    /// Also runs any necessary migrations for schema changes.
    pub fn init_schema(&self) -> Result<()> {
        self.conn
            .execute_batch(SCHEMA_SQL)
            .context("Failed to initialize schema")?;
        self.migrate_schema()?;
        Ok(())
    }

    /// Run schema migrations for any changes since the database was created.
    ///
    /// SQLite's CREATE TABLE IF NOT EXISTS doesn't add new columns to existing
    /// tables. This migration adds any columns that were added after the initial
    /// schema was created.
    fn migrate_schema(&self) -> Result<()> {
        // Check if next_comment_number column exists in threads table
        let has_column: bool = self
            .conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('threads') WHERE name = 'next_comment_number'",
                [],
                |row| row.get(0),
            )
            .context("Failed to check for next_comment_number column")?;

        if !has_column {
            self.conn
                .execute(
                    "ALTER TABLE threads ADD COLUMN next_comment_number INTEGER NOT NULL DEFAULT 1",
                    [],
                )
                .context("Failed to add next_comment_number column to threads")?;
        }

        let has_scm_kind: bool = self
            .conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('reviews') WHERE name = 'scm_kind'",
                [],
                |row| row.get(0),
            )
            .context("Failed to check for scm_kind column")?;

        if !has_scm_kind {
            self.conn
                .execute(
                    "ALTER TABLE reviews ADD COLUMN scm_kind TEXT NOT NULL DEFAULT 'jj'",
                    [],
                )
                .context("Failed to add scm_kind column to reviews")?;
        }

        let has_scm_anchor: bool = self
            .conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('reviews') WHERE name = 'scm_anchor'",
                [],
                |row| row.get(0),
            )
            .context("Failed to check for scm_anchor column")?;

        if !has_scm_anchor {
            self.conn
                .execute("ALTER TABLE reviews ADD COLUMN scm_anchor TEXT", [])
                .context("Failed to add scm_anchor column to reviews")?;
        }

        self.conn
            .execute(
                "UPDATE reviews SET scm_anchor = jj_change_id WHERE scm_anchor IS NULL OR scm_anchor = ''",
                [],
            )
            .context("Failed to backfill scm_anchor values")?;

        self.conn
            .execute(
                "UPDATE reviews SET scm_kind = 'jj' WHERE scm_kind IS NULL OR scm_kind = ''",
                [],
            )
            .context("Failed to backfill scm_kind values")?;

        self.conn
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_reviews_scm_anchor ON reviews(scm_kind, scm_anchor)",
                [],
            )
            .context("Failed to create idx_reviews_scm_anchor index")?;

        self.conn
            .execute_batch(REFRESH_VIEWS_SQL)
            .context("Failed to refresh projection views")?;

        // Add per-review file tracking table for monotonic sync (bd-jw3)
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS review_file_state (
                    review_id TEXT PRIMARY KEY,
                    line_count INTEGER NOT NULL,
                    byte_count INTEGER NOT NULL,
                    prefix_hash TEXT NOT NULL
                );",
            )
            .context("Failed to create review_file_state table")?;

        Ok(())
    }

    /// Get the last successfully processed line number from the event log.
    ///
    /// Returns 0 if no events have been processed yet.
    pub fn get_last_sync_line(&self) -> Result<usize> {
        let line: Option<i64> = self
            .conn
            .query_row(
                "SELECT last_line_number FROM sync_state WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .context("Failed to query sync_state")?;

        Ok(line.map_or(0, |l| l as usize))
    }

    /// Get the stored content hash of the event log prefix.
    ///
    /// Returns `None` if no hash has been stored yet.
    pub fn get_events_file_hash(&self) -> Result<Option<String>> {
        let hash: Option<String> = self
            .conn
            .query_row(
                "SELECT events_file_hash FROM sync_state WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .context("Failed to query events_file_hash")?
            .flatten();

        Ok(hash)
    }

    /// Update the last successfully processed line number.
    pub fn set_last_sync_line(&self, line: usize) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "UPDATE sync_state SET last_line_number = ?, last_sync_ts = ? WHERE id = 1",
                params![line as i64, now],
            )
            .context("Failed to update sync_state")?;
        Ok(())
    }

    /// Get a reference to the underlying connection (for advanced queries).
    #[must_use]
    pub const fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Delete the file state for a specific review (for --accept-regression).
    pub fn delete_review_file_state(&self, review_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM review_file_state WHERE review_id = ?",
            params![review_id],
        )?;
        Ok(())
    }
}

/// Sync the projection database from the event log.
///
/// Reads events starting from the last processed line and applies them
/// to the database. Returns the number of events processed.
///
/// Detects file replacement (e.g., from jj working copy restoration) using
/// two checks:
/// 1. **Truncation**: `last_sync_line > total_lines()` — file got shorter.
/// 2. **Content hash**: prefix hash of lines 0..last_sync_line changed —
///    file was replaced with same-or-longer content but different history.
///
/// Either triggers a full rebuild from scratch.
pub fn sync_from_log(db: &ProjectionDb, log: &impl AppendLog) -> Result<usize> {
    sync_from_log_with_backup(db, log, None)
}

/// Sync the projection database with optional orphan backup on truncation.
///
/// When `seal_dir` is provided and truncation/mismatch is detected, saves
/// orphaned review IDs to `.seal/orphaned-reviews-{timestamp}.json` before
/// rebuilding. This allows recovery of lost reviews from jj workspace history.
pub fn sync_from_log_with_backup(
    db: &ProjectionDb,
    log: &impl AppendLog,
    seal_dir: Option<&Path>,
) -> Result<usize> {
    let last_line = db.get_last_sync_line()?;

    if last_line > 0 {
        let total = log.total_lines()?;

        // Check 1: Truncation — file has fewer lines than our sync cursor.
        if last_line > total {
            eprintln!(
                "WARNING: events.jsonl truncated (expected >={last_line} lines, found {total}). Rebuilding projection."
            );
            return rebuild_projection_with_orphan_detection(db, log, seal_dir);
        }

        // Check 2: Content hash — the prefix we already processed changed.
        let stored_hash = db.get_events_file_hash()?;
        if let Some(ref expected) = stored_hash {
            if let Some(ref actual) = log.prefix_hash(last_line)? {
                if expected != actual {
                    eprintln!(
                        "WARNING: events.jsonl content changed (hash mismatch at line {last_line}). Rebuilding projection."
                    );
                    return rebuild_projection_with_orphan_detection(db, log, seal_dir);
                }
            }
        }
    }

    let events = log.read_from(last_line)?;

    if events.is_empty() {
        // Even if no new events, store hash if we don't have one yet
        // (backfill for databases created before hash tracking).
        if last_line > 0 && db.get_events_file_hash()?.is_none() {
            if let Some(hash) = log.prefix_hash(last_line)? {
                db.conn
                    .execute(
                        "UPDATE sync_state SET events_file_hash = ? WHERE id = 1",
                        params![hash],
                    )
                    .context("Failed to backfill events_file_hash")?;
            }
        }
        return Ok(0);
    }

    let count = events.len();

    // Advance cursor to the actual file line count (not just event count).
    // read_from skips by line index (counting all lines including empty),
    // but events.len() only counts non-empty parsed lines. Using total_lines()
    // ensures the cursor stays consistent when empty lines are present.
    let new_line = log.total_lines()?;

    // Compute new prefix hash covering all processed lines
    let new_hash = log.prefix_hash(new_line)?;

    // Process events in a transaction for atomicity
    let tx = db
        .conn
        .unchecked_transaction()
        .context("Failed to begin transaction")?;

    for event in &events {
        apply_event_inner(&tx, event).with_context(|| {
            format!(
                "Failed to apply event at line {} (type: {:?})",
                last_line,
                event_type_name(&event.event)
            )
        })?;
    }

    // Update sync state to point past all processed events
    let now = Utc::now().to_rfc3339();
    tx.execute(
        "UPDATE sync_state SET last_line_number = ?, last_sync_ts = ?, events_file_hash = ? WHERE id = 1",
        params![new_line as i64, now, new_hash],
    )
    .context("Failed to update sync_state")?;

    tx.commit().context("Failed to commit transaction")?;

    Ok(count)
}

/// Rebuild the projection from scratch by wiping all data and re-applying
/// all events from the log.
///
/// Called when file replacement is detected — either truncation
/// (last_sync_line > total file lines) or content hash mismatch
/// (same line count but different content).
#[allow(dead_code)]
fn rebuild_projection(db: &ProjectionDb, log: &impl AppendLog) -> Result<usize> {
    let events = log.read_all()?;
    let count = events.len();

    // Compute hash of the full file for the new sync state
    let new_hash = log.prefix_hash(count)?;

    let tx = db
        .conn
        .unchecked_transaction()
        .context("Failed to begin rebuild transaction")?;

    // Wipe all projection data (order matters for foreign keys)
    tx.execute_batch(
        "DELETE FROM comments;
         DELETE FROM threads;
         DELETE FROM reviewer_votes;
         DELETE FROM review_reviewers;
         DELETE FROM reviews;",
    )
    .context("Failed to wipe projection tables")?;

    // Re-apply all events
    for event in &events {
        apply_event_inner(&tx, event).with_context(|| {
            format!(
                "Failed to apply event during rebuild: {:?}",
                event_type_name(&event.event)
            )
        })?;
    }

    // Update sync state with line count and content hash
    let now = Utc::now().to_rfc3339();
    tx.execute(
        "UPDATE sync_state SET last_line_number = ?, last_sync_ts = ?, events_file_hash = ? WHERE id = 1",
        params![count as i64, now, new_hash],
    )
    .context("Failed to update sync_state after rebuild")?;

    tx.commit().context("Failed to commit rebuild")?;

    Ok(count)
}

/// Rebuild projection with orphan detection and backup.
///
/// Before wiping the projection, extracts current review IDs from the database.
/// After rebuilding from the new file, identifies which reviews were lost (orphaned)
/// and writes them to a timestamped backup file in the seal directory.
fn rebuild_projection_with_orphan_detection(
    db: &ProjectionDb,
    log: &impl AppendLog,
    seal_dir: Option<&Path>,
) -> Result<usize> {
    // Step 1: Capture current review IDs before rebuild
    let old_review_ids: Vec<String> = db
        .conn
        .prepare("SELECT review_id FROM reviews")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("Failed to query existing review IDs")?;

    // Step 2: Read new events and compute hash
    let events = log.read_all()?;
    let count = events.len();
    let new_hash = log.prefix_hash(count)?;

    // Step 3: Find which review IDs will exist after rebuild
    let new_review_ids: std::collections::HashSet<String> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ReviewCreated(rc) => Some(rc.review_id.clone()),
            _ => None,
        })
        .collect();

    // Step 4: Identify orphaned reviews (in old DB but not in new file)
    let orphaned: Vec<&String> = old_review_ids
        .iter()
        .filter(|id| !new_review_ids.contains(*id))
        .collect();

    // Step 5: If orphans exist and seal_dir provided, write backup
    if !orphaned.is_empty() {
        eprintln!(
            "WARNING: {} review(s) will be lost: {}",
            orphaned.len(),
            orphaned
                .iter()
                .take(5)
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        if orphaned.len() > 5 {
            eprintln!("  ... and {} more", orphaned.len() - 5);
        }

        let repo_root = seal_dir.and_then(Path::parent);

        if let Some(dir) = seal_dir {
            let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
            let backup_path = dir.join(format!("orphaned-reviews-{timestamp}.json"));

            // Gather detailed info about orphaned reviews
            let mut orphan_details: Vec<serde_json::Value> = Vec::new();
            for id in &orphaned {
                let detail: Option<(String, String, String)> = db
                    .conn
                    .query_row(
                        "SELECT title, author, status FROM reviews WHERE review_id = ?",
                        params![id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;

                if let Some((title, author, status)) = detail {
                    orphan_details.push(serde_json::json!({
                        "review_id": id,
                        "title": title,
                        "author": author,
                        "status": status,
                    }));
                }
            }

            let backup = serde_json::json!({
                "timestamp": Utc::now().to_rfc3339(),
                "reason": "events.jsonl truncation or content mismatch detected",
                "orphaned_reviews": orphan_details,
                "recovery_hint": rebuild_recovery_hint(repo_root)
            });

            std::fs::write(&backup_path, serde_json::to_string_pretty(&backup)?)?;
            eprintln!(
                "Orphaned review details saved to: {}",
                backup_path.display()
            );
        } else if let Some(command) =
            history_lookup_command(repo_root, &review_log_history_path(None))
        {
            eprintln!("HINT: Run '{command}' to find older versions");
        } else {
            eprintln!(
                "HINT: Inspect repository history for .seal/reviews/<review_id>/events.jsonl to find older versions"
            );
        }
    }

    // Step 6: Proceed with normal rebuild
    let tx = db
        .conn
        .unchecked_transaction()
        .context("Failed to begin rebuild transaction")?;

    tx.execute_batch(
        "DELETE FROM comments;
         DELETE FROM threads;
         DELETE FROM reviewer_votes;
         DELETE FROM review_reviewers;
         DELETE FROM reviews;",
    )
    .context("Failed to wipe projection tables")?;

    for event in &events {
        apply_event_inner(&tx, event).with_context(|| {
            format!(
                "Failed to apply event during rebuild: {:?}",
                event_type_name(&event.event)
            )
        })?;
    }

    let now = Utc::now().to_rfc3339();
    tx.execute(
        "UPDATE sync_state SET last_line_number = ?, last_sync_ts = ?, events_file_hash = ? WHERE id = 1",
        params![count as i64, now, new_hash],
    )
    .context("Failed to update sync_state after rebuild")?;

    tx.commit().context("Failed to commit rebuild")?;

    Ok(count)
}

// ============================================================================
// Orphaned event detection (bd-2ys)
// ============================================================================

/// Extract review_id from an event, if it directly carries one.
fn event_review_id(event: &Event) -> Option<&str> {
    match event {
        Event::ReviewCreated(e) => Some(&e.review_id),
        Event::ReviewersRequested(e) => Some(&e.review_id),
        Event::ReviewerVoted(e) => Some(&e.review_id),
        Event::ReviewApproved(e) => Some(&e.review_id),
        Event::ReviewMerged(e) => Some(&e.review_id),
        Event::ReviewAbandoned(e) => Some(&e.review_id),
        Event::ThreadCreated(e) => Some(&e.review_id),
        // These only carry thread_id:
        Event::ThreadResolved(_) | Event::ThreadReopened(_) | Event::CommentAdded(_) => None,
    }
}

/// Extract thread_id from an event, if it carries one.
fn event_thread_id(event: &Event) -> Option<&str> {
    match event {
        Event::ThreadCreated(e) => Some(&e.thread_id),
        Event::ThreadResolved(e) => Some(&e.thread_id),
        Event::ThreadReopened(e) => Some(&e.thread_id),
        Event::CommentAdded(e) => Some(&e.thread_id),
        _ => None,
    }
}

/// Filter out orphaned events that reference reviews without a ReviewCreated event.
///
/// When repository history restores an older version of events.jsonl, the
/// ReviewCreated event may be lost while ThreadCreated/CommentAdded events remain.
/// This function detects and removes such orphaned events, printing a warning.
///
/// Returns the filtered events and the count of skipped events.
#[cfg_attr(not(test), allow(dead_code))]
fn filter_orphaned_events(events: Vec<EventEnvelope>) -> (Vec<EventEnvelope>, usize) {
    filter_orphaned_events_for_repo(events, None)
}

fn filter_orphaned_events_for_repo(
    events: Vec<EventEnvelope>,
    seal_root: Option<&Path>,
) -> (Vec<EventEnvelope>, usize) {
    // Pass 1: collect known review_ids (from ReviewCreated) and thread→review map
    let mut known_reviews: HashSet<String> = HashSet::new();
    let mut thread_to_review: HashMap<String, String> = HashMap::new();

    for env in &events {
        if let Event::ReviewCreated(e) = &env.event {
            known_reviews.insert(e.review_id.clone());
        }
        if let Event::ThreadCreated(e) = &env.event {
            thread_to_review.insert(e.thread_id.clone(), e.review_id.clone());
        }
    }

    // Pass 2: identify orphaned review_ids
    let mut orphaned_reviews: HashSet<String> = HashSet::new();
    for env in &events {
        // Check events that carry review_id directly
        if let Some(rid) = event_review_id(&env.event) {
            if !known_reviews.contains(rid) {
                orphaned_reviews.insert(rid.to_string());
            }
        }
        // Check thread-only events via thread→review map
        if event_review_id(&env.event).is_none() {
            if let Some(tid) = event_thread_id(&env.event) {
                if let Some(rid) = thread_to_review.get(tid) {
                    if !known_reviews.contains(rid.as_str()) {
                        orphaned_reviews.insert(rid.clone());
                    }
                }
                // If thread_id not in map at all, that thread's ThreadCreated
                // is also missing — it will be caught as an FK error on threads
                // table, so also treat it as orphaned.
                if !thread_to_review.contains_key(tid) {
                    orphaned_reviews.insert(format!("unknown-thread:{tid}"));
                }
            }
        }
    }

    if orphaned_reviews.is_empty() {
        return (events, 0);
    }

    // Warn about orphaned reviews
    let mut real_orphans: Vec<String> = orphaned_reviews
        .iter()
        .filter(|review_id| !review_id.starts_with("unknown-thread:"))
        .cloned()
        .collect();
    real_orphans.sort();
    if !real_orphans.is_empty() {
        emit_orphaned_reviews_warning(seal_root, &real_orphans);
    }

    // Pass 3: filter out orphaned events
    let mut skipped = 0;
    let filtered: Vec<EventEnvelope> = events
        .into_iter()
        .filter(|env| {
            // Check direct review_id
            if let Some(rid) = event_review_id(&env.event) {
                if orphaned_reviews.contains(rid) {
                    skipped += 1;
                    return false;
                }
            }
            // Check thread-only events
            if event_review_id(&env.event).is_none() {
                if let Some(tid) = event_thread_id(&env.event) {
                    let is_orphan = match thread_to_review.get(tid) {
                        Some(rid) => orphaned_reviews.contains(rid.as_str()),
                        None => orphaned_reviews.contains(&format!("unknown-thread:{tid}")),
                    };
                    if is_orphan {
                        skipped += 1;
                        return false;
                    }
                }
            }
            true
        })
        .collect();

    (filtered, skipped)
}

// ============================================================================
// v2: Per-review event log sync
// ============================================================================

/// Stored state for a review file from the `review_file_state` table.
struct StoredFileState {
    line_count: usize,
    byte_count: u64,
    prefix_hash: String,
}

/// Sync the projection from per-review event logs (v2 format).
///
/// Uses per-file monotonic sync: each review file is independently tracked
/// and only new events (appended lines) are processed. Files that appear to
/// have regressed (shrunk, hash mismatch) are skipped to preserve existing
/// projection data. Returns a `SyncReport` with counts and anomalies.
pub fn sync_from_review_logs(db: &ProjectionDb, seal_root: &Path) -> Result<SyncReport> {
    let mut report = SyncReport {
        applied: 0,
        files_synced: 0,
        files_skipped: 0,
        anomalies: Vec::new(),
    };

    // Step 1: Load all review_file_state rows into a HashMap
    let mut stored_states: HashMap<String, StoredFileState> = HashMap::new();
    {
        let mut stmt = db
            .conn
            .prepare("SELECT review_id, line_count, byte_count, prefix_hash FROM review_file_state")
            .context("Failed to prepare review_file_state query")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    StoredFileState {
                        line_count: row.get::<_, i64>(1)? as usize,
                        byte_count: row.get::<_, i64>(2)? as u64,
                        prefix_hash: row.get::<_, String>(3)?,
                    },
                ))
            })
            .context("Failed to query review_file_state")?;
        for row in rows {
            let (review_id, state) = row.context("Failed to read review_file_state row")?;
            stored_states.insert(review_id, state);
        }
    }

    // Step 1b: Bootstrap — if projection has data but review_file_state is empty,
    // seed review_file_state from current on-disk files without replaying events.
    let projection_has_data: bool = db
        .conn
        .query_row("SELECT COUNT(*) > 0 FROM reviews", [], |row| row.get(0))
        .unwrap_or(false);

    if projection_has_data && stored_states.is_empty() {
        let on_disk_ids = list_review_ids(seal_root)?;
        for review_id in &on_disk_ids {
            if let Ok(log) = ReviewLog::new(seal_root, review_id) {
                let byte_count = log.byte_len().unwrap_or(0);
                let line_count = log.total_lines().unwrap_or(0);
                let prefix_hash = log
                    .prefix_hash(line_count)
                    .ok()
                    .flatten()
                    .unwrap_or_default();

                if byte_count > 0 && line_count > 0 && !prefix_hash.is_empty() {
                    db.conn
                        .execute(
                            "INSERT OR IGNORE INTO review_file_state (review_id, line_count, byte_count, prefix_hash) VALUES (?, ?, ?, ?)",
                            params![review_id, line_count as i64, byte_count as i64, prefix_hash],
                        )
                        .ok();
                    stored_states.insert(
                        review_id.clone(),
                        StoredFileState {
                            line_count,
                            byte_count,
                            prefix_hash,
                        },
                    );
                }
            }
        }
    }

    // Step 2: Discover files on disk
    let on_disk_ids = list_review_ids(seal_root)?;
    let on_disk_set: HashSet<&str> = on_disk_ids
        .iter()
        .map(std::string::String::as_str)
        .collect();

    // Step 3: Process each file
    for review_id in &on_disk_ids {
        let log = match ReviewLog::new(seal_root, review_id) {
            Ok(l) => l,
            Err(e) => {
                report.anomalies.push(SyncAnomaly {
                    review_id: review_id.clone(),
                    kind: AnomalyKind::ParseError,
                    detail: format!("Failed to open review log: {e}"),
                });
                continue;
            }
        };

        // Get current file stats
        let current_byte_count = match log.byte_len() {
            Ok(b) => b,
            Err(e) => {
                report.anomalies.push(SyncAnomaly {
                    review_id: review_id.clone(),
                    kind: AnomalyKind::ParseError,
                    detail: format!("Failed to stat file: {e}"),
                });
                continue;
            }
        };

        match stored_states.get(review_id.as_str()) {
            None => {
                // NEW file: read all events, apply
                sync_new_file(db, &log, review_id, seal_root, &mut report)?;
            }
            Some(stored) => {
                if current_byte_count == stored.byte_count {
                    // UNCHANGED: skip — cheap fast-path (no hashing needed)
                    report.files_skipped += 1;
                    continue;
                }

                // Byte count changed — need to inspect further
                let current_line_count = match log.total_lines() {
                    Ok(n) => n,
                    Err(e) => {
                        report.anomalies.push(SyncAnomaly {
                            review_id: review_id.clone(),
                            kind: AnomalyKind::ParseError,
                            detail: format!("Failed to count lines: {e}"),
                        });
                        continue;
                    }
                };

                if current_line_count < stored.line_count {
                    // SHRUNK: skip file, record anomaly
                    report.files_skipped += 1;
                    report.anomalies.push(SyncAnomaly {
                        review_id: review_id.clone(),
                        kind: AnomalyKind::Shrunk,
                        detail: format!(
                            "file shrunk (was {} lines, now {})",
                            stored.line_count, current_line_count
                        ),
                    });
                    continue;
                }

                // Same or more lines — check prefix hash
                let current_prefix_hash = match log.prefix_hash(stored.line_count) {
                    Ok(Some(h)) => h,
                    Ok(None) => {
                        // Empty prefix — treat as new file
                        sync_new_file(db, &log, review_id, seal_root, &mut report)?;
                        continue;
                    }
                    Err(e) => {
                        report.anomalies.push(SyncAnomaly {
                            review_id: review_id.clone(),
                            kind: AnomalyKind::ParseError,
                            detail: format!("Failed to compute prefix hash: {e}"),
                        });
                        continue;
                    }
                };

                if current_prefix_hash != stored.prefix_hash {
                    // HASH MISMATCH: skip file, record anomaly
                    report.files_skipped += 1;
                    report.anomalies.push(SyncAnomaly {
                        review_id: review_id.clone(),
                        kind: AnomalyKind::HashMismatch,
                        detail: format!(
                            "content changed (hash mismatch on first {} lines)",
                            stored.line_count
                        ),
                    });
                    continue;
                }

                // GREW: prefix matches, read new lines only
                sync_grew_file(
                    db,
                    &log,
                    review_id,
                    stored.line_count,
                    seal_root,
                    &mut report,
                )?;
            }
        }
    }

    // Step 4: Check for disappeared files
    for review_id in stored_states.keys() {
        if !on_disk_set.contains(review_id.as_str()) {
            report.anomalies.push(SyncAnomaly {
                review_id: review_id.clone(),
                kind: AnomalyKind::Missing,
                detail: "file disappeared from disk, projection data preserved".to_string(),
            });
        }
    }

    // Print warnings if there are anomalies
    emit_stale_review_logs_warning(seal_root, &report.anomalies);

    Ok(report)
}

/// Sync a new review file (no prior state).
///
/// Reads all events, applies them in a savepoint, and records file state on success.
fn sync_new_file(
    db: &ProjectionDb,
    log: &ReviewLog,
    review_id: &str,
    seal_root: &Path,
    report: &mut SyncReport,
) -> Result<()> {
    let events = match log.read_all() {
        Ok(e) => e,
        Err(e) => {
            report.anomalies.push(SyncAnomaly {
                review_id: review_id.to_string(),
                kind: AnomalyKind::ParseError,
                detail: format!("Failed to read events: {e}"),
            });
            return Ok(());
        }
    };

    if events.is_empty() {
        return Ok(());
    }

    // Filter orphaned events for this file
    let (events, _orphaned) = filter_orphaned_events_for_repo(events, Some(seal_root));
    if events.is_empty() {
        return Ok(());
    }

    let event_count = events.len();

    // Use a savepoint for isolation
    db.conn
        .execute_batch("SAVEPOINT sync_file")
        .context("Failed to create savepoint")?;

    let mut failed = false;
    for event in &events {
        if let Err(e) = apply_event_inner(&db.conn, event) {
            tracing::warn!("failed to apply event in {}: {}", review_id, e);
            report.anomalies.push(SyncAnomaly {
                review_id: review_id.to_string(),
                kind: AnomalyKind::ParseError,
                detail: format!("Failed to apply event: {e}"),
            });
            failed = true;
            break;
        }
    }

    if failed {
        db.conn
            .execute_batch("ROLLBACK TO SAVEPOINT sync_file")
            .context("Failed to rollback savepoint")?;
        db.conn
            .execute_batch("RELEASE SAVEPOINT sync_file")
            .context("Failed to release savepoint after rollback")?;
        return Ok(());
    }

    // Record file state
    let line_count = log.total_lines().unwrap_or(0);
    let byte_count = log.byte_len().unwrap_or(0);
    let prefix_hash = log
        .prefix_hash(line_count)
        .ok()
        .flatten()
        .unwrap_or_default();

    db.conn
        .execute(
            "INSERT OR REPLACE INTO review_file_state (review_id, line_count, byte_count, prefix_hash) VALUES (?, ?, ?, ?)",
            params![review_id, line_count as i64, byte_count as i64, prefix_hash],
        )
        .context("Failed to update review_file_state")?;

    db.conn
        .execute_batch("RELEASE SAVEPOINT sync_file")
        .context("Failed to release savepoint")?;

    report.applied += event_count;
    report.files_synced += 1;

    Ok(())
}

/// Sync a review file that grew (append-only new lines).
///
/// Reads events from the old line count onward, applies them in a savepoint,
/// and updates file state on success.
fn sync_grew_file(
    db: &ProjectionDb,
    log: &ReviewLog,
    review_id: &str,
    old_line_count: usize,
    _seal_root: &Path,
    report: &mut SyncReport,
) -> Result<()> {
    let new_events = match log.read_from(old_line_count) {
        Ok(e) => e,
        Err(e) => {
            report.anomalies.push(SyncAnomaly {
                review_id: review_id.to_string(),
                kind: AnomalyKind::ParseError,
                detail: format!("Failed to read new events: {e}"),
            });
            return Ok(());
        }
    };

    if new_events.is_empty() {
        // File grew in bytes but no new parseable events — update file state
        let line_count = log.total_lines().unwrap_or(0);
        let byte_count = log.byte_len().unwrap_or(0);
        let prefix_hash = log
            .prefix_hash(line_count)
            .ok()
            .flatten()
            .unwrap_or_default();

        db.conn
            .execute(
                "INSERT OR REPLACE INTO review_file_state (review_id, line_count, byte_count, prefix_hash) VALUES (?, ?, ?, ?)",
                params![review_id, line_count as i64, byte_count as i64, prefix_hash],
            )
            .context("Failed to update review_file_state")?;

        report.files_skipped += 1;
        return Ok(());
    }

    // Filter orphaned events for new events (need context from existing projection)
    // For grew files, we trust events that reference reviews already in the projection
    let (new_events, _orphaned) = filter_orphaned_events_with_projection(db, new_events);
    if new_events.is_empty() {
        return Ok(());
    }

    let event_count = new_events.len();

    // Use a savepoint for isolation
    db.conn
        .execute_batch("SAVEPOINT sync_file")
        .context("Failed to create savepoint")?;

    let mut failed = false;
    for event in &new_events {
        if let Err(e) = apply_event_inner(&db.conn, event) {
            tracing::warn!("failed to apply event in {}: {}", review_id, e);
            report.anomalies.push(SyncAnomaly {
                review_id: review_id.to_string(),
                kind: AnomalyKind::ParseError,
                detail: format!("Failed to apply event: {e}"),
            });
            failed = true;
            break;
        }
    }

    if failed {
        db.conn
            .execute_batch("ROLLBACK TO SAVEPOINT sync_file")
            .context("Failed to rollback savepoint")?;
        db.conn
            .execute_batch("RELEASE SAVEPOINT sync_file")
            .context("Failed to release savepoint after rollback")?;
        return Ok(());
    }

    // Update file state
    let line_count = log.total_lines().unwrap_or(0);
    let byte_count = log.byte_len().unwrap_or(0);
    let prefix_hash = log
        .prefix_hash(line_count)
        .ok()
        .flatten()
        .unwrap_or_default();

    db.conn
        .execute(
            "INSERT OR REPLACE INTO review_file_state (review_id, line_count, byte_count, prefix_hash) VALUES (?, ?, ?, ?)",
            params![review_id, line_count as i64, byte_count as i64, prefix_hash],
        )
        .context("Failed to update review_file_state")?;

    db.conn
        .execute_batch("RELEASE SAVEPOINT sync_file")
        .context("Failed to release savepoint")?;

    report.applied += event_count;
    report.files_synced += 1;

    Ok(())
}

/// Filter orphaned events, considering reviews already in the projection.
///
/// For incremental sync (grew files), events may reference reviews that are
/// already in the projection but not in the current event batch. We check
/// both the event batch and the projection for known reviews.
fn filter_orphaned_events_with_projection(
    db: &ProjectionDb,
    events: Vec<EventEnvelope>,
) -> (Vec<EventEnvelope>, usize) {
    // Build the set of known reviews from events
    let mut known_reviews: HashSet<String> = HashSet::new();
    let mut thread_to_review: HashMap<String, String> = HashMap::new();

    for env in &events {
        if let Event::ReviewCreated(e) = &env.event {
            known_reviews.insert(e.review_id.clone());
        }
        if let Event::ThreadCreated(e) = &env.event {
            thread_to_review.insert(e.thread_id.clone(), e.review_id.clone());
        }
    }

    // Also check the projection for known reviews and threads
    if let Ok(mut stmt) = db.conn.prepare("SELECT review_id FROM reviews") {
        if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
            for row in rows.flatten() {
                known_reviews.insert(row);
            }
        }
    }
    if let Ok(mut stmt) = db.conn.prepare("SELECT thread_id, review_id FROM threads") {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            for row in rows.flatten() {
                thread_to_review.insert(row.0, row.1);
            }
        }
    }

    // Now filter using the combined knowledge
    let mut orphaned_reviews: HashSet<String> = HashSet::new();
    for env in &events {
        if let Some(rid) = event_review_id(&env.event) {
            if !known_reviews.contains(rid) {
                orphaned_reviews.insert(rid.to_string());
            }
        }
        if event_review_id(&env.event).is_none() {
            if let Some(tid) = event_thread_id(&env.event) {
                if let Some(rid) = thread_to_review.get(tid) {
                    if !known_reviews.contains(rid.as_str()) {
                        orphaned_reviews.insert(rid.clone());
                    }
                }
                if !thread_to_review.contains_key(tid) {
                    orphaned_reviews.insert(format!("unknown-thread:{tid}"));
                }
            }
        }
    }

    if orphaned_reviews.is_empty() {
        return (events, 0);
    }

    let mut skipped = 0;
    let filtered: Vec<EventEnvelope> = events
        .into_iter()
        .filter(|env| {
            if let Some(rid) = event_review_id(&env.event) {
                if orphaned_reviews.contains(rid) {
                    skipped += 1;
                    return false;
                }
            }
            if event_review_id(&env.event).is_none() {
                if let Some(tid) = event_thread_id(&env.event) {
                    let is_orphan = match thread_to_review.get(tid) {
                        Some(rid) => orphaned_reviews.contains(rid.as_str()),
                        None => orphaned_reviews.contains(&format!("unknown-thread:{tid}")),
                    };
                    if is_orphan {
                        skipped += 1;
                        return false;
                    }
                }
            }
            true
        })
        .collect();

    (filtered, skipped)
}

/// Rebuild the projection from per-review event logs (v2 format).
///
/// Wipes all data and re-applies all events from all review logs.
pub fn rebuild_from_review_logs(db: &ProjectionDb, seal_root: &Path) -> Result<usize> {
    // Read all events from all review logs
    let all_events = read_all_reviews(seal_root)?;

    if all_events.is_empty() {
        return Ok(0);
    }

    // Filter out orphaned events (bd-2ys)
    let (all_events, _orphaned_count) =
        filter_orphaned_events_for_repo(all_events, Some(seal_root));

    if all_events.is_empty() {
        return Ok(0);
    }

    // Find the latest timestamp for sync state
    let max_ts = all_events
        .iter()
        .map(|e| &e.ts)
        .max()
        .expect("all_events is not empty");

    let tx = db
        .conn
        .unchecked_transaction()
        .context("Failed to begin rebuild transaction")?;

    // Wipe all projection data (order matters for foreign keys)
    tx.execute_batch(
        "DELETE FROM comments;
         DELETE FROM threads;
         DELETE FROM reviewer_votes;
         DELETE FROM review_reviewers;
         DELETE FROM reviews;
         DELETE FROM review_file_state;",
    )
    .context("Failed to wipe projection tables")?;

    // Apply all events
    for event in &all_events {
        apply_event_inner(&tx, event).with_context(|| {
            format!(
                "Failed to apply event during rebuild (type: {:?})",
                event_type_name(&event.event)
            )
        })?;
    }

    // Update sync state
    tx.execute(
        "UPDATE sync_state SET last_line_number = 0, last_sync_ts = ?, events_file_hash = NULL WHERE id = 1",
        params![max_ts.to_rfc3339()],
    )
    .context("Failed to update sync_state after rebuild")?;

    tx.commit().context("Failed to commit rebuild")?;

    Ok(all_events.len())
}

/// Apply a single event to the projection database.
pub fn apply_event(db: &ProjectionDb, event: &EventEnvelope) -> Result<()> {
    apply_event_inner(&db.conn, event)
}

/// Internal event application using a generic connection/transaction.
fn apply_event_inner(conn: &Connection, envelope: &EventEnvelope) -> Result<()> {
    let ts = &envelope.ts;
    let author = &envelope.author;

    match &envelope.event {
        Event::ReviewCreated(e) => apply_review_created(conn, e, author, ts),
        Event::ReviewersRequested(e) => apply_reviewers_requested(conn, e, author, ts),
        Event::ReviewerVoted(e) => apply_reviewer_voted(conn, e, author, ts),
        Event::ReviewApproved(e) => apply_review_approved(conn, e, author, ts),
        Event::ReviewMerged(e) => apply_review_merged(conn, e, author, ts),
        Event::ReviewAbandoned(e) => apply_review_abandoned(conn, e, author, ts),
        Event::ThreadCreated(e) => apply_thread_created(conn, e, author, ts),
        Event::ThreadResolved(e) => apply_thread_resolved(conn, e, author, ts),
        Event::ThreadReopened(e) => apply_thread_reopened(conn, e, author, ts),
        Event::CommentAdded(e) => apply_comment_added(conn, e, author, ts),
    }
}

// ============================================================================
// Review Event Handlers
// ============================================================================

fn apply_review_created(
    conn: &Connection,
    event: &ReviewCreated,
    author: &str,
    ts: &DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO reviews (
            review_id, jj_change_id, scm_kind, scm_anchor, initial_commit, title, description,
            author, created_at, status
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'open')",
        params![
            event.review_id,
            event.jj_change_id,
            event.scm_kind.as_deref().unwrap_or("jj"),
            event.scm_anchor.as_deref().unwrap_or(&event.jj_change_id),
            event.initial_commit,
            event.title,
            event.description,
            author,
            ts.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn apply_reviewers_requested(
    conn: &Connection,
    event: &ReviewersRequested,
    author: &str,
    ts: &DateTime<Utc>,
) -> Result<()> {
    let ts_str = ts.to_rfc3339();
    for reviewer in &event.reviewers {
        conn.execute(
            "INSERT INTO review_reviewers (
                review_id, reviewer, requested_at, requested_by
            ) VALUES (?, ?, ?, ?)
            ON CONFLICT (review_id, reviewer) DO UPDATE SET
                requested_at = excluded.requested_at,
                requested_by = excluded.requested_by",
            params![event.review_id, reviewer, ts_str, author],
        )?;
    }
    Ok(())
}

fn apply_reviewer_voted(
    conn: &Connection,
    event: &ReviewerVoted,
    author: &str,
    ts: &DateTime<Utc>,
) -> Result<()> {
    // Insert or replace vote (a reviewer can change their vote)
    conn.execute(
        "INSERT INTO reviewer_votes (review_id, reviewer, vote, reason, voted_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (review_id, reviewer) DO UPDATE SET
             vote = excluded.vote,
             reason = excluded.reason,
             voted_at = excluded.voted_at",
        params![
            event.review_id,
            author,
            event.vote.to_string(),
            event.reason,
            ts.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn apply_review_approved(
    conn: &Connection,
    event: &ReviewApproved,
    author: &str,
    ts: &DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "UPDATE reviews SET
            status = 'approved',
            status_changed_at = ?,
            status_changed_by = ?
        WHERE review_id = ? AND status = 'open'",
        params![ts.to_rfc3339(), author, event.review_id],
    )?;
    Ok(())
}

fn apply_review_merged(
    conn: &Connection,
    event: &ReviewMerged,
    author: &str,
    ts: &DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "UPDATE reviews SET
            status = 'merged',
            final_commit = ?,
            status_changed_at = ?,
            status_changed_by = ?
        WHERE review_id = ? AND status IN ('open', 'approved')",
        params![event.final_commit, ts.to_rfc3339(), author, event.review_id],
    )?;
    Ok(())
}

fn apply_review_abandoned(
    conn: &Connection,
    event: &ReviewAbandoned,
    author: &str,
    ts: &DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "UPDATE reviews SET
            status = 'abandoned',
            status_changed_at = ?,
            status_changed_by = ?,
            abandon_reason = ?
        WHERE review_id = ? AND status IN ('open', 'approved')",
        params![ts.to_rfc3339(), author, event.reason, event.review_id],
    )?;
    Ok(())
}

// ============================================================================
// Thread Event Handlers
// ============================================================================

fn apply_thread_created(
    conn: &Connection,
    event: &ThreadCreated,
    author: &str,
    ts: &DateTime<Utc>,
) -> Result<()> {
    let (selection_type, selection_start, selection_end) = match &event.selection {
        CodeSelection::Line { line } => ("line", *line, None),
        CodeSelection::Range { start, end } => ("range", *start, Some(*end)),
    };

    conn.execute(
        "INSERT OR IGNORE INTO threads (
            thread_id, review_id, file_path,
            selection_type, selection_start, selection_end,
            commit_hash, author, created_at, status
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'open')",
        params![
            event.thread_id,
            event.review_id,
            event.file_path,
            selection_type,
            selection_start,
            selection_end,
            event.commit_hash,
            author,
            ts.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn apply_thread_resolved(
    conn: &Connection,
    event: &ThreadResolved,
    author: &str,
    ts: &DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "UPDATE threads SET
            status = 'resolved',
            status_changed_at = ?,
            status_changed_by = ?,
            resolve_reason = ?
        WHERE thread_id = ? AND status = 'open'",
        params![ts.to_rfc3339(), author, event.reason, event.thread_id],
    )?;
    Ok(())
}

fn apply_thread_reopened(
    conn: &Connection,
    event: &ThreadReopened,
    author: &str,
    ts: &DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "UPDATE threads SET
            status = 'open',
            status_changed_at = ?,
            status_changed_by = ?,
            reopen_reason = ?
        WHERE thread_id = ? AND status = 'resolved'",
        params![ts.to_rfc3339(), author, event.reason, event.thread_id],
    )?;
    Ok(())
}

// ============================================================================
// Comment Event Handlers
// ============================================================================

fn apply_comment_added(
    conn: &Connection,
    event: &CommentAdded,
    author: &str,
    ts: &DateTime<Utc>,
) -> Result<()> {
    // Insert the comment
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO comments (
            comment_id, thread_id, body, author, created_at
        ) VALUES (?, ?, ?, ?, ?)",
        params![
            event.comment_id,
            event.thread_id,
            event.body,
            author,
            ts.to_rfc3339(),
        ],
    )?;
    if inserted > 0 {
        conn.execute(
            "UPDATE threads SET next_comment_number = next_comment_number + 1 WHERE thread_id = ?",
            params![event.thread_id],
        )?;
    }
    Ok(())
}

// ============================================================================
// Helpers
// ============================================================================

const fn event_type_name(event: &Event) -> &'static str {
    match event {
        Event::ReviewCreated(_) => "ReviewCreated",
        Event::ReviewersRequested(_) => "ReviewersRequested",
        Event::ReviewerVoted(_) => "ReviewerVoted",
        Event::ReviewApproved(_) => "ReviewApproved",
        Event::ReviewMerged(_) => "ReviewMerged",
        Event::ReviewAbandoned(_) => "ReviewAbandoned",
        Event::ThreadCreated(_) => "ThreadCreated",
        Event::ThreadResolved(_) => "ThreadResolved",
        Event::ThreadReopened(_) => "ThreadReopened",
        Event::CommentAdded(_) => "CommentAdded",
    }
}

// ============================================================================
// Schema SQL
// ============================================================================

const SCHEMA_SQL: &str = r"
-- SYNC STATE
CREATE TABLE IF NOT EXISTS sync_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last_line_number INTEGER NOT NULL DEFAULT 0,
    last_sync_ts TEXT,
    events_file_hash TEXT
);

INSERT OR IGNORE INTO sync_state (id, last_line_number) VALUES (1, 0);

-- REVIEWS
CREATE TABLE IF NOT EXISTS reviews (
    review_id TEXT PRIMARY KEY,
    jj_change_id TEXT NOT NULL,
    scm_kind TEXT NOT NULL DEFAULT 'jj' CHECK (scm_kind IN ('jj', 'git')),
    scm_anchor TEXT,
    initial_commit TEXT NOT NULL,
    final_commit TEXT,
    title TEXT NOT NULL,
    description TEXT,
    author TEXT NOT NULL,
    created_at TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'open'
        CHECK (status IN ('open', 'approved', 'merged', 'abandoned')),
    status_changed_at TEXT,
    status_changed_by TEXT,
    abandon_reason TEXT
);

CREATE INDEX IF NOT EXISTS idx_reviews_status ON reviews(status);
CREATE INDEX IF NOT EXISTS idx_reviews_author ON reviews(author);
CREATE INDEX IF NOT EXISTS idx_reviews_change_id ON reviews(jj_change_id);

-- REVIEWERS
CREATE TABLE IF NOT EXISTS review_reviewers (
    review_id TEXT NOT NULL REFERENCES reviews(review_id),
    reviewer TEXT NOT NULL,
    requested_at TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    PRIMARY KEY (review_id, reviewer)
);

CREATE INDEX IF NOT EXISTS idx_reviewers_reviewer ON review_reviewers(reviewer);

-- REVIEWER VOTES
CREATE TABLE IF NOT EXISTS reviewer_votes (
    review_id TEXT NOT NULL REFERENCES reviews(review_id),
    reviewer TEXT NOT NULL,
    vote TEXT NOT NULL CHECK (vote IN ('lgtm', 'block')),
    reason TEXT,
    voted_at TEXT NOT NULL,
    PRIMARY KEY (review_id, reviewer)
);

CREATE INDEX IF NOT EXISTS idx_votes_review ON reviewer_votes(review_id);
CREATE INDEX IF NOT EXISTS idx_votes_vote ON reviewer_votes(vote);

-- THREADS
CREATE TABLE IF NOT EXISTS threads (
    thread_id TEXT PRIMARY KEY,
    review_id TEXT NOT NULL REFERENCES reviews(review_id),
    file_path TEXT NOT NULL,
    selection_type TEXT NOT NULL CHECK (selection_type IN ('line', 'range')),
    selection_start INTEGER NOT NULL,
    selection_end INTEGER,
    commit_hash TEXT NOT NULL,
    author TEXT NOT NULL,
    created_at TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'open'
        CHECK (status IN ('open', 'resolved')),
    status_changed_at TEXT,
    status_changed_by TEXT,
    resolve_reason TEXT,
    reopen_reason TEXT,
    next_comment_number INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_threads_review_id ON threads(review_id);
CREATE INDEX IF NOT EXISTS idx_threads_status ON threads(status);
CREATE INDEX IF NOT EXISTS idx_threads_review_file ON threads(review_id, file_path);
CREATE INDEX IF NOT EXISTS idx_threads_review_status ON threads(review_id, status);

-- COMMENTS
CREATE TABLE IF NOT EXISTS comments (
    comment_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(thread_id),
    body TEXT NOT NULL,
    author TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_comments_thread_id ON comments(thread_id);

-- VIEWS
-- Note: open_thread_count only counts threads that are truly actionable.
-- Threads on merged/abandoned reviews are NOT counted as open, even if
-- they were never explicitly resolved (they're effectively resolved by
-- the review being completed).
CREATE VIEW IF NOT EXISTS v_reviews_summary AS
SELECT
    r.review_id,
    r.title,
    r.author,
    r.status,
    r.jj_change_id,
    r.created_at,
    COUNT(DISTINCT t.thread_id) AS thread_count,
    COUNT(DISTINCT CASE
        WHEN t.status = 'open' AND r.status NOT IN ('merged', 'abandoned')
        THEN t.thread_id
    END) AS open_thread_count
FROM reviews r
LEFT JOIN threads t ON t.review_id = r.review_id
GROUP BY r.review_id;

-- v_threads_detail includes an effective_status that considers parent review state.
-- If the review is merged/abandoned, threads are effectively 'resolved' even if
-- they were never explicitly resolved.
CREATE VIEW IF NOT EXISTS v_threads_detail AS
SELECT
    t.*,
    r.title AS review_title,
    r.status AS review_status,
    COUNT(c.comment_id) AS comment_count,
    CASE
        WHEN t.status = 'resolved' THEN 'resolved'
        WHEN r.status IN ('merged', 'abandoned') THEN 'resolved'
        ELSE 'open'
    END AS effective_status
FROM threads t
JOIN reviews r ON r.review_id = t.review_id
LEFT JOIN comments c ON c.thread_id = t.thread_id
GROUP BY t.thread_id;
";

const REFRESH_VIEWS_SQL: &str = r"
DROP VIEW IF EXISTS v_reviews_summary;
DROP VIEW IF EXISTS v_threads_detail;

CREATE VIEW v_reviews_summary AS
SELECT
    r.review_id,
    r.title,
    r.author,
    r.status,
    r.jj_change_id,
    r.scm_kind,
    r.scm_anchor,
    r.created_at,
    COUNT(DISTINCT t.thread_id) AS thread_count,
    COUNT(DISTINCT CASE
        WHEN t.status = 'open' AND r.status NOT IN ('merged', 'abandoned')
        THEN t.thread_id
    END) AS open_thread_count
FROM reviews r
LEFT JOIN threads t ON t.review_id = r.review_id
GROUP BY r.review_id;

CREATE VIEW v_threads_detail AS
SELECT
    t.*,
    r.title AS review_title,
    r.status AS review_status,
    COUNT(c.comment_id) AS comment_count,
    CASE
        WHEN t.status = 'resolved' THEN 'resolved'
        WHEN r.status IN ('merged', 'abandoned') THEN 'resolved'
        ELSE 'open'
    END AS effective_status
FROM threads t
JOIN reviews r ON r.review_id = t.review_id
LEFT JOIN comments c ON c.thread_id = t.thread_id
GROUP BY t.thread_id;
";

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{CodeSelection, Event};
    use crate::log::{open_or_create, AppendLog};
    use std::process::Command;
    use tempfile::tempdir;

    fn make_review_created(review_id: &str) -> EventEnvelope {
        EventEnvelope::new(
            "test_author",
            Event::ReviewCreated(ReviewCreated {
                review_id: review_id.to_string(),
                jj_change_id: "change123".to_string(),
                scm_kind: Some("jj".to_string()),
                scm_anchor: Some("change123".to_string()),
                initial_commit: "commit456".to_string(),
                title: format!("Review {review_id}"),
                description: Some("Test description".to_string()),
            }),
        )
    }

    fn make_thread_created(thread_id: &str, review_id: &str) -> EventEnvelope {
        EventEnvelope::new(
            "test_author",
            Event::ThreadCreated(ThreadCreated {
                thread_id: thread_id.to_string(),
                review_id: review_id.to_string(),
                file_path: "src/main.rs".to_string(),
                selection: CodeSelection::range(10, 20),
                commit_hash: "abc123".to_string(),
            }),
        )
    }

    #[test]
    fn test_history_lookup_command_uses_git_in_git_repo() {
        let dir = tempdir().unwrap();
        let status = Command::new("git")
            .arg("init")
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());

        let command = history_lookup_command(Some(dir.path()), ".seal/events.jsonl");
        assert_eq!(
            command.as_deref(),
            Some("git log --follow -p -- .seal/events.jsonl")
        );
    }

    #[test]
    fn test_rebuild_recovery_hint_falls_back_without_repo() {
        let hint = rebuild_recovery_hint(None);
        assert!(hint.contains("Check repository history"));
        assert!(!hint.contains("jj file annotate"));
        assert!(hint.contains(".seal/reviews/<review_id>/events.jsonl"));
    }

    #[test]
    fn test_open_and_init_schema() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let db = ProjectionDb::open(&db_path).unwrap();
        db.init_schema().unwrap();

        // Verify sync_state was initialized
        let line = db.get_last_sync_line().unwrap();
        assert_eq!(line, 0);
    }

    #[test]
    fn test_sync_state_roundtrip() {
        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        assert_eq!(db.get_last_sync_line().unwrap(), 0);

        db.set_last_sync_line(42).unwrap();
        assert_eq!(db.get_last_sync_line().unwrap(), 42);

        db.set_last_sync_line(100).unwrap();
        assert_eq!(db.get_last_sync_line().unwrap(), 100);
    }

    #[test]
    fn test_apply_review_created() {
        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        let event = make_review_created("cr-001");
        apply_event(&db, &event).unwrap();

        // Verify review was inserted
        let (title, status): (String, String) = db
            .conn()
            .query_row(
                "SELECT title, status FROM reviews WHERE review_id = ?",
                params!["cr-001"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(title, "Review cr-001");
        assert_eq!(status, "open");
    }

    #[test]
    fn test_apply_review_lifecycle() {
        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Create review
        apply_event(&db, &make_review_created("cr-001")).unwrap();

        // Request reviewers
        apply_event(
            &db,
            &EventEnvelope::new(
                "requester",
                Event::ReviewersRequested(ReviewersRequested {
                    review_id: "cr-001".to_string(),
                    reviewers: vec!["alice".to_string(), "bob".to_string()],
                }),
            ),
        )
        .unwrap();

        // Verify reviewers
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM review_reviewers WHERE review_id = ?",
                params!["cr-001"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);

        // Approve review
        apply_event(
            &db,
            &EventEnvelope::new(
                "alice",
                Event::ReviewApproved(ReviewApproved {
                    review_id: "cr-001".to_string(),
                }),
            ),
        )
        .unwrap();

        let status: String = db
            .conn()
            .query_row(
                "SELECT status FROM reviews WHERE review_id = ?",
                params!["cr-001"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "approved");

        // Merge review
        apply_event(
            &db,
            &EventEnvelope::new(
                "merger",
                Event::ReviewMerged(ReviewMerged {
                    review_id: "cr-001".to_string(),
                    final_commit: "final789".to_string(),
                }),
            ),
        )
        .unwrap();

        let (status, final_commit): (String, String) = db
            .conn()
            .query_row(
                "SELECT status, final_commit FROM reviews WHERE review_id = ?",
                params!["cr-001"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "merged");
        assert_eq!(final_commit, "final789");
    }

    #[test]
    fn test_apply_review_abandoned() {
        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        apply_event(&db, &make_review_created("cr-001")).unwrap();

        apply_event(
            &db,
            &EventEnvelope::new(
                "abandoner",
                Event::ReviewAbandoned(ReviewAbandoned {
                    review_id: "cr-001".to_string(),
                    reason: Some("No longer needed".to_string()),
                }),
            ),
        )
        .unwrap();

        let (status, reason): (String, Option<String>) = db
            .conn()
            .query_row(
                "SELECT status, abandon_reason FROM reviews WHERE review_id = ?",
                params!["cr-001"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "abandoned");
        assert_eq!(reason, Some("No longer needed".to_string()));
    }

    #[test]
    fn test_apply_thread_lifecycle() {
        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Create review first (for FK)
        apply_event(&db, &make_review_created("cr-001")).unwrap();

        // Create thread
        apply_event(&db, &make_thread_created("th-001", "cr-001")).unwrap();

        let (status, sel_type, sel_start, sel_end): (String, String, i64, Option<i64>) = db
            .conn()
            .query_row(
                "SELECT status, selection_type, selection_start, selection_end 
                 FROM threads WHERE thread_id = ?",
                params!["th-001"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(status, "open");
        assert_eq!(sel_type, "range");
        assert_eq!(sel_start, 10);
        assert_eq!(sel_end, Some(20));

        // Resolve thread
        apply_event(
            &db,
            &EventEnvelope::new(
                "resolver",
                Event::ThreadResolved(ThreadResolved {
                    thread_id: "th-001".to_string(),
                    reason: Some("Fixed".to_string()),
                }),
            ),
        )
        .unwrap();

        let status: String = db
            .conn()
            .query_row(
                "SELECT status FROM threads WHERE thread_id = ?",
                params!["th-001"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "resolved");

        // Reopen thread
        apply_event(
            &db,
            &EventEnvelope::new(
                "reopener",
                Event::ThreadReopened(ThreadReopened {
                    thread_id: "th-001".to_string(),
                    reason: Some("Not actually fixed".to_string()),
                }),
            ),
        )
        .unwrap();

        let status: String = db
            .conn()
            .query_row(
                "SELECT status FROM threads WHERE thread_id = ?",
                params!["th-001"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "open");
    }

    #[test]
    fn test_sync_from_log_empty() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let log = open_or_create(&log_path).unwrap();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        let count = sync_from_log(&db, &log).unwrap();
        assert_eq!(count, 0);
        assert_eq!(db.get_last_sync_line().unwrap(), 0);
    }

    #[test]
    fn test_sync_from_log_full() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let log = open_or_create(&log_path).unwrap();

        // Add events to log
        log.append(&make_review_created("cr-001")).unwrap();
        log.append(&make_review_created("cr-002")).unwrap();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // First sync
        let count = sync_from_log(&db, &log).unwrap();
        assert_eq!(count, 2);
        assert_eq!(db.get_last_sync_line().unwrap(), 2);

        // Verify reviews exist
        let review_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
            .unwrap();
        assert_eq!(review_count, 2);

        // Second sync (no new events)
        let count = sync_from_log(&db, &log).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_sync_from_log_incremental() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let log = open_or_create(&log_path).unwrap();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Add first batch
        log.append(&make_review_created("cr-001")).unwrap();
        let count = sync_from_log(&db, &log).unwrap();
        assert_eq!(count, 1);
        assert_eq!(db.get_last_sync_line().unwrap(), 1);

        // Add second batch
        log.append(&make_review_created("cr-002")).unwrap();
        log.append(&make_review_created("cr-003")).unwrap();
        let count = sync_from_log(&db, &log).unwrap();
        assert_eq!(count, 2);
        assert_eq!(db.get_last_sync_line().unwrap(), 3);

        // Verify all reviews
        let review_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
            .unwrap();
        assert_eq!(review_count, 3);
    }

    #[test]
    fn test_sync_with_file_persistence() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let db_path = dir.path().join("index.db");

        let log = open_or_create(&log_path).unwrap();
        log.append(&make_review_created("cr-001")).unwrap();
        log.append(&make_review_created("cr-002")).unwrap();

        // First sync
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let count = sync_from_log(&db, &log).unwrap();
            assert_eq!(count, 2);
        }

        // Add more events
        log.append(&make_review_created("cr-003")).unwrap();

        // Reopen database and sync
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            // Schema already exists, init is idempotent
            db.init_schema().unwrap();

            // Should only sync new event
            let count = sync_from_log(&db, &log).unwrap();
            assert_eq!(count, 1);
            assert_eq!(db.get_last_sync_line().unwrap(), 3);

            let review_count: i64 = db
                .conn()
                .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
                .unwrap();
            assert_eq!(review_count, 3);
        }
    }

    #[test]
    fn test_idempotent_event_application() {
        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        let event = make_review_created("cr-001");

        // Apply same event twice (simulates replay)
        apply_event(&db, &event).unwrap();
        apply_event(&db, &event).unwrap();

        // Should only have one review (INSERT OR IGNORE)
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_thread_single_line_selection() {
        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        apply_event(&db, &make_review_created("cr-001")).unwrap();

        let event = EventEnvelope::new(
            "test_author",
            Event::ThreadCreated(ThreadCreated {
                thread_id: "th-001".to_string(),
                review_id: "cr-001".to_string(),
                file_path: "src/lib.rs".to_string(),
                selection: CodeSelection::line(42),
                commit_hash: "abc123".to_string(),
            }),
        );
        apply_event(&db, &event).unwrap();

        let (sel_type, sel_start, sel_end): (String, i64, Option<i64>) = db
            .conn()
            .query_row(
                "SELECT selection_type, selection_start, selection_end 
                 FROM threads WHERE thread_id = ?",
                params!["th-001"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(sel_type, "line");
        assert_eq!(sel_start, 42);
        assert_eq!(sel_end, None);
    }

    #[test]
    fn test_views_work() {
        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        apply_event(&db, &make_review_created("cr-001")).unwrap();
        apply_event(&db, &make_thread_created("th-001", "cr-001")).unwrap();
        apply_event(&db, &make_thread_created("th-002", "cr-001")).unwrap();

        // Resolve one thread
        apply_event(
            &db,
            &EventEnvelope::new(
                "resolver",
                Event::ThreadResolved(ThreadResolved {
                    thread_id: "th-001".to_string(),
                    reason: None,
                }),
            ),
        )
        .unwrap();

        // Query the summary view
        let (thread_count, open_count): (i64, i64) = db
            .conn()
            .query_row(
                "SELECT thread_count, open_thread_count FROM v_reviews_summary WHERE review_id = ?",
                params!["cr-001"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(thread_count, 2);
        assert_eq!(open_count, 1);
    }

    // ========================================================================
    // bd-1s1 reproduction tests: LGTM vote doesn't override block in index
    // ========================================================================
    //
    // These tests simulate the exact CLI command sync pattern from the R4 eval
    // to reproduce the reported bug where `seal lgtm` succeeded but
    // `seal review` still showed a blocking vote.
    //
    // Hypotheses tested:
    // 1. Empty lines in events.jsonl cause sync offset drift
    // 2. Incremental sync with on-disk DB persistence loses vote state
    // 3. The eval's exact event sequence triggers the bug

    use crate::events::{ReviewerVoted, ReviewersRequested, VoteType};
    use std::io::Write;

    /// Helper: query the current vote for a reviewer on a review.
    fn query_vote(db: &ProjectionDb, review_id: &str, reviewer: &str) -> Option<String> {
        db.conn()
            .query_row(
                "SELECT vote FROM reviewer_votes WHERE review_id = ? AND reviewer = ?",
                params![review_id, reviewer],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
    }

    /// Helper: write raw string content to a file (no FileLog, exact bytes).
    fn write_raw(path: &std::path::Path, content: &str) {
        std::fs::write(path, content).unwrap();
    }

    /// Helper: append raw string content to a file.
    fn append_raw(path: &std::path::Path, content: &str) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();
    }

    /// Helper: make a block vote event.
    fn make_block_vote(review_id: &str, reviewer: &str, reason: &str) -> EventEnvelope {
        EventEnvelope::new(
            reviewer,
            Event::ReviewerVoted(ReviewerVoted {
                review_id: review_id.to_string(),
                vote: VoteType::Block,
                reason: Some(reason.to_string()),
            }),
        )
    }

    /// Helper: make an lgtm vote event.
    fn make_lgtm_vote(review_id: &str, reviewer: &str) -> EventEnvelope {
        EventEnvelope::new(
            reviewer,
            Event::ReviewerVoted(ReviewerVoted {
                review_id: review_id.to_string(),
                vote: VoteType::Lgtm,
                reason: Some("Looks good".to_string()),
            }),
        )
    }

    /// Baseline: incremental sync with block → lgtm votes, no empty lines.
    /// Simulates: seal block, then seal lgtm, then seal review.
    #[test]
    fn test_bd_1s1_baseline_incremental_vote_override() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let log = open_or_create(&log_path).unwrap();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Command 1: seal reviews create
        log.append(&make_review_created("cr-001")).unwrap();
        sync_from_log(&db, &log).unwrap();

        // Command 2: seal block
        log.append(&make_block_vote("cr-001", "reviewer-a", "Needs fixes"))
            .unwrap();
        sync_from_log(&db, &log).unwrap();
        assert_eq!(
            query_vote(&db, "cr-001", "reviewer-a"),
            Some("block".to_string())
        );
        assert!(db.has_blocking_votes("cr-001").unwrap());

        // Command 3: seal lgtm
        log.append(&make_lgtm_vote("cr-001", "reviewer-a")).unwrap();
        // The lgtm command does NOT re-sync; the NEXT command does.

        // Command 4: seal review (syncs the lgtm event)
        sync_from_log(&db, &log).unwrap();
        assert_eq!(
            query_vote(&db, "cr-001", "reviewer-a"),
            Some("lgtm".to_string())
        );
        assert!(!db.has_blocking_votes("cr-001").unwrap());
    }

    /// Test sync offset drift when empty line exists between block and lgtm.
    /// Verifies that empty lines cause re-processing but correct final state.
    #[test]
    fn test_bd_1s1_empty_line_between_votes() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let log = open_or_create(&log_path).unwrap();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Write review + block vote normally
        log.append(&make_review_created("cr-001")).unwrap();
        log.append(&make_block_vote("cr-001", "reviewer-a", "Needs fixes"))
            .unwrap();
        sync_from_log(&db, &log).unwrap();
        assert_eq!(db.get_last_sync_line().unwrap(), 2);
        assert_eq!(
            query_vote(&db, "cr-001", "reviewer-a"),
            Some("block".to_string())
        );

        // Inject empty line, then lgtm vote (bypassing FileLog::append)
        append_raw(&log_path, "\n"); // empty line at idx 2
        let lgtm = make_lgtm_vote("cr-001", "reviewer-a");
        let lgtm_json = lgtm.to_json_line().unwrap();
        append_raw(&log_path, &format!("{lgtm_json}\n")); // lgtm at idx 3

        // Sync: should pick up lgtm despite empty line
        let count = sync_from_log(&db, &log).unwrap();
        assert_eq!(count, 1, "Should process exactly 1 new event (lgtm)");
        assert_eq!(
            query_vote(&db, "cr-001", "reviewer-a"),
            Some("lgtm".to_string())
        );
        assert!(!db.has_blocking_votes("cr-001").unwrap());

        // Cursor correctly advances to total_lines (4), including the empty line
        let sync_line = db.get_last_sync_line().unwrap();
        let actual_lines: usize = std::fs::read_to_string(&log_path).unwrap().lines().count();
        assert_eq!(
            sync_line, 4,
            "Sync offset should match total_lines (no drift)"
        );
        assert_eq!(
            actual_lines, 4,
            "File should have 4 lines (including empty)"
        );

        // No re-processing needed — cursor is correct
        let count = sync_from_log(&db, &log).unwrap();
        assert_eq!(count, 0, "No drift means no re-processing");
        assert_eq!(
            query_vote(&db, "cr-001", "reviewer-a"),
            Some("lgtm".to_string())
        );
        assert!(!db.has_blocking_votes("cr-001").unwrap());
    }

    /// Test with trailing empty line (matches eval data: 20 events + empty line 21).
    /// Trailing empty lines do NOT cause drift because they're at the end:
    /// read_from(N) skips them and returns empty, so last_sync stays correct.
    #[test]
    fn test_bd_1s1_trailing_empty_line() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let log = open_or_create(&log_path).unwrap();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Write review + block + lgtm normally
        log.append(&make_review_created("cr-001")).unwrap();
        log.append(&make_block_vote("cr-001", "reviewer-a", "Needs fixes"))
            .unwrap();
        log.append(&make_lgtm_vote("cr-001", "reviewer-a")).unwrap();

        // Add trailing empty line (as seen in eval data)
        append_raw(&log_path, "\n");

        // Full sync from scratch
        let count = sync_from_log(&db, &log).unwrap();
        assert_eq!(count, 3);
        assert_eq!(
            query_vote(&db, "cr-001", "reviewer-a"),
            Some("lgtm".to_string())
        );
        assert!(!db.has_blocking_votes("cr-001").unwrap());

        // Sync line is 4 — total_lines() includes the trailing empty line
        let sync_line = db.get_last_sync_line().unwrap();
        assert_eq!(sync_line, 4);

        // No re-processing needed — cursor is at the correct position
        let count = sync_from_log(&db, &log).unwrap();
        assert_eq!(
            count, 0,
            "Trailing empty line should NOT cause re-processing"
        );
        assert_eq!(
            query_vote(&db, "cr-001", "reviewer-a"),
            Some("lgtm".to_string())
        );
    }

    /// Test with on-disk DB persistence (closer to real CLI behavior).
    /// Each "command" opens a fresh DB connection, syncs, then closes.
    #[test]
    fn test_bd_1s1_ondisk_persistence_vote_override() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let db_path = dir.path().join("index.db");
        let log = open_or_create(&log_path).unwrap();

        // Command 1: create review (opens fresh DB)
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            // No events yet, nothing to sync
            sync_from_log(&db, &log).unwrap();
        }
        log.append(&make_review_created("cr-001")).unwrap();

        // Command 2: block vote
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            sync_from_log(&db, &log).unwrap(); // syncs ReviewCreated
        }
        log.append(&make_block_vote("cr-001", "reviewer-a", "Needs fixes"))
            .unwrap();

        // Command 3: lgtm vote (syncs block, then writes lgtm)
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            sync_from_log(&db, &log).unwrap(); // syncs block vote
                                               // At this point, DB shows block
            assert_eq!(
                query_vote(&db, "cr-001", "reviewer-a"),
                Some("block".to_string())
            );
        }
        log.append(&make_lgtm_vote("cr-001", "reviewer-a")).unwrap();

        // Command 4: seal review (syncs lgtm, then reads)
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            sync_from_log(&db, &log).unwrap(); // syncs lgtm vote
                                               // Should show lgtm
            assert_eq!(
                query_vote(&db, "cr-001", "reviewer-a"),
                Some("lgtm".to_string())
            );
            assert!(!db.has_blocking_votes("cr-001").unwrap());
        }
    }

    /// Simulate the EXACT R4 eval pattern using raw event JSON lines.
    /// Uses the actual event data from /tmp/tmp.iyprC50GHo/.seal/events.jsonl.
    #[test]
    fn test_bd_1s1_eval_data_incremental_sync() {
        // Raw event lines from R4 eval (actual JSON from events.jsonl)
        let events: Vec<&str> = vec![
            r#"{"ts":"2026-01-31T17:18:32.592763788Z","author":"mystic-birch","event":"ReviewCreated","data":{"review_id":"cr-fjf9","jj_change_id":"lsrxqntnrzlyulstznuytqollqwpmnur","initial_commit":"ab7ef0a9cf8f1d01e7cd3c635429ba7195cf5e73","title":"feat: add GET /files/:name endpoint","description":"Adds file serving endpoint that reads from ./data"}}"#,
            r#"{"ts":"2026-01-31T17:18:36.109469869Z","author":"mystic-birch","event":"ReviewersRequested","data":{"review_id":"cr-fjf9","reviewers":["jasper-lattice"]}}"#,
            r#"{"ts":"2026-01-31T17:20:01.874064552Z","author":"jasper-lattice","event":"ThreadCreated","data":{"thread_id":"th-ooz8","review_id":"cr-fjf9","file_path":"src/main.rs","selection":{"type":"Line","line":22},"commit_hash":"517b3f84fcdba505957a74f79316ed5338911600"}}"#,
            r#"{"ts":"2026-01-31T17:20:01.874131748Z","author":"jasper-lattice","event":"CommentAdded","data":{"comment_id":"c-m9xz","thread_id":"th-ooz8","body":"CRITICAL: Path traversal vulnerability."}}"#,
            r#"{"ts":"2026-01-31T17:20:07.776996959Z","author":"jasper-lattice","event":"ThreadCreated","data":{"thread_id":"th-fj4b","review_id":"cr-fjf9","file_path":"src/main.rs","selection":{"type":"Line","line":24},"commit_hash":"e4c6aff98c3a5b4c133576d14818136f9d99b966"}}"#,
            r#"{"ts":"2026-01-31T17:20:07.777046382Z","author":"jasper-lattice","event":"CommentAdded","data":{"comment_id":"c-80oc","thread_id":"th-fj4b","body":"HIGH: Using synchronous filesystem I/O in async handler."}}"#,
            r#"{"ts":"2026-01-31T17:20:13.705333684Z","author":"jasper-lattice","event":"CommentAdded","data":{"comment_id":"c-ltji","thread_id":"th-fj4b","body":"HIGH: Unbounded memory consumption."}}"#,
            r#"{"ts":"2026-01-31T17:20:19.305357862Z","author":"jasper-lattice","event":"ThreadCreated","data":{"thread_id":"th-azov","review_id":"cr-fjf9","file_path":"src/main.rs","selection":{"type":"Line","line":29},"commit_hash":"c851779e0f7efbe3a9e3c2d6575c9c4d645eba37"}}"#,
            r#"{"ts":"2026-01-31T17:20:19.305407595Z","author":"jasper-lattice","event":"CommentAdded","data":{"comment_id":"c-3zdn","thread_id":"th-azov","body":"MEDIUM: Missing error information."}}"#,
            r#"{"ts":"2026-01-31T17:20:24.988565444Z","author":"jasper-lattice","event":"ThreadCreated","data":{"thread_id":"th-xgby","review_id":"cr-fjf9","file_path":"src/main.rs","selection":{"type":"Line","line":16},"commit_hash":"2dc3be9e8140dcc8b18b8aead907ca88a0a9bf0f"}}"#,
            r#"{"ts":"2026-01-31T17:20:24.988629895Z","author":"jasper-lattice","event":"CommentAdded","data":{"comment_id":"c-c8z0","thread_id":"th-xgby","body":"MEDIUM: Using unwrap() on production server startup."}}"#,
            r#"{"ts":"2026-01-31T17:20:31.713659068Z","author":"jasper-lattice","event":"ThreadCreated","data":{"thread_id":"th-a8ha","review_id":"cr-fjf9","file_path":"src/main.rs","selection":{"type":"Line","line":14},"commit_hash":"7240b65595aabcfc731f594b31fb37458c5ab58a"}}"#,
            r#"{"ts":"2026-01-31T17:20:31.713722667Z","author":"jasper-lattice","event":"CommentAdded","data":{"comment_id":"c-xla3","thread_id":"th-a8ha","body":"LOW: Binding to 0.0.0.0 exposes service to all network interfaces."}}"#,
            r#"{"ts":"2026-01-31T17:20:36.558747769Z","author":"jasper-lattice","event":"ReviewerVoted","data":{"review_id":"cr-fjf9","vote":"block","reason":"CRITICAL path traversal vulnerability allows reading arbitrary files."}}"#,
            r#"{"ts":"2026-01-31T17:27:06.693854670Z","author":"jasper-lattice","event":"ReviewerVoted","data":{"review_id":"cr-fjf9","vote":"lgtm","reason":"All issues resolved."}}"#,
            r#"{"ts":"2026-01-31T17:27:30.337341106Z","author":"jasper-lattice","event":"ReviewerVoted","data":{"review_id":"cr-fjf9","vote":"lgtm"}}"#,
            r#"{"ts":"2026-01-31T17:27:39.741734997Z","author":"jasper-lattice","event":"ReviewerVoted","data":{"review_id":"cr-fjf9","vote":"lgtm","reason":"All security issues resolved"}}"#,
            r#"{"ts":"2026-01-31T17:28:03.984462867Z","author":"jasper-lattice","event":"ReviewerVoted","data":{"review_id":"cr-fjf9","vote":"lgtm"}}"#,
            r#"{"ts":"2026-01-31T17:28:15.551617462Z","author":"jasper-lattice","event":"ReviewApproved","data":{"review_id":"cr-fjf9"}}"#,
            r#"{"ts":"2026-01-31T17:29:25.525546004Z","author":"mystic-birch","event":"ReviewMerged","data":{"review_id":"cr-fjf9","final_commit":"72f4e86c4ab0ca0c48150a6c26bfeeeb4e6b9d98"}}"#,
        ];

        // Simulate CLI command batches (which events are written per command):
        // Each tuple: (events_written_this_batch, description)
        let batches: Vec<(usize, &str)> = vec![
            (1, "seal reviews create"),            // event 0
            (1, "seal reviews request-reviewers"), // event 1
            (2, "seal comment (thread+comment)"),  // events 2-3
            (2, "seal comment (thread+comment)"),  // events 4-5
            (1, "seal reply"),                     // event 6
            (2, "seal comment (thread+comment)"),  // events 7-8
            (2, "seal comment (thread+comment)"),  // events 9-10
            (2, "seal comment (thread+comment)"),  // events 11-12
            (1, "seal block"),                     // event 13
            (1, "seal lgtm (attempt 1)"),          // event 14
            (1, "seal lgtm (attempt 2)"),          // event 15
            (1, "seal lgtm (attempt 3)"),          // event 16
            (1, "seal lgtm (attempt 4)"),          // event 17
            (1, "seal reviews approve"),           // event 18
            (1, "seal reviews mark-merged"),       // event 19
        ];

        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let db_path = dir.path().join("index.db");

        // Create empty events file
        std::fs::write(&log_path, "").unwrap();

        let mut event_idx = 0;
        for (batch_size, desc) in &batches {
            // Each CLI command: open DB, sync, close DB
            {
                let db = ProjectionDb::open(&db_path).unwrap();
                db.init_schema().unwrap();
                let log = crate::log::FileLog::new(&log_path);
                sync_from_log(&db, &log).unwrap();

                // After syncing block vote, check state
                if *desc == "seal lgtm (attempt 1)" {
                    // DB should have block vote at this point
                    assert_eq!(
                        query_vote(&db, "cr-fjf9", "jasper-lattice"),
                        Some("block".to_string()),
                        "Before first lgtm write, DB should show block"
                    );
                }

                // After syncing first lgtm, check state
                if *desc == "seal lgtm (attempt 2)" {
                    // DB should have lgtm vote (from first lgtm at line 14)
                    let vote = query_vote(&db, "cr-fjf9", "jasper-lattice");
                    assert_eq!(
                        vote,
                        Some("lgtm".to_string()),
                        "After syncing first lgtm, DB should show lgtm (was: {vote:?}) [{desc}]"
                    );
                    assert!(
                        !db.has_blocking_votes("cr-fjf9").unwrap(),
                        "No blocking votes after lgtm synced"
                    );
                }
            }

            // Then write this batch's events to the log
            for _ in 0..*batch_size {
                append_raw(&log_path, &format!("{}\n", events[event_idx]));
                event_idx += 1;
            }
        }

        // Final check: open DB, sync remaining events, verify merged state
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);
            sync_from_log(&db, &log).unwrap();

            assert_eq!(
                query_vote(&db, "cr-fjf9", "jasper-lattice"),
                Some("lgtm".to_string()),
                "Final state should be lgtm"
            );
            assert!(!db.has_blocking_votes("cr-fjf9").unwrap());

            // Verify review was merged
            let status: String = db
                .conn()
                .query_row(
                    "SELECT status FROM reviews WHERE review_id = ?",
                    params!["cr-fjf9"],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(status, "merged");
        }
    }

    /// Same as eval data test but with trailing empty line (as found in eval).
    /// Trailing empty lines do NOT cause drift — only middle empty lines do.
    #[test]
    fn test_bd_1s1_eval_data_with_trailing_empty_line() {
        let events: Vec<&str> = vec![
            r#"{"ts":"2026-01-31T17:18:32.592763788Z","author":"mystic-birch","event":"ReviewCreated","data":{"review_id":"cr-fjf9","jj_change_id":"lsrxqntnrzlyulstznuytqollqwpmnur","initial_commit":"ab7ef0a9cf8f1d01e7cd3c635429ba7195cf5e73","title":"feat: add GET /files/:name endpoint","description":"Adds file serving endpoint that reads from ./data"}}"#,
            r#"{"ts":"2026-01-31T17:18:36.109469869Z","author":"mystic-birch","event":"ReviewersRequested","data":{"review_id":"cr-fjf9","reviewers":["jasper-lattice"]}}"#,
            r#"{"ts":"2026-01-31T17:20:36.558747769Z","author":"jasper-lattice","event":"ReviewerVoted","data":{"review_id":"cr-fjf9","vote":"block","reason":"CRITICAL vulnerability"}}"#,
            r#"{"ts":"2026-01-31T17:27:06.693854670Z","author":"jasper-lattice","event":"ReviewerVoted","data":{"review_id":"cr-fjf9","vote":"lgtm","reason":"All issues resolved."}}"#,
        ];

        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");

        // Write all events + trailing empty line (matches eval file)
        let mut content = String::new();
        for event in &events {
            content.push_str(event);
            content.push('\n');
        }
        content.push('\n'); // trailing empty line
        write_raw(&log_path, &content);

        // Full sync from fresh DB
        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let log = crate::log::FileLog::new(&log_path);

        let count = sync_from_log(&db, &log).unwrap();
        assert_eq!(count, 4, "Should process 4 events (skip empty line)");

        assert_eq!(
            query_vote(&db, "cr-fjf9", "jasper-lattice"),
            Some("lgtm".to_string()),
            "Full sync should show lgtm"
        );
        assert!(!db.has_blocking_votes("cr-fjf9").unwrap());

        // Sync line is 5 (4 events + 1 trailing empty = 5 total_lines)
        assert_eq!(
            db.get_last_sync_line().unwrap(),
            5,
            "Sync line should be total_lines (5)"
        );

        // No re-processing needed — cursor is at the correct position
        let count = sync_from_log(&db, &log).unwrap();
        assert_eq!(count, 0, "No drift means no re-processing");
        assert_eq!(
            query_vote(&db, "cr-fjf9", "jasper-lattice"),
            Some("lgtm".to_string()),
            "Vote should still be lgtm after re-sync"
        );
    }

    /// Test that multiple empty lines cause proportionally more drift.
    /// With enough empty lines, sync could re-read the block vote.
    #[test]
    fn test_bd_1s1_multiple_empty_lines_drift() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");

        let review = make_review_created("cr-001");
        let block = make_block_vote("cr-001", "reviewer-a", "Needs fixes");
        let lgtm = make_lgtm_vote("cr-001", "reviewer-a");

        // Write: review, empty, empty, block, empty, empty, empty, lgtm
        let mut content = String::new();
        content.push_str(&review.to_json_line().unwrap());
        content.push('\n');
        content.push('\n'); // empty at idx 1
        content.push('\n'); // empty at idx 2
        content.push_str(&block.to_json_line().unwrap());
        content.push('\n'); // block at idx 3
        content.push('\n'); // empty at idx 4
        content.push('\n'); // empty at idx 5
        content.push('\n'); // empty at idx 6
        content.push_str(&lgtm.to_json_line().unwrap());
        content.push('\n'); // lgtm at idx 7
        write_raw(&log_path, &content);

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let log = crate::log::FileLog::new(&log_path);

        // Full sync: processes review, block, lgtm (3 events from 8 lines)
        let count = sync_from_log(&db, &log).unwrap();
        assert_eq!(count, 3, "Should process 3 events, skipping 5 empty lines");

        assert_eq!(
            query_vote(&db, "cr-001", "reviewer-a"),
            Some("lgtm".to_string()),
            "Full sync should show lgtm"
        );

        // Cursor correctly advances to total_lines (8), no drift
        let sync_line = db.get_last_sync_line().unwrap();
        assert_eq!(sync_line, 8, "Cursor should match total_lines (no drift)");

        // Re-sync should find nothing — cursor is at the correct position
        let count = sync_from_log(&db, &log).unwrap();
        assert_eq!(count, 0, "No drift means no re-processing");

        assert_eq!(
            query_vote(&db, "cr-001", "reviewer-a"),
            Some("lgtm".to_string()),
            "Vote should remain lgtm"
        );
    }

    /// Test the dangerous scenario: block is re-processed in isolation
    /// (without lgtm in the same batch) due to empty lines.
    ///
    /// This tests whether progressive drift accumulation can cause the block
    /// to be replayed WITHOUT the subsequent lgtm, which would regress the vote.
    #[test]
    fn test_bd_1s1_progressive_drift_vote_regression() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let db_path = dir.path().join("index.db");

        let review = make_review_created("cr-001");
        let block = make_block_vote("cr-001", "reviewer-a", "Needs fixes");
        let lgtm = make_lgtm_vote("cr-001", "reviewer-a");

        // Step 1: Write review + block, sync
        write_raw(&log_path, "");
        append_raw(&log_path, &format!("{}\n", review.to_json_line().unwrap()));
        append_raw(&log_path, &format!("{}\n", block.to_json_line().unwrap()));

        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);
            sync_from_log(&db, &log).unwrap();
            assert_eq!(db.get_last_sync_line().unwrap(), 2);
            assert_eq!(
                query_vote(&db, "cr-001", "reviewer-a"),
                Some("block".to_string())
            );
        }

        // Step 2: Inject empty line + lgtm, sync
        append_raw(&log_path, "\n"); // empty at idx 2
        append_raw(&log_path, &format!("{}\n", lgtm.to_json_line().unwrap())); // lgtm at idx 3

        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);
            sync_from_log(&db, &log).unwrap();
            // Cursor advances to total_lines (4), no drift
            assert_eq!(db.get_last_sync_line().unwrap(), 4);
            assert_eq!(
                query_vote(&db, "cr-001", "reviewer-a"),
                Some("lgtm".to_string()),
                "After syncing lgtm, vote should be lgtm"
            );
        }

        // Step 3: No new events — sync should be clean immediately
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);
            let count = sync_from_log(&db, &log).unwrap();

            assert_eq!(count, 0, "No drift means no re-processing");
            assert_eq!(
                query_vote(&db, "cr-001", "reviewer-a"),
                Some("lgtm".to_string()),
                "Vote must remain lgtm after re-sync"
            );
            assert_eq!(db.get_last_sync_line().unwrap(), 4);
        }
    }

    /// jj working copy restoration causes events.jsonl to revert to an older
    /// version while index.db retains a stale last_sync_line.
    ///
    /// With the truncation detection fix, sync_from_log detects that
    /// last_sync_line > total file lines and rebuilds the projection.
    #[test]
    fn test_bd_1s1_jj_restore_triggers_rebuild() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let db_path = dir.path().join("index.db");

        // === Phase 1: Normal operation — create review + block vote ===
        let review = make_review_created("cr-001");
        let reviewers = EventEnvelope::new(
            "author",
            Event::ReviewersRequested(ReviewersRequested {
                review_id: "cr-001".to_string(),
                reviewers: vec!["reviewer-a".to_string()],
            }),
        );
        let block = make_block_vote("cr-001", "reviewer-a", "Needs fixes");

        // Write 3 events
        write_raw(&log_path, "");
        for event in [&review, &reviewers, &block] {
            append_raw(&log_path, &format!("{}\n", event.to_json_line().unwrap()));
        }

        // Simulate CLI: open DB, sync all 3 events
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);
            let count = sync_from_log(&db, &log).unwrap();
            assert_eq!(count, 3);
            assert_eq!(db.get_last_sync_line().unwrap(), 3);
            assert_eq!(
                query_vote(&db, "cr-001", "reviewer-a"),
                Some("block".to_string())
            );
        }

        // === Phase 2: jj restores events.jsonl to an older version ===
        let restored_content = format!("{}\n", review.to_json_line().unwrap());
        write_raw(&log_path, &restored_content);
        // File: 1 event. DB: last_sync_line=3, block vote present.

        // === Phase 3: seal lgtm — sync detects truncation, rebuilds ===
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);
            let count = sync_from_log(&db, &log).unwrap();
            // Truncation detected (last_sync=3 > total_lines=1) → rebuild from file
            assert_eq!(count, 1, "Rebuild replays the 1 event in the restored file");
            assert_eq!(db.get_last_sync_line().unwrap(), 1);

            // Block vote is GONE — it's not in the restored file
            assert_eq!(query_vote(&db, "cr-001", "reviewer-a"), None);
            assert!(!db.has_blocking_votes("cr-001").unwrap());
        }

        // Append lgtm vote
        let lgtm = make_lgtm_vote("cr-001", "reviewer-a");
        append_raw(&log_path, &format!("{}\n", lgtm.to_json_line().unwrap()));

        // === Phase 4: seal review — picks up lgtm normally ===
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);
            let count = sync_from_log(&db, &log).unwrap();
            assert_eq!(count, 1, "Syncs the new lgtm event");

            // FIXED: vote is lgtm!
            assert_eq!(
                query_vote(&db, "cr-001", "reviewer-a"),
                Some("lgtm".to_string()),
                "Vote should be lgtm after rebuild + sync"
            );
            assert!(!db.has_blocking_votes("cr-001").unwrap());
        }
    }

    /// Variant: jj restores events.jsonl to a version with MORE events
    /// than index.db has seen (e.g., workspace merge brings in new events).
    /// This should work correctly.
    #[test]
    fn test_bd_1s1_jj_restore_with_more_events() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let db_path = dir.path().join("index.db");

        // Write just ReviewCreated, sync
        let review = make_review_created("cr-001");
        write_raw(&log_path, &format!("{}\n", review.to_json_line().unwrap()));

        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);
            sync_from_log(&db, &log).unwrap();
            assert_eq!(db.get_last_sync_line().unwrap(), 1);
        }

        // jj restore brings in a version with MORE events (block + lgtm)
        let block = make_block_vote("cr-001", "reviewer-a", "Needs fixes");
        let lgtm = make_lgtm_vote("cr-001", "reviewer-a");
        use std::fmt::Write as _;
        let mut content = format!("{}\n", review.to_json_line().unwrap());
        writeln!(content, "{}", block.to_json_line().unwrap()).unwrap();
        writeln!(content, "{}", lgtm.to_json_line().unwrap()).unwrap();
        write_raw(&log_path, &content);

        // Sync should pick up new events from line 1 onwards
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);
            let count = sync_from_log(&db, &log).unwrap();
            assert_eq!(count, 2, "Should pick up block + lgtm");
            assert_eq!(
                query_vote(&db, "cr-001", "reviewer-a"),
                Some("lgtm".to_string())
            );
        }
    }

    /// bd-oum: jj restores events.jsonl to a version with the SAME number
    /// of lines but DIFFERENT content. The truncation check (line count) passes,
    /// but the content hash detects the replacement and triggers a rebuild.
    #[test]
    fn test_bd_oum_same_length_content_replacement() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let db_path = dir.path().join("index.db");

        // === Phase 1: Create review cr-001 with a block vote, sync ===
        let review1 = make_review_created("cr-001");
        let block1 = make_block_vote("cr-001", "reviewer-a", "Needs fixes");
        write_raw(&log_path, "");
        for event in [&review1, &block1] {
            append_raw(&log_path, &format!("{}\n", event.to_json_line().unwrap()));
        }

        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);
            let count = sync_from_log(&db, &log).unwrap();
            assert_eq!(count, 2);
            assert_eq!(db.get_last_sync_line().unwrap(), 2);
            assert_eq!(
                query_vote(&db, "cr-001", "reviewer-a"),
                Some("block".to_string())
            );

            // Hash should be stored
            assert!(
                db.get_events_file_hash().unwrap().is_some(),
                "Hash should be stored after sync"
            );
        }

        // === Phase 2: jj replaces file with SAME line count, DIFFERENT content ===
        // A different review (cr-002) with an lgtm vote — same 2 lines, different content.
        let review2 = make_review_created("cr-002");
        let lgtm2 = make_lgtm_vote("cr-002", "reviewer-b");
        let replaced = format!(
            "{}\n{}\n",
            review2.to_json_line().unwrap(),
            lgtm2.to_json_line().unwrap()
        );
        write_raw(&log_path, &replaced);

        // File still has 2 lines — truncation check won't catch this.
        let line_count = std::fs::read_to_string(&log_path).unwrap().lines().count();
        assert_eq!(line_count, 2, "File should still have 2 lines");

        // === Phase 3: sync detects hash mismatch, rebuilds ===
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);
            let count = sync_from_log(&db, &log).unwrap();

            // Rebuild replays the 2 events from the replaced file
            assert_eq!(count, 2, "Rebuild replays all events from replaced file");
            assert_eq!(db.get_last_sync_line().unwrap(), 2);

            // cr-001 block vote is GONE (not in replaced file)
            assert_eq!(query_vote(&db, "cr-001", "reviewer-a"), None);

            // cr-002 lgtm vote is present (from replaced file)
            assert_eq!(
                query_vote(&db, "cr-002", "reviewer-b"),
                Some("lgtm".to_string())
            );
        }
    }

    /// bd-oum: Verify that the hash is backfilled on existing databases
    /// that were created before hash tracking was added.
    #[test]
    fn test_bd_oum_hash_backfill_on_noop_sync() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let db_path = dir.path().join("index.db");

        // Write 2 events and sync
        let review = make_review_created("cr-001");
        let block = make_block_vote("cr-001", "reviewer-a", "Needs fixes");
        write_raw(&log_path, "");
        for event in [&review, &block] {
            append_raw(&log_path, &format!("{}\n", event.to_json_line().unwrap()));
        }

        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);
            sync_from_log(&db, &log).unwrap();
            assert!(db.get_events_file_hash().unwrap().is_some());

            // Simulate a pre-hash database by clearing the hash
            db.conn()
                .execute(
                    "UPDATE sync_state SET events_file_hash = NULL WHERE id = 1",
                    [],
                )
                .unwrap();
            assert!(db.get_events_file_hash().unwrap().is_none());
        }

        // No-op sync should backfill the hash
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);
            let count = sync_from_log(&db, &log).unwrap();
            assert_eq!(count, 0, "No new events to process");
            assert!(
                db.get_events_file_hash().unwrap().is_some(),
                "Hash should be backfilled"
            );
        }

        // Now same-length replacement should be detected
        let review2 = make_review_created("cr-002");
        let lgtm2 = make_lgtm_vote("cr-002", "reviewer-b");
        let replaced = format!(
            "{}\n{}\n",
            review2.to_json_line().unwrap(),
            lgtm2.to_json_line().unwrap()
        );
        write_raw(&log_path, &replaced);

        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);
            let count = sync_from_log(&db, &log).unwrap();
            assert_eq!(count, 2, "Should rebuild from replaced file");
            assert_eq!(query_vote(&db, "cr-001", "reviewer-a"), None);
            assert_eq!(
                query_vote(&db, "cr-002", "reviewer-b"),
                Some("lgtm".to_string())
            );
        }
    }

    // ========================================================================
    // End of bd-1s1 reproduction tests
    // ========================================================================

    #[test]
    fn test_reviewer_vote_replacement() {
        use crate::events::VoteType;

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Create a review
        apply_event(&db, &make_review_created("cr-001")).unwrap();

        // Reviewer casts block vote
        apply_event(
            &db,
            &EventEnvelope::new(
                "reviewer",
                Event::ReviewerVoted(ReviewerVoted {
                    review_id: "cr-001".to_string(),
                    vote: VoteType::Block,
                    reason: Some("Needs fixes".to_string()),
                }),
            ),
        )
        .unwrap();

        // Verify block vote exists
        let (vote, reason): (String, Option<String>) = db
            .conn()
            .query_row(
                "SELECT vote, reason FROM reviewer_votes WHERE review_id = ? AND reviewer = ?",
                params!["cr-001", "reviewer"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(vote, "block");
        assert_eq!(reason, Some("Needs fixes".to_string()));

        // Reviewer changes to LGTM vote
        apply_event(
            &db,
            &EventEnvelope::new(
                "reviewer",
                Event::ReviewerVoted(ReviewerVoted {
                    review_id: "cr-001".to_string(),
                    vote: VoteType::Lgtm,
                    reason: Some("Looks good now".to_string()),
                }),
            ),
        )
        .unwrap();

        // Verify LGTM vote replaced block vote
        let (vote, reason): (String, Option<String>) = db
            .conn()
            .query_row(
                "SELECT vote, reason FROM reviewer_votes WHERE review_id = ? AND reviewer = ?",
                params!["cr-001", "reviewer"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(vote, "lgtm");
        assert_eq!(reason, Some("Looks good now".to_string()));

        // Verify only one vote row exists
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM reviewer_votes WHERE review_id = ? AND reviewer = ?",
                params!["cr-001", "reviewer"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Verify has_blocking_votes returns false
        let has_blocks = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM reviewer_votes WHERE review_id = ? AND vote = 'block'",
                params!["cr-001"],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(has_blocks, 0);
    }

    /// bd-2m6: Test orphan detection saves lost reviews to backup file
    /// when truncation is detected and seal_dir is provided.
    #[test]
    fn test_bd_2m6_orphan_detection_backup() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let db_path = dir.path().join("index.db");
        let seal_dir = dir.path();

        // === Phase 1: Create two reviews and sync ===
        let review1 = make_review_created("cr-001");
        let review2 = make_review_created("cr-002");
        write_raw(&log_path, "");
        append_raw(&log_path, &format!("{}\n", review1.to_json_line().unwrap()));
        append_raw(&log_path, &format!("{}\n", review2.to_json_line().unwrap()));

        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);
            let count = sync_from_log_with_backup(&db, &log, Some(seal_dir)).unwrap();
            assert_eq!(count, 2);

            // Both reviews should exist
            assert!(db.get_review("cr-001").unwrap().is_some());
            assert!(db.get_review("cr-002").unwrap().is_some());
        }

        // === Phase 2: Truncate file to only contain cr-002 ===
        // This simulates jj restoring an older version
        let truncated = format!("{}\n", review2.to_json_line().unwrap());
        write_raw(&log_path, &truncated);

        // === Phase 3: Sync with backup enabled ===
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap();
            let log = crate::log::FileLog::new(&log_path);

            // This should detect truncation and create a backup file
            let count = sync_from_log_with_backup(&db, &log, Some(seal_dir)).unwrap();
            assert_eq!(count, 1, "Rebuild from truncated file");

            // cr-001 is now gone (orphaned)
            assert!(db.get_review("cr-001").unwrap().is_none());
            // cr-002 still exists
            assert!(db.get_review("cr-002").unwrap().is_some());
        }

        // === Phase 4: Verify backup file was created ===
        let backup_files: Vec<_> = std::fs::read_dir(seal_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("orphaned-reviews-")
            })
            .collect();

        assert_eq!(backup_files.len(), 1, "Should have created one backup file");

        let backup_content = std::fs::read_to_string(backup_files[0].path()).unwrap();
        let backup: serde_json::Value = serde_json::from_str(&backup_content).unwrap();

        // Verify backup contains cr-001
        let orphaned = backup["orphaned_reviews"].as_array().unwrap();
        assert_eq!(orphaned.len(), 1);
        assert_eq!(orphaned[0]["review_id"], "cr-001");
    }

    // ========================================================================
    // bd-13r: Schema migration tests
    // ========================================================================

    #[test]
    fn test_migrate_schema_adds_next_comment_number() {
        // Simulate an old database that was created before next_comment_number
        // column was added to the threads table.
        //
        // This test creates a database with the full schema but manually
        // removes the next_comment_number column to simulate upgrading from
        // an old version.
        let tmp_dir = tempfile::tempdir().unwrap();
        let db_path = tmp_dir.path().join("test.db");

        // Create an old-style database without next_comment_number
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            // Old threads table schema (matches current schema except no next_comment_number)
            // We need to create all the tables since SCHEMA_SQL creates indexes and views
            // that reference them.
            conn.execute_batch(
                "CREATE TABLE sync_state (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    last_line_number INTEGER NOT NULL DEFAULT 0,
                    last_event_time TEXT
                );
                INSERT INTO sync_state (id) VALUES (1);

                CREATE TABLE reviews (
                    review_id TEXT PRIMARY KEY,
                    jj_change_id TEXT NOT NULL,
                    initial_commit TEXT NOT NULL,
                    final_commit TEXT,
                    title TEXT NOT NULL,
                    description TEXT,
                    author TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'open',
                    status_changed_at TEXT,
                    status_changed_by TEXT,
                    abandon_reason TEXT
                );

                -- Old threads table without next_comment_number
                CREATE TABLE threads (
                    thread_id TEXT PRIMARY KEY,
                    review_id TEXT NOT NULL REFERENCES reviews(review_id),
                    file_path TEXT NOT NULL,
                    selection_type TEXT NOT NULL CHECK (selection_type IN ('line', 'range')),
                    selection_start INTEGER NOT NULL,
                    selection_end INTEGER,
                    commit_hash TEXT NOT NULL,
                    author TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'open'
                        CHECK (status IN ('open', 'resolved')),
                    status_changed_at TEXT,
                    status_changed_by TEXT,
                    resolve_reason TEXT,
                    reopen_reason TEXT
                    -- NOTE: next_comment_number column is intentionally MISSING
                );

                CREATE TABLE comments (
                    comment_id TEXT PRIMARY KEY,
                    thread_id TEXT NOT NULL REFERENCES threads(thread_id),
                    body TEXT NOT NULL,
                    author TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );

                CREATE TABLE reviewer_requests (
                    review_id TEXT NOT NULL REFERENCES reviews(review_id),
                    reviewer TEXT NOT NULL,
                    requested_at TEXT NOT NULL,
                    requested_by TEXT NOT NULL,
                    PRIMARY KEY (review_id, reviewer)
                );

                CREATE TABLE reviewer_votes (
                    review_id TEXT NOT NULL REFERENCES reviews(review_id),
                    reviewer TEXT NOT NULL,
                    vote TEXT NOT NULL,
                    reason TEXT,
                    voted_at TEXT NOT NULL,
                    PRIMARY KEY (review_id, reviewer)
                );

                -- Add test data: a review and thread
                INSERT INTO reviews (review_id, jj_change_id, initial_commit, title, author, created_at)
                VALUES ('cr-001', 'abc123', 'def456', 'Test Review', 'test', '2026-01-01T00:00:00Z');

                INSERT INTO threads (thread_id, review_id, file_path, selection_type,
                    selection_start, commit_hash, author, created_at)
                VALUES ('th-001', 'cr-001', 'src/lib.rs', 'line', 42, 'abc123',
                    'test', '2026-01-01T00:00:00Z');",
            )
            .unwrap();

            // Verify no next_comment_number column exists
            let has_column: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('threads') WHERE name = 'next_comment_number'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(
                !has_column,
                "Old database should not have next_comment_number"
            );
        }

        // Now open with ProjectionDb which should run migration
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            // init_schema should run migrate_schema which adds the column
            db.init_schema().unwrap();

            // Verify column now exists
            let has_column: bool = db
                .conn()
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('threads') WHERE name = 'next_comment_number'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(
                has_column,
                "Migration should have added next_comment_number"
            );

            // Verify existing row got default value
            let next_num: i64 = db
                .conn()
                .query_row(
                    "SELECT next_comment_number FROM threads WHERE thread_id = ?",
                    params!["th-001"],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(next_num, 1, "Existing row should have default value 1");
        }

        // Verify migration is idempotent (can run again without error)
        {
            let db = ProjectionDb::open(&db_path).unwrap();
            db.init_schema().unwrap(); // Should not fail on second run
        }
    }

    // ========================================================================
    // bd-2ys: Orphaned event filtering tests
    // ========================================================================

    fn make_comment_added(comment_id: &str, thread_id: &str) -> EventEnvelope {
        EventEnvelope::new(
            "test_author",
            Event::CommentAdded(CommentAdded {
                comment_id: comment_id.to_string(),
                thread_id: thread_id.to_string(),
                body: "Test comment".to_string(),
            }),
        )
    }

    fn make_thread_resolved(thread_id: &str) -> EventEnvelope {
        EventEnvelope::new(
            "test_author",
            Event::ThreadResolved(ThreadResolved {
                thread_id: thread_id.to_string(),
                reason: Some("Fixed".to_string()),
            }),
        )
    }

    #[test]
    fn test_bd_2ys_filter_orphaned_skips_events_without_review_created() {
        // Simulate: ThreadCreated + CommentAdded for cr-orphan (no ReviewCreated)
        let events = vec![
            make_thread_created("th-001", "cr-orphan"),
            make_comment_added("th-001.1", "th-001"),
        ];

        let (filtered, skipped) = filter_orphaned_events(events);
        assert_eq!(skipped, 2);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_bd_2ys_filter_orphaned_keeps_valid_events() {
        // cr-good has ReviewCreated; cr-orphan does not
        let events = vec![
            make_review_created("cr-good"),
            make_thread_created("th-001", "cr-good"),
            make_comment_added("th-001.1", "th-001"),
            make_thread_created("th-002", "cr-orphan"),
            make_comment_added("th-002.1", "th-002"),
        ];

        let (filtered, skipped) = filter_orphaned_events(events);
        assert_eq!(skipped, 2, "should skip 2 orphaned events");
        assert_eq!(filtered.len(), 3, "should keep 3 valid events");
    }

    #[test]
    fn test_bd_2ys_filter_orphaned_handles_thread_only_events() {
        // ThreadResolved references a thread whose review is orphaned
        let events = vec![
            make_review_created("cr-good"),
            make_thread_created("th-001", "cr-good"),
            make_thread_created("th-002", "cr-orphan"),
            make_thread_resolved("th-002"),
            make_comment_added("th-002.1", "th-002"),
        ];

        let (filtered, skipped) = filter_orphaned_events(events);
        assert_eq!(
            skipped, 3,
            "should skip ThreadCreated + ThreadResolved + CommentAdded for orphan"
        );
        assert_eq!(
            filtered.len(),
            2,
            "should keep ReviewCreated + ThreadCreated for cr-good"
        );
    }

    #[test]
    fn test_bd_2ys_filter_orphaned_no_orphans_passes_through() {
        let events = vec![
            make_review_created("cr-001"),
            make_thread_created("th-001", "cr-001"),
            make_comment_added("th-001.1", "th-001"),
        ];

        let (filtered, skipped) = filter_orphaned_events(events);
        assert_eq!(skipped, 0);
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn test_bd_2ys_orphaned_events_dont_crash_sync() {
        // Integration test: orphaned events should not cause FK error during sync
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        // Write events WITHOUT ReviewCreated (simulating destroyed workspace)
        let log = crate::log::ReviewLog::new(seal_root, "cr-orphan").unwrap();
        log.append(&make_thread_created("th-001", "cr-orphan"))
            .unwrap();
        log.append(&make_comment_added("th-001.1", "th-001"))
            .unwrap();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // This should NOT fail with FK constraint error
        let result = sync_from_review_logs(&db, seal_root);
        assert!(
            result.is_ok(),
            "sync should succeed even with orphaned events: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_bd_2ys_orphaned_events_dont_crash_rebuild() {
        // Integration test: orphaned events should not cause FK error during rebuild
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        // Write a valid review
        let good_log = crate::log::ReviewLog::new(seal_root, "cr-good").unwrap();
        good_log.append(&make_review_created("cr-good")).unwrap();
        good_log
            .append(&make_thread_created("th-good", "cr-good"))
            .unwrap();

        // Write orphaned events (no ReviewCreated)
        let orphan_log = crate::log::ReviewLog::new(seal_root, "cr-orphan").unwrap();
        orphan_log
            .append(&make_thread_created("th-orphan", "cr-orphan"))
            .unwrap();
        orphan_log
            .append(&make_comment_added("th-orphan.1", "th-orphan"))
            .unwrap();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Rebuild should succeed, applying only cr-good events
        let count = rebuild_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(count, 2, "should apply 2 events from cr-good");

        // Verify cr-good was indexed
        let title: String = db
            .conn()
            .query_row(
                "SELECT title FROM reviews WHERE review_id = ?",
                params!["cr-good"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(title, "Review cr-good");

        // Verify cr-orphan was NOT indexed
        let orphan_exists: bool = db
            .conn()
            .query_row(
                "SELECT COUNT(*) > 0 FROM reviews WHERE review_id = ?",
                params!["cr-orphan"],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !orphan_exists,
            "orphaned review should not be in the projection"
        );
    }

    #[test]
    fn test_bd_1mn_identical_timestamps_not_skipped() {
        // Regression test: events sharing the exact same timestamp as the sync cursor
        // must not be skipped on incremental sync. This simulates `seal comment` which
        // writes ThreadCreated + CommentAdded with the same timestamp.
        use chrono::TimeZone;

        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Fixed timestamp to simulate simultaneous events
        let fixed_ts = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();

        // Batch 1: ReviewCreated + ThreadCreated + CommentAdded, all at the same timestamp
        let mut review_evt = make_review_created("cr-ts1");
        review_evt.ts = fixed_ts;

        let mut thread_evt = make_thread_created("th-ts1", "cr-ts1");
        thread_evt.ts = fixed_ts;

        let mut comment_evt = make_comment_added("th-ts1.1", "th-ts1");
        comment_evt.ts = fixed_ts;

        let log = crate::log::ReviewLog::new(seal_root, "cr-ts1").unwrap();
        log.append(&review_evt).unwrap();
        log.append(&thread_evt).unwrap();
        log.append(&comment_evt).unwrap();

        // First sync: all 3 events should be processed
        let report = sync_from_review_logs(&db, seal_root).unwrap();
        assert!(
            report.applied >= 3,
            "first sync should process all 3 events, got {}",
            report.applied
        );

        // Verify all data landed
        let review_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
            .unwrap();
        assert_eq!(review_count, 1, "should have 1 review");

        let thread_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(thread_count, 1, "should have 1 thread");

        let comment_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM comments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(comment_count, 1, "should have 1 comment");

        // Batch 2: Add another comment at the SAME timestamp as the sync cursor
        let mut comment2_evt = make_comment_added("th-ts1.2", "th-ts1");
        comment2_evt.ts = fixed_ts;
        log.append(&comment2_evt).unwrap();

        // Second sync: the new comment must be picked up
        let report2 = sync_from_review_logs(&db, seal_root).unwrap();
        assert!(
            report2.applied > 0,
            "second sync should pick up new events, got {}",
            report2.applied
        );

        let comment_count2: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM comments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            comment_count2, 2,
            "should have 2 comments after second sync"
        );
    }

    #[test]
    fn test_bd_1mn_resync_idempotent_no_duplicates() {
        // Verify that re-syncing with >= doesn't create duplicate data.
        // All apply_* handlers use INSERT OR IGNORE / ON CONFLICT / status guards.
        use chrono::TimeZone;

        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        let fixed_ts = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();

        let mut review_evt = make_review_created("cr-idem");
        review_evt.ts = fixed_ts;

        let mut thread_evt = make_thread_created("th-idem", "cr-idem");
        thread_evt.ts = fixed_ts;

        let mut comment_evt = make_comment_added("th-idem.1", "th-idem");
        comment_evt.ts = fixed_ts;

        let log = crate::log::ReviewLog::new(seal_root, "cr-idem").unwrap();
        log.append(&review_evt).unwrap();
        log.append(&thread_evt).unwrap();
        log.append(&comment_evt).unwrap();

        // Sync twice — second sync re-processes boundary events
        sync_from_review_logs(&db, seal_root).unwrap();
        sync_from_review_logs(&db, seal_root).unwrap();

        // No duplicates
        let review_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
            .unwrap();
        assert_eq!(review_count, 1, "no duplicate reviews");

        let thread_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(thread_count, 1, "no duplicate threads");

        let comment_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM comments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(comment_count, 1, "no duplicate comments");

        apply_event(&db, &comment_evt).unwrap();

        let next_comment_number: i64 = db
            .conn()
            .query_row(
                "SELECT next_comment_number FROM threads WHERE thread_id = ?",
                params!["th-idem"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            next_comment_number, 2,
            "duplicate comment replay must not advance next_comment_number"
        );
    }

    #[test]
    fn test_bd_2cn_orphaned_events_not_counted_in_sync() {
        // Test: sync_from_review_logs should return only the count of events
        // actually applied to the projection, excluding orphaned events (bd-2cn).
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Write a valid review with 2 events
        let good_log = crate::log::ReviewLog::new(seal_root, "cr-good").unwrap();
        good_log.append(&make_review_created("cr-good")).unwrap();
        good_log
            .append(&make_thread_created("th-good", "cr-good"))
            .unwrap();

        // Write orphaned events (no ReviewCreated) with 2 events
        let orphan_log = crate::log::ReviewLog::new(seal_root, "cr-orphan").unwrap();
        orphan_log
            .append(&make_thread_created("th-orphan", "cr-orphan"))
            .unwrap();
        orphan_log
            .append(&make_comment_added("th-orphan.1", "th-orphan"))
            .unwrap();

        // Sync should return 2 (only the good events), not 4
        let report = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(
            report.applied, 2,
            "sync should return 2 (events applied), not 4 (including orphaned), got {}",
            report.applied
        );

        // Verify the right data is in the projection
        let review_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
            .unwrap();
        assert_eq!(review_count, 1, "should have 1 review (cr-good only)");

        let thread_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(thread_count, 1, "should have 1 thread (th-good only)");
    }

    #[test]
    fn test_bd_2cn_all_orphaned_returns_zero() {
        // Test: when all new events are orphaned, sync_from_review_logs returns 0,
        // not the orphan count (bd-2cn).
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Write ONLY orphaned events (no ReviewCreated)
        let orphan_log = crate::log::ReviewLog::new(seal_root, "cr-orphan").unwrap();
        orphan_log
            .append(&make_thread_created("th-orphan", "cr-orphan"))
            .unwrap();
        orphan_log
            .append(&make_comment_added("th-orphan.1", "th-orphan"))
            .unwrap();
        orphan_log
            .append(&make_thread_created("th-orphan2", "cr-orphan"))
            .unwrap();

        // Sync should return 0 (all events filtered out), not 3
        let report = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(
            report.applied, 0,
            "sync with all orphaned events should return 0, not 3, got {}",
            report.applied
        );

        // Verify projection is empty
        let review_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
            .unwrap();
        assert_eq!(review_count, 0, "projection should be empty");
    }

    // ========================================================================
    // bd-jw3: Per-file monotonic sync tests
    // ========================================================================

    #[test]
    fn test_per_file_sync_new_file() {
        // New review file gets fully synced
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Write a review with events
        let log = crate::log::ReviewLog::new(seal_root, "cr-new").unwrap();
        log.append(&make_review_created("cr-new")).unwrap();
        log.append(&make_thread_created("th-new", "cr-new"))
            .unwrap();

        let report = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(report.applied, 2, "should apply 2 events");
        assert_eq!(report.files_synced, 1, "should sync 1 file");
        assert_eq!(report.files_skipped, 0, "should skip 0 files");
        assert!(report.anomalies.is_empty(), "no anomalies expected");

        // Verify data landed
        let review_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
            .unwrap();
        assert_eq!(review_count, 1);

        // Verify review_file_state was recorded
        let stored_lines: i64 = db
            .conn()
            .query_row(
                "SELECT line_count FROM review_file_state WHERE review_id = ?",
                params!["cr-new"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_lines, 2, "should record 2 lines");
    }

    #[test]
    fn test_per_file_sync_unchanged() {
        // Same byte_count skips the file (fast path)
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Write and sync
        let log = crate::log::ReviewLog::new(seal_root, "cr-unch").unwrap();
        log.append(&make_review_created("cr-unch")).unwrap();

        let report1 = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(report1.applied, 1);
        assert_eq!(report1.files_synced, 1);

        // Re-sync with no changes
        let report2 = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(report2.applied, 0, "no new events");
        assert_eq!(report2.files_skipped, 1, "should skip unchanged file");
        assert_eq!(report2.files_synced, 0, "no files synced");
        assert!(report2.anomalies.is_empty(), "no anomalies");
    }

    #[test]
    fn test_per_file_sync_grew() {
        // Append events, re-sync, only new events applied
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Write initial events and sync
        let log = crate::log::ReviewLog::new(seal_root, "cr-grow").unwrap();
        log.append(&make_review_created("cr-grow")).unwrap();

        let report1 = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(report1.applied, 1);

        // Append more events
        log.append(&make_thread_created("th-grow", "cr-grow"))
            .unwrap();
        log.append(&make_comment_added("th-grow.1", "th-grow"))
            .unwrap();

        // Re-sync: should only process the 2 new events
        let report2 = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(report2.applied, 2, "should apply only 2 new events");
        assert_eq!(report2.files_synced, 1, "should sync 1 grown file");
        assert!(report2.anomalies.is_empty(), "no anomalies");

        // Verify all data exists
        let thread_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(thread_count, 1, "should have 1 thread");

        let comment_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM comments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(comment_count, 1, "should have 1 comment");
    }

    #[test]
    fn test_per_file_sync_shrunk() {
        // Truncate file, re-sync, projection data preserved, anomaly recorded
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Write 3 events and sync
        let log = crate::log::ReviewLog::new(seal_root, "cr-shrink").unwrap();
        log.append(&make_review_created("cr-shrink")).unwrap();
        log.append(&make_thread_created("th-shrink", "cr-shrink"))
            .unwrap();
        log.append(&make_comment_added("th-shrink.1", "th-shrink"))
            .unwrap();

        let report1 = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(report1.applied, 3);

        // Truncate the file to 1 event (simulating jj workspace restore)
        let path = log.path();
        let content = std::fs::read_to_string(&path).unwrap();
        let first_line = content.lines().next().unwrap();
        std::fs::write(&path, format!("{first_line}\n")).unwrap();

        // Re-sync: should detect shrinkage, skip file, preserve data
        let report2 = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(report2.applied, 0, "no new events applied");
        assert_eq!(report2.files_skipped, 1, "should skip shrunk file");
        assert_eq!(report2.anomalies.len(), 1, "should have 1 anomaly");
        assert_eq!(report2.anomalies[0].kind, AnomalyKind::Shrunk);
        assert_eq!(report2.anomalies[0].review_id, "cr-shrink");

        // Projection data PRESERVED (monotonic)
        let review_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
            .unwrap();
        assert_eq!(review_count, 1, "review preserved");

        let thread_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(thread_count, 1, "thread preserved");

        let comment_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM comments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(comment_count, 1, "comment preserved");
    }

    #[test]
    fn test_per_file_sync_hash_mismatch() {
        // Replace content, re-sync, projection preserved, anomaly recorded
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Write events and sync
        let log = crate::log::ReviewLog::new(seal_root, "cr-hash").unwrap();
        log.append(&make_review_created("cr-hash")).unwrap();
        log.append(&make_thread_created("th-hash", "cr-hash"))
            .unwrap();

        let report1 = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(report1.applied, 2);

        // Replace file content with DIFFERENT events but MORE lines
        let replacement_event1 = make_review_created("cr-other");
        let replacement_event2 = make_thread_created("th-other", "cr-other");
        let replacement_event3 = make_comment_added("th-other.1", "th-other");
        let path = log.path();
        let mut content = String::new();
        content.push_str(&replacement_event1.to_json_line().unwrap());
        content.push('\n');
        content.push_str(&replacement_event2.to_json_line().unwrap());
        content.push('\n');
        content.push_str(&replacement_event3.to_json_line().unwrap());
        content.push('\n');
        std::fs::write(&path, &content).unwrap();

        // Re-sync: should detect hash mismatch, skip file, preserve data
        let report2 = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(report2.applied, 0, "no new events applied");
        assert_eq!(report2.anomalies.len(), 1, "should have 1 anomaly");
        assert_eq!(report2.anomalies[0].kind, AnomalyKind::HashMismatch);
        assert_eq!(report2.anomalies[0].review_id, "cr-hash");

        // Projection data PRESERVED (monotonic)
        let review_title: String = db
            .conn()
            .query_row(
                "SELECT title FROM reviews WHERE review_id = ?",
                params!["cr-hash"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(review_title, "Review cr-hash", "original data preserved");

        // cr-other should NOT exist (was not applied)
        let other_exists: bool = db
            .conn()
            .query_row(
                "SELECT COUNT(*) > 0 FROM reviews WHERE review_id = ?",
                params!["cr-other"],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!other_exists, "replaced data should NOT be applied");
    }

    #[test]
    fn test_per_file_sync_file_disappeared() {
        // Delete file, re-sync, review still in projection
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Write and sync
        let log = crate::log::ReviewLog::new(seal_root, "cr-gone").unwrap();
        log.append(&make_review_created("cr-gone")).unwrap();

        let report1 = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(report1.applied, 1);

        // Delete the review directory entirely
        let review_dir = seal_root.join(".seal").join("reviews").join("cr-gone");
        std::fs::remove_dir_all(&review_dir).unwrap();

        // Re-sync: should detect missing file, preserve projection, report anomaly
        let report2 = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(report2.applied, 0);
        assert_eq!(report2.anomalies.len(), 1, "should have 1 anomaly");
        assert_eq!(report2.anomalies[0].kind, AnomalyKind::Missing);
        assert_eq!(report2.anomalies[0].review_id, "cr-gone");

        // Projection data PRESERVED
        let review_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
            .unwrap();
        assert_eq!(review_count, 1, "review preserved despite file deletion");
    }

    #[test]
    fn test_per_file_sync_isolation() {
        // One file with parse error doesn't block others
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Write a valid review
        let good_log = crate::log::ReviewLog::new(seal_root, "cr-good2").unwrap();
        good_log.append(&make_review_created("cr-good2")).unwrap();

        // Write a review with valid then invalid content
        let bad_path = seal_root.join(".seal").join("reviews").join("cr-bad");
        std::fs::create_dir_all(&bad_path).unwrap();
        std::fs::write(bad_path.join("events.jsonl"), "this is not valid json\n").unwrap();

        let report = sync_from_review_logs(&db, seal_root).unwrap();

        // Good file should be synced
        let review_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
            .unwrap();
        assert_eq!(review_count, 1, "good review should be in projection");

        // Bad file should have an anomaly but not block the good one
        assert!(
            report.files_synced >= 1,
            "should sync at least the good file"
        );
        assert!(
            report
                .anomalies
                .iter()
                .any(|a| a.review_id == "cr-bad" && a.kind == AnomalyKind::ParseError),
            "should have parse error anomaly for cr-bad"
        );
    }

    #[test]
    fn test_per_file_sync_bootstrap_existing_projection() {
        // Existing projection with no review_file_state should bootstrap
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Directly insert a review into the projection (simulating pre-upgrade state)
        let event = make_review_created("cr-legacy");
        apply_event(&db, &event).unwrap();

        // Write the same review's log file on disk
        let log = crate::log::ReviewLog::new(seal_root, "cr-legacy").unwrap();
        log.append(&event).unwrap();

        // Verify review_file_state is empty before sync
        let state_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM review_file_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state_count, 0, "should start with empty review_file_state");

        // Sync should bootstrap (seed review_file_state) without replaying events
        let report = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(report.applied, 0, "should NOT replay events on bootstrap");
        assert_eq!(
            report.files_skipped, 1,
            "file should be skipped (already synced via bootstrap)"
        );

        // review_file_state should now have a row
        let state_count2: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM review_file_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state_count2, 1, "should have seeded review_file_state");
    }

    #[test]
    fn test_per_file_sync_multiple_files() {
        // Multiple files: one grows, one unchanged, one new
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Write 2 reviews and sync
        let log1 = crate::log::ReviewLog::new(seal_root, "cr-multi1").unwrap();
        log1.append(&make_review_created("cr-multi1")).unwrap();

        let log2 = crate::log::ReviewLog::new(seal_root, "cr-multi2").unwrap();
        log2.append(&make_review_created("cr-multi2")).unwrap();

        let report1 = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(report1.applied, 2);
        assert_eq!(report1.files_synced, 2);

        // Append to one, add a new file
        log1.append(&make_thread_created("th-multi1", "cr-multi1"))
            .unwrap();

        let log3 = crate::log::ReviewLog::new(seal_root, "cr-multi3").unwrap();
        log3.append(&make_review_created("cr-multi3")).unwrap();

        let report2 = sync_from_review_logs(&db, seal_root).unwrap();
        assert_eq!(report2.applied, 2, "1 new event from grew + 1 from new");
        assert_eq!(report2.files_synced, 2, "1 grew + 1 new");
        assert_eq!(report2.files_skipped, 1, "cr-multi2 unchanged");
        assert!(report2.anomalies.is_empty());
    }
}

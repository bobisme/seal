//! Migration command: v1 -> v2 data format upgrade.
//!
//! Converts from single `.seal/events.jsonl` to per-review event logs.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::output::OutputFormat;
use seal_core::events::{Event, EventEnvelope};
use seal_core::log::{open_or_create_review, AppendLog, FileLog};
use seal_core::version::{detect_version, write_version_file, DataVersion, CURRENT_VERSION};

/// Check if a string looks like a legacy review ID.
/// More permissive than `is_review_id` - just checks prefix and basic format.
/// Used for migration to support pre-terseid IDs that don't have digits.
fn is_legacy_review_id(s: &str) -> bool {
    s.starts_with("cr-") && s.len() >= 6 && s[3..].chars().all(|c| c.is_ascii_alphanumeric())
}

/// Extract a stable, unique key for an event for dedup purposes.
/// Includes timestamp, event type discriminant, and event-specific IDs.
fn event_dedup_key(e: &EventEnvelope) -> String {
    let specific = match &e.event {
        Event::ReviewCreated(ev) => ev.review_id.clone(),
        Event::ReviewersRequested(ev) => ev.review_id.clone(),
        Event::ReviewerVoted(ev) => format!("{}:{}", ev.review_id, e.author),
        Event::ReviewApproved(ev) => ev.review_id.clone(),
        Event::ReviewMerged(ev) => ev.review_id.clone(),
        Event::ReviewAbandoned(ev) => ev.review_id.clone(),
        Event::ThreadCreated(ev) => ev.thread_id.clone(),
        Event::CommentAdded(ev) => ev.comment_id.clone(),
        Event::ThreadResolved(ev) => ev.thread_id.clone(),
        Event::ThreadReopened(ev) => ev.thread_id.clone(),
    };
    format!(
        "{}:{:?}:{}",
        e.ts.to_rfc3339(),
        std::mem::discriminant(&e.event),
        specific
    )
}

/// Return a short label for an event type (for logging without leaking content).
const fn event_type_label(event: &Event) -> &'static str {
    match event {
        Event::ReviewCreated(_) => "ReviewCreated",
        Event::ReviewersRequested(_) => "ReviewersRequested",
        Event::ReviewerVoted(_) => "ReviewerVoted",
        Event::ReviewApproved(_) => "ReviewApproved",
        Event::ReviewMerged(_) => "ReviewMerged",
        Event::ReviewAbandoned(_) => "ReviewAbandoned",
        Event::ThreadCreated(_) => "ThreadCreated",
        Event::CommentAdded(_) => "CommentAdded",
        Event::ThreadResolved(_) => "ThreadResolved",
        Event::ThreadReopened(_) => "ThreadReopened",
    }
}

/// Path to legacy events.jsonl
fn legacy_events_path(seal_root: &Path) -> std::path::PathBuf {
    seal_root.join(".seal").join("events.jsonl")
}

fn ensure_valid_migration_targets<'a>(
    review_ids: impl IntoIterator<Item = &'a String>,
) -> Result<()> {
    for review_id in review_ids {
        if !is_legacy_review_id(review_id) {
            bail!(
                "Invalid review_id '{review_id}' found in events — refusing to create path. \
                 Expected format: cr-XXXX"
            );
        }
    }
    Ok(())
}

fn ensure_backup_path_available(legacy_path: &Path, backup: bool) -> Result<()> {
    if !backup {
        return Ok(());
    }

    let backup_path = legacy_path.with_extension("jsonl.v1.backup");
    if backup_path.exists() {
        bail!(
            "Backup already exists: {}\n  Move or remove it before running migration again.",
            backup_path.display()
        );
    }

    Ok(())
}

/// Run the migrate command.
pub fn run_migrate(
    seal_root: &Path,
    dry_run: bool,
    backup: bool,
    from_backup: bool,
    format: OutputFormat,
) -> Result<()> {
    // If --from-backup, handle re-migration from v1 backup
    if from_backup {
        return run_remigrate_from_backup(seal_root, dry_run, format);
    }

    // Check current version
    let version = detect_version(seal_root)?;

    match version {
        Some(DataVersion::V2) => {
            if format == OutputFormat::Json {
                println!(
                    r#"{{"status":"already_migrated","version":2,"message":"Repository already uses v2 format. Use --from-backup to re-migrate from v1 backup."}}"#
                );
            } else {
                println!("✓ Repository already uses v2 format. No migration needed.");
                println!("  Tip: Use --from-backup to re-migrate from events.jsonl.v1.backup");
            }
            return Ok(());
        }
        None => {
            // Check if .seal/ exists at all
            let seal_dir = seal_root.join(".seal");
            if !seal_dir.exists() {
                bail!(
                    "No .seal/ directory found. Run 'seal init' first to initialize the repository."
                );
            }

            // Empty repo - just write v2 version file
            if dry_run {
                if format == OutputFormat::Json {
                    println!(
                        r#"{{"status":"dry_run","version":2,"message":"Would create v2 version file (no events to migrate)"}}"#
                    );
                } else {
                    println!("Would create v2 version file (no events to migrate).");
                }
            } else {
                write_version_file(seal_root, DataVersion::V2)?;
                if format == OutputFormat::Json {
                    println!(
                        r#"{{"status":"success","version":2,"message":"Created v2 version file","events_migrated":0}}"#
                    );
                } else {
                    println!("✓ Created v2 version file. Repository is now v2.");
                }
            }
            return Ok(());
        }
        Some(DataVersion::V1) => {
            // Proceed with migration
        }
    }

    // Read all events from legacy file
    let legacy_path = legacy_events_path(seal_root);
    let legacy_log = FileLog::new(&legacy_path);
    let events = legacy_log
        .read_all()
        .context("Failed to read legacy events.jsonl")?;

    if events.is_empty() {
        if dry_run {
            if format == OutputFormat::Json {
                println!(
                    r#"{{"status":"dry_run","version":2,"message":"Would migrate (no events in v1 file)"}}"#
                );
            } else {
                println!("Would migrate empty events.jsonl to v2 format.");
            }
        } else {
            // Backup and write version file
            if backup {
                let backup_path = legacy_path.with_extension("jsonl.v1.backup");
                fs::rename(&legacy_path, &backup_path).context("Failed to backup events.jsonl")?;
            } else {
                fs::remove_file(&legacy_path).context("Failed to remove events.jsonl")?;
            }
            write_version_file(seal_root, DataVersion::V2)?;
            if format == OutputFormat::Json {
                println!(r#"{{"status":"success","version":2,"events_migrated":0}}"#);
            } else {
                println!("✓ Migrated to v2 (no events).");
            }
        }
        return Ok(());
    }

    // Two-pass grouping: first build thread_id→review_id map, then group all events.
    // CommentAdded, ThreadResolved, and ThreadReopened only have thread_id, not review_id,
    // so we need the map from ThreadCreated events to resolve them.
    let thread_to_review = build_thread_review_map(&events);

    let mut events_by_review: HashMap<String, Vec<EventEnvelope>> = HashMap::new();
    let mut orphaned_count = 0;

    for event in &events {
        let review_id = resolve_review_id(&event.event, &thread_to_review);
        if let Some(id) = review_id {
            events_by_review
                .entry(id.to_string())
                .or_default()
                .push(event.clone());
        } else {
            orphaned_count += 1;
            eprintln!(
                "WARNING: Could not resolve review_id for {} event at {}",
                event_type_label(&event.event),
                event.ts.to_rfc3339()
            );
        }
    }

    // Summary
    let total_events = events.len();
    let migrated_events: usize = events_by_review.values().map(std::vec::Vec::len).sum();
    let review_count = events_by_review.len();

    if dry_run {
        if format == OutputFormat::Json {
            let reviews: Vec<_> = events_by_review
                .iter()
                .map(|(id, evts)| {
                    serde_json::json!({
                        "review_id": id,
                        "event_count": evts.len()
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::json!({
                    "status": "dry_run",
                    "total_events": total_events,
                    "review_count": review_count,
                    "reviews": reviews
                })
            );
        } else {
            println!("DRY RUN - Would migrate:");
            println!("  Total events: {total_events}");
            println!("  Reviews: {review_count}");
            for (id, evts) in &events_by_review {
                println!("    {}: {} events", id, evts.len());
            }
        }
        return Ok(());
    }

    ensure_valid_migration_targets(events_by_review.keys())?;
    ensure_backup_path_available(&legacy_path, backup)?;

    // Perform migration: write events to per-review logs
    for (review_id, review_events) in &events_by_review {
        let log = open_or_create_review(seal_root, review_id)?;

        // Sort by timestamp (should already be sorted, but be safe)
        let mut sorted_events = review_events.clone();
        sorted_events.sort_by(|a, b| a.ts.cmp(&b.ts));

        for event in &sorted_events {
            log.append(event)?;
        }
    }

    // Backup or remove legacy file
    if backup {
        let backup_path = legacy_path.with_extension("jsonl.v1.backup");
        fs::rename(&legacy_path, &backup_path).context("Failed to backup events.jsonl")?;
    } else {
        fs::remove_file(&legacy_path).context("Failed to remove events.jsonl")?;
    }

    // Write version file
    write_version_file(seal_root, DataVersion::V2)?;

    // Delete old index.db to force rebuild from new structure
    let index_path = seal_root.join(".seal").join("index.db");
    if index_path.exists() {
        fs::remove_file(&index_path).context("Failed to remove old index.db")?;
    }

    if format == OutputFormat::Json {
        let mut result = serde_json::json!({
            "status": "success",
            "version": CURRENT_VERSION,
            "events_migrated": migrated_events,
            "events_total": total_events,
            "reviews_created": review_count
        });
        if orphaned_count > 0 {
            result["orphaned_events"] = serde_json::json!(orphaned_count);
        }
        println!("{result}");
    } else {
        println!("✓ Migration complete!");
        println!("  Events migrated: {migrated_events}/{total_events}");
        println!("  Reviews: {review_count}");
        if orphaned_count > 0 {
            println!(
                "  WARNING: {orphaned_count} event(s) could not be associated with a review (orphaned)"
            );
        }
        if backup {
            println!(
                "  Backup: {}",
                legacy_path.with_extension("jsonl.v1.backup").display()
            );
        }
    }

    Ok(())
}

/// Re-migrate from a v1 backup file (events.jsonl.v1.backup).
///
/// This reads the backup, groups events by review (using the thread→review map),
/// and merges missing events into existing per-review logs. Useful for recovering
/// CommentAdded/ThreadResolved/ThreadReopened events that were dropped by a
/// buggy earlier migration.
fn run_remigrate_from_backup(seal_root: &Path, dry_run: bool, format: OutputFormat) -> Result<()> {
    let backup_path = seal_root.join(".seal").join("events.jsonl.v1.backup");

    if !backup_path.exists() {
        bail!(
            "No v1 backup found at {}\n  The --from-backup flag requires events.jsonl.v1.backup from a previous migration.",
            backup_path.display()
        );
    }

    let legacy_log = FileLog::new(&backup_path);
    let backup_events = legacy_log.read_all().context("Failed to read v1 backup")?;

    if backup_events.is_empty() {
        if format == OutputFormat::Json {
            println!(r#"{{"status":"no_events","message":"Backup file is empty"}}"#);
        } else {
            println!("Backup file is empty. Nothing to re-migrate.");
        }
        return Ok(());
    }

    // Build thread→review map and group events
    let thread_to_review = build_thread_review_map(&backup_events);

    let mut events_by_review: HashMap<String, Vec<EventEnvelope>> = HashMap::new();
    let mut orphaned_count = 0;

    for event in &backup_events {
        let review_id = resolve_review_id(&event.event, &thread_to_review);
        if let Some(id) = review_id {
            events_by_review
                .entry(id.to_string())
                .or_default()
                .push(event.clone());
        } else {
            orphaned_count += 1;
            eprintln!(
                "WARNING: Could not resolve review_id for {} event at {}",
                event_type_label(&event.event),
                event.ts.to_rfc3339()
            );
        }
    }

    // For each review, read existing per-review log and find missing events
    let mut total_recovered = 0usize;
    ensure_valid_migration_targets(events_by_review.keys())?;

    for (review_id, backup_review_events) in &events_by_review {
        let log = open_or_create_review(seal_root, review_id)?;
        let existing_events = log.read_all().unwrap_or_default();

        // Build set of existing event keys for dedup (includes event-specific IDs)
        let existing_keys: std::collections::HashSet<String> =
            existing_events.iter().map(event_dedup_key).collect();

        let mut sorted = backup_review_events.clone();
        sorted.sort_by(|a, b| a.ts.cmp(&b.ts));

        let mut recovered_for_review = 0;
        for event in &sorted {
            let key = event_dedup_key(event);
            if !existing_keys.contains(&key) {
                if !dry_run {
                    log.append(event)?;
                }
                recovered_for_review += 1;
            }
        }

        if recovered_for_review > 0 && !dry_run {
            // Delete index.db to force rebuild with new events
            let index_path = seal_root.join(".seal").join("index.db");
            if index_path.exists() {
                fs::remove_file(&index_path).context("Failed to remove index.db for rebuild")?;
            }
        }

        total_recovered += recovered_for_review;
    }

    if format == OutputFormat::Json {
        let mut result = serde_json::json!({
            "status": if dry_run { "dry_run" } else { "success" },
            "events_recovered": total_recovered,
            "reviews_affected": events_by_review.len(),
            "backup_total_events": backup_events.len(),
        });
        if orphaned_count > 0 {
            result["orphaned_events"] = serde_json::json!(orphaned_count);
        }
        println!("{result}");
    } else {
        if dry_run {
            println!("DRY RUN - Would recover:");
        } else {
            println!("✓ Re-migration complete!");
        }
        println!("  Events recovered: {total_recovered}");
        println!("  Reviews affected: {}", events_by_review.len());
        println!("  Backup total events: {}", backup_events.len());
        if orphaned_count > 0 {
            println!("  WARNING: {orphaned_count} event(s) could not be associated with a review");
        }
    }

    Ok(())
}

/// Build a map from `thread_id` to `review_id` using `ThreadCreated` events.
fn build_thread_review_map(events: &[EventEnvelope]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for event in events {
        if let Event::ThreadCreated(e) = &event.event {
            map.insert(e.thread_id.clone(), e.review_id.clone());
        }
    }
    map
}

/// Resolve the `review_id` for an event, using the thread→review map for
/// events that only carry a `thread_id` (`CommentAdded`, `ThreadResolved`, `ThreadReopened`).
fn resolve_review_id<'a>(
    event: &'a Event,
    thread_to_review: &'a HashMap<String, String>,
) -> Option<&'a str> {
    match event {
        Event::ReviewCreated(e) => Some(&e.review_id),
        Event::ReviewersRequested(e) => Some(&e.review_id),
        Event::ReviewerVoted(e) => Some(&e.review_id),
        Event::ReviewApproved(e) => Some(&e.review_id),
        Event::ReviewMerged(e) => Some(&e.review_id),
        Event::ReviewAbandoned(e) => Some(&e.review_id),
        Event::ThreadCreated(e) => Some(&e.review_id),
        Event::CommentAdded(e) => thread_to_review
            .get(&e.thread_id)
            .map(std::string::String::as_str),
        Event::ThreadResolved(e) => thread_to_review
            .get(&e.thread_id)
            .map(std::string::String::as_str),
        Event::ThreadReopened(e) => thread_to_review
            .get(&e.thread_id)
            .map(std::string::String::as_str),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seal_core::events::{
        CodeSelection, CommentAdded, ReviewCreated, ReviewerVoted, ThreadCreated, ThreadReopened,
        ThreadResolved, VoteType,
    };
    use seal_core::log::{list_review_ids, open_or_create};
    use tempfile::tempdir;

    fn make_review_created(review_id: &str) -> EventEnvelope {
        EventEnvelope::new(
            "test_agent",
            Event::ReviewCreated(ReviewCreated {
                review_id: review_id.to_string(),
                jj_change_id: "change123".to_string(),
                scm_kind: Some("jj".to_string()),
                scm_anchor: Some("change123".to_string()),
                initial_commit: "commit456".to_string(),
                title: format!("Test Review {review_id}"),
                description: None,
            }),
        )
    }

    fn make_thread_created(review_id: &str, thread_id: &str) -> EventEnvelope {
        EventEnvelope::new(
            "test_agent",
            Event::ThreadCreated(ThreadCreated {
                thread_id: thread_id.to_string(),
                review_id: review_id.to_string(),
                file_path: "src/main.rs".to_string(),
                selection: CodeSelection::line(42),
                commit_hash: "abc123".to_string(),
            }),
        )
    }

    fn make_vote(review_id: &str) -> EventEnvelope {
        EventEnvelope::new(
            "reviewer",
            Event::ReviewerVoted(ReviewerVoted {
                review_id: review_id.to_string(),
                vote: VoteType::Lgtm,
                reason: Some("Looks good".to_string()),
            }),
        )
    }

    fn make_comment(thread_id: &str, comment_id: &str, body: &str) -> EventEnvelope {
        EventEnvelope::new(
            "reviewer",
            Event::CommentAdded(CommentAdded {
                comment_id: comment_id.to_string(),
                thread_id: thread_id.to_string(),
                body: body.to_string(),
            }),
        )
    }

    fn make_thread_resolved(thread_id: &str) -> EventEnvelope {
        EventEnvelope::new(
            "test_agent",
            Event::ThreadResolved(ThreadResolved {
                thread_id: thread_id.to_string(),
                reason: Some("Fixed".to_string()),
            }),
        )
    }

    fn make_thread_reopened(thread_id: &str) -> EventEnvelope {
        EventEnvelope::new(
            "test_agent",
            Event::ThreadReopened(ThreadReopened {
                thread_id: thread_id.to_string(),
                reason: Some("Not fixed".to_string()),
            }),
        )
    }

    #[test]
    fn test_migrate_empty_repo() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        // Create empty .seal/ directory
        fs::create_dir(seal_root.join(".seal")).unwrap();

        // Run migration
        run_migrate(seal_root, false, true, false, OutputFormat::Text).unwrap();

        // Check version file created
        let version_content = fs::read_to_string(seal_root.join(".seal").join("version")).unwrap();
        assert_eq!(version_content.trim(), "2");
    }

    #[test]
    fn test_migrate_v1_to_v2() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        // Create v1 structure with events
        let seal_dir = seal_root.join(".seal");
        fs::create_dir(&seal_dir).unwrap();

        let legacy_path = seal_dir.join("events.jsonl");
        let log = open_or_create(&legacy_path).unwrap();

        // Add events for two reviews, including comments
        log.append(&make_review_created("cr-001")).unwrap();
        log.append(&make_thread_created("cr-001", "th-001"))
            .unwrap();
        log.append(&make_comment("th-001", "c-001", "First comment"))
            .unwrap();
        log.append(&make_review_created("cr-002")).unwrap();
        log.append(&make_vote("cr-001")).unwrap();
        log.append(&make_thread_created("cr-002", "th-002"))
            .unwrap();
        log.append(&make_comment("th-002", "c-002", "Second comment"))
            .unwrap();

        // Run migration
        run_migrate(seal_root, false, true, false, OutputFormat::Text).unwrap();

        // Check version file
        let version_content = fs::read_to_string(seal_dir.join("version")).unwrap();
        assert_eq!(version_content.trim(), "2");

        // Check backup exists
        assert!(legacy_path.with_extension("jsonl.v1.backup").exists());
        assert!(!legacy_path.exists());

        // Check review directories created
        let review_ids = list_review_ids(seal_root).unwrap();
        assert_eq!(review_ids.len(), 2);
        assert!(review_ids.contains(&"cr-001".to_string()));
        assert!(review_ids.contains(&"cr-002".to_string()));

        // Check events in cr-001 (ReviewCreated, ThreadCreated, CommentAdded, ReviewerVoted)
        let log1 = seal_core::log::ReviewLog::new(seal_root, "cr-001").unwrap();
        let events1 = log1.read_all().unwrap();
        assert_eq!(events1.len(), 4);

        // Check events in cr-002 (ReviewCreated, ThreadCreated, CommentAdded)
        let log2 = seal_core::log::ReviewLog::new(seal_root, "cr-002").unwrap();
        let events2 = log2.read_all().unwrap();
        assert_eq!(events2.len(), 3);
    }

    /// Regression test: v1→v2 migration must preserve `CommentAdded`,
    /// `ThreadResolved`, and `ThreadReopened` events (they only have `thread_id`,
    /// not `review_id`, and were previously dropped).
    #[test]
    fn test_migrate_preserves_thread_linked_events() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let seal_dir = seal_root.join(".seal");
        fs::create_dir(&seal_dir).unwrap();

        let legacy_path = seal_dir.join("events.jsonl");
        let log = open_or_create(&legacy_path).unwrap();

        // Review with thread, comments, resolve, and reopen
        log.append(&make_review_created("cr-001")).unwrap();
        log.append(&make_thread_created("cr-001", "th-001"))
            .unwrap();
        log.append(&make_comment("th-001", "c-001", "Issue found"))
            .unwrap();
        log.append(&make_comment("th-001", "c-002", "Will fix"))
            .unwrap();
        log.append(&make_thread_resolved("th-001")).unwrap();
        log.append(&make_thread_reopened("th-001")).unwrap();
        log.append(&make_comment("th-001", "c-003", "Not actually fixed"))
            .unwrap();

        // Run migration
        run_migrate(seal_root, false, true, false, OutputFormat::Text).unwrap();

        // All 7 events must be present in the per-review log
        let review_log = seal_core::log::ReviewLog::new(seal_root, "cr-001").unwrap();
        let events = review_log.read_all().unwrap();
        assert_eq!(
            events.len(),
            7,
            "Expected 7 events (ReviewCreated, ThreadCreated, 3x CommentAdded, ThreadResolved, ThreadReopened), got {}",
            events.len()
        );

        // Verify event types
        let event_types: Vec<&str> = events
            .iter()
            .map(|e| match &e.event {
                Event::ReviewCreated(_) => "ReviewCreated",
                Event::ThreadCreated(_) => "ThreadCreated",
                Event::CommentAdded(_) => "CommentAdded",
                Event::ThreadResolved(_) => "ThreadResolved",
                Event::ThreadReopened(_) => "ThreadReopened",
                _ => "Other",
            })
            .collect();

        assert_eq!(
            event_types,
            vec![
                "ReviewCreated",
                "ThreadCreated",
                "CommentAdded",
                "CommentAdded",
                "ThreadResolved",
                "ThreadReopened",
                "CommentAdded",
            ]
        );
    }

    #[test]
    fn test_migrate_dry_run() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        // Create v1 structure
        let seal_dir = seal_root.join(".seal");
        fs::create_dir(&seal_dir).unwrap();

        let legacy_path = seal_dir.join("events.jsonl");
        let log = open_or_create(&legacy_path).unwrap();
        log.append(&make_review_created("cr-001")).unwrap();

        // Run dry run
        run_migrate(seal_root, true, true, false, OutputFormat::Text).unwrap();

        // Nothing should change
        assert!(legacy_path.exists());
        assert!(!seal_dir.join("version").exists());
        assert!(!seal_dir.join("reviews").exists());
    }

    #[test]
    fn test_migrate_already_v2() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        // Create v2 structure
        let seal_dir = seal_root.join(".seal");
        fs::create_dir(&seal_dir).unwrap();
        fs::write(seal_dir.join("version"), "2\n").unwrap();

        // Run migration - should be no-op
        run_migrate(seal_root, false, true, false, OutputFormat::Text).unwrap();

        // Still v2
        let version_content = fs::read_to_string(seal_dir.join("version")).unwrap();
        assert_eq!(version_content.trim(), "2");
    }

    /// Test --from-backup recovers events that were dropped by a buggy v1→v2 migration.
    #[test]
    fn test_remigrate_from_backup_recovers_comments() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let seal_dir = seal_root.join(".seal");
        fs::create_dir(&seal_dir).unwrap();

        // Create events once so timestamps match across backup and existing logs
        let ev_review = make_review_created("cr-001");
        let ev_thread = make_thread_created("cr-001", "th-001");
        let ev_comment1 = make_comment("th-001", "c-001", "Bug here");
        let ev_comment2 = make_comment("th-001", "c-002", "Will fix");
        let ev_resolved = make_thread_resolved("th-001");

        // Write all 5 events to v1 backup (simulating original v1 file)
        let backup_path = seal_dir.join("events.jsonl.v1.backup");
        let backup_log = open_or_create(&backup_path).unwrap();
        backup_log.append(&ev_review).unwrap();
        backup_log.append(&ev_thread).unwrap();
        backup_log.append(&ev_comment1).unwrap();
        backup_log.append(&ev_comment2).unwrap();
        backup_log.append(&ev_resolved).unwrap();

        // Simulate existing v2 state with only ReviewCreated + ThreadCreated (comments lost)
        // Uses the SAME events so timestamps match for dedup
        fs::write(seal_dir.join("version"), "2\n").unwrap();
        let review_log = open_or_create_review(seal_root, "cr-001").unwrap();
        review_log.append(&ev_review).unwrap();
        review_log.append(&ev_thread).unwrap();

        // Verify only 2 events before recovery
        let pre_events = seal_core::log::ReviewLog::new(seal_root, "cr-001")
            .unwrap()
            .read_all()
            .unwrap();
        assert_eq!(pre_events.len(), 2);

        // Run --from-backup
        run_migrate(seal_root, false, true, true, OutputFormat::Text).unwrap();

        // Should now have all 5 events (2 existing + 3 recovered)
        let post_events = seal_core::log::ReviewLog::new(seal_root, "cr-001")
            .unwrap()
            .read_all()
            .unwrap();
        assert_eq!(
            post_events.len(),
            5,
            "Expected 5 events after recovery, got {}",
            post_events.len()
        );
    }

    #[test]
    fn test_remigrate_rejects_invalid_review_id_before_partial_writes() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let seal_dir = seal_root.join(".seal");
        fs::create_dir(&seal_dir).unwrap();
        fs::write(seal_dir.join("version"), "2\n").unwrap();

        let backup_path = seal_dir.join("events.jsonl.v1.backup");
        let backup_log = open_or_create(&backup_path).unwrap();
        backup_log.append(&make_review_created("cr-001")).unwrap();
        backup_log
            .append(&EventEnvelope::new(
                "test_agent",
                Event::ReviewCreated(ReviewCreated {
                    review_id: "../../../tmp/evil".to_string(),
                    jj_change_id: "change123".to_string(),
                    scm_kind: Some("jj".to_string()),
                    scm_anchor: Some("change123".to_string()),
                    initial_commit: "commit456".to_string(),
                    title: "Malicious review".to_string(),
                    description: None,
                }),
            ))
            .unwrap();

        let result = run_migrate(seal_root, false, true, true, OutputFormat::Text);

        assert!(result.is_err(), "Should reject invalid review_id");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Invalid review_id"),
            "Error should mention invalid review_id, got: {err}"
        );
        assert!(
            !seal_dir.join("reviews").exists(),
            "re-migration should not write earlier valid review logs before validation"
        );
    }

    #[test]
    fn test_migrate_no_backup() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        // Create v1 structure
        let seal_dir = seal_root.join(".seal");
        fs::create_dir(&seal_dir).unwrap();

        let legacy_path = seal_dir.join("events.jsonl");
        let log = open_or_create(&legacy_path).unwrap();
        log.append(&make_review_created("cr-001")).unwrap();

        // Run migration without backup
        run_migrate(seal_root, false, false, false, OutputFormat::Text).unwrap();

        // Original file should be gone, no backup
        assert!(!legacy_path.exists());
        assert!(!legacy_path.with_extension("jsonl.v1.backup").exists());
    }

    #[test]
    fn test_migrate_rejects_existing_backup_before_writing_logs() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let seal_dir = seal_root.join(".seal");
        fs::create_dir(&seal_dir).unwrap();

        let legacy_path = seal_dir.join("events.jsonl");
        let log = open_or_create(&legacy_path).unwrap();
        log.append(&make_review_created("cr-001")).unwrap();
        fs::write(
            legacy_path.with_extension("jsonl.v1.backup"),
            "existing backup\n",
        )
        .unwrap();

        let result = run_migrate(seal_root, false, true, false, OutputFormat::Text);

        assert!(result.is_err(), "Should reject existing backup path");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Backup already exists"),
            "Error should mention existing backup, got: {err}"
        );
        assert!(legacy_path.exists(), "legacy events file should remain");
        assert!(
            !seal_dir.join("reviews").exists(),
            "migration should not create review logs before backup validation"
        );
        assert!(
            !seal_dir.join("version").exists(),
            "migration should not write version before backup validation"
        );
    }

    /// Regression test: crafted `review_id` with path traversal must be rejected.
    #[test]
    fn test_migrate_rejects_path_traversal_review_id() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let seal_dir = seal_root.join(".seal");
        fs::create_dir(&seal_dir).unwrap();

        let legacy_path = seal_dir.join("events.jsonl");
        let log = open_or_create(&legacy_path).unwrap();

        // Craft a malicious ReviewCreated with a path traversal review_id
        log.append(&EventEnvelope::new(
            "test_agent",
            Event::ReviewCreated(ReviewCreated {
                review_id: "../../../tmp/evil".to_string(),
                jj_change_id: "change123".to_string(),
                scm_kind: Some("jj".to_string()),
                scm_anchor: Some("change123".to_string()),
                initial_commit: "commit456".to_string(),
                title: "Malicious review".to_string(),
                description: None,
            }),
        ))
        .unwrap();

        let result = run_migrate(seal_root, false, true, false, OutputFormat::Text);
        assert!(result.is_err(), "Should reject invalid review_id");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Invalid review_id"),
            "Error should mention invalid review_id, got: {err}"
        );
    }

    #[test]
    fn test_migrate_rejects_invalid_review_id_before_partial_writes() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let seal_dir = seal_root.join(".seal");
        fs::create_dir(&seal_dir).unwrap();

        let legacy_path = seal_dir.join("events.jsonl");
        let log = open_or_create(&legacy_path).unwrap();
        log.append(&make_review_created("cr-001")).unwrap();
        log.append(&EventEnvelope::new(
            "test_agent",
            Event::ReviewCreated(ReviewCreated {
                review_id: "../../../tmp/evil".to_string(),
                jj_change_id: "change123".to_string(),
                scm_kind: Some("jj".to_string()),
                scm_anchor: Some("change123".to_string()),
                initial_commit: "commit456".to_string(),
                title: "Malicious review".to_string(),
                description: None,
            }),
        ))
        .unwrap();

        let result = run_migrate(seal_root, false, true, false, OutputFormat::Text);

        assert!(result.is_err(), "Should reject invalid review_id");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Invalid review_id"),
            "Error should mention invalid review_id, got: {err}"
        );
        assert!(legacy_path.exists(), "legacy events file should remain");
        assert!(
            !seal_dir.join("reviews").exists(),
            "migration should not create logs for earlier valid reviews before validation"
        );
        assert!(
            !seal_dir.join("version").exists(),
            "migration should not write version after failed validation"
        );
    }
}

//! Sync service — sync projection, rebuild, accept regression.

use crate::projection::{
    rebuild_from_review_logs, rebuild_review_from_log, sync_from_review_logs, ProjectionDb,
    SyncReport,
};

use super::{CoreContext, CoreError, CoreResult};

/// Service for sync operations.
pub struct SyncService<'a> {
    ctx: &'a CoreContext,
    db: &'a ProjectionDb,
}

impl<'a> SyncService<'a> {
    pub(crate) const fn new(ctx: &'a CoreContext, db: &'a ProjectionDb) -> Self {
        Self { ctx, db }
    }

    /// Sync the projection from event logs (incremental).
    ///
    /// Only processes new/changed review log files since the last sync.
    pub fn sync(&self) -> CoreResult<SyncReport> {
        sync_from_review_logs(self.db, self.ctx.seal_root()).map_err(CoreError::Internal)
    }

    /// Full destructive rebuild of the projection database.
    ///
    /// Drops all projected data and replays all events from scratch.
    /// Returns the number of events that were replayed during the rebuild phase.
    /// After rebuild, a sync is performed to re-populate file state tracking.
    pub fn rebuild(&self) -> CoreResult<RebuildResult> {
        let events_rebuilt =
            rebuild_from_review_logs(self.db, self.ctx.seal_root()).map_err(CoreError::Internal)?;
        let sync_report =
            sync_from_review_logs(self.db, self.ctx.seal_root()).map_err(CoreError::Internal)?;

        Ok(RebuildResult {
            events_rebuilt,
            sync_report,
        })
    }

    /// Accept a regression for a single review by rebuilding it from its log.
    ///
    /// Atomically deletes every projected row owned by the review (and its
    /// file state), replays the review's current log so its content becomes
    /// the new baseline, then runs a normal sync for all other files. Other
    /// reviews' projected rows are never modified, even if their logs are
    /// missing. On replay failure the projection is left unchanged.
    pub fn accept_regression(&self, review_id: &str) -> CoreResult<SyncReport> {
        let applied = rebuild_review_from_log(self.db, self.ctx.seal_root(), review_id)
            .map_err(CoreError::Internal)?;
        let mut report =
            sync_from_review_logs(self.db, self.ctx.seal_root()).map_err(CoreError::Internal)?;
        if applied > 0 {
            // The follow-up sync saw the rebuilt file as unchanged.
            report.applied += applied;
            report.files_synced += 1;
            report.files_skipped = report.files_skipped.saturating_sub(1);
        }
        Ok(report)
    }
}

/// Result of a full rebuild operation.
#[derive(Debug)]
pub struct RebuildResult {
    /// Number of events replayed during the rebuild phase.
    pub events_rebuilt: usize,
    /// Sync report from the post-rebuild incremental sync.
    pub sync_report: SyncReport,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{
        CodeSelection, CommentAdded, Event, EventEnvelope, ReviewCreated, ReviewRetargeted,
        ReviewerVoted, ReviewersRequested, ThreadCreated, ThreadResolved, VoteType,
    };
    use crate::log::{open_or_create_review, review_events_path, AppendLog};
    use crate::projection::AnomalyKind;
    use rusqlite::types::Value;
    use tempfile::{tempdir, TempDir};

    fn init_context() -> (TempDir, CoreContext) {
        let dir = tempdir().expect("tempdir");
        let seal_dir = dir.path().join(".seal");
        std::fs::create_dir(&seal_dir).expect("create .seal");
        std::fs::write(seal_dir.join("version"), "2\n").expect("write version");
        std::fs::create_dir(seal_dir.join("reviews")).expect("create reviews dir");
        let ctx = CoreContext::new(dir.path(), &seal_dir.join("index.db")).expect("context");
        (dir, ctx)
    }

    fn open_db(ctx: &CoreContext) -> ProjectionDb {
        let db = ProjectionDb::open(ctx.db_path()).expect("open db");
        db.init_schema().expect("init schema");
        db
    }

    fn append(ctx: &CoreContext, review_id: &str, author: &str, event: Event) {
        open_or_create_review(ctx.seal_root(), review_id)
            .expect("review log")
            .append(&EventEnvelope::new(author, event))
            .expect("append");
    }

    /// Write a review log exercising every projected table: reviewers, votes
    /// before and after a retarget, a thread with comments, and a resolve.
    fn write_full_review(ctx: &CoreContext, review_id: &str, thread_id: &str) {
        append(
            ctx,
            review_id,
            "alice",
            Event::ReviewCreated(ReviewCreated {
                review_id: review_id.to_string(),
                jj_change_id: "change-1".to_string(),
                scm_kind: Some("git".to_string()),
                scm_anchor: Some("refs/heads/v1".to_string()),
                initial_commit: "c1".to_string(),
                base_commit: Some("base".to_string()),
                title: format!("Review {review_id}"),
                description: Some("desc".to_string()),
            }),
        );
        append(
            ctx,
            review_id,
            "alice",
            Event::ReviewersRequested(ReviewersRequested {
                review_id: review_id.to_string(),
                reviewers: vec!["bob".to_string(), "carol".to_string()],
            }),
        );
        append(
            ctx,
            review_id,
            "bob",
            Event::ReviewerVoted(ReviewerVoted {
                review_id: review_id.to_string(),
                vote: VoteType::Lgtm,
                reason: None,
                target_commit: Some("c1".to_string()),
            }),
        );
        append(
            ctx,
            review_id,
            "alice",
            Event::ReviewRetargeted(ReviewRetargeted {
                review_id: review_id.to_string(),
                old_scm_anchor: "refs/heads/v1".to_string(),
                new_scm_anchor: "refs/heads/v2".to_string(),
                old_target_commit: "c1".to_string(),
                new_target_commit: "c2".to_string(),
                old_base_commit: None,
                new_base_commit: None,
            }),
        );
        append(
            ctx,
            review_id,
            "carol",
            Event::ReviewerVoted(ReviewerVoted {
                review_id: review_id.to_string(),
                vote: VoteType::Block,
                reason: Some("needs work".to_string()),
                target_commit: Some("c2".to_string()),
            }),
        );
        append(
            ctx,
            review_id,
            "carol",
            Event::ThreadCreated(ThreadCreated {
                thread_id: thread_id.to_string(),
                review_id: review_id.to_string(),
                file_path: "src/lib.rs".to_string(),
                selection: CodeSelection::range(3, 7),
                commit_hash: "c2".to_string(),
            }),
        );
        for n in 1..=2 {
            append(
                ctx,
                review_id,
                "carol",
                Event::CommentAdded(CommentAdded {
                    comment_id: format!("{thread_id}.{n}"),
                    thread_id: thread_id.to_string(),
                    body: format!("comment {n}"),
                }),
            );
        }
        append(
            ctx,
            review_id,
            "alice",
            Event::ThreadResolved(ThreadResolved {
                thread_id: thread_id.to_string(),
                reason: Some("fixed".to_string()),
            }),
        );
    }

    /// Dump every projected row belonging to `review_id`, across all
    /// per-review tables, in a deterministic order.
    fn dump_review_rows(db: &ProjectionDb, review_id: &str) -> Vec<(String, Vec<Value>)> {
        let queries = [
            ("reviews", "SELECT * FROM reviews WHERE review_id = ?1"),
            (
                "review_reviewers",
                "SELECT * FROM review_reviewers WHERE review_id = ?1 ORDER BY reviewer",
            ),
            (
                "reviewer_votes",
                "SELECT * FROM reviewer_votes WHERE review_id = ?1 ORDER BY reviewer",
            ),
            (
                "threads",
                "SELECT * FROM threads WHERE review_id = ?1 ORDER BY thread_id",
            ),
            (
                "comments",
                "SELECT c.* FROM comments c JOIN threads t ON t.thread_id = c.thread_id \
                 WHERE t.review_id = ?1 ORDER BY c.comment_id",
            ),
            (
                "review_file_state",
                "SELECT * FROM review_file_state WHERE review_id = ?1",
            ),
        ];
        let mut out = Vec::new();
        for (table, sql) in queries {
            let mut stmt = db.conn().prepare(sql).expect("prepare dump");
            let cols = stmt.column_count();
            let rows = stmt
                .query_map([review_id], |row| {
                    (0..cols).map(|i| row.get::<_, Value>(i)).collect()
                })
                .expect("query dump");
            for row in rows {
                out.push((table.to_string(), row.expect("row")));
            }
        }
        out
    }

    fn count_rows(rows: &[(String, Vec<Value>)], table: &str) -> usize {
        rows.iter().filter(|(t, _)| t == table).count()
    }

    /// Fresh projection of the on-disk logs, independent of `ctx`'s db.
    fn fresh_projection(ctx: &CoreContext) -> ProjectionDb {
        let db = ProjectionDb::open_in_memory().expect("in-memory db");
        db.init_schema().expect("init schema");
        let report = sync_from_review_logs(&db, ctx.seal_root()).expect("fresh sync");
        assert!(
            report.anomalies.is_empty(),
            "fresh sync: {:?}",
            report.anomalies
        );
        db
    }

    /// Force the stored file state for `review_id` to disagree with the file,
    /// so a normal sync reports a hash-mismatch regression.
    fn simulate_hash_mismatch(db: &ProjectionDb, review_id: &str) {
        db.conn()
            .execute(
                "UPDATE review_file_state SET prefix_hash = 'bogus', byte_count = 1, line_count = 1 \
                 WHERE review_id = ?1",
                [review_id],
            )
            .expect("corrupt file state");
    }

    /// Write a divergent history for `review_id` that the projection absorbs
    /// before the "real" log is restored: an extra reviewer, a vote after the
    /// retarget, and a thread + comment that the restored log does not contain.
    fn write_stale_review(ctx: &CoreContext, review_id: &str) {
        append(
            ctx,
            review_id,
            "alice",
            Event::ReviewCreated(ReviewCreated {
                review_id: review_id.to_string(),
                jj_change_id: "change-1".to_string(),
                scm_kind: Some("git".to_string()),
                scm_anchor: Some("refs/heads/v1".to_string()),
                initial_commit: "c1".to_string(),
                base_commit: Some("base".to_string()),
                title: format!("Review {review_id}"),
                description: Some("desc".to_string()),
            }),
        );
        append(
            ctx,
            review_id,
            "alice",
            Event::ReviewRetargeted(ReviewRetargeted {
                review_id: review_id.to_string(),
                old_scm_anchor: "refs/heads/v1".to_string(),
                new_scm_anchor: "refs/heads/v2".to_string(),
                old_target_commit: "c1".to_string(),
                new_target_commit: "c2".to_string(),
                old_base_commit: None,
                new_base_commit: None,
            }),
        );
        append(
            ctx,
            review_id,
            "alice",
            Event::ReviewersRequested(ReviewersRequested {
                review_id: review_id.to_string(),
                reviewers: vec!["dave".to_string()],
            }),
        );
        append(
            ctx,
            review_id,
            "dave",
            Event::ReviewerVoted(ReviewerVoted {
                review_id: review_id.to_string(),
                vote: VoteType::Lgtm,
                reason: None,
                target_commit: Some("c2".to_string()),
            }),
        );
        append(
            ctx,
            review_id,
            "dave",
            Event::ThreadCreated(ThreadCreated {
                thread_id: "th-stale".to_string(),
                review_id: review_id.to_string(),
                file_path: "src/stale.rs".to_string(),
                selection: CodeSelection::line(1),
                commit_hash: "c2".to_string(),
            }),
        );
        append(
            ctx,
            review_id,
            "dave",
            Event::CommentAdded(CommentAdded {
                comment_id: "th-stale.1".to_string(),
                thread_id: "th-stale".to_string(),
                body: "stale".to_string(),
            }),
        );
    }

    #[test]
    fn accept_regression_replays_retargeted_review_like_fresh_projection() {
        let (_dir, ctx) = init_context();
        write_stale_review(&ctx, "cr-a");
        write_full_review(&ctx, "cr-b", "th-b");
        let db = open_db(&ctx);
        let sync = SyncService::new(&ctx, &db);
        assert!(sync.sync().expect("sync").anomalies.is_empty());

        // Restore a different (complete, valid) history for cr-a.
        let path = review_events_path(ctx.seal_root(), "cr-a");
        std::fs::remove_file(&path).expect("remove cr-a log");
        write_full_review(&ctx, "cr-a", "th-a");
        let regressed = sync.sync().expect("sync");
        assert!(
            regressed
                .anomalies
                .iter()
                .any(|a| a.review_id == "cr-a" && a.kind == AnomalyKind::HashMismatch),
            "{:?}",
            regressed.anomalies
        );

        let report = sync.accept_regression("cr-a").expect("accept regression");
        assert!(
            report.anomalies.is_empty(),
            "accept_regression must not report anomalies: {:?}",
            report.anomalies
        );

        let expected = dump_review_rows(&fresh_projection(&ctx), "cr-a");
        let actual = dump_review_rows(&db, "cr-a");
        assert_eq!(count_rows(&expected, "reviews"), 1);
        assert_eq!(count_rows(&expected, "review_reviewers"), 2);
        assert_eq!(count_rows(&expected, "reviewer_votes"), 1);
        assert_eq!(count_rows(&expected, "threads"), 1);
        assert_eq!(count_rows(&expected, "comments"), 2);
        assert_eq!(actual, expected);

        // The rebuilt review is re-baselined: a plain sync is now clean.
        assert!(sync.sync().expect("sync").anomalies.is_empty());
    }

    #[test]
    fn accept_regression_with_identical_log_is_idempotent() {
        let (_dir, ctx) = init_context();
        write_full_review(&ctx, "cr-a", "th-a");
        // A second file-state row keeps sync out of its bootstrap path.
        write_full_review(&ctx, "cr-b", "th-b");
        let db = open_db(&ctx);
        let sync = SyncService::new(&ctx, &db);
        sync.sync().expect("sync");
        let before = dump_review_rows(&db, "cr-a");

        for _ in 0..2 {
            let report = sync.accept_regression("cr-a").expect("accept regression");
            assert!(report.anomalies.is_empty(), "{:?}", report.anomalies);
            assert_eq!(dump_review_rows(&db, "cr-a"), before);
        }
    }

    fn assert_other_review_preserved(remove_other_log: bool) {
        let (_dir, ctx) = init_context();
        write_full_review(&ctx, "cr-a", "th-a");
        write_full_review(&ctx, "cr-b", "th-b");
        let db = open_db(&ctx);
        let sync = SyncService::new(&ctx, &db);
        assert!(sync.sync().expect("sync").anomalies.is_empty());

        let b_before = dump_review_rows(&db, "cr-b");
        assert_eq!(count_rows(&b_before, "reviews"), 1);
        assert_eq!(count_rows(&b_before, "comments"), 2);
        assert_eq!(count_rows(&b_before, "review_file_state"), 1);

        if remove_other_log {
            let path = review_events_path(ctx.seal_root(), "cr-b");
            std::fs::remove_dir_all(path.parent().expect("review dir")).expect("remove cr-b");
        }
        simulate_hash_mismatch(&db, "cr-a");

        let report = sync.accept_regression("cr-a").expect("accept regression");
        assert!(
            !report.anomalies.iter().any(|a| a.review_id == "cr-a"),
            "{:?}",
            report.anomalies
        );
        if remove_other_log {
            assert!(report
                .anomalies
                .iter()
                .any(|a| a.review_id == "cr-b" && a.kind == AnomalyKind::Missing));
        }

        assert_eq!(dump_review_rows(&db, "cr-b"), b_before);
        assert_eq!(count_rows(&dump_review_rows(&db, "cr-a"), "reviews"), 1);
    }

    #[test]
    fn accept_regression_preserves_other_review_rows() {
        assert_other_review_preserved(false);
    }

    #[test]
    fn accept_regression_preserves_other_review_rows_when_its_log_is_missing() {
        assert_other_review_preserved(true);
    }

    #[test]
    fn accept_regression_failed_replay_leaves_projection_unchanged() {
        let (_dir, ctx) = init_context();
        write_full_review(&ctx, "cr-a", "th-a");
        write_full_review(&ctx, "cr-b", "th-b");
        let db = open_db(&ctx);
        let sync = SyncService::new(&ctx, &db);
        assert!(sync.sync().expect("sync").anomalies.is_empty());
        let a_before = dump_review_rows(&db, "cr-a");
        let b_before = dump_review_rows(&db, "cr-b");

        // Replace cr-a's log with one whose retarget cannot apply: the
        // reset must be rolled back along with the partial replay.
        let path = review_events_path(ctx.seal_root(), "cr-a");
        std::fs::remove_file(&path).expect("remove cr-a log");
        write_stale_review(&ctx, "cr-a");
        append(
            &ctx,
            "cr-a",
            "alice",
            Event::ReviewRetargeted(ReviewRetargeted {
                review_id: "cr-a".to_string(),
                old_scm_anchor: "refs/heads/nope".to_string(),
                new_scm_anchor: "refs/heads/v3".to_string(),
                old_target_commit: "c2".to_string(),
                new_target_commit: "c3".to_string(),
                old_base_commit: None,
                new_base_commit: None,
            }),
        );

        let err = sync
            .accept_regression("cr-a")
            .expect_err("invalid log must fail");
        assert!(err.to_string().contains("ReviewRetargeted"), "{err}");
        assert_eq!(dump_review_rows(&db, "cr-a"), a_before);
        assert_eq!(dump_review_rows(&db, "cr-b"), b_before);
    }

    #[test]
    fn accept_regression_drops_stale_rows_absent_from_restored_log() {
        let (_dir, ctx) = init_context();
        write_stale_review(&ctx, "cr-a");
        write_full_review(&ctx, "cr-b", "th-b");
        let db = open_db(&ctx);
        let sync = SyncService::new(&ctx, &db);
        assert!(sync.sync().expect("sync").anomalies.is_empty());

        // Restored log only creates the review: no retarget to clear votes,
        // so every stale row must go away through the reset alone.
        let path = review_events_path(ctx.seal_root(), "cr-a");
        std::fs::remove_file(&path).expect("remove cr-a log");
        append(
            &ctx,
            "cr-a",
            "alice",
            Event::ReviewCreated(ReviewCreated {
                review_id: "cr-a".to_string(),
                jj_change_id: "change-2".to_string(),
                scm_kind: Some("git".to_string()),
                scm_anchor: Some("refs/heads/v9".to_string()),
                initial_commit: "c9".to_string(),
                base_commit: None,
                title: "Restored".to_string(),
                description: None,
            }),
        );

        let report = sync.accept_regression("cr-a").expect("accept regression");
        assert!(report.anomalies.is_empty(), "{:?}", report.anomalies);

        let expected = dump_review_rows(&fresh_projection(&ctx), "cr-a");
        assert_eq!(count_rows(&expected, "reviews"), 1);
        assert_eq!(expected.len(), 2, "only reviews + review_file_state rows");
        assert_eq!(dump_review_rows(&db, "cr-a"), expected);
    }
}

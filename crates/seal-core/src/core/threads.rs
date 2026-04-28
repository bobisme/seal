//! Thread service — create, list, resolve, reopen.

use crate::events::{
    get_agent_identity, new_thread_id, CodeSelection, Event, EventEnvelope, ThreadCreated,
    ThreadReopened, ThreadResolved,
};
use crate::log::{open_or_create_review, AppendLog};
use crate::projection::{ProjectionDb, ThreadDetail, ThreadSummary};

use super::{CoreContext, CoreError, CoreResult};

/// Service for thread operations.
pub struct ThreadService<'a> {
    ctx: &'a CoreContext,
    db: &'a ProjectionDb,
}

impl<'a> ThreadService<'a> {
    pub(crate) const fn new(ctx: &'a CoreContext, db: &'a ProjectionDb) -> Self {
        Self { ctx, db }
    }

    fn ensure_review_accepts_thread_changes(&self, review_id: &str) -> CoreResult<()> {
        let review = self
            .db
            .get_review(review_id)
            .map_err(CoreError::Internal)?
            .ok_or_else(|| CoreError::ReviewNotFound {
                review_id: review_id.to_string(),
            })?;

        if review.status != "open" && review.status != "approved" {
            return Err(CoreError::InvalidReviewStatus {
                review_id: review_id.to_string(),
                actual: review.status,
                expected: "open or approved".to_string(),
            });
        }

        Ok(())
    }

    /// List threads for a review with optional status and file filtering.
    pub fn list(
        &self,
        review_id: &str,
        status: Option<&str>,
        file: Option<&str>,
    ) -> CoreResult<Vec<ThreadSummary>> {
        self.db
            .list_threads(review_id, status, file)
            .map_err(CoreError::Internal)
    }

    /// Get detailed information about a single thread.
    ///
    /// Returns `Err(CoreError::ThreadNotFound)` if the thread does not exist.
    pub fn get(&self, thread_id: &str) -> CoreResult<ThreadDetail> {
        self.db
            .get_thread(thread_id)
            .map_err(CoreError::Internal)?
            .ok_or_else(|| CoreError::ThreadNotFound {
                thread_id: thread_id.to_string(),
            })
    }

    /// Get detailed information about a thread, returning `None` if not found.
    pub fn get_optional(&self, thread_id: &str) -> CoreResult<Option<ThreadDetail>> {
        self.db.get_thread(thread_id).map_err(CoreError::Internal)
    }

    /// Create a new thread on a review.
    ///
    /// Returns the new thread ID.
    pub fn create(
        &self,
        review_id: &str,
        file_path: &str,
        selection: CodeSelection,
        commit_hash: String,
        author: Option<&str>,
    ) -> CoreResult<String> {
        self.ensure_review_accepts_thread_changes(review_id)?;

        let thread_id = new_thread_id();
        let author_str = get_agent_identity(author).map_err(CoreError::Internal)?;

        let event = EventEnvelope::new(
            &author_str,
            Event::ThreadCreated(ThreadCreated {
                thread_id: thread_id.clone(),
                review_id: review_id.to_string(),
                file_path: file_path.to_string(),
                selection,
                commit_hash,
            }),
        );

        let log =
            open_or_create_review(self.ctx.seal_root(), review_id).map_err(CoreError::Internal)?;
        log.append(&event).map_err(CoreError::Internal)?;

        Ok(thread_id)
    }

    /// Find an existing open thread at a specific file and line.
    pub fn find_at_location(
        &self,
        review_id: &str,
        file_path: &str,
        line: i64,
    ) -> CoreResult<Option<String>> {
        self.db
            .find_thread_at_location(review_id, file_path, line)
            .map_err(CoreError::Internal)
    }

    /// Resolve a thread.
    pub fn resolve(
        &self,
        thread_id: &str,
        reason: Option<String>,
        author: Option<&str>,
    ) -> CoreResult<()> {
        let thread = self.get(thread_id)?;
        self.ensure_review_accepts_thread_changes(&thread.review_id)?;

        if thread.status != "open" {
            return Err(CoreError::InvalidReviewStatus {
                review_id: thread.review_id.clone(),
                actual: format!("thread status: {}", thread.status),
                expected: "open".to_string(),
            });
        }

        let author_str = get_agent_identity(author).map_err(CoreError::Internal)?;

        let event = EventEnvelope::new(
            &author_str,
            Event::ThreadResolved(ThreadResolved {
                thread_id: thread_id.to_string(),
                reason,
            }),
        );

        let log = open_or_create_review(self.ctx.seal_root(), &thread.review_id)
            .map_err(CoreError::Internal)?;
        log.append(&event).map_err(CoreError::Internal)?;

        Ok(())
    }

    /// Reopen a resolved thread.
    pub fn reopen(
        &self,
        thread_id: &str,
        reason: Option<String>,
        author: Option<&str>,
    ) -> CoreResult<()> {
        let thread = self.get(thread_id)?;
        self.ensure_review_accepts_thread_changes(&thread.review_id)?;

        if thread.status != "resolved" {
            return Err(CoreError::InvalidReviewStatus {
                review_id: thread.review_id.clone(),
                actual: format!("thread status: {}", thread.status),
                expected: "resolved".to_string(),
            });
        }

        let author_str = get_agent_identity(author).map_err(CoreError::Internal)?;

        let event = EventEnvelope::new(
            &author_str,
            Event::ThreadReopened(ThreadReopened {
                thread_id: thread_id.to_string(),
                reason,
            }),
        );

        let log = open_or_create_review(self.ctx.seal_root(), &thread.review_id)
            .map_err(CoreError::Internal)?;
        log.append(&event).map_err(CoreError::Internal)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ReviewCreated, ReviewMerged};
    use crate::log::{open_or_create_review, AppendLog};
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

    fn append_review_created(ctx: &CoreContext, review_id: &str) {
        let event = EventEnvelope::new(
            "alice",
            Event::ReviewCreated(ReviewCreated {
                review_id: review_id.to_string(),
                jj_change_id: "change-id".to_string(),
                scm_kind: Some("git".to_string()),
                scm_anchor: Some("refs/heads/main".to_string()),
                initial_commit: "abc123".to_string(),
                title: "Review".to_string(),
                description: None,
            }),
        );
        open_or_create_review(ctx.seal_root(), review_id)
            .expect("review log")
            .append(&event)
            .expect("append review");
    }

    fn append_thread_created(ctx: &CoreContext, review_id: &str, thread_id: &str) {
        let event = EventEnvelope::new(
            "alice",
            Event::ThreadCreated(ThreadCreated {
                thread_id: thread_id.to_string(),
                review_id: review_id.to_string(),
                file_path: "src/lib.rs".to_string(),
                selection: CodeSelection::line(12),
                commit_hash: "abc123".to_string(),
            }),
        );
        open_or_create_review(ctx.seal_root(), review_id)
            .expect("review log")
            .append(&event)
            .expect("append thread");
    }

    fn append_review_merged(ctx: &CoreContext, review_id: &str) {
        let event = EventEnvelope::new(
            "alice",
            Event::ReviewMerged(ReviewMerged {
                review_id: review_id.to_string(),
                final_commit: "def456".to_string(),
            }),
        );
        open_or_create_review(ctx.seal_root(), review_id)
            .expect("review log")
            .append(&event)
            .expect("append merge");
    }

    #[test]
    fn create_rejects_missing_review_without_writing_orphan_log() {
        let (_dir, ctx) = init_context();
        let services = ctx.services().expect("services");

        let err = services
            .threads()
            .create(
                "cr-missing",
                "src/lib.rs",
                CodeSelection::line(1),
                "abc123".to_string(),
                Some("alice"),
            )
            .expect_err("missing review should fail");

        assert!(matches!(err, CoreError::ReviewNotFound { .. }));
        assert!(!ctx
            .seal_root()
            .join(".seal/reviews/cr-missing/events.jsonl")
            .exists());
    }

    #[test]
    fn create_rejects_completed_review() {
        let (_dir, ctx) = init_context();
        append_review_created(&ctx, "cr-done");
        append_review_merged(&ctx, "cr-done");
        let services = ctx.services().expect("services");

        let err = services
            .threads()
            .create(
                "cr-done",
                "src/lib.rs",
                CodeSelection::line(1),
                "abc123".to_string(),
                Some("alice"),
            )
            .expect_err("completed review should fail");

        assert!(matches!(err, CoreError::InvalidReviewStatus { .. }));
    }

    #[test]
    fn resolve_rejects_completed_review() {
        let (_dir, ctx) = init_context();
        append_review_created(&ctx, "cr-done");
        append_thread_created(&ctx, "cr-done", "th-open1");
        append_review_merged(&ctx, "cr-done");
        let services = ctx.services().expect("services");

        let err = services
            .threads()
            .resolve("th-open1", None, Some("alice"))
            .expect_err("completed review should fail");

        assert!(matches!(err, CoreError::InvalidReviewStatus { .. }));
    }
}

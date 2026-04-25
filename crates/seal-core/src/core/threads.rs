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

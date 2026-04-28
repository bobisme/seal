//! Comment service — add comment, add comment with auto-thread-create.

use crate::events::{
    get_agent_identity, make_comment_id, new_thread_id, CodeSelection, CommentAdded, Event,
    EventEnvelope, ThreadCreated,
};
use crate::log::{open_or_create_review, AppendLog};
use crate::projection::{Comment, ProjectionDb};

use super::threads::validate_selection;
use super::{CoreContext, CoreError, CoreResult};

/// Result of adding a comment, including IDs for the caller.
#[derive(Debug, Clone)]
pub struct AddCommentResult {
    /// The comment ID that was created (e.g., "th-abc.1").
    pub comment_id: String,
    /// The thread ID the comment was added to.
    pub thread_id: String,
    /// Whether a new thread was created for this comment.
    pub thread_created: bool,
}

/// Service for comment operations.
pub struct CommentService<'a> {
    ctx: &'a CoreContext,
    db: &'a ProjectionDb,
}

impl<'a> CommentService<'a> {
    pub(crate) const fn new(ctx: &'a CoreContext, db: &'a ProjectionDb) -> Self {
        Self { ctx, db }
    }

    /// Add a comment to an existing thread.
    ///
    /// Validates that the thread and its parent review exist and are in valid states.
    pub fn add_to_thread(
        &self,
        thread_id: &str,
        body: &str,
        author: Option<&str>,
    ) -> CoreResult<AddCommentResult> {
        let thread = self
            .db
            .get_thread(thread_id)
            .map_err(CoreError::Internal)?
            .ok_or_else(|| CoreError::ThreadNotFound {
                thread_id: thread_id.to_string(),
            })?;

        // Verify review is open or approved
        if let Some(review) = self
            .db
            .get_review(&thread.review_id)
            .map_err(CoreError::Internal)?
        {
            if review.status != "open" && review.status != "approved" {
                return Err(CoreError::InvalidReviewStatus {
                    review_id: thread.review_id.clone(),
                    actual: review.status,
                    expected: "open or approved".to_string(),
                });
            }
        }

        let comment_number = self
            .db
            .get_next_comment_number(thread_id)
            .map_err(CoreError::Internal)?
            .ok_or_else(|| CoreError::ThreadNotFound {
                thread_id: thread_id.to_string(),
            })?;

        let comment_id = make_comment_id(thread_id, comment_number);
        let author_str = get_agent_identity(author).map_err(CoreError::Internal)?;

        let event = EventEnvelope::new(
            &author_str,
            Event::CommentAdded(CommentAdded {
                comment_id: comment_id.clone(),
                thread_id: thread_id.to_string(),
                body: body.to_string(),
            }),
        );

        let log = open_or_create_review(self.ctx.seal_root(), &thread.review_id)
            .map_err(CoreError::Internal)?;
        log.append(&event).map_err(CoreError::Internal)?;

        Ok(AddCommentResult {
            comment_id,
            thread_id: thread_id.to_string(),
            thread_created: false,
        })
    }

    /// Add a comment to a review, auto-creating a thread if needed.
    ///
    /// This mirrors the `seal comment` behavior:
    /// - If an open thread exists at the file+line, adds a comment to it
    /// - If no thread exists, creates one and adds the comment
    ///
    /// The `commit_hash` is used when creating a new thread. If `None`, the
    /// caller must provide an SCM repo to resolve the commit from the review's
    /// anchor.
    pub fn add_to_review(
        &self,
        review_id: &str,
        file_path: &str,
        selection: CodeSelection,
        body: &str,
        commit_hash: String,
        author: Option<&str>,
    ) -> CoreResult<AddCommentResult> {
        validate_selection(&selection)?;

        // Verify review exists and is open or approved
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

        let author_str = get_agent_identity(author).map_err(CoreError::Internal)?;
        let start_line = i64::from(selection.start_line());

        // Check for existing thread at this location
        let (thread_id, comment_number, thread_created) = if let Some(existing_id) = self
            .db
            .find_thread_at_location(review_id, file_path, start_line)
            .map_err(CoreError::Internal)?
        {
            let comment_number = self
                .db
                .get_next_comment_number(&existing_id)
                .map_err(CoreError::Internal)?
                .ok_or_else(|| CoreError::ThreadNotFound {
                    thread_id: existing_id.clone(),
                })?;
            (existing_id, comment_number, false)
        } else {
            // Create new thread
            let new_thread_id = new_thread_id();

            let thread_event = EventEnvelope::new(
                &author_str,
                Event::ThreadCreated(ThreadCreated {
                    thread_id: new_thread_id.clone(),
                    review_id: review_id.to_string(),
                    file_path: file_path.to_string(),
                    selection,
                    commit_hash,
                }),
            );

            let log = open_or_create_review(self.ctx.seal_root(), review_id)
                .map_err(CoreError::Internal)?;
            log.append(&thread_event).map_err(CoreError::Internal)?;

            (new_thread_id, 1, true)
        };

        // Add the comment
        let comment_id = make_comment_id(&thread_id, comment_number);

        let comment_event = EventEnvelope::new(
            &author_str,
            Event::CommentAdded(CommentAdded {
                comment_id: comment_id.clone(),
                thread_id: thread_id.clone(),
                body: body.to_string(),
            }),
        );

        let log =
            open_or_create_review(self.ctx.seal_root(), review_id).map_err(CoreError::Internal)?;
        log.append(&comment_event).map_err(CoreError::Internal)?;

        Ok(AddCommentResult {
            comment_id,
            thread_id,
            thread_created,
        })
    }

    /// List comments for a thread.
    pub fn list(&self, thread_id: &str) -> CoreResult<Vec<Comment>> {
        self.db
            .list_comments(thread_id)
            .map_err(CoreError::Internal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ReviewCreated;
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

    #[test]
    fn add_to_review_rejects_zero_line_selection_without_writing_thread() {
        let (_dir, ctx) = init_context();
        append_review_created(&ctx, "cr-open");
        let services = ctx.services().expect("services");

        let err = services
            .comments()
            .add_to_review(
                "cr-open",
                "src/lib.rs",
                CodeSelection::line(0),
                "body",
                "abc123".to_string(),
                Some("alice"),
            )
            .expect_err("invalid selection should fail");

        assert!(matches!(err, CoreError::InvalidCodeSelection { .. }));

        let services = ctx.services().expect("services");
        let threads = services
            .threads()
            .list("cr-open", None, None)
            .expect("list threads");
        assert!(threads.is_empty());
    }
}

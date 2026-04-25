//! Review service — list, get, create, request reviewers, vote, approve, abandon, mark merged.

use crate::events::{
    get_agent_identity, new_review_id, Event, EventEnvelope, ReviewAbandoned, ReviewApproved,
    ReviewCreated, ReviewMerged, ReviewerVoted, ReviewersRequested, VoteType,
};
use crate::log::{open_or_create_review, AppendLog};
use crate::projection::{ProjectionDb, ReviewDetail, ReviewSummary};
use crate::scm::ScmRepo;

use super::{CoreContext, CoreError, CoreResult};

/// Service for review operations.
pub struct ReviewService<'a> {
    ctx: &'a CoreContext,
    db: &'a ProjectionDb,
}

impl<'a> ReviewService<'a> {
    pub(crate) const fn new(ctx: &'a CoreContext, db: &'a ProjectionDb) -> Self {
        Self { ctx, db }
    }

    /// List reviews with optional filtering by status and author.
    pub fn list(
        &self,
        status: Option<&str>,
        author: Option<&str>,
    ) -> CoreResult<Vec<ReviewSummary>> {
        self.db
            .list_reviews(status, author)
            .map_err(CoreError::Internal)
    }

    /// List reviews with extended filtering options.
    pub fn list_filtered(
        &self,
        status: Option<&str>,
        author: Option<&str>,
        needs_reviewer: Option<&str>,
        has_unresolved: bool,
    ) -> CoreResult<Vec<ReviewSummary>> {
        self.db
            .list_reviews_filtered(status, author, needs_reviewer, has_unresolved)
            .map_err(CoreError::Internal)
    }

    /// Get detailed information about a single review.
    ///
    /// Returns `Err(CoreError::ReviewNotFound)` if the review does not exist.
    pub fn get(&self, review_id: &str) -> CoreResult<ReviewDetail> {
        self.db
            .get_review(review_id)
            .map_err(CoreError::Internal)?
            .ok_or_else(|| CoreError::ReviewNotFound {
                review_id: review_id.to_string(),
            })
    }

    /// Get detailed information about a review, returning `None` if not found.
    pub fn get_optional(&self, review_id: &str) -> CoreResult<Option<ReviewDetail>> {
        self.db.get_review(review_id).map_err(CoreError::Internal)
    }

    /// Create a new review.
    ///
    /// Generates a new review ID, writes a `ReviewCreated` event, and optionally
    /// writes a `ReviewersRequested` event.
    ///
    /// Returns the new review ID.
    pub fn create(
        &self,
        scm: &dyn ScmRepo,
        title: String,
        description: Option<String>,
        reviewers: Option<Vec<String>>,
        author: Option<&str>,
    ) -> CoreResult<String> {
        let change_id = scm.current_anchor().map_err(CoreError::Internal)?;
        let commit_id = scm.current_commit().map_err(CoreError::Internal)?;

        let review_id = new_review_id();
        let author_str = get_agent_identity(author).map_err(CoreError::Internal)?;

        let scm_kind = scm.kind().as_str().to_string();

        let event = EventEnvelope::new(
            &author_str,
            Event::ReviewCreated(ReviewCreated {
                review_id: review_id.clone(),
                jj_change_id: change_id.clone(),
                scm_kind: Some(scm_kind),
                scm_anchor: Some(change_id),
                initial_commit: commit_id,
                title,
                description,
            }),
        );

        let log =
            open_or_create_review(self.ctx.seal_root(), &review_id).map_err(CoreError::Internal)?;
        log.append(&event).map_err(CoreError::Internal)?;

        // Request reviewers if specified
        if let Some(reviewers) = reviewers {
            if !reviewers.is_empty() {
                let reviewer_event = EventEnvelope::new(
                    &author_str,
                    Event::ReviewersRequested(ReviewersRequested {
                        review_id: review_id.clone(),
                        reviewers,
                    }),
                );
                log.append(&reviewer_event).map_err(CoreError::Internal)?;
            }
        }

        Ok(review_id)
    }

    /// Request reviewers for an existing review.
    pub fn request_reviewers(
        &self,
        review_id: &str,
        reviewers: Vec<String>,
        author: Option<&str>,
    ) -> CoreResult<()> {
        // Verify review exists and is open
        let review = self.get(review_id)?;
        if review.status != "open" && review.status != "approved" {
            return Err(CoreError::InvalidReviewStatus {
                review_id: review_id.to_string(),
                actual: review.status,
                expected: "open or approved".to_string(),
            });
        }

        let author_str = get_agent_identity(author).map_err(CoreError::Internal)?;

        let event = EventEnvelope::new(
            &author_str,
            Event::ReviewersRequested(ReviewersRequested {
                review_id: review_id.to_string(),
                reviewers,
            }),
        );

        let log =
            open_or_create_review(self.ctx.seal_root(), review_id).map_err(CoreError::Internal)?;
        log.append(&event).map_err(CoreError::Internal)?;

        Ok(())
    }

    /// Vote on a review (LGTM or block).
    pub fn vote(
        &self,
        review_id: &str,
        vote: VoteType,
        reason: Option<String>,
        author: Option<&str>,
    ) -> CoreResult<()> {
        let review = self.get(review_id)?;
        if review.status != "open" && review.status != "approved" {
            return Err(CoreError::InvalidReviewStatus {
                review_id: review_id.to_string(),
                actual: review.status,
                expected: "open or approved".to_string(),
            });
        }

        let author_str = get_agent_identity(author).map_err(CoreError::Internal)?;

        let event = EventEnvelope::new(
            &author_str,
            Event::ReviewerVoted(ReviewerVoted {
                review_id: review_id.to_string(),
                vote,
                reason,
            }),
        );

        let log =
            open_or_create_review(self.ctx.seal_root(), review_id).map_err(CoreError::Internal)?;
        log.append(&event).map_err(CoreError::Internal)?;

        Ok(())
    }

    /// Approve a review.
    pub fn approve(&self, review_id: &str, author: Option<&str>) -> CoreResult<()> {
        let review = self.get(review_id)?;
        if review.status != "open" {
            return Err(CoreError::InvalidReviewStatus {
                review_id: review_id.to_string(),
                actual: review.status,
                expected: "open".to_string(),
            });
        }

        let author_str = get_agent_identity(author).map_err(CoreError::Internal)?;

        let event = EventEnvelope::new(
            &author_str,
            Event::ReviewApproved(ReviewApproved {
                review_id: review_id.to_string(),
            }),
        );

        let log =
            open_or_create_review(self.ctx.seal_root(), review_id).map_err(CoreError::Internal)?;
        log.append(&event).map_err(CoreError::Internal)?;

        Ok(())
    }

    /// Abandon a review.
    pub fn abandon(
        &self,
        review_id: &str,
        reason: Option<String>,
        author: Option<&str>,
    ) -> CoreResult<()> {
        let review = self.get(review_id)?;
        if review.status == "merged" || review.status == "abandoned" {
            return Err(CoreError::InvalidReviewStatus {
                review_id: review_id.to_string(),
                actual: review.status,
                expected: "open or approved".to_string(),
            });
        }

        let author_str = get_agent_identity(author).map_err(CoreError::Internal)?;

        let event = EventEnvelope::new(
            &author_str,
            Event::ReviewAbandoned(ReviewAbandoned {
                review_id: review_id.to_string(),
                reason,
            }),
        );

        let log =
            open_or_create_review(self.ctx.seal_root(), review_id).map_err(CoreError::Internal)?;
        log.append(&event).map_err(CoreError::Internal)?;

        Ok(())
    }

    /// Mark a review as merged.
    pub fn mark_merged(
        &self,
        review_id: &str,
        final_commit: String,
        author: Option<&str>,
    ) -> CoreResult<()> {
        let review = self.get(review_id)?;
        if review.status != "open" && review.status != "approved" {
            return Err(CoreError::InvalidReviewStatus {
                review_id: review_id.to_string(),
                actual: review.status,
                expected: "open or approved".to_string(),
            });
        }

        let author_str = get_agent_identity(author).map_err(CoreError::Internal)?;

        let event = EventEnvelope::new(
            &author_str,
            Event::ReviewMerged(ReviewMerged {
                review_id: review_id.to_string(),
                final_commit,
            }),
        );

        let log =
            open_or_create_review(self.ctx.seal_root(), review_id).map_err(CoreError::Internal)?;
        log.append(&event).map_err(CoreError::Internal)?;

        Ok(())
    }
}

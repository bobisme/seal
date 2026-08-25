//! Review service — list, get, create, request reviewers, vote, approve, abandon, mark merged.

use crate::events::{
    get_agent_identity, new_review_id, Event, EventEnvelope, ReviewAbandoned, ReviewApproved,
    ReviewCreated, ReviewMerged, ReviewRetargeted, ReviewerVoted, ReviewersRequested, VoteType,
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

/// The stable audit details recorded by a successful review retarget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetargetResult {
    pub review_id: String,
    pub old_scm_anchor: String,
    pub new_scm_anchor: String,
    pub old_target_commit: String,
    pub new_target_commit: String,
    pub approval_reset: bool,
    pub cleared_votes: usize,
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
    /// `base_commit` is the exclusive start of the reviewed range. It is
    /// persisted on the event, so every later read covers the same commits
    /// even after commits are added on top.
    ///
    /// Returns the new review ID.
    pub fn create(
        &self,
        scm: &dyn ScmRepo,
        title: String,
        description: Option<String>,
        reviewers: Option<Vec<String>>,
        author: Option<&str>,
        base_commit: Option<String>,
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
                base_commit,
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

    /// Move an open or approved review to the current SCM target.
    ///
    /// Only the original author may retarget a review. The new target must use
    /// the same backend and retain the review's persisted base. Applying the
    /// event reopens approval and clears effective votes, so the new code must
    /// receive a fresh decision.
    pub fn retarget(
        &self,
        scm: &dyn ScmRepo,
        review_id: &str,
        author: Option<&str>,
    ) -> CoreResult<RetargetResult> {
        let review = self.get(review_id)?;
        if review.status != "open" && review.status != "approved" {
            return Err(CoreError::InvalidReviewStatus {
                review_id: review_id.to_string(),
                actual: review.status,
                expected: "open or approved".to_string(),
            });
        }

        let author_str = get_agent_identity(author).map_err(CoreError::Internal)?;
        if author_str != review.author {
            return Err(CoreError::ReviewAuthorRequired {
                review_id: review_id.to_string(),
                review_author: review.author,
                actor: author_str,
            });
        }

        let scm_kind = scm.kind().as_str();
        if scm_kind != review.scm_kind {
            return Err(CoreError::ReviewScmMismatch {
                review_id: review_id.to_string(),
                actual: scm_kind.to_string(),
                expected: review.scm_kind,
            });
        }

        // Resolve every target before opening the append log. A bad or stale
        // SCM anchor must leave the event stream untouched.
        let old_target_commit = scm.commit_for_anchor(&review.scm_anchor).map_err(|error| {
            CoreError::RetargetAnchorUnresolved {
                review_id: review_id.to_string(),
                target_name: "existing review",
                anchor: review.scm_anchor.clone(),
                reason: error.to_string(),
            }
        })?;
        let new_scm_anchor = scm.current_anchor().map_err(CoreError::Internal)?;
        let new_target_commit = scm.commit_for_anchor(&new_scm_anchor).map_err(|error| {
            CoreError::RetargetAnchorUnresolved {
                review_id: review_id.to_string(),
                target_name: "current workspace",
                anchor: new_scm_anchor.clone(),
                reason: error.to_string(),
            }
        })?;

        if old_target_commit == new_target_commit {
            return Err(CoreError::ReviewTargetUnchanged {
                review_id: review_id.to_string(),
                target_commit: new_target_commit,
            });
        }

        // A persisted base defines the complete review range. Legacy reviews
        // have no base, so require the new target to extend their old target.
        let range_base = review.base_commit.as_deref().unwrap_or(&old_target_commit);
        let base_commit = scm.commit_for_anchor(range_base).map_err(|error| {
            CoreError::RetargetAnchorUnresolved {
                review_id: review_id.to_string(),
                target_name: "review base",
                anchor: range_base.to_string(),
                reason: error.to_string(),
            }
        })?;
        let preserves_range = scm
            .is_ancestor(&base_commit, &new_target_commit)
            .map_err(CoreError::Internal)?;
        if !preserves_range {
            return Err(CoreError::InvalidRetargetRange {
                review_id: review_id.to_string(),
                base_commit,
                target_commit: new_target_commit,
            });
        }

        let result = RetargetResult {
            review_id: review_id.to_string(),
            old_scm_anchor: review.scm_anchor,
            new_scm_anchor,
            old_target_commit,
            new_target_commit,
            approval_reset: review.approved_commit.is_some() || review.status == "approved",
            cleared_votes: review.votes.len(),
        };

        let event = EventEnvelope::new(
            &author_str,
            Event::ReviewRetargeted(ReviewRetargeted {
                review_id: result.review_id.clone(),
                old_scm_anchor: result.old_scm_anchor.clone(),
                new_scm_anchor: result.new_scm_anchor.clone(),
                old_target_commit: result.old_target_commit.clone(),
                new_target_commit: result.new_target_commit.clone(),
            }),
        );

        let log =
            open_or_create_review(self.ctx.seal_root(), review_id).map_err(CoreError::Internal)?;
        log.append(&event).map_err(CoreError::Internal)?;

        Ok(result)
    }

    /// Vote on a review (LGTM or block).
    ///
    /// `target_commit` records the code the vote applied to, so a later
    /// commit can be recognised as unreviewed.
    pub fn vote(
        &self,
        review_id: &str,
        vote: VoteType,
        reason: Option<String>,
        author: Option<&str>,
        target_commit: Option<String>,
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
                target_commit,
            }),
        );

        let log =
            open_or_create_review(self.ctx.seal_root(), review_id).map_err(CoreError::Internal)?;
        log.append(&event).map_err(CoreError::Internal)?;

        Ok(())
    }

    /// Approve a review.
    ///
    /// `target_commit` records the code the approval applied to. Without it,
    /// nothing can prove the approval still covers what is being merged.
    pub fn approve(
        &self,
        review_id: &str,
        author: Option<&str>,
        target_commit: Option<String>,
    ) -> CoreResult<()> {
        let review = self.get(review_id)?;
        // An already-approved review can be approved again to move the
        // approval onto newer code. Without this a review whose approval
        // went stale could never be approved again.
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
            Event::ReviewApproved(ReviewApproved {
                review_id: review_id.to_string(),
                target_commit,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{open_or_create_review, AppendLog};
    use crate::scm::git::GitRepo;
    use std::path::Path;
    use std::process::Command;
    use tempfile::{tempdir, TempDir};

    fn run_git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_head(repo: &Path) -> String {
        let output = Command::new("git")
            .current_dir(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("get HEAD");
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .expect("utf8 HEAD")
            .trim()
            .to_string()
    }

    fn commit_file(repo: &Path, path: &str, body: &str, message: &str) -> String {
        std::fs::write(repo.join(path), body).expect("write committed file");
        run_git(repo, &["add", path]);
        run_git(repo, &["commit", "-m", message]);
        git_head(repo)
    }

    fn init_detached_git_context() -> (TempDir, CoreContext, GitRepo, String) {
        let dir = tempdir().expect("tempdir");
        run_git(dir.path(), &["init"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test User"]);
        let base_commit = commit_file(dir.path(), "base.txt", "base\n", "base");
        run_git(dir.path(), &["checkout", "--detach"]);
        commit_file(dir.path(), "feature.txt", "feature\n", "feature");

        let seal_dir = dir.path().join(".seal");
        std::fs::create_dir(&seal_dir).expect("create .seal");
        std::fs::write(seal_dir.join("version"), "2\n").expect("write version");
        std::fs::create_dir(seal_dir.join("reviews")).expect("create reviews dir");

        let ctx = CoreContext::new(dir.path(), &seal_dir.join("index.db")).expect("context");
        let scm = GitRepo::new(dir.path().to_path_buf());
        (dir, ctx, scm, base_commit)
    }

    fn create_review(ctx: &CoreContext, scm: &GitRepo, base_commit: String) -> String {
        ctx.services()
            .expect("services")
            .reviews()
            .create(
                scm,
                "Review".to_string(),
                None,
                Some(vec!["security".to_string()]),
                Some("alice"),
                Some(base_commit),
            )
            .expect("create review")
    }

    #[test]
    fn retarget_git_descendant_reopens_and_rebuilds_with_fresh_votes() {
        let (dir, ctx, scm, base_commit) = init_detached_git_context();
        let review_id = create_review(&ctx, &scm, base_commit);
        let old_target = scm.current_commit().expect("old target");

        ctx.services()
            .expect("services")
            .reviews()
            .vote(
                &review_id,
                VoteType::Lgtm,
                None,
                Some("security"),
                Some(old_target.clone()),
            )
            .expect("vote");
        ctx.services()
            .expect("services")
            .reviews()
            .approve(&review_id, Some("security"), Some(old_target.clone()))
            .expect("approve");

        let new_target = commit_file(dir.path(), "fix.txt", "fixed\n", "fix");
        let result = ctx
            .services()
            .expect("services")
            .reviews()
            .retarget(&scm, &review_id, Some("alice"))
            .expect("retarget");

        assert_eq!(result.old_target_commit, old_target);
        assert_eq!(result.new_target_commit, new_target);
        assert!(result.approval_reset);
        assert_eq!(result.cleared_votes, 1);

        let projected = ctx
            .services()
            .expect("services")
            .reviews()
            .get(&review_id)
            .expect("projected review");
        assert_eq!(projected.status, "open");
        assert_eq!(projected.approved_commit, None);
        assert!(projected.votes.is_empty());
        assert_eq!(
            scm.commit_for_anchor(&projected.scm_anchor).unwrap(),
            new_target
        );

        let events = open_or_create_review(ctx.seal_root(), &review_id)
            .expect("review log")
            .read_all()
            .expect("events");
        assert!(matches!(
            events.last().map(|event| &event.event),
            Some(Event::ReviewRetargeted(ReviewRetargeted { new_target_commit, .. }))
                if new_target_commit == &result.new_target_commit
        ));

        // Removing the cache forces a complete replay from the append-only log.
        std::fs::remove_file(ctx.db_path()).expect("remove projection");
        let rebuilt = ctx
            .services()
            .expect("rebuilt services")
            .reviews()
            .get(&review_id)
            .expect("rebuilt review");
        assert_eq!(rebuilt.scm_anchor, result.new_scm_anchor);
        assert_eq!(rebuilt.status, "open");
        assert_eq!(rebuilt.approved_commit, None);
        assert!(rebuilt.votes.is_empty());

        ctx.services()
            .expect("services")
            .reviews()
            .vote(
                &review_id,
                VoteType::Lgtm,
                None,
                Some("security"),
                Some(new_target.clone()),
            )
            .expect("fresh vote");
        ctx.services()
            .expect("services")
            .reviews()
            .approve(&review_id, Some("security"), Some(new_target.clone()))
            .expect("fresh approval");

        let approved = ctx
            .services()
            .expect("services")
            .reviews()
            .get(&review_id)
            .expect("approved review");
        assert_eq!(approved.status, "approved");
        assert_eq!(
            approved.approved_commit.as_deref(),
            Some(new_target.as_str())
        );
        assert_eq!(approved.votes.len(), 1);
    }

    #[test]
    fn retarget_rejects_wrong_author_and_unchanged_target_without_append() {
        let (_dir, ctx, scm, base_commit) = init_detached_git_context();
        let review_id = create_review(&ctx, &scm, base_commit);
        let log = open_or_create_review(ctx.seal_root(), &review_id).expect("review log");
        let original_count = log.read_all().expect("events").len();

        let wrong_author = ctx
            .services()
            .expect("services")
            .reviews()
            .retarget(&scm, &review_id, Some("bob"))
            .expect_err("wrong author");
        assert!(matches!(
            wrong_author,
            CoreError::ReviewAuthorRequired { .. }
        ));

        let unchanged = ctx
            .services()
            .expect("services")
            .reviews()
            .retarget(&scm, &review_id, Some("alice"))
            .expect_err("unchanged target");
        assert!(matches!(unchanged, CoreError::ReviewTargetUnchanged { .. }));
        assert_eq!(log.read_all().expect("events").len(), original_count);
    }

    #[test]
    fn retarget_rejects_completed_review_and_unresolved_old_anchor() {
        let (_dir, ctx, scm, base_commit) = init_detached_git_context();
        let review_id = create_review(&ctx, &scm, base_commit.clone());
        ctx.services()
            .expect("services")
            .reviews()
            .mark_merged(&review_id, git_head(scm.root()), Some("alice"))
            .expect("merge review");

        let completed = ctx
            .services()
            .expect("services")
            .reviews()
            .retarget(&scm, &review_id, Some("alice"))
            .expect_err("completed review");
        assert!(matches!(completed, CoreError::InvalidReviewStatus { .. }));

        let unresolved_id = "cr-bad1";
        let unresolved = EventEnvelope::new(
            "alice",
            Event::ReviewCreated(ReviewCreated {
                review_id: unresolved_id.to_string(),
                jj_change_id: "detached:missing".to_string(),
                scm_kind: Some("git".to_string()),
                scm_anchor: Some("detached:missing".to_string()),
                initial_commit: "missing".to_string(),
                base_commit: Some(base_commit),
                title: "Unresolved".to_string(),
                description: None,
            }),
        );
        let unresolved_log =
            open_or_create_review(ctx.seal_root(), unresolved_id).expect("unresolved review log");
        unresolved_log
            .append(&unresolved)
            .expect("append unresolved");

        let unresolved_error = ctx
            .services()
            .expect("services")
            .reviews()
            .retarget(&scm, unresolved_id, Some("alice"))
            .expect_err("unresolved old target");
        assert!(matches!(
            unresolved_error,
            CoreError::RetargetAnchorUnresolved { .. }
        ));
        assert_eq!(unresolved_log.read_all().expect("events").len(), 1);
    }
}

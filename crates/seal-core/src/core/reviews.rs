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
    /// The persisted range base before the retarget (`None` for legacy
    /// reviews created before bases were recorded).
    pub old_base_commit: Option<String>,
    /// The range base after the retarget. Equals `old_base_commit` unless
    /// `base_changed`.
    pub new_base_commit: Option<String>,
    /// True when the retarget moved the range base (a rebase onto newer
    /// trunk, or an explicit `--base`). Only then does the event record it.
    pub base_changed: bool,
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
    /// the same backend and descend from the chosen range base. Applying the
    /// event reopens approval and clears effective votes, so the new code must
    /// receive a fresh decision.
    ///
    /// The range base is chosen as follows:
    /// - `explicit_base`, when given, wins.
    /// - Otherwise, when the review has a persisted base, the base is
    ///   re-derived with `discover_base_commit(new_target)`. A discovered base
    ///   that strictly descends from the persisted one means trunk moved
    ///   forward under the branch (a rebase or a trunk merge), so the base
    ///   moves to it and the range again holds only the branch's own commits.
    ///   Any other outcome (nothing discovered, same base, an older or
    ///   unrelated base) keeps the persisted base.
    /// - Legacy reviews without a base keep none.
    pub fn retarget(
        &self,
        scm: &dyn ScmRepo,
        review_id: &str,
        author: Option<&str>,
        explicit_base: Option<&str>,
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

        // A persisted base defines the complete review range. Legacy reviews
        // have no base, so require the new target to extend their old target.
        let persisted_base = match review.base_commit.as_deref() {
            Some(base) => Some(scm.commit_for_anchor(base).map_err(|error| {
                CoreError::RetargetAnchorUnresolved {
                    review_id: review_id.to_string(),
                    target_name: "review base",
                    anchor: base.to_string(),
                    reason: error.to_string(),
                }
            })?),
            None => None,
        };
        let chosen_base = match explicit_base {
            Some(rev) => Some(scm.commit_for_anchor(rev).map_err(|error| {
                CoreError::RetargetBaseUnresolved {
                    review_id: review_id.to_string(),
                    base: rev.to_string(),
                    reason: error.to_string(),
                }
            })?),
            None => match persisted_base.as_deref() {
                Some(persisted) => Some(rederive_base(scm, persisted, &new_target_commit)?),
                None => None,
            },
        };
        let base_changed = chosen_base.is_some() && chosen_base != persisted_base;

        // A branch anchor follows a rebase by itself, so its target may not
        // move while its base must. Only a retarget that changes neither is
        // refused.
        if old_target_commit == new_target_commit && !base_changed {
            return Err(CoreError::ReviewTargetUnchanged {
                review_id: review_id.to_string(),
                target_commit: new_target_commit,
            });
        }

        let base_commit = chosen_base
            .clone()
            .unwrap_or_else(|| old_target_commit.clone());
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
            new_base_commit: if base_changed {
                chosen_base
            } else {
                review.base_commit.clone()
            },
            old_base_commit: review.base_commit,
            base_changed,
        };

        let event = EventEnvelope::new(
            &author_str,
            Event::ReviewRetargeted(ReviewRetargeted {
                review_id: result.review_id.clone(),
                old_scm_anchor: result.old_scm_anchor.clone(),
                new_scm_anchor: result.new_scm_anchor.clone(),
                old_target_commit: result.old_target_commit.clone(),
                new_target_commit: result.new_target_commit.clone(),
                old_base_commit: if result.base_changed {
                    result.old_base_commit.clone()
                } else {
                    None
                },
                new_base_commit: if result.base_changed {
                    result.new_base_commit.clone()
                } else {
                    None
                },
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

/// Re-derive a review's range base for `target`.
///
/// Returns the discovered base when it strictly descends from `persisted`:
/// trunk moved forward under the branch, so the persisted base would pull the
/// trunk commits in between into the range. Otherwise returns `persisted`.
fn rederive_base(scm: &dyn ScmRepo, persisted: &str, target: &str) -> CoreResult<String> {
    let Ok(Some(discovered)) = scm.discover_base_commit(target) else {
        return Ok(persisted.to_string());
    };
    if discovered != persisted
        && scm
            .is_ancestor(persisted, &discovered)
            .map_err(CoreError::Internal)?
    {
        return Ok(discovered);
    }
    Ok(persisted.to_string())
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
            .retarget(&scm, &review_id, Some("alice"), None)
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
            .retarget(&scm, &review_id, Some("bob"), None)
            .expect_err("wrong author");
        assert!(matches!(
            wrong_author,
            CoreError::ReviewAuthorRequired { .. }
        ));

        let unchanged = ctx
            .services()
            .expect("services")
            .reviews()
            .retarget(&scm, &review_id, Some("alice"), None)
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
            .retarget(&scm, &review_id, Some("alice"), None)
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
            .retarget(&scm, unresolved_id, Some("alice"), None)
            .expect_err("unresolved old target");
        assert!(matches!(
            unresolved_error,
            CoreError::RetargetAnchorUnresolved { .. }
        ));
        assert_eq!(unresolved_log.read_all().expect("events").len(), 1);
    }

    // ------------------------------------------------------------------
    // bn-jnp9: retarget after a rebase onto newer trunk
    // ------------------------------------------------------------------

    fn git_out(repo: &Path, args: &[&str]) -> String {
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
        String::from_utf8(output.stdout)
            .expect("utf8")
            .trim()
            .to_string()
    }

    /// Subjects of the commits in `base..target`, oldest first.
    fn subjects(repo: &Path, base: &str, target: &str) -> Vec<String> {
        let range = format!("{base}..{target}");
        git_out(repo, &["log", "--reverse", "--format=%s", &range])
            .lines()
            .map(ToString::to_string)
            .collect()
    }

    /// A maw-like workspace: `main` holds trunk, HEAD is detached on the
    /// workspace's own commits. Nothing in `.seal/` is tracked.
    struct Workspace {
        dir: TempDir,
        ctx: CoreContext,
        scm: GitRepo,
        trunk_commits: usize,
        branch_commits: usize,
    }

    impl Workspace {
        fn new() -> Self {
            let dir = tempdir().expect("tempdir");
            let root = dir.path();
            run_git(root, &["init", "-q"]);
            run_git(root, &["config", "user.email", "test@example.com"]);
            run_git(root, &["config", "user.name", "Test User"]);
            std::fs::write(root.join(".git/info/exclude"), ".seal/\n").expect("exclude");
            commit_file(root, "trunk-0.txt", "0\n", "trunk-0");
            run_git(root, &["branch", "-M", "main"]);
            run_git(root, &["checkout", "-q", "--detach"]);

            let seal_dir = root.join(".seal");
            std::fs::create_dir(&seal_dir).expect("create .seal");
            std::fs::write(seal_dir.join("version"), "2\n").expect("write version");
            std::fs::create_dir(seal_dir.join("reviews")).expect("create reviews dir");
            let ctx = CoreContext::new(root, &seal_dir.join("index.db")).expect("context");
            let scm = GitRepo::new(root.to_path_buf());
            Self {
                dir,
                ctx,
                scm,
                trunk_commits: 1,
                branch_commits: 0,
            }
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        /// Land a commit on `main` without moving the workspace.
        fn trunk_commit(&mut self) {
            let head = git_head(self.root());
            let name = format!("trunk-{}", self.trunk_commits);
            self.trunk_commits += 1;
            run_git(self.root(), &["checkout", "-q", "main"]);
            commit_file(self.root(), &format!("{name}.txt"), "t\n", &name);
            run_git(self.root(), &["checkout", "-q", "--detach", &head]);
        }

        fn branch_commit(&mut self) {
            let name = format!("branch-{}", self.branch_commits);
            self.branch_commits += 1;
            commit_file(self.root(), &format!("{name}.txt"), "b\n", &name);
        }

        /// What `maw ws sync` does: replay the workspace commits onto trunk.
        fn rebase_onto_trunk(&self) {
            run_git(self.root(), &["rebase", "-q", "main"]);
        }

        fn branch_subjects(&self) -> Vec<String> {
            (0..self.branch_commits)
                .map(|i| format!("branch-{i}"))
                .collect()
        }

        fn create_review(&self) -> String {
            let target = self.scm.current_commit().expect("target");
            let base =
                crate::range::resolve_new_review_base(&self.scm, &target, None).expect("base");
            create_review(&self.ctx, &self.scm, base)
        }

        fn retarget(&self, id: &str, base: Option<&str>) -> CoreResult<RetargetResult> {
            self.ctx.services().expect("services").reviews().retarget(
                &self.scm,
                id,
                Some("alice"),
                base,
            )
        }

        fn review(&self, id: &str) -> ReviewDetail {
            self.ctx
                .services()
                .expect("services")
                .reviews()
                .get(id)
                .expect("review")
        }

        fn events(&self, id: &str) -> Vec<EventEnvelope> {
            open_or_create_review(self.ctx.seal_root(), id)
                .expect("log")
                .read_all()
                .expect("events")
        }

        /// The range readers show for the review now.
        fn range(&self, id: &str) -> crate::range::ReviewRange {
            crate::range::resolve_review_range(&self.scm, &self.review(id))
        }
    }

    fn last_retarget(events: &[EventEnvelope]) -> ReviewRetargeted {
        match events.last().map(|event| &event.event) {
            Some(Event::ReviewRetargeted(retargeted)) => retargeted.clone(),
            other => panic!("expected ReviewRetargeted, got {other:?}"),
        }
    }

    /// Acceptance for bn-jnp9: rebase onto a newer trunk, retarget, and the
    /// range is again exactly the workspace's own commits.
    #[test]
    fn retarget_after_rebase_moves_base_to_new_fork_point() {
        let mut ws = Workspace::new();
        for _ in 0..3 {
            ws.branch_commit();
        }
        let id = ws.create_review();
        let old_base = ws.review(&id).base_commit.expect("persisted base");
        let old_target = ws.scm.current_commit().expect("old target");
        assert_eq!(
            ws.scm
                .count_commits_between(&old_base, &old_target)
                .unwrap(),
            Some(3)
        );
        ws.ctx
            .services()
            .expect("services")
            .reviews()
            .vote(
                &id,
                VoteType::Lgtm,
                None,
                Some("security"),
                Some(old_target),
            )
            .expect("vote");

        ws.trunk_commit();
        ws.trunk_commit();
        ws.rebase_onto_trunk();
        let new_target = ws.scm.current_commit().expect("new target");
        let trunk = ws.scm.commit_for_anchor("refs/heads/main").expect("main");
        // The bug: the old base now pulls both trunk commits into the range.
        assert_eq!(
            ws.scm
                .count_commits_between(&old_base, &new_target)
                .unwrap(),
            Some(5)
        );

        let result = ws.retarget(&id, None).expect("retarget");
        assert!(result.base_changed);
        assert_eq!(result.old_base_commit.as_deref(), Some(old_base.as_str()));
        assert_eq!(result.new_base_commit.as_deref(), Some(trunk.as_str()));
        assert_eq!(result.cleared_votes, 1);

        let review = ws.review(&id);
        assert_eq!(review.base_commit.as_deref(), Some(trunk.as_str()));
        assert!(review.votes.is_empty());
        let range = ws.range(&id);
        assert_eq!(range.target, new_target);
        assert_eq!(
            ws.scm
                .count_commits_between(&range.base, &range.target)
                .unwrap(),
            Some(3)
        );
        assert_eq!(
            subjects(ws.root(), &range.base, &range.target),
            ws.branch_subjects()
        );

        let retargeted = last_retarget(&ws.events(&id));
        assert_eq!(
            retargeted.old_base_commit.as_deref(),
            Some(old_base.as_str())
        );
        assert_eq!(retargeted.new_base_commit.as_deref(), Some(trunk.as_str()));

        // A full replay of the log gives the same base.
        std::fs::remove_file(ws.ctx.db_path()).expect("remove projection");
        assert_eq!(ws.review(&id).base_commit.as_deref(), Some(trunk.as_str()));
    }

    #[test]
    fn retarget_without_rebase_keeps_base_and_records_no_base_change() {
        let mut ws = Workspace::new();
        ws.branch_commit();
        let id = ws.create_review();
        let base = ws.review(&id).base_commit.expect("base");
        // Trunk moves, but the workspace is not rebased: the fork point stays.
        ws.trunk_commit();
        ws.branch_commit();

        let result = ws.retarget(&id, None).expect("retarget");
        assert!(!result.base_changed);
        assert_eq!(result.new_base_commit.as_deref(), Some(base.as_str()));
        let retargeted = last_retarget(&ws.events(&id));
        assert_eq!(retargeted.old_base_commit, None);
        assert_eq!(retargeted.new_base_commit, None);
        assert_eq!(ws.review(&id).base_commit.as_deref(), Some(base.as_str()));
        let range = ws.range(&id);
        assert_eq!(
            subjects(ws.root(), &range.base, &range.target),
            ws.branch_subjects()
        );
    }

    /// A persisted base nearer the target than the discovered one (an author
    /// who reviewed only the top of a stack) is not widened by retarget.
    #[test]
    fn retarget_keeps_a_base_that_descends_from_the_discovered_one() {
        let mut ws = Workspace::new();
        ws.branch_commit();
        let near_base = ws.scm.current_commit().expect("near base");
        ws.branch_commit();
        let id = create_review(&ws.ctx, &ws.scm, near_base.clone());
        ws.branch_commit();
        // Nor does `seal diff` warn about it: only a base trunk has moved
        // past is stale.
        let range = ws.range(&id);
        assert_eq!(
            crate::range::detect_stale_base(&ws.scm, &ws.review(&id), &range),
            None
        );

        let result = ws.retarget(&id, None).expect("retarget");
        assert!(!result.base_changed);
        assert_eq!(
            ws.review(&id).base_commit.as_deref(),
            Some(near_base.as_str())
        );
    }

    #[test]
    fn retarget_explicit_base_overrides_discovery_and_bad_base_appends_nothing() {
        let mut ws = Workspace::new();
        ws.branch_commit();
        ws.branch_commit();
        let id = ws.create_review();
        let first = ws.scm.commit_for_anchor("HEAD~1").expect("first");
        ws.branch_commit();

        let before = ws.events(&id).len();
        let err = ws
            .retarget(&id, Some("no-such-rev"))
            .expect_err("bad --base");
        assert!(
            matches!(err, CoreError::RetargetBaseUnresolved { .. }),
            "{err}"
        );
        assert_eq!(ws.events(&id).len(), before);

        // A base that is not an ancestor of the target fails the invariant.
        run_git(ws.root(), &["branch", "side", "main"]);
        let side_head = git_head(ws.root());
        run_git(ws.root(), &["checkout", "-q", "side"]);
        commit_file(ws.root(), "side.txt", "s\n", "side");
        run_git(ws.root(), &["checkout", "-q", "--detach", &side_head]);
        let err = ws
            .retarget(&id, Some("refs/heads/side"))
            .expect_err("unrelated --base");
        assert!(
            matches!(err, CoreError::InvalidRetargetRange { .. }),
            "{err}"
        );
        assert_eq!(ws.events(&id).len(), before);

        let result = ws.retarget(&id, Some(&first)).expect("retarget");
        assert!(result.base_changed);
        assert_eq!(result.new_base_commit.as_deref(), Some(first.as_str()));
        assert_eq!(ws.review(&id).base_commit.as_deref(), Some(first.as_str()));
        let range = ws.range(&id);
        assert_eq!(
            subjects(ws.root(), &range.base, &range.target),
            vec!["branch-1".to_string(), "branch-2".to_string()]
        );
    }

    /// A review anchored on a branch follows the rebase by itself, so its
    /// target does not move. `seal diff` warns, and retarget fixes only the
    /// base.
    #[test]
    fn branch_review_after_rebase_warns_and_retarget_moves_only_the_base() {
        let mut ws = Workspace::new();
        run_git(ws.root(), &["checkout", "-q", "-b", "feature"]);
        for _ in 0..2 {
            ws.branch_commit();
        }
        let id = ws.create_review();
        let range = ws.range(&id);
        assert_eq!(
            crate::range::detect_stale_base(&ws.scm, &ws.review(&id), &range),
            None
        );

        // Advance main without leaving the feature branch.
        run_git(ws.root(), &["checkout", "-q", "main"]);
        commit_file(ws.root(), "trunk-1.txt", "t\n", "trunk-1");
        run_git(ws.root(), &["checkout", "-q", "feature"]);
        ws.rebase_onto_trunk();
        let trunk = ws.scm.commit_for_anchor("refs/heads/main").expect("main");

        let range = ws.range(&id);
        let stale = crate::range::detect_stale_base(&ws.scm, &ws.review(&id), &range)
            .expect("stale base warning");
        assert_eq!(stale.discovered, trunk);
        assert!(stale
            .warning(&id)
            .contains(&format!("seal reviews retarget {id}")));
        assert_eq!(subjects(ws.root(), &range.base, &range.target).len(), 3);

        let result = ws.retarget(&id, None).expect("retarget");
        assert_eq!(result.old_target_commit, result.new_target_commit);
        assert!(result.base_changed);
        let range = ws.range(&id);
        assert_eq!(
            subjects(ws.root(), &range.base, &range.target),
            ws.branch_subjects()
        );
        assert_eq!(
            crate::range::detect_stale_base(&ws.scm, &ws.review(&id), &range),
            None
        );

        // Nothing moved now: retarget is refused and appends nothing.
        let before = ws.events(&id).len();
        let err = ws.retarget(&id, None).expect_err("unchanged");
        assert!(matches!(err, CoreError::ReviewTargetUnchanged { .. }));
        assert_eq!(ws.events(&id).len(), before);
    }

    /// `git push -u origin feature` makes `origin/feature` the branch's
    /// upstream. Its merge base with the target is the pushed tip, so treating
    /// it as trunk would move the base forward and silently drop the pushed
    /// feature commits from the review.
    #[test]
    fn pushed_feature_upstream_is_not_trunk_for_create_or_retarget() {
        let mut ws = Workspace::new();
        let origin = tempdir().expect("origin dir");
        run_git(origin.path(), &["init", "-q", "--bare"]);
        let origin_path = origin.path().to_str().expect("utf8 path");
        run_git(ws.root(), &["remote", "add", "origin", origin_path]);
        run_git(ws.root(), &["push", "-q", "origin", "main"]);
        run_git(ws.root(), &["checkout", "-q", "-b", "feature"]);
        for _ in 0..3 {
            ws.branch_commit();
        }
        let main = ws.scm.commit_for_anchor("refs/heads/main").expect("main");
        let id = ws.create_review();
        assert_eq!(ws.review(&id).base_commit.as_deref(), Some(main.as_str()));

        run_git(ws.root(), &["push", "-q", "-u", "origin", "feature"]);
        assert_eq!(
            git_out(ws.root(), &["rev-parse", "--abbrev-ref", "@{upstream}"]),
            "origin/feature"
        );
        ws.branch_commit();

        // Discovery still finds the fork from main, not the pushed tip.
        let target = ws.scm.current_commit().expect("target");
        assert_eq!(
            ws.scm.discover_base_commit(&target).expect("discover"),
            Some(main.clone())
        );
        // A new review on the pushed branch covers every feature commit.
        let fresh = ws.create_review();
        let range = ws.range(&fresh);
        assert_eq!(range.base, main);
        assert_eq!(subjects(ws.root(), &range.base, &range.target).len(), 4);

        // The branch anchor already follows the new commit and the base must
        // not move, so there is nothing to retarget and nothing is appended.
        let before = ws.events(&id).len();
        let err = ws.retarget(&id, None).expect_err("nothing to retarget");
        assert!(
            matches!(err, CoreError::ReviewTargetUnchanged { .. }),
            "{err}"
        );
        assert_eq!(ws.events(&id).len(), before);
        let range = ws.range(&id);
        assert_eq!(range.base, main);
        assert_eq!(
            ws.scm
                .count_commits_between(&range.base, &range.target)
                .unwrap(),
            Some(4)
        );
        assert_eq!(
            subjects(ws.root(), &range.base, &range.target),
            ws.branch_subjects()
        );
        assert_eq!(
            crate::range::detect_stale_base(&ws.scm, &ws.review(&id), &range),
            None
        );
    }

    /// An upstream that is another branch (feature created with
    /// `--track origin/main`) still marks trunk.
    #[test]
    fn upstream_on_another_branch_still_marks_trunk() {
        let mut ws = Workspace::new();
        let remote = tempdir().expect("remote dir");
        run_git(remote.path(), &["init", "-q", "--bare"]);
        let remote_path = remote.path().to_str().expect("utf8 path");
        // A remote not named origin, and no local main: only the upstream is
        // left to mark trunk, since no fixed candidate ref resolves.
        run_git(ws.root(), &["remote", "add", "up", remote_path]);
        ws.trunk_commit();
        run_git(ws.root(), &["push", "-q", "up", "main"]);
        let pushed = ws.scm.commit_for_anchor("refs/heads/main").expect("main");
        run_git(
            ws.root(),
            &["checkout", "-q", "--track", "-b", "feature", "up/main"],
        );
        run_git(ws.root(), &["branch", "-q", "-D", "main"]);
        ws.branch_commit();
        let target = ws.scm.current_commit().expect("target");
        assert_eq!(
            ws.scm.discover_base_commit(&target).expect("discover"),
            Some(pushed)
        );
    }

    #[derive(Debug, Clone, Copy)]
    enum WsOp {
        TrunkCommit,
        BranchCommit,
        Rebase,
        Retarget,
    }

    fn ws_op() -> impl proptest::strategy::Strategy<Value = WsOp> {
        use proptest::prelude::*;
        prop_oneof![
            Just(WsOp::TrunkCommit),
            Just(WsOp::BranchCommit),
            Just(WsOp::Rebase),
            Just(WsOp::Retarget),
        ]
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 24,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Model-based check over a real git repo: after every retarget the
        /// review range is exactly the workspace's own commits, whatever
        /// sequence of trunk commits, workspace commits and rebases came first.
        #[test]
        fn retarget_range_is_always_the_workspace_commits(
            ops in proptest::collection::vec(ws_op(), 1..10)
        ) {
            let mut ws = Workspace::new();
            ws.branch_commit();
            let id = ws.create_review();
            for op in ops {
                match op {
                    WsOp::TrunkCommit => ws.trunk_commit(),
                    WsOp::BranchCommit => ws.branch_commit(),
                    WsOp::Rebase => ws.rebase_onto_trunk(),
                    WsOp::Retarget => {
                        match ws.retarget(&id, None) {
                            Ok(_) | Err(CoreError::ReviewTargetUnchanged { .. }) => {}
                            Err(other) => proptest::prop_assert!(false, "retarget failed: {other}"),
                        }
                        let range = ws.range(&id);
                        proptest::prop_assert_eq!(&range.target, &git_head(ws.root()));
                        proptest::prop_assert_eq!(
                            subjects(ws.root(), &range.base, &range.target),
                            ws.branch_subjects()
                        );
                        proptest::prop_assert!(ws.review(&id).votes.is_empty());
                    }
                }
            }
        }
    }
}

//! Resolution of the commit range a review covers.
//!
//! A review covers `base..target`. Before ranges were supported the base was
//! always the parent of the target, so a review could only ever cover one
//! commit. A feature branch with several commits was reviewed from its tip
//! parent, which silently dropped every earlier commit.
//!
//! Reviews now persist an explicit base. This module resolves the range for
//! both new and old reviews, so every reader shows the same commits.

use crate::approval_exempt::range_is_exempt;
use crate::projection::ReviewDetail;
use crate::scm::ScmRepo;

/// The commit range a review covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRange {
    /// Exclusive start of the range.
    pub base: String,
    /// Inclusive end of the range.
    pub target: String,
    /// True when the base came from the review's persisted `base_commit`.
    ///
    /// False means the base fell back to the parent of the target, which
    /// happens for reviews created before ranges were supported and for
    /// reviews whose persisted base no longer resolves.
    pub base_is_persisted: bool,
}

/// Resolve the current target commit of a review.
///
/// Priority order:
/// 1. `final_commit`, set when the review merged
/// 2. the commit for `scm_anchor`
/// 3. the commit for the legacy `jj_change_id`
/// 4. `initial_commit`
#[must_use]
pub fn resolve_target_commit(scm: &dyn ScmRepo, review: &ReviewDetail) -> String {
    review
        .final_commit
        .clone()
        .or_else(|| scm.commit_for_anchor(&review.scm_anchor).ok())
        .or_else(|| scm.commit_for_anchor(&review.jj_change_id).ok())
        .unwrap_or_else(|| review.initial_commit.clone())
}

/// Resolve the full commit range a review covers.
///
/// Uses the review's persisted base when it still resolves in the repository.
/// A rewrite can strand a persisted base, so an unresolvable base falls back
/// to the parent of the target rather than failing the read.
#[must_use]
pub fn resolve_review_range(scm: &dyn ScmRepo, review: &ReviewDetail) -> ReviewRange {
    let target = resolve_target_commit(scm, review);

    if let Some(base) = &review.base_commit {
        if let Ok(resolved) = scm.commit_for_anchor(base) {
            return ReviewRange {
                base: resolved,
                target,
                base_is_persisted: true,
            };
        }
    }

    let base = scm
        .parent_commit(&target)
        .unwrap_or_else(|_| review.initial_commit.clone());

    ReviewRange {
        base,
        target,
        base_is_persisted: false,
    }
}

/// Resolve the base commit to record on a new review.
///
/// An explicit revision wins. Otherwise the backend discovers the fork point
/// of the feature branch or workspace. When neither gives a base, the caller
/// gets the parent of the target, which covers one commit.
///
/// # Errors
///
/// Returns an error if `explicit` does not resolve to a commit.
pub fn resolve_new_review_base(
    scm: &dyn ScmRepo,
    target: &str,
    explicit: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(rev) = explicit {
        return scm.commit_for_anchor(rev).map_err(|e| {
            anyhow::anyhow!(
                "Failed to resolve --base {rev}: {e}\n  To fix: pass a revision this repository knows, such as a commit hash, a branch name, or HEAD~1"
            )
        });
    }

    if let Ok(Some(discovered)) = scm.discover_base_commit(target) {
        return Ok(discovered);
    }

    scm.parent_commit(target)
}

/// A persisted review base that trunk has moved past.
///
/// After the branch is rebased onto newer trunk (or merges trunk), the fork
/// point descends from the persisted base, so `base..target` also holds the
/// trunk commits in between. `seal reviews retarget` moves the base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleBase {
    /// The persisted base the range uses now.
    pub persisted: String,
    /// The fork point discovered for the current target.
    pub discovered: String,
}

impl StaleBase {
    /// One-line warning with the command that fixes the range.
    #[must_use]
    pub fn warning(&self, review_id: &str) -> String {
        format!(
            "review base {} is behind the branch fork point {}; the range includes trunk commits. To fix: seal reviews retarget {review_id}",
            self.persisted, self.discovered
        )
    }
}

/// Detect a persisted base that trunk has moved past.
///
/// Returns `Some` only for an open or approved review whose range uses its
/// persisted base, when the base discovered for the current target differs
/// from the persisted base and descends from it. Any failure to discover or
/// compare returns `None`: this is a warning, never a read failure.
#[must_use]
pub fn detect_stale_base(
    scm: &dyn ScmRepo,
    review: &ReviewDetail,
    range: &ReviewRange,
) -> Option<StaleBase> {
    if !range.base_is_persisted || !matches!(review.status.as_str(), "open" | "approved") {
        return None;
    }
    let discovered = scm.discover_base_commit(&range.target).ok()??;
    if discovered == range.base || !scm.is_ancestor(&range.base, &discovered).ok()? {
        return None;
    }
    Some(StaleBase {
        persisted: range.base.clone(),
        discovered,
    })
}

/// Why an approval no longer covers the code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaleReason {
    /// The approval names no commit, so nothing can be checked against it.
    /// Applies to reviews approved before approvals recorded a commit.
    NoApprovedCommit,
    /// The target differs from the approved commit in something other than
    /// review data under `.seal/`: commits landed on top of it, it was
    /// rewritten, or it cannot be compared (unrelated or unresolvable).
    CommitsAdded,
}

/// How an approval relates to the code the review now points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalCoverage {
    /// The commit the approval applied to, when it recorded one.
    pub approved_commit: Option<String>,
    /// The commit the review points at now.
    pub current_target: String,
    /// Commits in `approved_commit..current_target` that the approval does
    /// not cover.
    ///
    /// Exemption is judged on the net tree change of the whole range (see
    /// [`crate::approval_exempt`]), not commit by commit. So this is `0` when
    /// the range only records review activity under `.seal/`, and the full
    /// commit count of the range when it changes anything else. When the
    /// approved commit is not an ancestor of the target (a rewrite or an
    /// unrelated history), this is `0` if the trees differ only in exempt
    /// ways and `None` otherwise. Also `None` when the commits cannot be
    /// counted or the approval records no commit.
    pub uncovered_commits: Option<usize>,
    /// Commits in `approved_commit..current_target` that were ignored because
    /// the range only records review activity under `.seal/`.
    ///
    /// `Some(0)` when there is nothing to ignore or the approval is stale on
    /// a descendant target. `None` when the range has no meaningful count:
    /// the approved commit is not an ancestor of the target (fresh or stale),
    /// or the approval records no commit.
    pub exempt_commits: Option<usize>,
    /// Set when the approval does not cover `current_target`.
    pub stale_reason: Option<StaleReason>,
}

impl ApprovalCoverage {
    /// True when the approval does not cover the current target.
    #[must_use]
    pub const fn is_stale(&self) -> bool {
        self.stale_reason.is_some()
    }
}

/// Check whether a review's approval still covers the code it points at.
///
/// Returns `None` for reviews that are not approved.
///
/// An approval is stale when commits landed after it. Merging then ships
/// code that no reviewer saw. An approval that recorded no commit is also
/// stale, because nothing proves it covers the current target.
///
/// Commits that only record review activity under `.seal/` do not make it
/// stale; [`crate::approval_exempt`] states the exact rule. Without this,
/// committing the review log after the LGTM would always stale the LGTM.
///
/// When the target is not a descendant of the approved commit (a rewrite of
/// the approved commit, or an unrelated branch), the same tree comparison
/// decides. A rewrite that only adds review activity under `.seal/` stays
/// fresh; anything else, including an approved commit that no longer
/// resolves, is stale. No commit count is reported for such a pair, since
/// the distance between unrelated commits means nothing.
///
/// The approval is checked against [`effective_approval_target`], not only
/// the anchored review target. Use [`approval_coverage_at`] to check it
/// against one exact commit, such as the commit a merge records.
#[must_use]
pub fn approval_coverage(scm: &dyn ScmRepo, review: &ReviewDetail) -> Option<ApprovalCoverage> {
    if review.status != "approved" {
        return None;
    }
    let target = effective_approval_target(scm, review);
    approval_coverage_at(scm, review, &target)
}

/// The commit an approval must cover when seen from the current workspace.
///
/// This is the workspace's current commit when it equals or descends from
/// the anchored review target ([`resolve_target_commit`]). Otherwise, and for
/// a merged review, it is the anchored target.
///
/// Why: the workspace is where the next merge comes from. A detached HEAD
/// (a git worktree, such as a maw workspace) anchors the review to a fixed
/// commit, and a jj change anchor stays behind after `jj new`. New commits
/// then land on top of the anchor without moving it, and a check against the
/// anchor alone reports an approval as fresh while unreviewed code sits in
/// the workspace, ready to merge (bn-2ypz).
///
/// A checkout that does not descend from the anchor (trunk before the merge,
/// another reviewer's workspace) cannot merge that code, so the anchor
/// decides. Any failure to read or compare the current commit also falls
/// back to the anchor, which is the behaviour before this rule.
///
/// Only the approval check uses this target. The review range, the diff and
/// thread locations still use the anchored target.
#[must_use]
pub fn effective_approval_target(scm: &dyn ScmRepo, review: &ReviewDetail) -> String {
    let anchored = resolve_target_commit(scm, review);
    if review.final_commit.is_some() {
        return anchored;
    }
    let Ok(head) = scm.current_commit() else {
        return anchored;
    };
    if head == anchored || scm.is_ancestor(&anchored, &head).unwrap_or(false) {
        head
    } else {
        anchored
    }
}

/// Check whether a review's approval covers one exact commit.
///
/// Same rules as [`approval_coverage`], with `target` as the code the
/// approval must cover. `seal reviews mark-merged` passes the commit it
/// records as `final_commit`, so the gate checks exactly the code that
/// merges. Returns `None` for reviews that are not approved.
#[must_use]
pub fn approval_coverage_at(
    scm: &dyn ScmRepo,
    review: &ReviewDetail,
    target: &str,
) -> Option<ApprovalCoverage> {
    if review.status != "approved" {
        return None;
    }

    let current_target = target.to_string();

    let Some(approved_commit) = review.approved_commit.clone() else {
        return Some(ApprovalCoverage {
            approved_commit: None,
            current_target,
            uncovered_commits: None,
            exempt_commits: None,
            stale_reason: Some(StaleReason::NoApprovedCommit),
        });
    };

    // The approval must name a commit id, not a revision expression. A
    // symbolic name such as `HEAD` or `@` would resolve to the current target
    // and compare the target with itself. Such an approval, or one that no
    // longer resolves, is stale.
    if scm.commit_for_anchor(&approved_commit).ok().as_deref() != Some(approved_commit.as_str()) {
        return Some(ApprovalCoverage {
            approved_commit: Some(approved_commit),
            current_target,
            uncovered_commits: None,
            exempt_commits: None,
            stale_reason: Some(StaleReason::CommitsAdded),
        });
    }

    if approved_commit == current_target {
        return Some(ApprovalCoverage {
            approved_commit: Some(approved_commit),
            current_target,
            uncovered_commits: Some(0),
            exempt_commits: Some(0),
            stale_reason: None,
        });
    }

    // Only commits built on top of the approved commit are uncovered work.
    let is_descendant = scm
        .is_ancestor(&approved_commit, &current_target)
        .unwrap_or(false);

    if !is_descendant {
        // A rewrite of the approved commit (jj amending the review log into
        // the approved change, `git commit --amend`) or an unrelated history.
        // The exemption rule compares trees, not history, so it still
        // decides: if the two trees differ only in exempt `.seal/` changes,
        // the code that merges is the code that was approved. A commit that
        // does not resolve, or any other failure, makes the check false.
        // A commit count between unrelated commits means nothing, so both
        // counts stay unknown except that nothing is uncovered when fresh.
        if range_is_exempt(scm, &approved_commit, &current_target) {
            return Some(ApprovalCoverage {
                approved_commit: Some(approved_commit),
                current_target,
                uncovered_commits: Some(0),
                exempt_commits: None,
                stale_reason: None,
            });
        }
        return Some(ApprovalCoverage {
            approved_commit: Some(approved_commit),
            current_target,
            uncovered_commits: None,
            exempt_commits: None,
            stale_reason: Some(StaleReason::CommitsAdded),
        });
    }

    let in_range = scm
        .count_commits_between(&approved_commit, &current_target)
        .unwrap_or(None);

    // Commits that only record review activity (appending to review logs,
    // adding other .seal/ data) cannot change the approved code. The check
    // compares the trees of the two commits, so merges in between need no
    // special case, and any failure counts as a real change.
    if range_is_exempt(scm, &approved_commit, &current_target) {
        return Some(ApprovalCoverage {
            approved_commit: Some(approved_commit),
            current_target,
            uncovered_commits: Some(0),
            exempt_commits: in_range,
            stale_reason: None,
        });
    }

    Some(ApprovalCoverage {
        approved_commit: Some(approved_commit),
        current_target,
        uncovered_commits: in_range,
        exempt_commits: Some(0),
        stale_reason: Some(StaleReason::CommitsAdded),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scm::{ScmKind, ScmRepo};
    use anyhow::{bail, Result};
    use std::path::{Path, PathBuf};

    /// Minimal in-memory backend. Commits form one chain, newest last.
    struct FakeScm {
        root: PathBuf,
        chain: Vec<String>,
        discovered: Option<String>,
    }

    impl FakeScm {
        fn new(chain: &[&str]) -> Self {
            Self {
                root: PathBuf::from("/fake"),
                chain: chain.iter().map(|c| (*c).to_string()).collect(),
                discovered: None,
            }
        }

        fn with_discovered(mut self, base: &str) -> Self {
            self.discovered = Some(base.to_string());
            self
        }
    }

    impl ScmRepo for FakeScm {
        fn kind(&self) -> ScmKind {
            ScmKind::Git
        }
        fn root(&self) -> &Path {
            &self.root
        }
        fn current_anchor(&self) -> Result<String> {
            Ok("refs/heads/feature".to_string())
        }
        fn current_commit(&self) -> Result<String> {
            Ok(self.chain.last().cloned().unwrap_or_default())
        }
        fn commit_for_anchor(&self, anchor: &str) -> Result<String> {
            if anchor == "refs/heads/feature" {
                return self.current_commit();
            }
            if self.chain.iter().any(|c| c == anchor) {
                return Ok(anchor.to_string());
            }
            bail!("unknown anchor {anchor}")
        }
        fn parent_commit(&self, commit: &str) -> Result<String> {
            let idx = self
                .chain
                .iter()
                .position(|c| c == commit)
                .ok_or_else(|| anyhow::anyhow!("unknown commit {commit}"))?;
            if idx == 0 {
                bail!("{commit} is the root commit")
            }
            Ok(self.chain[idx - 1].clone())
        }
        fn diff_git(&self, _from: &str, _to: &str) -> Result<String> {
            Ok(String::new())
        }
        fn diff_git_file(&self, _from: &str, _to: &str, _file: &str) -> Result<String> {
            Ok(String::new())
        }
        fn changed_files_between(&self, _from: &str, _to: &str) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn file_exists(&self, _rev: &str, _path: &str) -> Result<bool> {
            Ok(false)
        }
        fn show_file(&self, _rev: &str, _path: &str) -> Result<String> {
            Ok(String::new())
        }
        fn discover_base_commit(&self, _target: &str) -> Result<Option<String>> {
            Ok(self.discovered.clone())
        }
        fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
            let (Some(a), Some(d)) = (
                self.chain.iter().position(|c| c == ancestor),
                self.chain.iter().position(|c| c == descendant),
            ) else {
                return Ok(false);
            };
            Ok(a < d)
        }
        fn count_commits_between(&self, base: &str, target: &str) -> Result<Option<usize>> {
            let (Some(b), Some(t)) = (
                self.chain.iter().position(|c| c == base),
                self.chain.iter().position(|c| c == target),
            ) else {
                return Ok(None);
            };
            Ok(Some(t.saturating_sub(b)))
        }
    }

    fn review(base_commit: Option<&str>) -> ReviewDetail {
        ReviewDetail {
            review_id: "cr-test".to_string(),
            jj_change_id: "refs/heads/feature".to_string(),
            scm_kind: "git".to_string(),
            scm_anchor: "refs/heads/feature".to_string(),
            initial_commit: "c3".to_string(),
            base_commit: base_commit.map(ToString::to_string),
            approved_commit: None,
            final_commit: None,
            title: "t".to_string(),
            description: None,
            author: "a".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            status: "open".to_string(),
            status_changed_at: None,
            status_changed_by: None,
            abandon_reason: None,
            thread_count: 0,
            open_thread_count: 0,
            reviewers: Vec::new(),
            votes: Vec::new(),
        }
    }

    #[test]
    fn test_persisted_base_covers_whole_range() {
        let scm = FakeScm::new(&["c0", "c1", "c2", "c3"]);
        let range = resolve_review_range(&scm, &review(Some("c0")));

        assert_eq!(range.base, "c0");
        assert_eq!(range.target, "c3");
        assert!(range.base_is_persisted);
    }

    #[test]
    fn test_missing_base_falls_back_to_target_parent() {
        let scm = FakeScm::new(&["c0", "c1", "c2", "c3"]);
        let range = resolve_review_range(&scm, &review(None));

        assert_eq!(range.base, "c2");
        assert_eq!(range.target, "c3");
        assert!(!range.base_is_persisted);
    }

    #[test]
    fn test_unresolvable_base_falls_back_to_target_parent() {
        let scm = FakeScm::new(&["c0", "c1", "c2", "c3"]);
        let range = resolve_review_range(&scm, &review(Some("stranded")));

        assert_eq!(range.base, "c2");
        assert!(!range.base_is_persisted);
    }

    #[test]
    fn test_explicit_base_wins_over_discovery() {
        let scm = FakeScm::new(&["c0", "c1", "c2", "c3"]).with_discovered("c0");
        let base = resolve_new_review_base(&scm, "c3", Some("c1")).expect("base");

        assert_eq!(base, "c1");
    }

    #[test]
    fn test_explicit_base_that_does_not_resolve_is_an_error() {
        let scm = FakeScm::new(&["c0", "c1"]);
        let err = resolve_new_review_base(&scm, "c1", Some("nope")).expect_err("should fail");

        assert!(err.to_string().contains("--base nope"));
    }

    #[test]
    fn test_discovered_base_is_used_when_no_explicit_base() {
        let scm = FakeScm::new(&["c0", "c1", "c2", "c3"]).with_discovered("c0");
        let base = resolve_new_review_base(&scm, "c3", None).expect("base");

        assert_eq!(base, "c0");
    }

    #[test]
    fn test_no_discovery_falls_back_to_parent() {
        let scm = FakeScm::new(&["c0", "c1", "c2", "c3"]);
        let base = resolve_new_review_base(&scm, "c3", None).expect("base");

        assert_eq!(base, "c2");
    }

    fn approved_review(status: &str, approved_commit: Option<&str>) -> ReviewDetail {
        let mut r = review(Some("c0"));
        r.status = status.to_string();
        r.approved_commit = approved_commit.map(ToString::to_string);
        r
    }

    #[test]
    fn test_open_review_has_no_approval_coverage() {
        let scm = FakeScm::new(&["c0", "c1", "c2", "c3"]);
        assert_eq!(
            approval_coverage(&scm, &approved_review("open", None)),
            None
        );
    }

    #[test]
    fn test_approval_at_the_current_target_is_not_stale() {
        let scm = FakeScm::new(&["c0", "c1", "c2", "c3"]);
        let coverage =
            approval_coverage(&scm, &approved_review("approved", Some("c3"))).expect("coverage");

        assert!(!coverage.is_stale());
        assert_eq!(coverage.uncovered_commits, Some(0));
    }

    #[test]
    fn test_commits_added_after_approval_are_uncovered() {
        // Approved at c1, but the branch now points at c3.
        let scm = FakeScm::new(&["c0", "c1", "c2", "c3"]);
        let coverage =
            approval_coverage(&scm, &approved_review("approved", Some("c1"))).expect("coverage");

        assert!(coverage.is_stale());
        assert_eq!(coverage.stale_reason, Some(StaleReason::CommitsAdded));
        assert_eq!(coverage.uncovered_commits, Some(2));
        assert_eq!(coverage.current_target, "c3");
    }

    #[test]
    fn test_approval_without_a_commit_is_stale() {
        // Reviews approved before approvals recorded a commit.
        let scm = FakeScm::new(&["c0", "c1", "c2", "c3"]);
        let coverage =
            approval_coverage(&scm, &approved_review("approved", None)).expect("coverage");

        assert!(coverage.is_stale());
        assert_eq!(coverage.stale_reason, Some(StaleReason::NoApprovedCommit));
        assert_eq!(coverage.uncovered_commits, None);
    }

    #[test]
    fn test_unrelated_approved_commit_reports_no_commit_count() {
        // A rewrite can strand the approved commit. Counting a distance
        // between unrelated commits would be meaningless.
        let scm = FakeScm::new(&["c0", "c1", "c2", "c3"]);
        let coverage = approval_coverage(&scm, &approved_review("approved", Some("stranded")))
            .expect("coverage");

        assert!(coverage.is_stale());
        assert_eq!(coverage.uncovered_commits, None);
    }

    // ---- Real Git repositories: commits after the approval that only record
    // ---- review activity under .seal/ (bn-rk34).

    mod git_exemption {
        use super::approved_review;
        use crate::range::{approval_coverage, ApprovalCoverage, StaleReason};
        use crate::scm::git::GitRepo;
        use std::path::Path;
        use std::process::Command;
        use tempfile::TempDir;

        const LOG: &str = ".seal/reviews/cr-1/events.jsonl";

        fn git(repo: &Path, args: &[&str]) -> String {
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

        fn write(repo: &Path, path: &str, body: &str) {
            let full = repo.join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, body).expect("write");
        }

        fn append(repo: &Path, path: &str, body: &str) {
            let old = std::fs::read_to_string(repo.join(path)).expect("read");
            write(repo, path, &format!("{old}{body}"));
        }

        fn commit_all(repo: &Path, message: &str) -> String {
            git(repo, &["add", "-A"]);
            git(repo, &["commit", "-q", "--allow-empty", "-m", message]);
            git(repo, &["rev-parse", "HEAD"])
        }

        /// A repo on `main` whose approved commit A holds source, a review
        /// log and other seal data. Returns (dir, scm, A).
        fn repo_approved_at_a() -> (TempDir, GitRepo, String) {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            git(root, &["init", "-q"]);
            git(root, &["config", "user.email", "test@example.com"]);
            git(root, &["config", "user.name", "Test User"]);
            // Renames must never hide the old side, whatever the config says.
            git(root, &["config", "diff.renames", "copies"]);
            write(root, "src/lib.rs", "pub fn f() {}\n");
            write(root, ".seal/version", "2\n");
            write(root, LOG, "{\"event\":\"created\"}\n");
            let a = commit_all(root, "feature");
            git(root, &["branch", "-M", "main"]);
            let scm = GitRepo::new(root.to_path_buf());
            (dir, scm, a)
        }

        fn coverage(scm: &GitRepo, approved: &str) -> ApprovalCoverage {
            let mut review = approved_review("approved", Some(approved));
            review.scm_anchor = "refs/heads/main".to_string();
            review.jj_change_id = "refs/heads/main".to_string();
            review.initial_commit = approved.to_string();
            approval_coverage(scm, &review).expect("coverage")
        }

        fn assert_stale(scm: &GitRepo, a: &str, commits: usize) {
            let c = coverage(scm, a);
            assert_eq!(c.stale_reason, Some(StaleReason::CommitsAdded));
            assert_eq!(c.uncovered_commits, Some(commits));
            assert_eq!(c.exempt_commits, Some(0));
        }

        fn assert_fresh(scm: &GitRepo, a: &str, commits: usize) {
            let c = coverage(scm, a);
            assert!(!c.is_stale(), "expected fresh, got {c:?}");
            assert_eq!(c.uncovered_commits, Some(0));
            assert_eq!(c.exempt_commits, Some(commits));
        }

        #[test]
        fn appending_to_review_log_keeps_approval_fresh() {
            let (dir, scm, a) = repo_approved_at_a();
            append(dir.path(), LOG, "{\"event\":\"lgtm\"}\n");
            commit_all(dir.path(), "chore: seal review cr-1");
            append(dir.path(), LOG, "{\"event\":\"comment\"}\n");
            commit_all(dir.path(), "chore: seal reply");
            assert_fresh(&scm, &a, 2);
        }

        #[test]
        fn new_review_log_and_seal_data_keep_approval_fresh() {
            let (dir, scm, a) = repo_approved_at_a();
            write(dir.path(), ".seal/reviews/cr-2/events.jsonl", "{}\n");
            write(dir.path(), ".seal/version", "3\n");
            commit_all(dir.path(), "chore: seal");
            assert_fresh(&scm, &a, 1);
        }

        #[test]
        fn empty_commit_keeps_approval_fresh() {
            let (dir, scm, a) = repo_approved_at_a();
            commit_all(dir.path(), "empty");
            assert_fresh(&scm, &a, 1);
        }

        #[test]
        fn mixed_seal_and_source_commit_is_stale() {
            let (dir, scm, a) = repo_approved_at_a();
            append(dir.path(), LOG, "{\"event\":\"lgtm\"}\n");
            append(dir.path(), "src/lib.rs", "pub fn g() {}\n");
            commit_all(dir.path(), "mixed");
            assert_stale(&scm, &a, 1);
        }

        #[test]
        fn source_commit_after_seal_commit_is_stale() {
            let (dir, scm, a) = repo_approved_at_a();
            append(dir.path(), LOG, "{\"event\":\"lgtm\"}\n");
            commit_all(dir.path(), "seal");
            write(dir.path(), "src/new.rs", "\n");
            commit_all(dir.path(), "code");
            assert_stale(&scm, &a, 2);
        }

        #[test]
        fn deleting_review_log_is_stale() {
            let (dir, scm, a) = repo_approved_at_a();
            git(dir.path(), &["rm", "-q", LOG]);
            commit_all(dir.path(), "drop log");
            assert_stale(&scm, &a, 1);
        }

        #[test]
        fn rewriting_review_log_is_stale() {
            let (dir, scm, a) = repo_approved_at_a();
            write(
                dir.path(),
                LOG,
                "{\"event\":\"forged\"}\n{\"event\":\"lgtm\"}\n",
            );
            commit_all(dir.path(), "rewrite log");
            assert_stale(&scm, &a, 1);
        }

        #[test]
        fn truncating_review_log_is_stale() {
            let (dir, scm, a) = repo_approved_at_a();
            write(dir.path(), LOG, "");
            commit_all(dir.path(), "truncate log");
            assert_stale(&scm, &a, 1);
        }

        #[test]
        fn renaming_source_into_seal_is_stale() {
            let (dir, scm, a) = repo_approved_at_a();
            git(dir.path(), &["mv", "src/lib.rs", ".seal/lib.rs"]);
            commit_all(dir.path(), "hide source");
            assert_stale(&scm, &a, 1);
        }

        #[test]
        fn renaming_seal_data_out_of_seal_is_stale() {
            let (dir, scm, a) = repo_approved_at_a();
            git(dir.path(), &["mv", ".seal/version", "src/version"]);
            commit_all(dir.path(), "move out");
            assert_stale(&scm, &a, 1);
        }

        #[test]
        fn renaming_review_log_within_seal_is_stale() {
            let (dir, scm, a) = repo_approved_at_a();
            git(
                dir.path(),
                &["mv", ".seal/reviews/cr-1", ".seal/reviews/cr-9"],
            );
            commit_all(dir.path(), "move log");
            assert_stale(&scm, &a, 1);
        }

        #[test]
        fn sealignore_change_is_stale() {
            let (dir, scm, a) = repo_approved_at_a();
            write(dir.path(), ".sealignore", "src/\n");
            commit_all(dir.path(), "ignore everything");
            assert_stale(&scm, &a, 1);
        }

        #[test]
        fn nested_seal_directory_is_stale() {
            let (dir, scm, a) = repo_approved_at_a();
            write(dir.path(), "src/.seal/x", "x\n");
            commit_all(dir.path(), "nested");
            assert_stale(&scm, &a, 1);
        }

        #[test]
        fn merge_bringing_in_source_is_stale() {
            let (dir, scm, a) = repo_approved_at_a();
            let root = dir.path();
            git(root, &["checkout", "-q", "-b", "side"]);
            write(root, "src/side.rs", "\n");
            commit_all(root, "side code");
            git(root, &["checkout", "-q", "main"]);
            append(root, LOG, "{\"event\":\"lgtm\"}\n");
            commit_all(root, "seal");
            git(root, &["merge", "-q", "--no-edit", "side"]);
            // seal, side code, and the merge.
            assert_stale(&scm, &a, 3);
        }

        #[test]
        fn merge_of_seal_only_side_branch_is_fresh() {
            let (dir, scm, a) = repo_approved_at_a();
            let root = dir.path();
            git(root, &["checkout", "-q", "-b", "side"]);
            write(root, ".seal/reviews/cr-2/events.jsonl", "{}\n");
            commit_all(root, "side seal");
            git(root, &["checkout", "-q", "main"]);
            append(root, LOG, "{\"event\":\"lgtm\"}\n");
            commit_all(root, "seal");
            git(root, &["merge", "-q", "--no-edit", "side"]);
            assert_fresh(&scm, &a, 3);
        }

        #[test]
        fn reverted_source_change_is_fresh_because_trees_match() {
            // The merged code is byte-identical to the approved code.
            let (dir, scm, a) = repo_approved_at_a();
            let root = dir.path();
            write(root, "src/lib.rs", "evil\n");
            commit_all(root, "change");
            write(root, "src/lib.rs", "pub fn f() {}\n");
            commit_all(root, "revert");
            assert_fresh(&scm, &a, 2);
        }

        // ---- The approved commit is no longer an ancestor of the target
        // ---- (bn-3rk0). The tree comparison still decides.

        /// Fresh although `a` is not an ancestor: the count of an unrelated
        /// range is meaningless, so `exempt_commits` is `None`.
        fn assert_fresh_unrelated(scm: &GitRepo, a: &str) {
            let c = coverage(scm, a);
            assert!(!c.is_stale(), "expected fresh, got {c:?}");
            assert_eq!(c.uncovered_commits, Some(0));
            assert_eq!(c.exempt_commits, None);
        }

        fn assert_stale_unrelated(scm: &GitRepo, a: &str) {
            let c = coverage(scm, a);
            assert_eq!(c.stale_reason, Some(StaleReason::CommitsAdded));
            assert_eq!(c.uncovered_commits, None);
            assert_eq!(c.exempt_commits, None);
        }

        fn amend_all(repo: &Path) -> String {
            git(repo, &["add", "-A"]);
            git(repo, &["commit", "-q", "--amend", "--no-edit"]);
            git(repo, &["rev-parse", "HEAD"])
        }

        #[test]
        fn amending_only_the_review_log_into_approved_commit_is_fresh() {
            let (dir, scm, a) = repo_approved_at_a();
            append(dir.path(), LOG, "{\"event\":\"lgtm\"}\n");
            let t = amend_all(dir.path());
            assert_ne!(t, a);
            assert_fresh_unrelated(&scm, &a);
        }

        #[test]
        fn amending_a_source_change_into_approved_commit_is_stale() {
            let (dir, scm, a) = repo_approved_at_a();
            append(dir.path(), LOG, "{\"event\":\"lgtm\"}\n");
            append(dir.path(), "src/lib.rs", "pub fn g() {}\n");
            amend_all(dir.path());
            assert_stale_unrelated(&scm, &a);
        }

        #[test]
        fn amending_a_rewrite_of_the_review_log_is_stale() {
            let (dir, scm, a) = repo_approved_at_a();
            write(dir.path(), LOG, "{\"event\":\"forged\"}\n");
            amend_all(dir.path());
            assert_stale_unrelated(&scm, &a);
        }

        #[test]
        fn amending_a_truncation_of_the_review_log_is_stale() {
            let (dir, scm, a) = repo_approved_at_a();
            write(dir.path(), LOG, "");
            amend_all(dir.path());
            assert_stale_unrelated(&scm, &a);
        }

        #[test]
        fn approval_on_unrelated_branch_with_other_source_is_stale() {
            let (dir, scm, _) = repo_approved_at_a();
            let root = dir.path();
            git(root, &["checkout", "-q", "--orphan", "other"]);
            write(root, "src/lib.rs", "pub fn other() {}\n");
            let a = commit_all(root, "unrelated");
            git(root, &["checkout", "-q", "main"]);
            assert_stale_unrelated(&scm, &a);
        }

        #[test]
        fn approval_at_unresolvable_commit_is_stale() {
            let (_dir, scm, _) = repo_approved_at_a();
            assert_stale_unrelated(&scm, "0123456789abcdef0123456789abcdef01234567");
            assert_stale_unrelated(&scm, "no-such-ref");
        }

        /// A symbolic approval resolves to the target itself, so a tree
        /// comparison alone would call it fresh. It must name a commit id.
        #[test]
        fn approval_naming_a_revision_expression_is_stale() {
            let (dir, scm, _) = repo_approved_at_a();
            append(dir.path(), "src/lib.rs", "pub fn unreviewed() {}\n");
            commit_all(dir.path(), "unreviewed");
            assert_stale_unrelated(&scm, "HEAD");
            assert_stale_unrelated(&scm, "HEAD~0");
        }

        #[test]
        fn path_changes_between_compares_trees_of_unrelated_commits() {
            use crate::scm::{PathChange, ScmRepo};
            let (dir, scm, a) = repo_approved_at_a();
            append(dir.path(), LOG, "{\"event\":\"lgtm\"}\n");
            let t = amend_all(dir.path());
            let changes = scm
                .path_changes_between(&a, &t)
                .expect("diff")
                .expect("supported");
            assert_eq!(changes, vec![PathChange::Modified(LOG.to_string())]);
        }
    }

    // ---- Detached HEAD (git worktrees, maw workspaces): the anchor is a
    // ---- fixed commit that new commits do not move (bn-2ypz).

    mod git_detached {
        use super::approved_review;
        use crate::range::{
            approval_coverage, approval_coverage_at, effective_approval_target, ApprovalCoverage,
            StaleReason,
        };
        use crate::scm::git::GitRepo;
        use crate::scm::ScmRepo;
        use std::path::Path;
        use std::process::Command;
        use tempfile::TempDir;

        const LOG: &str = ".seal/reviews/cr-1/events.jsonl";

        fn git(repo: &Path, args: &[&str]) -> String {
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

        fn append(repo: &Path, path: &str, body: &str) {
            let full = repo.join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            let old = std::fs::read_to_string(&full).unwrap_or_default();
            std::fs::write(full, format!("{old}{body}")).expect("write");
        }

        fn commit_all(repo: &Path, message: &str) -> String {
            git(repo, &["add", "-A"]);
            git(repo, &["commit", "-q", "--allow-empty", "-m", message]);
            git(repo, &["rev-parse", "HEAD"])
        }

        /// A repo on a detached HEAD at approved commit A. The review is
        /// anchored the way `seal reviews create` anchors it there.
        fn detached_at_a() -> (TempDir, GitRepo, String) {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            git(root, &["init", "-q"]);
            git(root, &["config", "user.email", "test@example.com"]);
            git(root, &["config", "user.name", "Test User"]);
            append(root, "src/lib.rs", "pub fn f() {}\n");
            append(root, LOG, "{\"event\":\"created\"}\n");
            let a = commit_all(root, "feature");
            git(root, &["branch", "-M", "main"]);
            git(root, &["checkout", "-q", "--detach"]);
            let scm = GitRepo::new(root.to_path_buf());
            assert_eq!(
                scm.current_anchor().expect("anchor"),
                format!("detached:{a}")
            );
            (dir, scm, a)
        }

        /// An approved review anchored at `detached:<commit>`, as
        /// `seal reviews create` anchors it on a detached HEAD.
        fn detached_review(commit: &str) -> crate::projection::ReviewDetail {
            let mut review = approved_review("approved", Some(commit));
            review.scm_anchor = format!("detached:{commit}");
            review.jj_change_id.clone_from(&review.scm_anchor);
            review.initial_commit = commit.to_string();
            review
        }

        fn coverage(scm: &GitRepo, a: &str) -> ApprovalCoverage {
            approval_coverage(scm, &detached_review(a)).expect("coverage")
        }

        #[test]
        fn detached_source_commit_after_lgtm_is_stale() {
            let (dir, scm, a) = detached_at_a();
            append(dir.path(), "src/lib.rs", "pub fn unreviewed() {}\n");
            let b = commit_all(dir.path(), "unreviewed");
            let c = coverage(&scm, &a);
            assert_eq!(c.current_target, b, "checked against the workspace commit");
            assert_eq!(c.stale_reason, Some(StaleReason::CommitsAdded));
            assert_eq!(c.uncovered_commits, Some(1));
        }

        #[test]
        fn detached_review_log_commit_keeps_approval_fresh() {
            let (dir, scm, a) = detached_at_a();
            append(dir.path(), LOG, "{\"event\":\"lgtm\"}\n");
            let b = commit_all(dir.path(), "chore: seal review cr-1");
            let c = coverage(&scm, &a);
            assert_eq!(c.current_target, b);
            assert!(!c.is_stale(), "expected fresh, got {c:?}");
            assert_eq!(c.uncovered_commits, Some(0));
            assert_eq!(c.exempt_commits, Some(1));
        }

        #[test]
        fn detached_log_then_source_commit_is_stale() {
            let (dir, scm, a) = detached_at_a();
            append(dir.path(), LOG, "{\"event\":\"lgtm\"}\n");
            commit_all(dir.path(), "chore: seal review cr-1");
            append(dir.path(), "src/lib.rs", "pub fn unreviewed() {}\n");
            commit_all(dir.path(), "unreviewed");
            let c = coverage(&scm, &a);
            assert_eq!(c.stale_reason, Some(StaleReason::CommitsAdded));
            assert_eq!(c.uncovered_commits, Some(2));
        }

        /// A checkout that does not build on the anchor (trunk before the
        /// merge, another reviewer's workspace) cannot merge the reviewed
        /// code, so the approval is judged against the anchor.
        #[test]
        fn checkout_not_descending_from_anchor_reports_against_anchor() {
            let (dir, scm, a) = detached_at_a();
            // The reviewed change B sits on top of A, detached.
            append(dir.path(), "src/lib.rs", "pub fn reviewed() {}\n");
            let b = commit_all(dir.path(), "reviewed");
            // Trunk moves on with unrelated source work, and we look from it.
            git(dir.path(), &["checkout", "-q", "main"]);
            append(dir.path(), "src/other.rs", "pub fn trunk() {}\n");
            let t = commit_all(dir.path(), "trunk work");
            assert_ne!(scm.current_commit().expect("head"), b);

            let review = detached_review(&b);
            assert_eq!(effective_approval_target(&scm, &review), b);
            let c = approval_coverage(&scm, &review).expect("coverage");
            assert_eq!(c.current_target, b);
            assert!(!c.is_stale(), "expected fresh, got {c:?}");

            // Trunk root (an ancestor of the anchor) also reports the anchor.
            git(dir.path(), &["checkout", "-q", "--detach", &a]);
            assert_eq!(effective_approval_target(&scm, &review), b);
            // And an explicit target is checked as given.
            let at_t = approval_coverage_at(&scm, &review, &t).expect("coverage");
            assert_eq!(at_t.current_target, t);
            assert!(at_t.is_stale(), "trunk work is not covered: {at_t:?}");
        }

        /// A merged review is judged against its recorded final commit, never
        /// the workspace.
        #[test]
        fn merged_review_ignores_the_workspace_commit() {
            let (dir, scm, a) = detached_at_a();
            append(dir.path(), "src/lib.rs", "pub fn later() {}\n");
            commit_all(dir.path(), "later");
            let mut review = detached_review(&a);
            review.final_commit = Some(a.clone());
            assert_eq!(effective_approval_target(&scm, &review), a);
        }

        /// Branch anchors move with their commits, so on the branch the
        /// workspace commit and the anchored target agree, as before.
        #[test]
        fn branch_anchor_on_the_branch_is_unchanged() {
            let (dir, scm, a) = detached_at_a();
            git(dir.path(), &["checkout", "-q", "main"]);
            append(dir.path(), LOG, "{\"event\":\"lgtm\"}\n");
            let b = commit_all(dir.path(), "chore: seal");
            let mut review = approved_review("approved", Some(&a));
            review.scm_anchor = "refs/heads/main".to_string();
            review.jj_change_id.clone_from(&review.scm_anchor);
            review.initial_commit.clone_from(&a);
            let c = approval_coverage(&scm, &review).expect("coverage");
            assert_eq!(c.current_target, b);
            assert!(!c.is_stale());
            append(dir.path(), "src/lib.rs", "pub fn g() {}\n");
            commit_all(dir.path(), "source");
            let c = approval_coverage(&scm, &review).expect("coverage");
            assert_eq!(c.stale_reason, Some(StaleReason::CommitsAdded));
            assert_eq!(c.uncovered_commits, Some(2));
        }
    }

    // ---- jj: writing the review log into the approved change rewrites the
    // ---- approved commit (bn-3rk0).

    mod jj_rewrite {
        use super::approved_review;
        use crate::range::{approval_coverage, ApprovalCoverage, StaleReason};
        use crate::scm::jj::JjScmRepo;
        use crate::scm::ScmRepo;
        use std::path::Path;
        use std::process::Command;

        const LOG: &str = ".seal/reviews/cr-1/events.jsonl";

        fn jj(root: &Path, args: &[&str]) -> String {
            let output = Command::new("jj")
                .current_dir(root)
                .args([
                    "--config",
                    "user.name=Test",
                    "--config",
                    "user.email=t@example.com",
                ])
                .args(args)
                .output()
                .expect("run jj");
            assert!(
                output.status.success(),
                "jj {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout)
                .expect("utf8")
                .trim()
                .to_string()
        }

        fn write(root: &Path, path: &str, body: &str) {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, body).expect("write");
        }

        fn append(root: &Path, path: &str, body: &str) {
            let old = std::fs::read_to_string(root.join(path)).expect("read");
            write(root, path, &format!("{old}{body}"));
        }

        /// A jj repo whose working-copy change holds source and a review
        /// log, approved at its current commit. Returns (dir, scm, change, A).
        fn change_approved_at_a() -> Option<(tempfile::TempDir, JjScmRepo, String, String)> {
            if which::which("jj").is_err() {
                eprintln!("skipping: jj not installed");
                return None;
            }
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            jj(root, &["git", "init", "."]);
            write(root, "src/lib.rs", "pub fn f() {}\n");
            write(root, LOG, "{\"event\":\"created\"}\n");
            let change = jj(root, &["log", "-r", "@", "--no-graph", "-T", "change_id"]);
            let scm = JjScmRepo::new(root);
            let a = scm.commit_for_anchor("@").expect("approved commit");
            Some((dir, scm, change, a))
        }

        fn coverage(scm: &JjScmRepo, change: &str, approved: &str) -> ApprovalCoverage {
            let mut review = approved_review("approved", Some(approved));
            review.scm_kind = "jj".to_string();
            review.scm_anchor = change.to_string();
            review.jj_change_id = change.to_string();
            review.initial_commit = approved.to_string();
            approval_coverage(scm, &review).expect("coverage")
        }

        #[test]
        fn jj_amending_only_the_review_log_keeps_approval_fresh() {
            let Some((dir, scm, change, a)) = change_approved_at_a() else {
                return;
            };
            append(dir.path(), LOG, "{\"event\":\"lgtm\"}\n");
            let c = coverage(&scm, &change, &a);
            assert_ne!(c.current_target, a, "the amend must rewrite the commit");
            assert!(
                !scm.is_ancestor(&a, &c.current_target).expect("ancestry"),
                "the old commit must not be an ancestor of the rewrite"
            );
            assert!(!c.is_stale(), "expected fresh, got {c:?}");
            assert_eq!(c.uncovered_commits, Some(0));
            assert_eq!(c.exempt_commits, None);
        }

        #[test]
        fn jj_amending_a_source_change_makes_approval_stale() {
            let Some((dir, scm, change, a)) = change_approved_at_a() else {
                return;
            };
            append(dir.path(), LOG, "{\"event\":\"lgtm\"}\n");
            append(dir.path(), "src/lib.rs", "pub fn g() {}\n");
            let c = coverage(&scm, &change, &a);
            assert_ne!(c.current_target, a);
            assert_eq!(c.stale_reason, Some(StaleReason::CommitsAdded));
            assert_eq!(c.uncovered_commits, None);
            assert_eq!(c.exempt_commits, None);
        }

        /// `jj new` leaves the anchored change behind. Code in the new
        /// working-copy change is what merges next, so it stales the LGTM.
        #[test]
        fn jj_new_change_with_source_makes_approval_stale() {
            let Some((dir, scm, change, a)) = change_approved_at_a() else {
                return;
            };
            jj(dir.path(), &["new"]);
            append(dir.path(), "src/lib.rs", "pub fn g() {}\n");
            let c = coverage(&scm, &change, &a);
            assert_eq!(c.current_target, scm.current_commit().expect("@"));
            assert_ne!(c.current_target, a);
            assert_eq!(c.stale_reason, Some(StaleReason::CommitsAdded));
            assert_eq!(c.uncovered_commits, Some(1));
        }

        #[test]
        fn jj_new_change_with_review_log_keeps_approval_fresh() {
            let Some((dir, scm, change, a)) = change_approved_at_a() else {
                return;
            };
            jj(dir.path(), &["new"]);
            append(dir.path(), LOG, "{\"event\":\"lgtm\"}\n");
            let c = coverage(&scm, &change, &a);
            assert_eq!(c.current_target, scm.current_commit().expect("@"));
            assert!(!c.is_stale(), "expected fresh, got {c:?}");
        }

        #[test]
        fn jj_unresolvable_approved_commit_is_stale() {
            let Some((_dir, scm, change, _)) = change_approved_at_a() else {
                return;
            };
            let c = coverage(&scm, &change, "0123456789abcdef0123456789abcdef01234567");
            assert_eq!(c.stale_reason, Some(StaleReason::CommitsAdded));
        }
    }
}

//! Locate a thread's line anchor in the code a review shows now.
//!
//! A thread stores the commit it was created on plus a file and a line range.
//! The review can later move to other code: an amend, new commits, or a rebase
//! onto newer trunk followed by `seal reviews retarget`. Readers must show the
//! thread where its lines are in the review's current target, or say that
//! the lines changed. They must never hide the thread.
//!
//! The mapping diffs the anchor commit against the target for the one file and
//! walks the hunks (see [`crate::jj::drift`]). After a rebase the two commits
//! are not in an ancestor relation. The diff then also holds the trunk changes
//! to that file. This is correct: those changes are exactly what moved or
//! edited the anchored lines.

use std::collections::HashMap;

use serde::Serialize;

use crate::jj::drift::{drift_range_through_hunks, parse_hunks, Hunk, RangeDrift};
use crate::scm::ScmRepo;

/// How a thread anchor relates to the target code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorStatus {
    /// The anchored lines are at the same line numbers.
    Unchanged,
    /// The anchored lines are intact but at other line numbers.
    Shifted,
    /// At least one anchored line was edited or removed.
    Modified,
    /// All anchored lines were removed.
    Deleted,
    /// The anchor could not be mapped, for example because its commit no
    /// longer exists in this repository.
    Unknown,
}

/// The location of a thread anchor in a target commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnchorLocation {
    /// How the anchor relates to the target.
    pub status: AnchorStatus,
    /// First anchored line in the anchor commit.
    pub original_start: u32,
    /// Last anchored line in the anchor commit.
    pub original_end: u32,
    /// First anchored line in the target, when the lines are intact.
    pub current_start: Option<u32>,
    /// Last anchored line in the target, when the lines are intact.
    pub current_end: Option<u32>,
    /// Why the anchor could not be mapped (only for [`AnchorStatus::Unknown`]).
    pub reason: Option<String>,
}

impl AnchorLocation {
    /// True when the target no longer holds the anchored lines as they were
    /// commented on. An outdated thread must still be shown, marked outdated.
    #[must_use]
    pub const fn is_outdated(&self) -> bool {
        matches!(
            self.status,
            AnchorStatus::Modified | AnchorStatus::Deleted | AnchorStatus::Unknown
        )
    }

    /// A short label: `unchanged`, `shifted(+N)`/`shifted(-N)`, `modified`,
    /// `deleted` or `unknown`.
    #[must_use]
    pub fn drift_label(&self) -> String {
        match (self.status, self.current_start) {
            (AnchorStatus::Shifted, Some(current)) => {
                let delta = i64::from(current) - i64::from(self.original_start);
                let sign = if delta > 0 { "+" } else { "" };
                format!("shifted({sign}{delta})")
            }
            (AnchorStatus::Unchanged | AnchorStatus::Shifted, _) => "unchanged".to_string(),
            (AnchorStatus::Modified, _) => "modified".to_string(),
            (AnchorStatus::Deleted, _) => "deleted".to_string(),
            (AnchorStatus::Unknown, _) => "unknown".to_string(),
        }
    }

    const fn unknown(original_start: u32, original_end: u32, reason: String) -> Self {
        Self {
            status: AnchorStatus::Unknown,
            original_start,
            original_end,
            current_start: None,
            current_end: None,
            reason: Some(reason),
        }
    }
}

/// Convert a stored selection (SQLite `i64`) to the `u32` line numbers the
/// drift walk uses. Returns `None` for values that are not line numbers.
fn stored_line(value: i64) -> Option<u32> {
    u32::try_from(value).ok()
}

/// Maps thread anchors to a target commit, parsing each
/// `(anchor commit, target commit, file)` diff at most once.
pub struct AnchorLocator<'a> {
    scm: &'a dyn ScmRepo,
    hunks: HashMap<(String, String, String), Result<Vec<Hunk>, String>>,
}

impl<'a> AnchorLocator<'a> {
    /// Create a locator over `scm`.
    #[must_use]
    pub fn new(scm: &'a dyn ScmRepo) -> Self {
        Self {
            scm,
            hunks: HashMap::new(),
        }
    }

    /// Locate the stored selection `start..=end` of `file`, created on
    /// `anchor_commit`, in `target_commit`.
    ///
    /// This never fails. When the diff cannot be computed (for example the
    /// anchor commit was garbage collected), the result is
    /// [`AnchorStatus::Unknown`] with the reason, so one bad thread cannot
    /// fail or hide the rest of a command's output.
    pub fn locate(
        &mut self,
        file: &str,
        start: i64,
        end: Option<i64>,
        anchor_commit: &str,
        target_commit: &str,
    ) -> AnchorLocation {
        let end = end.unwrap_or(start);
        let (Some(start), Some(end)) = (stored_line(start), stored_line(end)) else {
            return AnchorLocation::unknown(
                u32::try_from(start.max(0)).unwrap_or(u32::MAX),
                u32::try_from(end.max(0)).unwrap_or(u32::MAX),
                format!("stored selection {start}-{end} is not a valid line range"),
            );
        };
        let (start, end) = (start.min(end), start.max(end));

        if anchor_commit == target_commit {
            return AnchorLocation {
                status: AnchorStatus::Unchanged,
                original_start: start,
                original_end: end,
                current_start: Some(start),
                current_end: Some(end),
                reason: None,
            };
        }

        let key = (
            anchor_commit.to_string(),
            target_commit.to_string(),
            file.to_string(),
        );
        let scm = self.scm;
        let hunks = self.hunks.entry(key).or_insert_with(|| {
            scm.diff_git_file(anchor_commit, target_commit, file)
                .and_then(|diff| parse_hunks(&diff))
                .map_err(|error| format!("{error:#}"))
        });
        let hunks = match hunks {
            Ok(hunks) => hunks,
            Err(reason) => {
                return AnchorLocation::unknown(
                    start,
                    end,
                    format!("cannot diff anchor commit {anchor_commit}: {reason}"),
                )
            }
        };

        match drift_range_through_hunks(hunks, start, end) {
            Ok(RangeDrift::Mapped {
                start: current_start,
                end: current_end,
            }) => AnchorLocation {
                status: if current_start == start {
                    AnchorStatus::Unchanged
                } else {
                    AnchorStatus::Shifted
                },
                original_start: start,
                original_end: end,
                current_start: Some(current_start),
                current_end: Some(current_end),
                reason: None,
            },
            Ok(drift @ (RangeDrift::Modified | RangeDrift::Deleted)) => AnchorLocation {
                status: if drift == RangeDrift::Deleted {
                    AnchorStatus::Deleted
                } else {
                    AnchorStatus::Modified
                },
                original_start: start,
                original_end: end,
                current_start: None,
                current_end: None,
                reason: None,
            },
            Err(error) => AnchorLocation::unknown(start, end, format!("{error:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::CoreContext;
    use crate::events::CodeSelection;
    use crate::scm::git::GitRepo;
    use std::path::Path;
    use std::process::Command;
    use tempfile::{tempdir, TempDir};

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

    fn base_lines() -> Vec<String> {
        (1..=10).map(|i| format!("base {i}")).collect()
    }

    fn write_lines(repo: &Path, path: &str, lines: &[String]) {
        let mut body = lines.join("\n");
        body.push('\n');
        std::fs::write(repo.join(path), body).expect("write file");
    }

    fn read_lines(repo: &Path, path: &str) -> Vec<String> {
        std::fs::read_to_string(repo.join(path))
            .expect("read file")
            .lines()
            .map(ToString::to_string)
            .collect()
    }

    /// A maw-like workspace. `main` holds trunk, HEAD is detached on the
    /// feature commits, and `.seal/` is not tracked. `f.txt` starts as
    /// `base 1..=10`; the feature appends `feature A/B/C` as lines 11-13.
    struct Fixture {
        dir: TempDir,
        ctx: CoreContext,
        scm: GitRepo,
        review_id: String,
    }

    impl Fixture {
        /// Build the workspace. With `split_feature`, the feature lines land in
        /// one commit and a second feature commit touches another file, so
        /// the anchored lines were introduced by a commit below the target.
        fn new(split_feature: bool) -> Self {
            let dir = tempdir().expect("tempdir");
            let root = dir.path();
            git(root, &["init", "-q"]);
            git(root, &["config", "user.email", "test@example.com"]);
            git(root, &["config", "user.name", "Test User"]);
            std::fs::write(root.join(".git/info/exclude"), ".seal/\n").expect("exclude");
            write_lines(root, "f.txt", &base_lines());
            git(root, &["add", "f.txt"]);
            git(root, &["commit", "-qm", "base"]);
            git(root, &["branch", "-M", "main"]);
            git(root, &["checkout", "-q", "--detach"]);

            let mut lines = base_lines();
            lines.extend(["feature A", "feature B", "feature C"].map(String::from));
            write_lines(root, "f.txt", &lines);
            git(root, &["commit", "-qam", "feature lines"]);
            if split_feature {
                std::fs::write(root.join("other.txt"), "other\n").expect("write other");
                git(root, &["add", "other.txt"]);
                git(root, &["commit", "-qm", "feature other"]);
            }

            let seal_dir = root.join(".seal");
            std::fs::create_dir(&seal_dir).expect("create .seal");
            std::fs::write(seal_dir.join("version"), "2\n").expect("write version");
            std::fs::create_dir(seal_dir.join("reviews")).expect("create reviews dir");
            let ctx = CoreContext::new(root, &seal_dir.join("index.db")).expect("context");
            let scm = GitRepo::new(root.to_path_buf());

            let target = scm.current_commit().expect("target");
            let base = crate::range::resolve_new_review_base(&scm, &target, None).expect("base");
            let review_id = ctx
                .services()
                .expect("services")
                .reviews()
                .create(
                    &scm,
                    "Review".to_string(),
                    None,
                    None,
                    Some("alice"),
                    Some(base),
                )
                .expect("create review");
            Self {
                dir,
                ctx,
                scm,
                review_id,
            }
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        /// Comment on `selection` of `f.txt` at the review's current target,
        /// the way `seal comment` does.
        fn thread(&self, selection: CodeSelection) -> String {
            let review = self
                .ctx
                .services()
                .expect("services")
                .reviews()
                .get(&self.review_id)
                .expect("review");
            let commit = crate::range::resolve_target_commit(&self.scm, &review);
            self.ctx
                .services()
                .expect("services")
                .threads()
                .create(&self.review_id, "f.txt", selection, commit, Some("alice"))
                .expect("create thread")
        }

        /// Land a trunk commit that rewrites `f.txt` with `edit`, then rebase
        /// the workspace onto it and retarget the review.
        fn trunk_edit_rebase_retarget(&self, edit: impl FnOnce(&mut Vec<String>)) {
            let root = self.root();
            let head = git(root, &["rev-parse", "HEAD"]);
            git(root, &["checkout", "-q", "main"]);
            let mut lines = read_lines(root, "f.txt");
            edit(&mut lines);
            write_lines(root, "f.txt", &lines);
            git(root, &["commit", "-qam", "trunk edit"]);
            git(root, &["checkout", "-q", "--detach", &head]);
            git(root, &["rebase", "-q", "main"]);
            assert_ne!(git(root, &["rev-parse", "HEAD"]), head, "rebase rewrote");
            self.ctx
                .services()
                .expect("services")
                .reviews()
                .retarget(&self.scm, &self.review_id, Some("alice"), None)
                .expect("retarget");
        }

        /// Locate every thread of the review in its current target, the way
        /// the readers do. Returns `(thread_id, location)` in list order.
        fn locate_all(&self) -> Vec<(String, AnchorLocation)> {
            let services = self.ctx.services().expect("services");
            let review = services.reviews().get(&self.review_id).expect("review");
            let target = crate::range::resolve_target_commit(&self.scm, &review);
            let mut locator = AnchorLocator::new(&self.scm);
            services
                .threads()
                .list(&self.review_id, None, None)
                .expect("threads")
                .into_iter()
                .map(|thread| {
                    let location = locator.locate(
                        &thread.file_path,
                        thread.selection_start,
                        thread.selection_end,
                        &thread.commit_hash,
                        &target,
                    );
                    (thread.thread_id, location)
                })
                .collect()
        }

        fn locate(&self, thread_id: &str) -> AnchorLocation {
            self.locate_all()
                .into_iter()
                .find(|(id, _)| id == thread_id)
                .map(|(_, location)| location)
                .expect("every thread is still listed")
        }

        /// The target's text for a located anchor.
        fn target_text(&self, location: &AnchorLocation) -> Vec<String> {
            let lines = read_lines(self.root(), "f.txt");
            let start = location.current_start.expect("mapped start") as usize;
            let end = location.current_end.expect("mapped end") as usize;
            lines[start - 1..end].to_vec()
        }
    }

    /// (a) Trunk inserts lines above the anchor; after rebase + retarget the
    /// thread maps to N+k and the mapped line holds the commented text.
    #[test]
    fn rebase_with_trunk_insertions_above_shifts_threads() {
        let fx = Fixture::new(false);
        let on_feature = fx.thread(CodeSelection::line(12));
        let on_base = fx.thread(CodeSelection::line(5));
        let on_range = fx.thread(CodeSelection::range(11, 13));

        fx.trunk_edit_rebase_retarget(|lines| {
            lines.insert(0, "top 1".to_string());
            lines.insert(0, "top 2".to_string());
            lines.insert(0, "top 3".to_string());
        });

        let feature = fx.locate(&on_feature);
        assert_eq!(feature.status, AnchorStatus::Shifted);
        assert_eq!(feature.current_start, Some(15));
        assert_eq!(feature.drift_label(), "shifted(+3)");
        assert!(!feature.is_outdated());
        assert_eq!(fx.target_text(&feature), ["feature B"]);

        let base = fx.locate(&on_base);
        assert_eq!(base.current_start, Some(8));
        assert_eq!(fx.target_text(&base), ["base 5"]);

        let range = fx.locate(&on_range);
        assert_eq!(
            (range.current_start, range.current_end),
            (Some(14), Some(16))
        );
        assert_eq!(
            fx.target_text(&range),
            ["feature A", "feature B", "feature C"]
        );
    }

    /// (b) Trunk edits the anchored line itself: the thread is still listed,
    /// flagged modified and outdated, with no current line. Its neighbours
    /// keep mapping.
    #[test]
    fn rebase_that_edits_the_anchored_line_marks_thread_outdated() {
        let fx = Fixture::new(false);
        let edited = fx.thread(CodeSelection::line(5));
        let range_over_edit = fx.thread(CodeSelection::range(4, 6));
        let untouched = fx.thread(CodeSelection::line(12));

        fx.trunk_edit_rebase_retarget(|lines| {
            lines.insert(0, "top".to_string());
            lines[5] = "base FIVE".to_string();
        });

        let located = fx.locate_all();
        assert_eq!(located.len(), 3, "no thread is dropped");

        let edited = fx.locate(&edited);
        assert_eq!(edited.status, AnchorStatus::Modified);
        assert!(edited.is_outdated());
        assert_eq!(edited.current_start, None);
        assert_eq!(edited.drift_label(), "modified");

        let range = fx.locate(&range_over_edit);
        assert_eq!(range.status, AnchorStatus::Modified);
        assert!(range.is_outdated());

        let untouched = fx.locate(&untouched);
        assert_eq!(untouched.current_start, Some(13));
        assert_eq!(fx.target_text(&untouched), ["feature B"]);
    }

    /// Trunk deletes the anchored lines: deleted, outdated, still listed.
    #[test]
    fn rebase_that_deletes_the_anchored_line_marks_thread_deleted() {
        let fx = Fixture::new(false);
        let deleted = fx.thread(CodeSelection::line(3));
        fx.trunk_edit_rebase_retarget(|lines| {
            lines.remove(2);
        });
        let deleted = fx.locate(&deleted);
        assert_eq!(deleted.status, AnchorStatus::Deleted);
        assert!(deleted.is_outdated());
        assert_eq!(deleted.drift_label(), "deleted");
    }

    /// (c) The commit that introduced the anchored lines sits below the
    /// thread's commit and is itself rewritten by the rebase (and reworded
    /// before it). The thread still maps.
    #[test]
    fn rebase_rewriting_the_introducing_commit_still_maps() {
        let fx = Fixture::new(true);
        let introduced = git(fx.root(), &["rev-parse", "HEAD~1"]);
        let thread = fx.thread(CodeSelection::line(12));
        git(
            fx.root(),
            &["commit", "-q", "--amend", "-m", "feature other v2"],
        );

        fx.trunk_edit_rebase_retarget(|lines| {
            lines.insert(0, "top".to_string());
            lines[2] = "base 2 edited".to_string();
        });
        assert_ne!(git(fx.root(), &["rev-parse", "HEAD~1"]), introduced);

        let location = fx.locate(&thread);
        assert_eq!(location.status, AnchorStatus::Shifted);
        assert_eq!(location.current_start, Some(13));
        assert_eq!(fx.target_text(&location), ["feature B"]);
    }

    /// (d) The thread's commit is garbage collected after the rebase. The
    /// thread is still listed, marked unknown and outdated with a reason, and
    /// other threads of the same review still map.
    #[test]
    fn unresolvable_anchor_commit_is_outdated_not_an_error() {
        let fx = Fixture::new(false);
        let orphaned = fx.thread(CodeSelection::line(12));
        let orphaned_commit = git(fx.root(), &["rev-parse", "HEAD"]);
        fx.trunk_edit_rebase_retarget(|lines| lines.insert(0, "top".to_string()));
        let fresh = fx.thread(CodeSelection::line(13));

        git(fx.root(), &["reflog", "expire", "--expire=now", "--all"]);
        git(fx.root(), &["gc", "-q", "--prune=now"]);
        let probe = Command::new("git")
            .current_dir(fx.root())
            .args(["cat-file", "-e", &format!("{orphaned_commit}^{{commit}}")])
            .output()
            .expect("run git");
        assert!(!probe.status.success(), "anchor commit was pruned");

        let located = fx.locate_all();
        assert_eq!(located.len(), 2, "no thread is dropped");

        let orphaned = fx.locate(&orphaned);
        assert_eq!(orphaned.status, AnchorStatus::Unknown);
        assert!(orphaned.is_outdated());
        assert_eq!(orphaned.current_start, None);
        assert_eq!(orphaned.drift_label(), "unknown");
        assert!(orphaned
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains(&orphaned_commit)));

        let fresh = fx.locate(&fresh);
        assert_eq!(fresh.status, AnchorStatus::Unchanged);
        assert_eq!(fx.target_text(&fresh), ["feature B"]);
    }

    /// An insertion strictly inside an anchored range means the reviewed
    /// block changed, even though every original line survives.
    #[test]
    fn insertion_inside_a_range_is_modified() {
        let fx = Fixture::new(false);
        let range = fx.thread(CodeSelection::range(2, 4));
        fx.trunk_edit_rebase_retarget(|lines| lines.insert(2, "wedge".to_string()));
        assert_eq!(fx.locate(&range).status, AnchorStatus::Modified);
    }
}

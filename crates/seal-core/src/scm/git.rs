use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::scm::{
    parse_git_name_status_z, validate_anchor, validate_repo_relative_path, PathChange, ScmKind,
    ScmRepo,
};

#[derive(Debug, Clone)]
pub struct GitRepo {
    root: PathBuf,
}

impl GitRepo {
    #[must_use]
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn run_git(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .current_dir(&self.root)
            .args(args)
            .output()
            .with_context(|| {
                if let Err(e) = which::which("git") {
                    format!("git command not found. Please install git: {e}")
                } else {
                    format!("Failed to execute git command: {args:?}")
                }
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "git command failed with status {}: {}",
                output.status,
                stderr.trim()
            );
        }

        String::from_utf8(output.stdout).context("git output was not valid UTF-8")
    }

    fn maybe_symbolic_ref_head(&self) -> Option<String> {
        let output = Command::new("git")
            .current_dir(&self.root)
            .args(["symbolic-ref", "--quiet", "HEAD"])
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let stdout = String::from_utf8(output.stdout).ok()?;
        let value = stdout.trim().to_string();
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }

    /// Return the configured upstream of the current branch when it can mark
    /// a trunk, as a full ref name.
    ///
    /// An upstream that is the branch's own remote copy (`git push -u origin
    /// feature` makes `origin/feature` the upstream of `feature`) is not
    /// trunk. Its merge base is the pushed tip, the nearest of all candidates,
    /// so accepting it would drop every pushed feature commit from the range.
    fn maybe_upstream_ref(&self) -> Option<String> {
        let head = self.maybe_symbolic_ref_head()?;
        let output = self
            .run_git(&[
                "for-each-ref",
                "--format=%(upstream)%00%(upstream:remoteref)",
                "--end-of-options",
                &head,
            ])
            .ok()?;
        let line = output.lines().next()?;
        let mut fields = line.split('\0');
        let upstream = fields.next()?.trim();
        let remote_ref = fields.next().unwrap_or_default();
        if upstream.is_empty() || is_own_remote_copy(&head, remote_ref) {
            return None;
        }
        Some(upstream.to_string())
    }

    /// Return the merge base of `candidate` and `target`, if both resolve.
    fn maybe_merge_base(&self, candidate: &str, target: &str) -> Option<String> {
        let candidate_commit = self.commit_for_anchor(candidate).ok()?;
        let target_commit = self.commit_for_anchor(target).ok()?;

        let output = self
            .run_git(&["merge-base", &candidate_commit, &target_commit])
            .ok()?;

        let base = output.trim().to_string();
        if base.is_empty() {
            None
        } else {
            Some(base)
        }
    }
}

/// Refs tried, in order, when discovering the fork point of a feature branch.
///
/// The upstream of the current branch comes first and is added at runtime.
const GIT_BASE_CANDIDATES: &[&str] = &[
    "refs/remotes/origin/HEAD",
    "refs/heads/main",
    "refs/heads/master",
    "refs/heads/trunk",
    "refs/remotes/origin/main",
    "refs/remotes/origin/master",
];

#[must_use]
pub fn detect_git_root(start_path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .current_dir(start_path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let root = stdout.trim();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

impl ScmRepo for GitRepo {
    fn kind(&self) -> ScmKind {
        ScmKind::Git
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn current_anchor(&self) -> Result<String> {
        if let Some(anchor) = self.maybe_symbolic_ref_head() {
            return Ok(anchor);
        }

        let commit = self.current_commit()?;
        Ok(format!("detached:{commit}"))
    }

    fn current_commit(&self) -> Result<String> {
        let output = self
            .run_git(&["rev-parse", "HEAD"])
            .context("Failed to get current commit")?;
        Ok(output.trim().to_string())
    }

    fn commit_for_anchor(&self, anchor: &str) -> Result<String> {
        validate_anchor(anchor)?;

        let anchor = anchor
            .strip_prefix("detached:")
            .map_or(anchor, |commit| commit);
        let rev = format!("{anchor}^{{commit}}");

        let output = self
            .run_git(&["rev-parse", "--verify", "--end-of-options", &rev])
            .with_context(|| format!("Failed to resolve commit for anchor {anchor}"))?;

        Ok(output.trim().to_string())
    }

    fn parent_commit(&self, commit: &str) -> Result<String> {
        validate_anchor(commit)?;
        let output = self
            .run_git(&["log", "-1", "--format=%P", "--end-of-options", commit])
            .with_context(|| format!("Failed to resolve parent commit for {commit}"))?;

        let parents = output.trim();
        if parents.is_empty() {
            // Root commit (no parents), use Git's empty tree hash
            Ok("4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string())
        } else {
            // For merge commits, %P returns space-separated parents. Take the first.
            Ok(parents
                .split_whitespace()
                .next()
                .unwrap_or(parents)
                .to_string())
        }
    }

    fn diff_git(&self, from: &str, to: &str) -> Result<String> {
        validate_anchor(from)?;
        validate_anchor(to)?;
        let range = format!("{from}..{to}");
        self.run_git(&["diff", "--no-color", &range])
            .with_context(|| format!("Failed to diff from {from} to {to}"))
    }

    fn diff_git_file(&self, from: &str, to: &str, file: &str) -> Result<String> {
        validate_anchor(from)?;
        validate_anchor(to)?;
        validate_repo_relative_path(file)?;
        let range = format!("{from}..{to}");
        self.run_git(&["diff", "--no-color", &range, "--", file])
            .with_context(|| format!("Failed to diff file {file} from {from} to {to}"))
    }

    fn changed_files_between(&self, from: &str, to: &str) -> Result<Vec<String>> {
        validate_anchor(from)?;
        validate_anchor(to)?;
        let range = format!("{from}..{to}");
        let output = self
            .run_git(&["diff", "-z", "--name-only", &range])
            .with_context(|| format!("Failed to list changed files from {from} to {to}"))?;

        Ok(output
            .split('\0')
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect())
    }

    fn file_exists(&self, rev: &str, path: &str) -> Result<bool> {
        validate_repo_relative_path(path)?;
        let commit = self.commit_for_anchor(rev)?;
        let spec = format!("{commit}:{path}");

        let output = Command::new("git")
            .current_dir(&self.root)
            .args(["cat-file", "-t", "--end-of-options", &spec])
            .output()
            .context("Failed to execute git cat-file")?;

        if !output.status.success() {
            return Ok(false);
        }

        let obj_type = String::from_utf8_lossy(&output.stdout);
        Ok(obj_type.trim() == "blob")
    }

    fn show_file(&self, rev: &str, path: &str) -> Result<String> {
        validate_repo_relative_path(path)?;
        let commit = self.commit_for_anchor(rev)?;
        let spec = format!("{commit}:{path}");
        self.run_git(&["show", "--end-of-options", &spec])
            .with_context(|| format!("Failed to show file {path} at {rev}"))
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        validate_anchor(ancestor)?;
        validate_anchor(descendant)?;

        let ancestor_commit = self.commit_for_anchor(ancestor)?;
        let descendant_commit = self.commit_for_anchor(descendant)?;

        let output = Command::new("git")
            .current_dir(&self.root)
            .args([
                "merge-base",
                "--is-ancestor",
                &ancestor_commit,
                &descendant_commit,
            ])
            .output()
            .context("Failed to execute git merge-base --is-ancestor")?;

        Ok(output.status.success())
    }

    fn count_commits_between(&self, base: &str, target: &str) -> Result<Option<usize>> {
        validate_anchor(base)?;
        validate_anchor(target)?;

        let range = format!("{base}..{target}");
        let output = self
            .run_git(&["rev-list", "--count", "--end-of-options", &range])
            .with_context(|| format!("Failed to count commits in {range}"))?;

        Ok(output.trim().parse::<usize>().ok())
    }

    fn path_changes_between(&self, from: &str, to: &str) -> Result<Option<Vec<PathChange>>> {
        validate_anchor(from)?;
        validate_anchor(to)?;

        // Each flag closes a way to hide a change from this list:
        // --no-renames      a rename shows as D old + A new, so the old side
        //                   is never dropped (diff.renames is on by default).
        // --no-relative     diff.relative=true would drop paths outside cwd.
        // --ignore-submodules=none
        //                   diff.ignoreSubmodules=all would hide gitlink moves.
        // --no-ext-diff     no external diff driver runs.
        let output = self
            .run_git(&[
                "diff",
                "-z",
                "--name-status",
                "--no-renames",
                "--no-relative",
                "--ignore-submodules=none",
                "--no-ext-diff",
                "--end-of-options",
                from,
                to,
                "--",
            ])
            .with_context(|| format!("Failed to list path changes from {from} to {to}"))?;

        parse_git_name_status_z(&output).map(Some)
    }

    fn discover_base_commit(&self, target: &str) -> Result<Option<String>> {
        validate_anchor(target)?;

        let target_commit = self.commit_for_anchor(target)?;

        let candidates = self
            .maybe_upstream_ref()
            .into_iter()
            .chain(GIT_BASE_CANDIDATES.iter().map(|c| (*c).to_string()));

        // Every candidate that resolves gives a merge base with the target.
        // All of them are ancestors of the target, so the nearest one bounds
        // the smallest range that still excludes every trunk commit. Taking
        // the first candidate instead picks a stale base whenever that ref
        // lags another: origin/main behind unpushed local main makes a new
        // review cover the unpushed trunk commits too.
        let mut bases = Vec::new();
        for candidate in candidates {
            let Some(base) = self.maybe_merge_base(&candidate, &target_commit) else {
                continue;
            };

            // The candidate is an ancestor of the target, or the target
            // itself. Either way the target is not ahead of it, so this ref
            // marks no feature range.
            if base == target_commit {
                continue;
            }

            bases.push(base);
        }

        Ok(nearest_base(bases, |ancestor, descendant| {
            self.is_ancestor(ancestor, descendant).unwrap_or(false)
        }))
    }
}

/// True when an upstream whose ref on its remote is `remote_ref` is the
/// remote copy of the branch `head_ref` itself (same branch name), rather
/// than another branch such as `main`.
fn is_own_remote_copy(head_ref: &str, remote_ref: &str) -> bool {
    remote_ref == head_ref
}

/// Pick the merge base nearest the target from `bases`, in priority order.
///
/// The result replaces the current pick only with a strict descendant of it.
/// When the bases are totally ordered by ancestry (the usual case: every trunk
/// ref is a copy of the same line of history at a different age), this is the
/// base that descends from all the others. When two bases are unrelated (both
/// sides of a merge), the earlier candidate wins against the later one, so the
/// result is deterministic and follows the candidate priority order.
pub(crate) fn nearest_base<T: PartialEq>(
    bases: impl IntoIterator<Item = T>,
    is_ancestor: impl Fn(&T, &T) -> bool,
) -> Option<T> {
    let mut best: Option<T> = None;
    for base in bases {
        best = match best {
            Some(current) if current == base || !is_ancestor(&current, &base) => Some(current),
            _ => Some(base),
        };
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn run_git_at(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(repo)
            .args(args)
            .status()
            .expect("failed to run git command");
        assert!(status.success(), "git command failed: {args:?}");
    }

    fn setup_git_repo() -> PathBuf {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        run_git_at(&path, &["init"]);
        run_git_at(&path, &["config", "user.email", "test@example.com"]);
        run_git_at(&path, &["config", "user.name", "Test User"]);
        std::fs::write(path.join("file.txt"), "line1\nline2\n").expect("write file");
        run_git_at(&path, &["add", "file.txt"]);
        run_git_at(&path, &["commit", "-m", "initial"]);
        std::mem::forget(dir);
        path
    }

    #[test]
    fn test_detect_git_root() {
        let repo = setup_git_repo();
        let detected = detect_git_root(&repo).expect("detect root");
        assert_eq!(detected, repo);
    }

    #[test]
    fn test_current_anchor_branch_ref() {
        let repo_path = setup_git_repo();
        let repo = GitRepo::new(repo_path);
        let anchor = repo.current_anchor().expect("anchor");
        assert!(anchor.starts_with("refs/heads/"));
    }

    /// Build the shape from bn-t1rl: a feature branch of three commits whose
    /// FIRST commit holds the substantive change. Returns the repo path.
    fn setup_three_commit_feature() -> PathBuf {
        let path = setup_git_repo();
        // The initial commit is on whatever branch git init chose. Name the
        // fork point explicitly so the test does not depend on init.defaultBranch.
        run_git_at(&path, &["branch", "-M", "main"]);

        run_git_at(&path, &["checkout", "-b", "feature"]);

        // Commit 1: the substantive change.
        std::fs::write(path.join("feature.rs"), "fn hook() {}\n").expect("write");
        run_git_at(&path, &["add", "feature.rs"]);
        run_git_at(&path, &["commit", "-m", "feat: add hook"]);

        // Commits 2 and 3: conflict resolution and integration.
        std::fs::write(path.join("resolved.txt"), "resolved\n").expect("write");
        run_git_at(&path, &["add", "resolved.txt"]);
        run_git_at(&path, &["commit", "-m", "fix: resolve conflict"]);

        std::fs::write(path.join("integration.txt"), "integrated\n").expect("write");
        run_git_at(&path, &["add", "integration.txt"]);
        run_git_at(&path, &["commit", "-m", "chore: integrate"]);

        path
    }

    #[test]
    fn test_discover_base_finds_branch_fork_point() {
        let repo_path = setup_three_commit_feature();
        let repo = GitRepo::new(repo_path);

        let target = repo.current_commit().expect("target");
        let fork_point = repo.commit_for_anchor("refs/heads/main").expect("main");

        let base = repo
            .discover_base_commit(&target)
            .expect("discover")
            .expect("a base");

        assert_eq!(base, fork_point);
        assert_ne!(
            base,
            repo.parent_commit(&target).expect("parent"),
            "discovery must not collapse to the tip parent"
        );
    }

    #[test]
    fn test_discovered_range_covers_the_whole_feature() {
        let repo_path = setup_three_commit_feature();
        let repo = GitRepo::new(repo_path);

        let target = repo.current_commit().expect("target");
        let base = repo
            .discover_base_commit(&target)
            .expect("discover")
            .expect("a base");

        assert_eq!(
            repo.count_commits_between(&base, &target).expect("count"),
            Some(3)
        );

        let files = repo.changed_files_between(&base, &target).expect("files");
        // The substantive first commit is the one the old tip-only range dropped.
        assert!(
            files.contains(&"feature.rs".to_string()),
            "first commit missing from range: {files:?}"
        );
        assert!(files.contains(&"resolved.txt".to_string()));
        assert!(files.contains(&"integration.txt".to_string()));
    }

    #[test]
    fn test_tip_only_range_drops_the_first_commit() {
        // Documents the old behaviour this change fixes.
        let repo_path = setup_three_commit_feature();
        let repo = GitRepo::new(repo_path);

        let target = repo.current_commit().expect("target");
        let tip_parent = repo.parent_commit(&target).expect("parent");

        let files = repo
            .changed_files_between(&tip_parent, &target)
            .expect("files");

        assert_eq!(files, vec!["integration.txt".to_string()]);
    }

    #[test]
    fn test_range_survives_a_commit_added_after_creation() {
        let repo_path = setup_three_commit_feature();
        let repo = GitRepo::new(repo_path.clone());

        let base = repo
            .discover_base_commit(&repo.current_commit().expect("target"))
            .expect("discover")
            .expect("a base");

        // A later commit on the same branch must not push earlier work out.
        std::fs::write(repo_path.join("later.txt"), "later\n").expect("write");
        run_git_at(&repo_path, &["add", "later.txt"]);
        run_git_at(&repo_path, &["commit", "-m", "fix: hardening"]);

        let new_target = repo.current_commit().expect("target");
        let files = repo
            .changed_files_between(&base, &new_target)
            .expect("files");

        assert!(files.contains(&"feature.rs".to_string()));
        assert!(files.contains(&"later.txt".to_string()));
        assert_eq!(
            repo.count_commits_between(&base, &new_target)
                .expect("count"),
            Some(4)
        );
    }

    #[test]
    fn test_is_ancestor_tracks_commits_added_after_approval() {
        let repo_path = setup_three_commit_feature();
        let repo = GitRepo::new(repo_path.clone());

        // Stand in for the commit an approval was recorded against.
        let approved = repo.current_commit().expect("approved");

        std::fs::write(repo_path.join("later.txt"), "later\n").expect("write");
        run_git_at(&repo_path, &["add", "later.txt"]);
        run_git_at(&repo_path, &["commit", "-m", "fix: hardening"]);
        let new_target = repo.current_commit().expect("target");

        assert!(repo.is_ancestor(&approved, &new_target).expect("ancestor"));
        assert!(!repo.is_ancestor(&new_target, &approved).expect("ancestor"));
        assert_eq!(
            repo.count_commits_between(&approved, &new_target)
                .expect("count"),
            Some(1)
        );
    }

    #[test]
    fn test_is_ancestor_is_false_for_unrelated_branches() {
        let repo_path = setup_three_commit_feature();
        let repo = GitRepo::new(repo_path.clone());

        let feature_tip = repo.current_commit().expect("tip");

        run_git_at(&repo_path, &["checkout", "-q", "main"]);
        run_git_at(&repo_path, &["checkout", "-qb", "other"]);
        std::fs::write(repo_path.join("other.txt"), "other\n").expect("write");
        run_git_at(&repo_path, &["add", "other.txt"]);
        run_git_at(&repo_path, &["commit", "-m", "feat: other"]);
        let other_tip = repo.current_commit().expect("other tip");

        assert!(!repo
            .is_ancestor(&feature_tip, &other_tip)
            .expect("ancestor"));
    }

    #[test]
    fn test_discover_base_returns_none_on_the_default_branch() {
        // No fork point exists, so callers keep the single-commit fallback.
        let repo_path = setup_git_repo();
        run_git_at(&repo_path, &["branch", "-M", "main"]);
        let repo = GitRepo::new(repo_path.clone());

        std::fs::write(repo_path.join("second.txt"), "second\n").expect("write");
        run_git_at(&repo_path, &["add", "second.txt"]);
        run_git_at(&repo_path, &["commit", "-m", "second"]);

        let target = repo.current_commit().expect("target");
        assert_eq!(repo.discover_base_commit(&target).expect("discover"), None);
    }

    /// Seen in seal itself (bn-jnp9): origin/main lagged local main by
    /// unpushed commits, and discovery took origin/HEAD first, so a new review
    /// covered the unpushed trunk commits too. The nearest merge base wins.
    #[test]
    fn test_discover_base_prefers_local_main_over_lagging_origin() {
        let path = setup_git_repo();
        run_git_at(&path, &["branch", "-M", "main"]);
        let repo = GitRepo::new(path.clone());
        let pushed = repo.current_commit().expect("pushed");
        for name in ["unpushed1", "unpushed2"] {
            std::fs::write(path.join(name), "u\n").expect("write");
            run_git_at(&path, &["add", name]);
            run_git_at(&path, &["commit", "-m", name]);
        }
        let local_main = repo.current_commit().expect("local main");
        run_git_at(&path, &["update-ref", "refs/remotes/origin/main", &pushed]);
        run_git_at(
            &path,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );

        run_git_at(&path, &["checkout", "-q", "--detach"]);
        std::fs::write(path.join("work.txt"), "w\n").expect("write");
        run_git_at(&path, &["add", "work.txt"]);
        run_git_at(&path, &["commit", "-m", "work"]);
        let target = repo.current_commit().expect("target");

        let base = repo
            .discover_base_commit(&target)
            .expect("discover")
            .expect("a base");
        assert_eq!(base, local_main);
        assert_eq!(
            repo.count_commits_between(&base, &target).expect("count"),
            Some(1)
        );
    }

    #[test]
    fn test_nearest_base_picks_descendant_else_first() {
        // Total order by `<=`: the greatest element wins, in any order.
        let le = |a: &u32, b: &u32| a <= b;
        assert_eq!(nearest_base([1, 3, 2], le), Some(3));
        assert_eq!(nearest_base([3, 1, 2], le), Some(3));
        assert_eq!(nearest_base(Vec::<u32>::new(), le), None);
        // Unrelated bases: the earlier candidate keeps priority.
        let unrelated = |a: &u32, b: &u32| a == b;
        assert_eq!(nearest_base([7, 5], unrelated), Some(7));
    }
}

//! JJ (Jujutsu) command wrapper for botseal.
//!
//! Provides a structured interface for executing jj commands and parsing their output.

pub mod context;
pub mod drift;

pub use context::{extract_context, format_context, CodeContext, ContextLine};
pub use drift::{calculate_drift, DriftResult, RangeDrift};

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolve the local jj repo/workspace root from any path within the repo.
///
/// This returns the directory containing `.jj/`, WITHOUT following workspace pointers.
/// Use this when you need the current workspace's root (e.g., for writing files that
/// should be tracked in the workspace's working copy).
///
/// # Algorithm
/// 1. Start from `start_path` and walk up to find `.jj/` directory
/// 2. Return the parent of `.jj/` (the workspace root)
///
/// # Errors
///
/// Returns an error if no `.jj/` directory is found.
pub fn resolve_workspace_root(start_path: &Path) -> Result<PathBuf> {
    // Walk up to find .jj directory
    let mut current = start_path.to_path_buf();
    loop {
        let jj_path = current.join(".jj");
        if jj_path.exists() {
            return Ok(current);
        }
        if !current.pop() {
            bail!("Not in a jj repository (no .jj directory found)");
        }
    }
}

/// Resolve the canonical jj repo root from any path within the repo.
///
/// In jj workspaces, each workspace has its own working copy directory,
/// but `.jj/repo` is a file containing the path to the main repo's `.jj/repo` directory.
/// This function follows that pointer to find the main repo root.
///
/// NOTE: For most seal operations, use `resolve_workspace_root()` instead.
/// This function is useful when you need to find the main repo (e.g., for
/// cross-workspace operations).
///
/// # Algorithm
/// 1. Start from `start_path` and walk up to find `.jj/` directory
/// 2. If `.jj/repo` is a directory, this is the main repo - return its parent
/// 3. If `.jj/repo` is a file, read it to get the path to the main repo's `.jj/repo`
/// 4. Return the parent of that `.jj/repo` directory (the main repo root)
///
/// # Errors
///
/// Returns an error if no `.jj/` directory is found or the repo structure is invalid.
pub fn resolve_repo_root(start_path: &Path) -> Result<PathBuf> {
    // Walk up to find .jj directory
    let mut current = start_path.to_path_buf();
    let jj_dir = loop {
        let jj_path = current.join(".jj");
        if jj_path.exists() {
            break jj_path;
        }
        if !current.pop() {
            bail!("Not in a jj repository (no .jj directory found)");
        }
    };

    let repo_pointer = jj_dir.join("repo");

    if repo_pointer.is_dir() {
        // This is the main repo - .jj/repo is a directory
        // Return the parent of .jj (the repo root)
        Ok(jj_dir
            .parent()
            .context("Invalid jj directory structure")?
            .to_path_buf())
    } else if repo_pointer.is_file() {
        // This is a workspace - .jj/repo is a file containing path to main repo's .jj/repo
        let main_repo_jj_repo = fs::read_to_string(&repo_pointer).with_context(|| {
            format!(
                "Failed to read workspace pointer: {}",
                repo_pointer.display()
            )
        })?;
        let main_repo_jj_repo_raw = PathBuf::from(main_repo_jj_repo.trim());
        let main_repo_jj_repo = if main_repo_jj_repo_raw.is_absolute() {
            main_repo_jj_repo_raw
        } else {
            jj_dir.join(main_repo_jj_repo_raw)
        };

        // The path points to the main repo's .jj/repo directory
        // Return the parent of .jj (two levels up from .jj/repo)
        main_repo_jj_repo
            .parent() // .jj
            .and_then(|jj| jj.parent()) // repo root
            .map(std::path::Path::to_path_buf)
            .context("Invalid main repo path in workspace pointer")
    } else {
        bail!("Invalid jj repository structure: .jj/repo is neither file nor directory");
    }
}

/// Resolve the .seal repository root from a given path.
///
/// This function intelligently finds the .seal directory from various input paths:
/// - If path points to .seal directory itself, returns parent
/// - If path points to repo root (contains .seal), returns it
/// - If path points to subdirectory, walks up to find .seal
///
/// # Arguments
///
/// * `path` - Starting path (can be .seal dir, repo root, or subdirectory)
///
/// # Returns
///
/// The repository root directory (parent of .seal)
///
/// # Errors
///
/// Returns an error if no .seal directory is found in the path hierarchy.
pub fn resolve_seal_root_from_path(path: &Path) -> Result<PathBuf> {
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("Failed to resolve path: {}", path.display()))?;

    // If path points directly to .seal, return its parent
    if canonical_path.file_name() == Some(std::ffi::OsStr::new(".seal")) {
        return canonical_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .context(".seal directory has no parent");
    }

    // Walk up to find .seal directory
    let mut current = canonical_path.clone();
    loop {
        let crit_path = current.join(".seal");
        if crit_path.is_dir() {
            return Ok(current);
        }

        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => bail!(
                "No .seal directory found in path hierarchy.\nSearched from: {}",
                canonical_path.display()
            ),
        }
    }
}

/// Local bookmarks that name the trunk, for base discovery.
const LOCAL_TRUNK_BOOKMARKS: &str =
    r#"bookmarks(exact:"main") | bookmarks(exact:"master") | bookmarks(exact:"trunk")"#;

/// Wrapper for executing jj commands against a repository.
#[derive(Debug, Clone)]
pub struct JjRepo {
    repo_path: PathBuf,
}

impl JjRepo {
    /// Create a new `JjRepo` wrapper for the given repository path.
    #[must_use]
    pub fn new(repo_path: &Path) -> Self {
        Self {
            repo_path: repo_path.to_path_buf(),
        }
    }

    /// Get the repository root path.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.repo_path
    }

    /// Execute a jj command and return its stdout.
    ///
    /// Always uses `--color=never` for parseable output.
    fn run_jj(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("jj");
        cmd.current_dir(&self.repo_path)
            .arg("--color=never")
            .args(args);

        let output = cmd.output().with_context(|| {
            if let Err(e) = which::which("jj") {
                format!("jj command not found. Please install jj (Jujutsu): {e}")
            } else {
                format!("Failed to execute jj command: {args:?}")
            }
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "jj command failed with status {}: {}",
                output.status,
                stderr.trim()
            );
        }

        let stdout = String::from_utf8(output.stdout).context("jj output was not valid UTF-8")?;

        Ok(stdout)
    }

    /// Execute a jj command and return stdout, ignoring exit code.
    ///
    /// Used for commands like `file list` where exit code 0 doesn't mean success.
    fn run_jj_ignore_status(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("jj");
        cmd.current_dir(&self.repo_path)
            .arg("--color=never")
            .args(args);

        let output = cmd.output().with_context(|| {
            if let Err(e) = which::which("jj") {
                format!("jj command not found. Please install jj (Jujutsu): {e}")
            } else {
                format!("Failed to execute jj command: {args:?}")
            }
        })?;

        let stdout = String::from_utf8(output.stdout).context("jj output was not valid UTF-8")?;

        Ok(stdout)
    }

    /// Get the `change_id` for the current working copy (@).
    ///
    /// The `change_id` is jj's stable identifier that survives rewrites.
    ///
    /// # Errors
    ///
    /// Returns an error if the jj command fails or produces invalid output.
    pub fn get_current_change_id(&self) -> Result<String> {
        let output = self
            .run_jj(&["log", "-r", "@", "--no-graph", "-T", "change_id"])
            .context("Failed to get current change_id")?;

        Ok(output.trim().to_string())
    }

    /// Get the `commit_id` (Git SHA) for the current working copy (@).
    ///
    /// # Errors
    ///
    /// Returns an error if the jj command fails or produces invalid output.
    pub fn get_current_commit(&self) -> Result<String> {
        let output = self
            .run_jj(&["log", "-r", "@", "--no-graph", "-T", "commit_id"])
            .context("Failed to get current commit_id")?;

        Ok(output.trim().to_string())
    }

    /// Get the `commit_id` (Git SHA) for a given revset.
    ///
    /// # Errors
    ///
    /// Returns an error if the jj command fails or the revset is invalid.
    pub fn get_commit_for_rev(&self, rev: &str) -> Result<String> {
        let output = self
            .run_jj(&["log", "-r", rev, "--no-graph", "-T", "commit_id"])
            .with_context(|| format!("Failed to get commit_id for {rev}"))?;

        Ok(output.trim().to_string())
    }

    /// Get the parent `commit_id` for a given commit.
    ///
    /// Uses jj's `parents()` revset function to find the parent.
    /// For commits with multiple parents (merges), returns the first parent.
    ///
    /// # Errors
    ///
    /// Returns an error if the commit has no parents (root) or the command fails.
    pub fn get_parent_commit(&self, commit: &str) -> Result<String> {
        let revset = format!("parents({commit})");
        let output = self
            .run_jj(&["log", "-r", &revset, "--no-graph", "-T", "commit_id"])
            .with_context(|| format!("Failed to get parent of {commit}"))?;

        let parent = output.trim();
        if parent.is_empty() {
            bail!("Commit {commit} has no parent (root commit)");
        }
        // If multiple parents, take the first one
        Ok(parent.lines().next().unwrap_or(parent).to_string())
    }

    /// Get a git-format diff between two revisions.
    ///
    /// Both `from` and `to` should be valid jj revsets (e.g., "@", `root()`, `change_id`).
    ///
    /// # Errors
    ///
    /// Returns an error if the jj command fails or the revsets are invalid.
    pub fn diff_git(&self, from: &str, to: &str) -> Result<String> {
        self.run_jj(&["diff", "--from", from, "--to", to, "--git"])
            .with_context(|| format!("Failed to get diff from {from} to {to}"))
    }

    /// Get a git-format diff for a specific file between two revisions.
    ///
    /// # Errors
    ///
    /// Returns an error if the jj command fails or the revsets/file are invalid.
    pub fn diff_git_file(&self, from: &str, to: &str, file: &str) -> Result<String> {
        self.run_jj(&["diff", "--from", from, "--to", to, "--git", file])
            .with_context(|| format!("Failed to get diff for file {file} from {from} to {to}"))
    }

    /// Check if a file exists at a given revision.
    ///
    /// Note: jj file list returns exit code 0 even for non-existent files,
    /// so we check stdout instead.
    ///
    /// # Errors
    ///
    /// Returns an error if the jj command fails to execute.
    pub fn file_exists(&self, rev: &str, path: &str) -> Result<bool> {
        let output = self
            .run_jj_ignore_status(&["file", "list", "-r", rev, path])
            .with_context(|| format!("Failed to check if file {path} exists at {rev}"))?;

        Ok(!output.trim().is_empty())
    }

    /// Get the contents of a file at a given revision.
    ///
    /// # Errors
    ///
    /// Returns an error if the file doesn't exist or the jj command fails.
    pub fn show_file(&self, rev: &str, path: &str) -> Result<String> {
        self.run_jj(&["file", "show", "-r", rev, path])
            .with_context(|| format!("Failed to show file {path} at {rev}"))
    }

    /// List files changed in a revision (compared to its parent).
    ///
    /// # Errors
    ///
    /// Returns an error if the jj command fails or the revision is invalid.
    pub fn changed_files(&self, rev: &str) -> Result<Vec<String>> {
        let output = self
            .run_jj(&["diff", "-r", rev, "--name-only"])
            .with_context(|| format!("Failed to list changed files for {rev}"))?;

        Ok(output
            .lines()
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect())
    }

    /// List files changed between two revisions.
    ///
    /// # Errors
    ///
    /// Returns an error if the jj command fails or the revsets are invalid.
    pub fn changed_files_between(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let output = self
            .run_jj(&["diff", "--from", from, "--to", to, "--name-only"])
            .with_context(|| format!("Failed to list changed files from {from} to {to}"))?;

        Ok(output
            .lines()
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect())
    }

    /// List every path that differs between the trees of `from` and `to`.
    ///
    /// `--name-only` prints only the new side of a rename, so this uses a
    /// template that names both sides. Fields are NUL-terminated, five per
    /// entry: status, source path, target path, source type, target type.
    ///
    /// # Errors
    ///
    /// Returns an error if the jj command fails or its output does not parse.
    pub fn path_changes_between(
        &self,
        from: &str,
        to: &str,
    ) -> Result<Vec<crate::scm::PathChange>> {
        const TEMPLATE: &str = r#"status ++ "\0" ++ source.path() ++ "\0" ++ target.path() ++ "\0" ++ source.file_type() ++ "\0" ++ target.file_type() ++ "\0""#;

        let output = self
            .run_jj(&["diff", "--from", from, "--to", to, "-T", TEMPLATE])
            .with_context(|| format!("Failed to list path changes from {from} to {to}"))?;

        parse_jj_diff_entries(&output)
    }

    /// Count the commits in the revset `base..target`.
    ///
    /// # Errors
    ///
    /// Returns an error if the jj command fails.
    pub fn count_commits_between(&self, base: &str, target: &str) -> Result<usize> {
        let revset = format!("{base}..{target}");
        let output = self
            .run_jj(&["log", "-r", &revset, "--no-graph", "-T", r#""x\n""#])
            .with_context(|| format!("Failed to count commits in {revset}"))?;

        Ok(output.lines().filter(|line| !line.is_empty()).count())
    }

    /// Report whether `ancestor` is an ancestor of `descendant`.
    ///
    /// # Errors
    ///
    /// Returns an error if the jj command fails.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let revset = format!("{ancestor} & ::{descendant}");
        let output = self
            .run_jj(&["log", "-r", &revset, "--no-graph", "-T", "commit_id"])
            .with_context(|| format!("Failed to test whether {ancestor} precedes {descendant}"))?;

        Ok(!output.trim().is_empty())
    }

    /// Find the newest immutable ancestor of `target`.
    ///
    /// This is where the mutable feature work forked from published history,
    /// so it is the base of the range under review.
    ///
    /// The root commit is excluded on purpose. A repository with no remote
    /// has no trunk, so `immutable()` holds only the root, and accepting it
    /// would widen every review to the whole history. `None` then tells the
    /// caller to keep the single-commit fallback, and the author passes
    /// `--base` to state the range.
    ///
    /// Also returns `None` when this jj version rejects the revset.
    ///
    /// # Errors
    ///
    /// Never returns an error. The signature matches the SCM trait, where
    /// other backends can fail.
    pub fn find_immutable_base(&self, target: &str) -> Result<Option<String>> {
        // `immutable()` follows `trunk()`, which is the REMOTE trunk bookmark.
        // Local trunk commits that are not pushed yet are trunk too, so the
        // history of the local main/master/trunk bookmarks counts as base
        // material. Otherwise a lagging main@origin makes the range include
        // the unpushed trunk commits.
        //
        // heads() of an ancestor set of `target` usually has one member. When
        // it has several (unrelated trunk sides of a merge), `latest()` picks
        // one deterministically by committer date.
        let with_local_trunk = format!(
            "latest(heads(::{target} & ((immutable() | ::({LOCAL_TRUNK_BOOKMARKS})) ~ root())))"
        );
        let immutable_only = format!("latest(heads(::{target} & (immutable() ~ root())))");
        let Some(output) = [with_local_trunk, immutable_only]
            .iter()
            .find_map(|revset| {
                self.run_jj(&["log", "-r", revset, "--no-graph", "-T", "commit_id"])
                    .ok()
            })
        else {
            // Older jj versions have no immutable() revset function.
            return Ok(None);
        };

        Ok(output
            .lines()
            .find(|line| !line.trim().is_empty())
            .map(|line| line.trim().to_string()))
    }
}

/// Parse the NUL-separated output of [`JjRepo::path_changes_between`].
///
/// Any status or type combination this code does not know becomes
/// [`crate::scm::PathChange::Other`], which never passes as harmless.
///
/// # Errors
///
/// Returns an error when the output does not split into whole entries.
pub fn parse_jj_diff_entries(output: &str) -> Result<Vec<crate::scm::PathChange>> {
    use crate::scm::PathChange;

    let Some(body) = output.strip_suffix('\0') else {
        if output.is_empty() {
            return Ok(Vec::new());
        }
        bail!("jj diff: output does not end with a field terminator");
    };

    let fields: Vec<&str> = body.split('\0').collect();
    if !fields.len().is_multiple_of(5) {
        bail!("jj diff: expected 5 fields per entry, got {}", fields.len());
    }

    let (entries, _) = fields.as_chunks::<5>();
    let mut changes = Vec::with_capacity(entries.len());
    for [status, source, target, source_type, target_type] in entries {
        if source.is_empty() || target.is_empty() {
            bail!("jj diff: entry with status {status} has an empty path");
        }

        let change = match *status {
            "added" => PathChange::Added((*target).to_string()),
            "removed" => PathChange::Deleted((*source).to_string()),
            "modified"
                if source == target && source_type == target_type && *source_type != "conflict" =>
            {
                PathChange::Modified((*target).to_string())
            }
            "renamed" => PathChange::Renamed {
                from: (*source).to_string(),
                to: (*target).to_string(),
            },
            "copied" => PathChange::Copied {
                from: (*source).to_string(),
                to: (*target).to_string(),
            },
            _ => PathChange::Other((*target).to_string()),
        };
        changes.push(change);
    }

    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    /// Get the repo path for testing. Uses `CARGO_MANIFEST_DIR` which points to
    /// the botseal repo root.
    fn test_repo() -> Option<JjRepo> {
        let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        let manifest_path = Path::new(&manifest_dir);
        if resolve_workspace_root(manifest_path).is_err() {
            return None;
        }
        Some(JjRepo::new(manifest_path))
    }

    /// Build a jj repo holding a three-change feature whose FIRST change
    /// carries the substantive work. Mirrors the bn-t1rl repro.
    ///
    /// Returns `None` when jj is not installed, so the suite still runs.
    fn setup_three_change_feature() -> Option<(JjRepo, PathBuf)> {
        if which::which("jj").is_err() {
            return None;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();

        let run = |args: &[&str]| {
            let status = Command::new("jj")
                .current_dir(&path)
                .arg("--color=never")
                .args(args)
                .status()
                .expect("failed to run jj");
            assert!(status.success(), "jj command failed: {args:?}");
        };

        run(&["git", "init"]);
        run(&["config", "set", "--repo", "user.name", "Test User"]);
        run(&["config", "set", "--repo", "user.email", "test@example.com"]);

        // Change 1: the substantive work.
        std::fs::write(path.join("feature.rs"), "fn hook() {}\n").expect("write");
        run(&["describe", "-m", "feat: add hook"]);
        run(&["new"]);

        // Changes 2 and 3: conflict resolution and integration.
        std::fs::write(path.join("resolved.txt"), "resolved\n").expect("write");
        run(&["describe", "-m", "fix: resolve conflict"]);
        run(&["new"]);

        std::fs::write(path.join("integration.txt"), "integrated\n").expect("write");
        run(&["describe", "-m", "chore: integrate"]);

        std::mem::forget(dir);
        Some((JjRepo::new(&path), path))
    }

    #[test]
    fn test_jj_explicit_base_covers_the_whole_feature() {
        let Some((repo, _path)) = setup_three_change_feature() else {
            return;
        };

        let target = repo.get_current_commit().expect("target");
        // The base is the commit the feature forked from: the parent of the
        // first change. This is what `--base` persists on the review.
        let base = repo
            .get_commit_for_rev("roots(mutable())-")
            .expect("fork point");

        assert_eq!(
            repo.count_commits_between(&base, &target).expect("count"),
            3
        );

        let files = repo.changed_files_between(&base, &target).expect("files");
        assert!(
            files.contains(&"feature.rs".to_string()),
            "first change missing from range: {files:?}"
        );
        assert!(files.contains(&"resolved.txt".to_string()));
        assert!(files.contains(&"integration.txt".to_string()));
    }

    #[test]
    fn test_jj_tip_only_range_drops_the_first_change() {
        // Documents the old behaviour this change fixes.
        let Some((repo, _path)) = setup_three_change_feature() else {
            return;
        };

        let target = repo.get_current_commit().expect("target");
        let tip_parent = repo.get_parent_commit(&target).expect("parent");

        let files = repo
            .changed_files_between(&tip_parent, &target)
            .expect("files");

        assert_eq!(files, vec!["integration.txt".to_string()]);
    }

    #[test]
    fn test_jj_explicit_base_survives_a_change_added_after_creation() {
        let Some((repo, path)) = setup_three_change_feature() else {
            return;
        };

        let base = repo
            .get_commit_for_rev("roots(mutable())-")
            .expect("fork point");

        // A later change on the same stack must not push earlier work out.
        let status = Command::new("jj")
            .current_dir(&path)
            .args(["--color=never", "new"])
            .status()
            .expect("jj new");
        assert!(status.success());
        std::fs::write(path.join("later.txt"), "later\n").expect("write");

        let new_target = repo.get_current_commit().expect("target");
        let files = repo
            .changed_files_between(&base, &new_target)
            .expect("files");

        assert!(files.contains(&"feature.rs".to_string()));
        assert!(files.contains(&"later.txt".to_string()));
        assert_eq!(
            repo.count_commits_between(&base, &new_target)
                .expect("count"),
            4
        );
    }

    #[test]
    fn test_jj_is_ancestor_tracks_changes_added_after_approval() {
        let Some((repo, path)) = setup_three_change_feature() else {
            return;
        };

        // Stand in for the commit an approval was recorded against.
        let approved = repo.get_current_commit().expect("approved");

        let status = Command::new("jj")
            .current_dir(&path)
            .args(["--color=never", "new"])
            .status()
            .expect("jj new");
        assert!(status.success());
        std::fs::write(path.join("later.txt"), "later\n").expect("write");

        let new_target = repo.get_current_commit().expect("target");

        assert!(repo.is_ancestor(&approved, &new_target).expect("ancestor"));
        assert!(!repo.is_ancestor(&new_target, &approved).expect("ancestor"));
        assert_eq!(
            repo.count_commits_between(&approved, &new_target)
                .expect("count"),
            1
        );
    }

    /// Unpushed local trunk commits are trunk. With no remote, `immutable()`
    /// is only the root, so before local trunk bookmarks counted, discovery
    /// found nothing (and with a lagging main@origin it found a stale base).
    #[test]
    fn test_jj_discovery_uses_local_trunk_bookmark() {
        if which::which("jj").is_err() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        let run = |args: &[&str]| {
            let status = Command::new("jj")
                .current_dir(&path)
                .arg("--color=never")
                .args(args)
                .status()
                .expect("failed to run jj");
            assert!(status.success(), "jj command failed: {args:?}");
        };
        run(&["git", "init"]);
        run(&["config", "set", "--repo", "user.name", "Test User"]);
        run(&["config", "set", "--repo", "user.email", "test@example.com"]);
        for name in ["trunk1", "trunk2"] {
            std::fs::write(path.join(name), "t\n").expect("write");
            run(&["commit", "-m", name]);
        }
        run(&["bookmark", "create", "main", "-r", "@-"]);
        for name in ["f1", "f2", "f3"] {
            std::fs::write(path.join(name), "f\n").expect("write");
            run(&["commit", "-m", name]);
        }
        let repo = JjRepo::new(&path);
        let main = repo.get_commit_for_rev("main").expect("main");
        let target = repo.get_commit_for_rev("@-").expect("target");
        let base = repo
            .find_immutable_base(&target)
            .expect("discover")
            .expect("a base");
        assert_eq!(base, main);
        assert_eq!(
            repo.count_commits_between(&base, &target).expect("count"),
            3
        );
        std::mem::forget(dir);
    }

    /// The jj analogue of a `push -u` upstream: a pushed feature bookmark is
    /// tracked, so `feature@origin` is mutable and not in `immutable()`, and
    /// the local-trunk revset names only main/master/trunk. Discovery must
    /// keep the fork from main, not the pushed feature tip.
    #[test]
    fn test_jj_discovery_ignores_pushed_feature_bookmark() {
        if which::which("jj").is_err() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let origin = dir.path().join("origin.git");
        let path = dir.path().join("work");
        std::fs::create_dir(&path).expect("mkdir");
        let status = Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&origin)
            .status()
            .expect("git init");
        assert!(status.success());
        let run = |args: &[&str]| {
            let output = Command::new("jj")
                .current_dir(&path)
                .args([
                    "--color=never",
                    "--config",
                    "user.name=T",
                    "--config",
                    "user.email=t@e",
                ])
                .args(args)
                .output()
                .expect("failed to run jj");
            assert!(
                output.status.success(),
                "jj {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["git", "init"]);
        run(&[
            "git",
            "remote",
            "add",
            "origin",
            origin.to_str().expect("utf8"),
        ]);
        std::fs::write(path.join("trunk"), "t\n").expect("write");
        run(&["commit", "-m", "trunk"]);
        run(&["bookmark", "create", "main", "-r", "@-"]);
        run(&["git", "push", "--bookmark", "main"]);
        for name in ["f1", "f2", "f3"] {
            std::fs::write(path.join(name), "f\n").expect("write");
            run(&["commit", "-m", name]);
        }
        run(&["bookmark", "create", "feature", "-r", "@-"]);
        run(&["git", "push", "--bookmark", "feature"]);
        std::fs::write(path.join("f4"), "f\n").expect("write");
        run(&["commit", "-m", "f4"]);

        let repo = JjRepo::new(&path);
        let main = repo.get_commit_for_rev("main").expect("main");
        let pushed = repo.get_commit_for_rev("feature@origin").expect("pushed");
        let target = repo.get_commit_for_rev("@-").expect("target");
        assert_ne!(pushed, main);
        let base = repo
            .find_immutable_base(&target)
            .expect("discover")
            .expect("a base");
        assert_eq!(base, main);
        assert_eq!(
            repo.count_commits_between(&base, &target).expect("count"),
            4
        );
    }

    #[test]
    fn test_jj_discovery_declines_the_root_commit() {
        // With no trunk, immutable() holds only root(). Widening a review to
        // the whole history would be wrong, so discovery returns None and the
        // caller keeps the single-commit fallback.
        let Some((repo, _path)) = setup_three_change_feature() else {
            return;
        };

        let target = repo.get_current_commit().expect("target");
        assert_eq!(repo.find_immutable_base(&target).expect("discover"), None);
    }

    #[test]
    fn test_get_current_change_id() {
        let Some(repo) = test_repo() else {
            return;
        };
        let change_id = repo.get_current_change_id().unwrap();

        // Change IDs are 32 lowercase hex chars
        assert_eq!(change_id.len(), 32);
        assert!(change_id.chars().all(|c| c.is_ascii_lowercase()));
    }

    #[test]
    fn test_get_current_commit() {
        let Some(repo) = test_repo() else {
            return;
        };
        let commit_id = repo.get_current_commit().unwrap();

        // Git commit IDs are 40 hex chars
        assert_eq!(commit_id.len(), 40);
        assert!(commit_id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_diff_git() {
        let Some(repo) = test_repo() else {
            return;
        };
        // Diff from root to current - should always work
        let diff = repo.diff_git("root()", "@");
        assert!(diff.is_ok(), "diff should succeed: {:?}", diff.err());
    }

    #[test]
    fn test_diff_git_file() {
        let Some(repo) = test_repo() else {
            return;
        };
        // Diff a file that definitely exists
        let diff = repo.diff_git_file("@-", "@", "Cargo.toml");
        // This may error if Cargo.toml wasn't changed, but the command structure is valid
        // The important thing is the jj command executes without crashing
        let _ = diff; // May succeed or fail depending on changes
    }

    #[test]
    fn test_file_exists() {
        let Some(repo) = test_repo() else {
            return;
        };

        // Cargo.toml should exist at @
        let exists = repo.file_exists("@", "Cargo.toml").unwrap();
        assert!(exists, "Cargo.toml should exist");

        // A nonsense file should not exist
        let exists = repo
            .file_exists("@", "this-file-definitely-does-not-exist-xyz.txt")
            .unwrap();
        assert!(!exists, "Non-existent file should return false");
    }

    #[test]
    fn test_show_file() {
        let Some(repo) = test_repo() else {
            return;
        };

        // Should be able to read Cargo.toml
        let contents = repo.show_file("@", "Cargo.toml").unwrap();
        assert!(contents.contains("[package]"));
        assert!(contents.contains("seal"));
    }

    #[test]
    fn test_changed_files() {
        let Some(repo) = test_repo() else {
            return;
        };

        // Get changed files for current commit
        let files = repo.changed_files("@");
        assert!(files.is_ok());
        // Files is a Vec<String>, each entry is a path
        let files = files.unwrap();
        for file in &files {
            assert!(!file.is_empty());
            assert!(!file.contains('\n'));
        }
    }

    #[test]
    fn test_resolve_workspace_root() {
        // Use CARGO_MANIFEST_DIR which points to the botseal repo root
        let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        let manifest_path = Path::new(&manifest_dir);

        let result = resolve_workspace_root(manifest_path);
        if manifest_path.join(".jj").exists() {
            let root = result.unwrap();
            assert!(
                root.join(".jj").exists(),
                "Root should contain .jj directory"
            );

            // Should also work from a subdirectory
            let subdir = manifest_path.join("src");
            let root_from_subdir = resolve_workspace_root(&subdir).unwrap();
            assert_eq!(
                root, root_from_subdir,
                "Should find same root from subdirectory"
            );
        } else {
            assert!(result.is_err(), "Expected no jj repo in test environment");
        }
    }

    #[test]
    fn test_resolve_workspace_root_not_in_repo() {
        // /tmp is unlikely to be inside a jj repo
        let result = resolve_workspace_root(Path::new("/tmp"));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Not in a jj repository"));
    }
}

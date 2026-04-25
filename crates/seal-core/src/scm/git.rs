use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::scm::{validate_anchor, validate_repo_relative_path, ScmKind, ScmRepo};

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
}

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
}

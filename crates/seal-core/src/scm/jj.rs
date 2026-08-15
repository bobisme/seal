use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::jj::{resolve_workspace_root, JjRepo};
use crate::scm::{validate_anchor, validate_repo_relative_path, ScmKind, ScmRepo};

#[derive(Debug, Clone)]
pub struct JjScmRepo {
    inner: JjRepo,
    root: PathBuf,
}

impl JjScmRepo {
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self {
            inner: JjRepo::new(root),
            root: root.to_path_buf(),
        }
    }
}

#[must_use]
pub fn detect_jj_root(start_path: &Path) -> Option<PathBuf> {
    resolve_workspace_root(start_path).ok()
}

impl ScmRepo for JjScmRepo {
    fn kind(&self) -> ScmKind {
        ScmKind::Jj
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn current_anchor(&self) -> Result<String> {
        self.inner.get_current_change_id()
    }

    fn current_commit(&self) -> Result<String> {
        self.inner.get_current_commit()
    }

    fn commit_for_anchor(&self, anchor: &str) -> Result<String> {
        validate_anchor(anchor)?;
        self.inner.get_commit_for_rev(anchor)
    }

    fn parent_commit(&self, commit: &str) -> Result<String> {
        validate_anchor(commit)?;
        self.inner.get_parent_commit(commit)
    }

    fn diff_git(&self, from: &str, to: &str) -> Result<String> {
        validate_anchor(from)?;
        validate_anchor(to)?;
        self.inner.diff_git(from, to)
    }

    fn diff_git_file(&self, from: &str, to: &str, file: &str) -> Result<String> {
        validate_anchor(from)?;
        validate_anchor(to)?;
        validate_repo_relative_path(file)?;
        self.inner.diff_git_file(from, to, file)
    }

    fn changed_files_between(&self, from: &str, to: &str) -> Result<Vec<String>> {
        validate_anchor(from)?;
        validate_anchor(to)?;
        self.inner.changed_files_between(from, to)
    }

    fn file_exists(&self, rev: &str, path: &str) -> Result<bool> {
        validate_anchor(rev)?;
        validate_repo_relative_path(path)?;
        self.inner.file_exists(rev, path)
    }

    fn show_file(&self, rev: &str, path: &str) -> Result<String> {
        validate_anchor(rev)?;
        validate_repo_relative_path(path)?;
        self.inner
            .show_file(rev, path)
            .with_context(|| format!("Failed to show file {path} at {rev}"))
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        validate_anchor(ancestor)?;
        validate_anchor(descendant)?;
        self.inner.is_ancestor(ancestor, descendant)
    }

    fn count_commits_between(&self, base: &str, target: &str) -> Result<Option<usize>> {
        validate_anchor(base)?;
        validate_anchor(target)?;
        self.inner.count_commits_between(base, target).map(Some)
    }

    fn discover_base_commit(&self, target: &str) -> Result<Option<String>> {
        validate_anchor(target)?;

        let target_commit = self.inner.get_commit_for_rev(target)?;
        let Some(base) = self.inner.find_immutable_base(&target_commit)? else {
            return Ok(None);
        };

        // The target is itself immutable, so it starts no feature range.
        if base == target_commit {
            return Ok(None);
        }

        Ok(Some(base))
    }
}

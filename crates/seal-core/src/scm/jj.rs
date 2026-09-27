use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::jj::{resolve_workspace_root, JjRepo};
use crate::scm::{validate_anchor, validate_repo_relative_path, PathChange, ScmKind, ScmRepo};

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

    fn path_changes_between(&self, from: &str, to: &str) -> Result<Option<Vec<PathChange>>> {
        validate_anchor(from)?;
        validate_anchor(to)?;
        self.inner.path_changes_between(from, to).map(Some)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval_exempt::range_is_exempt;
    use crate::jj::parse_jj_diff_entries;
    use std::process::Command;

    #[test]
    fn parse_jj_entries_keeps_both_rename_sides() {
        let out = "renamed\0src/x.rs\0.seal/x.rs\0file\0file\0\
                   modified\0.seal/a\0.seal/a\0file\0file\0\
                   modified\0.seal/b\0.seal/b\0file\0symlink\0\
                   modified\0.seal/c\0.seal/c\0conflict\0conflict\0\
                   removed\0d\0d\0file\0\0\
                   added\0n\0n\0\0file\0\
                   copied\0.seal/p\0.seal/q\0file\0file\0\
                   weird\0w\0w\0file\0file\0";
        assert_eq!(
            parse_jj_diff_entries(out).expect("parse"),
            vec![
                PathChange::Renamed {
                    from: "src/x.rs".to_string(),
                    to: ".seal/x.rs".to_string()
                },
                PathChange::Modified(".seal/a".to_string()),
                PathChange::Other(".seal/b".to_string()),
                PathChange::Other(".seal/c".to_string()),
                PathChange::Deleted("d".to_string()),
                PathChange::Added("n".to_string()),
                PathChange::Copied {
                    from: ".seal/p".to_string(),
                    to: ".seal/q".to_string()
                },
                PathChange::Other("w".to_string()),
            ]
        );
        assert_eq!(parse_jj_diff_entries("").expect("empty"), Vec::new());
        assert!(parse_jj_diff_entries("added\0n\0n\0\0file").is_err());
        assert!(parse_jj_diff_entries("added\0n\0n\0").is_err());
    }

    fn jj(root: &Path, args: &[&str]) {
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
    }

    fn write(root: &Path, path: &str, body: &str) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        std::fs::write(full, body).expect("write");
    }

    /// jj reports renames as one entry; both sides must reach the classifier.
    #[test]
    fn jj_rename_into_seal_is_not_exempt_but_log_append_is() {
        if which::which("jj").is_err() {
            eprintln!("skipping: jj not installed");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        jj(root, &["git", "init", "."]);
        write(root, "src/x.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
        write(root, ".seal/reviews/cr-1/events.jsonl", "{}\n");
        jj(root, &["commit", "-m", "approved"]);
        let repo = JjScmRepo::new(root);
        let approved = repo.commit_for_anchor("@-").expect("approved");

        // Append only to the log.
        write(
            root,
            ".seal/reviews/cr-1/events.jsonl",
            "{}\n{\"lgtm\":1}\n",
        );
        let target = repo.commit_for_anchor("@").expect("target");
        assert!(range_is_exempt(&repo, &approved, &target));

        // Now also move source into .seal/.
        std::fs::rename(root.join("src/x.rs"), root.join(".seal/x.rs")).expect("mv");
        let target = repo.commit_for_anchor("@").expect("target");
        assert!(!range_is_exempt(&repo, &approved, &target));
        let changes = repo
            .path_changes_between(&approved, &target)
            .expect("changes")
            .expect("supported");
        assert!(
            changes.contains(&PathChange::Renamed {
                from: "src/x.rs".to_string(),
                to: ".seal/x.rs".to_string()
            }),
            "{changes:?}"
        );
    }
}

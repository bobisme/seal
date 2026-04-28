use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};

pub mod git;
pub mod jj;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum ScmPreference {
    Auto,
    Git,
    Jj,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScmKind {
    Git,
    Jj,
}

impl ScmKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Jj => "jj",
        }
    }
}

pub trait ScmRepo {
    fn kind(&self) -> ScmKind;
    fn root(&self) -> &Path;

    fn current_anchor(&self) -> Result<String>;
    fn current_commit(&self) -> Result<String>;
    fn commit_for_anchor(&self, anchor: &str) -> Result<String>;
    fn parent_commit(&self, commit: &str) -> Result<String>;

    fn diff_git(&self, from: &str, to: &str) -> Result<String>;
    fn diff_git_file(&self, from: &str, to: &str, file: &str) -> Result<String>;
    fn changed_files_between(&self, from: &str, to: &str) -> Result<Vec<String>>;

    fn file_exists(&self, rev: &str, path: &str) -> Result<bool>;
    fn show_file(&self, rev: &str, path: &str) -> Result<String>;
}

/// Return the repository path represented by one file section from a git-format diff.
///
/// Prefer the new-side `+++ b/path` marker because `diff --git a/path b/path`
/// headers are ambiguous when paths contain spaces. Deleted files fall back to
/// the old-side `--- a/path` marker.
#[must_use]
pub fn git_diff_section_path(section: &str) -> Option<&str> {
    let mut old_path = None;
    let mut new_path = None;
    let mut renamed_to = None;

    for line in section.lines() {
        if line.starts_with("@@ ") {
            break;
        }

        if let Some(path) = diff_marker_path(line, "--- ") {
            old_path = Some(path);
        } else if let Some(path) = diff_marker_path(line, "+++ ") {
            new_path = Some(path);
        } else if let Some(path) = line.strip_prefix("rename to ") {
            renamed_to = Some(path);
        }
    }

    valid_diff_path(new_path)
        .or_else(|| valid_diff_path(renamed_to))
        .or_else(|| valid_diff_path(old_path))
        .or_else(|| section.lines().next().and_then(git_diff_header_new_path))
}

#[must_use]
pub fn git_diff_changed_paths(diff: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut current_start = None;
    let mut offset = 0;

    for line in diff.lines() {
        let byte_offset = offset;
        offset += line.len() + 1;

        if line.starts_with("diff --git") {
            if let Some(start) = current_start {
                if let Some(path) = git_diff_section_path(&diff[start..byte_offset]) {
                    paths.push(path.to_string());
                }
            }
            current_start = Some(byte_offset);
        }
    }

    if let Some(start) = current_start {
        if let Some(path) = git_diff_section_path(&diff[start..]) {
            paths.push(path.to_string());
        }
    }

    paths
}

fn diff_marker_path<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let path = line.strip_prefix(prefix)?;

    if path == "/dev/null" {
        return Some(path);
    }

    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .or(Some(path))
}

fn valid_diff_path(path: Option<&str>) -> Option<&str> {
    path.filter(|value| !value.is_empty() && *value != "/dev/null")
}

fn git_diff_header_new_path(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("diff --git a/")?;
    let mut fallback = None;

    for (idx, _) in rest.match_indices(" b/") {
        let old_path = &rest[..idx];
        let new_path = &rest[idx + 3..];
        fallback = Some(new_path);

        if old_path == new_path {
            return Some(new_path);
        }
    }

    fallback
}

#[derive(Debug, Clone)]
pub struct BackendDetection {
    pub git_root: Option<PathBuf>,
    pub jj_root: Option<PathBuf>,
}

impl BackendDetection {
    #[must_use]
    pub fn detect(start_path: &Path) -> Self {
        Self {
            git_root: git::detect_git_root(start_path),
            jj_root: jj::detect_jj_root(start_path),
        }
    }

    #[must_use]
    pub const fn has_both(&self) -> bool {
        self.git_root.is_some() && self.jj_root.is_some()
    }

    #[must_use]
    pub fn roots_match(&self) -> bool {
        let Some(git_root) = &self.git_root else {
            return false;
        };
        let Some(jj_root) = &self.jj_root else {
            return false;
        };

        let git_root = canonicalize_maybe(git_root);
        let jj_workspace_root = canonicalize_maybe(jj_root);

        if git_root == jj_workspace_root {
            return true;
        }

        // In jj workspaces, the workspace path can differ from the shared repo root.
        // Compare Git root against jj's canonical repo root before declaring mismatch.
        if let Ok(jj_repo_root) = crate::jj::resolve_repo_root(jj_root) {
            return git_root == canonicalize_maybe(&jj_repo_root);
        }

        false
    }
}

#[must_use]
pub fn parse_preference(value: &str) -> Option<ScmPreference> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(ScmPreference::Auto),
        "git" => Some(ScmPreference::Git),
        "jj" => Some(ScmPreference::Jj),
        _ => None,
    }
}

pub fn resolve_backend(
    start_path: &Path,
    preference: ScmPreference,
) -> Result<Box<dyn ScmRepo + Send + Sync>> {
    let detection = BackendDetection::detect(start_path);

    let selected = match preference {
        ScmPreference::Git => {
            let root = detection.git_root.ok_or_else(|| {
                anyhow::anyhow!(
                    "Requested SCM backend 'git' but no Git repository was detected.\n  To fix: run in a Git repository, or use --scm jj"
                )
            })?;
            ScmKind::Git.with_root(root)
        }
        ScmPreference::Jj => {
            let root = detection.jj_root.ok_or_else(|| {
                anyhow::anyhow!(
                    "Requested SCM backend 'jj' but no jj repository was detected.\n  To fix: run in a jj repository, or use --scm git"
                )
            })?;
            ScmKind::Jj.with_root(root)
        }
        ScmPreference::Auto => resolve_auto_backend(&detection)?,
    };

    let backend: Box<dyn ScmRepo + Send + Sync> = match selected.kind {
        ScmKind::Git => Box::new(git::GitRepo::new(selected.root)),
        ScmKind::Jj => Box::new(jj::JjScmRepo::new(&selected.root)),
    };

    Ok(backend)
}

fn resolve_auto_backend(detection: &BackendDetection) -> Result<SelectedBackend> {
    match (&detection.git_root, &detection.jj_root) {
        (Some(git_root), Some(jj_root)) => {
            if canonicalize_maybe(git_root) != canonicalize_maybe(jj_root) {
                bail!(
                    "Detected both Git and jj backends with different roots.\n  Git root: {}\n  jj root: {}\n  To fix: rerun with explicit backend, e.g. `seal --scm git ...` or `seal --scm jj ...`",
                    git_root.display(),
                    jj_root.display(),
                );
            }

            // Keep backward compatibility in mixed repositories.
            Ok(ScmKind::Jj.with_root(jj_root.clone()))
        }
        (Some(git_root), None) => Ok(ScmKind::Git.with_root(git_root.clone())),
        (None, Some(jj_root)) => Ok(ScmKind::Jj.with_root(jj_root.clone())),
        (None, None) => bail!(
            "No supported SCM backend detected (expected Git or jj repository).\n  To fix: run `git init` or `jj git init`, or pass `--path` to an existing repository root"
        ),
    }
}

#[derive(Debug, Clone)]
struct SelectedBackend {
    kind: ScmKind,
    root: PathBuf,
}

impl ScmKind {
    const fn with_root(self, root: PathBuf) -> SelectedBackend {
        SelectedBackend { kind: self, root }
    }
}

pub fn validate_anchor(anchor: &str) -> Result<()> {
    if anchor.trim().is_empty() {
        bail!("SCM anchor/reference cannot be empty");
    }

    if anchor.starts_with('-') {
        bail!("SCM anchor/reference cannot start with '-': {anchor}");
    }

    if anchor.contains('\0') || anchor.contains('\n') || anchor.contains('\r') {
        bail!("SCM anchor/reference contains invalid control characters");
    }

    Ok(())
}

pub fn validate_repo_relative_path(path: &str) -> Result<()> {
    if path.trim().is_empty() {
        bail!("Path cannot be empty");
    }

    let path_ref = Path::new(path);
    if path_ref.is_absolute() {
        bail!("Path must be repository-relative: {path}");
    }

    for component in path_ref.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => bail!("Path must be normalized (no '.'): {path}"),
            Component::ParentDir => bail!("Path traversal is not allowed: {path}"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("Path must be repository-relative: {path}")
            }
        }
    }

    Ok(())
}

fn canonicalize_maybe(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub fn resolve_preference(cli_preference: Option<ScmPreference>) -> Result<ScmPreference> {
    if let Some(cli) = cli_preference {
        return Ok(cli);
    }

    // Check SEAL_SCM first, fall back to legacy CRIT_SCM
    for var in &["SEAL_SCM", "CRIT_SCM"] {
        if let Ok(env_value) = std::env::var(var) {
            return parse_preference(&env_value).with_context(|| {
                format!("Invalid {var} value '{env_value}'. Expected one of: auto, git, jj")
            });
        }
    }

    Ok(ScmPreference::Auto)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    #[test]
    fn test_validate_anchor_rejects_dash_prefix() {
        assert!(validate_anchor("-bad").is_err());
    }

    #[test]
    fn test_validate_anchor_accepts_normal_values() {
        assert!(validate_anchor("refs/heads/main").is_ok());
        assert!(validate_anchor("@").is_ok());
    }

    #[test]
    fn test_validate_repo_relative_path() {
        assert!(validate_repo_relative_path("src/main.rs").is_ok());
        assert!(validate_repo_relative_path("../etc/passwd").is_err());
        assert!(validate_repo_relative_path("/absolute/path").is_err());
        assert!(validate_repo_relative_path("./src/main.rs").is_err());
    }

    #[test]
    fn test_git_diff_section_path_allows_spaces() {
        let section = "\
diff --git a/src/has space.rs b/src/has space.rs
--- a/src/has space.rs
+++ b/src/has space.rs
@@ -1 +1 @@
-old
+new
";

        assert_eq!(git_diff_section_path(section), Some("src/has space.rs"));
    }

    #[test]
    fn test_git_diff_section_path_deleted_file_uses_old_path() {
        let section = "\
diff --git a/src/old name.rs b/src/old name.rs
--- a/src/old name.rs
+++ /dev/null
@@ -1 +0,0 @@
-old
";

        assert_eq!(git_diff_section_path(section), Some("src/old name.rs"));
    }

    #[test]
    fn test_git_diff_section_path_binary_file_falls_back_to_header() {
        let section = "\
diff --git a/assets/icon large.png b/assets/icon large.png
index 123..456 100644
Binary files a/assets/icon large.png and b/assets/icon large.png differ
";

        assert_eq!(
            git_diff_section_path(section),
            Some("assets/icon large.png")
        );
    }

    #[test]
    fn test_git_diff_changed_paths_extracts_multiple_spaced_paths() {
        let diff = "\
diff --git a/src/has space.rs b/src/has space.rs
--- a/src/has space.rs
+++ b/src/has space.rs
@@ -1 +1 @@
-old
+new
diff --git a/docs/old note.md b/docs/old note.md
--- a/docs/old note.md
+++ /dev/null
@@ -1 +0,0 @@
-old
";

        assert_eq!(
            git_diff_changed_paths(diff),
            vec![
                "src/has space.rs".to_string(),
                "docs/old note.md".to_string()
            ]
        );
    }

    #[test]
    fn test_resolve_backend_rejects_mismatched_mixed_roots() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();

        let status = Command::new("git")
            .current_dir(root)
            .args(["init"])
            .status()
            .expect("git init");
        assert!(status.success());

        let nested = root.join("nested");
        std::fs::create_dir_all(nested.join(".jj")).expect("create .jj");

        let err = match resolve_backend(&nested, ScmPreference::Auto) {
            Ok(_) => panic!("expected mismatch failure"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("different roots"), "unexpected error: {err}");
    }
}

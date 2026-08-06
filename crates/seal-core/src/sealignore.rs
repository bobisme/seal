//! Critignore file support for excluding files from reviews.
//!
//! Uses gitignore-style patterns from `.sealignore` files.
//! The `.seal/` directory is always excluded regardless of patterns.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::Path;

use crate::scm::git_diff_section_path;

/// Hard-coded patterns that are always excluded from reviews.
/// These are excluded regardless of `.sealignore` contents.
const HARD_IGNORED: &[&str] = &[
    ".seal/",  // Review metadata - must never be reviewed
    ".beads/", // Issue tracking data
];

/// Result of loading sealignore patterns.
pub struct SealIgnore {
    /// The gitignore matcher (if .sealignore exists)
    gitignore: Option<Gitignore>,
}

impl SealIgnore {
    /// Load sealignore patterns from a repository root.
    ///
    /// Looks for `.sealignore` in the repo root.
    /// If no file exists, only hard-coded patterns apply.
    #[must_use]
    pub fn load(repo_root: &Path) -> Self {
        let sealignore_path = repo_root.join(".sealignore");

        let gitignore = if sealignore_path.exists() {
            let mut builder = GitignoreBuilder::new(repo_root);
            // Add patterns from .sealignore file
            if builder.add(&sealignore_path).is_none() {
                builder.build().ok()
            } else {
                None
            }
        } else {
            None
        };

        Self { gitignore }
    }

    /// Check if a file path should be ignored.
    ///
    /// Returns true if the file should be excluded from reviews.
    #[must_use]
    pub fn is_ignored(&self, path: &str) -> bool {
        // Check hard-coded patterns first
        for pattern in HARD_IGNORED {
            if path.starts_with(pattern) {
                return true;
            }
        }

        // Check .sealignore patterns if loaded
        if let Some(ref gitignore) = self.gitignore {
            let is_dir = path.ends_with('/');
            // Use matched_path_or_any_parents to handle directory patterns like "target/"
            // which should match "target/debug/binary"
            gitignore
                .matched_path_or_any_parents(path, is_dir)
                .is_ignore()
        } else {
            false
        }
    }

    /// Filter a list of file paths, removing ignored files.
    ///
    /// Returns the filtered list and a count of how many were ignored.
    #[must_use]
    pub fn filter_files(&self, files: Vec<String>) -> (Vec<String>, usize) {
        let original_count = files.len();
        let filtered: Vec<String> = files.into_iter().filter(|f| !self.is_ignored(f)).collect();
        let ignored_count = original_count - filtered.len();
        (filtered, ignored_count)
    }

    /// Filter ignored file sections from a git-format diff.
    ///
    /// Diff payloads must be filtered independently from changed-file metadata:
    /// callers commonly serialize both, and filtering only the metadata can leak
    /// large generated files into reviewer context. Unparseable content outside a
    /// `diff --git` section is omitted so ignore handling fails closed.
    #[must_use]
    pub fn filter_diff(&self, diff: &str) -> String {
        let mut filtered = String::new();
        let mut current_start = None;
        let mut offset = 0;

        for line in diff.lines() {
            let byte_offset = offset;
            offset += line.len() + 1;

            if line.starts_with("diff --git") {
                if let Some(start) = current_start {
                    self.append_reviewable_section(&mut filtered, &diff[start..byte_offset]);
                }
                current_start = Some(byte_offset);
            }
        }

        if let Some(start) = current_start {
            self.append_reviewable_section(&mut filtered, &diff[start..]);
        }

        filtered
    }

    fn append_reviewable_section(&self, output: &mut String, section: &str) {
        let Some(path) = git_diff_section_path(section) else {
            return;
        };
        if self.is_ignored(path) {
            return;
        }

        output.push_str(section);
        if !section.ends_with('\n') {
            output.push('\n');
        }
    }

    /// Check if a .sealignore file exists in the repo.
    #[must_use]
    pub fn has_sealignore_file(repo_root: &Path) -> bool {
        repo_root.join(".sealignore").exists()
    }
}

/// Error returned when all files in a review are ignored.
#[derive(Debug, Clone)]
pub struct AllFilesIgnoredError {
    /// Number of files that were ignored
    pub ignored_count: usize,
    /// Whether a .sealignore file exists
    pub has_sealignore: bool,
}

impl std::fmt::Display for AllFilesIgnoredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "All {} files in this review are ignored",
            self.ignored_count
        )?;

        if self.has_sealignore {
            write!(
                f,
                "\n  Check .sealignore patterns if files were excluded unexpectedly"
            )?;
        }

        write!(
            f,
            "\n  Hard-ignored directories: {}",
            HARD_IGNORED.join(", ")
        )?;

        Ok(())
    }
}

impl std::error::Error for AllFilesIgnoredError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_hard_ignored_patterns() {
        let temp = TempDir::new().unwrap();
        let ignore = SealIgnore::load(temp.path());

        // Hard-coded patterns should always be ignored
        assert!(ignore.is_ignored(".seal/events.jsonl"));
        assert!(ignore.is_ignored(".seal/reviews/"));
        assert!(ignore.is_ignored(".beads/issues.jsonl"));

        // Regular files should not be ignored
        assert!(!ignore.is_ignored("src/main.rs"));
        assert!(!ignore.is_ignored("README.md"));
    }

    #[test]
    fn test_sealignore_file() {
        let temp = TempDir::new().unwrap();

        // Create .sealignore file
        fs::write(
            temp.path().join(".sealignore"),
            "*.log\ntarget/\ndocs/*.md\n",
        )
        .unwrap();

        let ignore = SealIgnore::load(temp.path());

        // Patterns from file should be ignored
        assert!(ignore.is_ignored("debug.log"));
        assert!(ignore.is_ignored("target/debug/binary"));

        // Non-matching files should not be ignored
        assert!(!ignore.is_ignored("src/main.rs"));
        assert!(!ignore.is_ignored("README.md")); // Only docs/*.md is ignored
    }

    #[test]
    fn test_filter_files() {
        let temp = TempDir::new().unwrap();
        let ignore = SealIgnore::load(temp.path());

        let files = vec![
            ".seal/events.jsonl".to_string(),
            "src/main.rs".to_string(),
            ".beads/issues.jsonl".to_string(),
            "README.md".to_string(),
        ];

        let (filtered, ignored_count) = ignore.filter_files(files);

        assert_eq!(ignored_count, 2);
        assert_eq!(filtered, vec!["src/main.rs", "README.md"]);
    }

    #[test]
    fn test_filter_diff_removes_entire_ignored_section() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join(".sealignore"), "web/engine.js\n").unwrap();
        let ignore = SealIgnore::load(temp.path());
        let diff = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1 +1 @@
-old
+kept-change
diff --git a/web/engine.js b/web/engine.js
--- a/web/engine.js
+++ b/web/engine.js
@@ -1 +1 @@
-old
+IGNORED_MINIFIED_PAYLOAD
";

        let filtered = ignore.filter_diff(diff);

        assert!(filtered.contains("src/main.rs"));
        assert!(filtered.contains("kept-change"));
        assert!(!filtered.contains("web/engine.js"));
        assert!(!filtered.contains("IGNORED_MINIFIED_PAYLOAD"));
    }

    #[test]
    fn test_no_sealignore_file() {
        let temp = TempDir::new().unwrap();
        let ignore = SealIgnore::load(temp.path());

        // Should still apply hard-coded patterns
        assert!(ignore.is_ignored(".seal/foo"));

        // But nothing else
        assert!(!ignore.is_ignored("anything.txt"));
    }

    #[test]
    fn test_all_files_ignored_error() {
        let err = AllFilesIgnoredError {
            ignored_count: 3,
            has_sealignore: true,
        };

        let msg = err.to_string();
        assert!(msg.contains("All 3 files"));
        assert!(msg.contains(".sealignore patterns"));
        assert!(msg.contains(".seal/"));
    }
}

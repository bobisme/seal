//! Context extraction for code review threads.
//!
//! Provides functionality to extract code context around a specific line range,
//! useful for displaying thread context with surrounding code.

use anyhow::{Context as AnyhowContext, Result};
use serde::Serialize;

use crate::scm::ScmRepo;

/// A single line of code context.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ContextLine {
    /// The 1-based line number in the file.
    pub line_number: u32,
    /// The content of the line (without trailing newline).
    pub content: String,
    /// Whether this line is part of the anchored selection (vs surrounding context).
    pub is_anchor: bool,
}

/// Extracted code context around an anchored selection.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CodeContext {
    /// The context lines (anchor lines + surrounding context).
    pub lines: Vec<ContextLine>,
    /// First line number in the context (1-based).
    pub start_line: u32,
    /// Last line number in the context (1-based).
    pub end_line: u32,
    /// Start of the anchored selection within the file (1-based).
    pub anchor_start: u32,
    /// End of the anchored selection within the file (1-based, inclusive).
    pub anchor_end: u32,
}

impl CodeContext {
    /// Check if the context is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Get the number of lines in the context.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.lines.len()
    }
}

/// Extract code context around an anchored line range.
///
/// # Arguments
///
/// * `repo` - The jj repository wrapper
/// * `file` - Path to the file within the repository
/// * `commit` - The commit/revision to read the file from
/// * `anchor_start` - Start line of the anchor (1-based, inclusive)
/// * `anchor_end` - End line of the anchor (1-based, inclusive)
/// * `context_lines` - Number of context lines to include before and after
///
/// # Returns
///
/// A `CodeContext` containing the anchor lines plus surrounding context.
///
/// # Errors
///
/// Returns an error if:
/// - The file doesn't exist at the given commit
/// - The jj command fails
/// - The anchor lines are out of bounds
pub fn extract_context(
    repo: &dyn ScmRepo,
    file: &str,
    commit: &str,
    anchor_start: u32,
    anchor_end: u32,
    context_lines: u32,
) -> Result<CodeContext> {
    // Validate anchor range
    if anchor_start == 0 || anchor_end == 0 {
        anyhow::bail!("Line numbers must be 1-based (got anchor_start={anchor_start}, anchor_end={anchor_end})");
    }
    if anchor_start > anchor_end {
        anyhow::bail!("anchor_start ({anchor_start}) must be <= anchor_end ({anchor_end})");
    }

    // Get file contents
    let contents = repo
        .show_file(commit, file)
        .with_context(|| format!("Failed to get file {file} at {commit}"))?;

    let file_lines: Vec<&str> = contents.lines().collect();
    let total_lines = file_lines.len() as u32;

    // Handle empty file
    if total_lines == 0 {
        anyhow::bail!("File {file} is empty at {commit}");
    }

    if anchor_start > total_lines || anchor_end > total_lines {
        anyhow::bail!(
            "Anchor range {anchor_start}-{anchor_end} is out of bounds for {file} at {commit} ({total_lines} lines)"
        );
    }

    // Calculate context range
    let start_line = anchor_start.saturating_sub(context_lines).max(1);
    let end_line = (anchor_end + context_lines).min(total_lines);

    // Extract lines
    let mut lines = Vec::new();
    for line_num in start_line..=end_line {
        let idx = (line_num - 1) as usize;
        let content = file_lines.get(idx).unwrap_or(&"").to_string();
        let is_anchor = line_num >= anchor_start && line_num <= anchor_end;

        lines.push(ContextLine {
            line_number: line_num,
            content,
            is_anchor,
        });
    }

    Ok(CodeContext {
        lines,
        start_line,
        end_line,
        anchor_start,
        anchor_end,
    })
}

/// Format code context for display.
///
/// Outputs in a unified diff-like style with line numbers.
/// Anchor lines are prefixed with `>` to highlight them.
///
/// # Example output
///
/// ```text
///    41 |     fn parse_buffer(buf: &str) {
/// >  42 |         // AGENT NOTE: This buffer isn't cleared
/// >  43 |         let x = buf.len();
///    44 |     }
/// ```
#[must_use]
pub fn format_context(ctx: &CodeContext) -> String {
    if ctx.is_empty() {
        return String::new();
    }

    // Determine the width needed for line numbers
    let max_line_num = ctx.end_line;
    let line_num_width = max_line_num.to_string().len();

    let mut output = String::new();

    for line in &ctx.lines {
        let prefix = if line.is_anchor { ">" } else { " " };
        let formatted = format!(
            "{} {:>width$} | {}\n",
            prefix,
            line.line_number,
            line.content,
            width = line_num_width
        );
        output.push_str(&formatted);
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scm::jj::JjScmRepo;
    use crate::scm::{ScmKind, ScmRepo};
    use std::env;
    use std::path::{Path, PathBuf};

    struct MockRepo {
        root: PathBuf,
        contents: String,
    }

    impl MockRepo {
        fn new(contents: &str) -> Self {
            Self {
                root: PathBuf::from("/tmp/seal-context-test"),
                contents: contents.to_string(),
            }
        }
    }

    impl ScmRepo for MockRepo {
        fn kind(&self) -> ScmKind {
            ScmKind::Git
        }

        fn root(&self) -> &Path {
            &self.root
        }

        fn current_anchor(&self) -> Result<String> {
            Ok("current".to_string())
        }

        fn current_commit(&self) -> Result<String> {
            Ok("current".to_string())
        }

        fn commit_for_anchor(&self, anchor: &str) -> Result<String> {
            Ok(anchor.to_string())
        }

        fn parent_commit(&self, commit: &str) -> Result<String> {
            Ok(format!("{commit}^"))
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
            Ok(true)
        }

        fn show_file(&self, _rev: &str, _path: &str) -> Result<String> {
            Ok(self.contents.clone())
        }
    }

    fn test_repo() -> Option<JjScmRepo> {
        let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        let root = Path::new(&manifest_dir);
        if !root.join(".jj").exists() {
            return None;
        }
        Some(JjScmRepo::new(root))
    }

    #[test]
    fn test_extract_context_middle_of_file() {
        let Some(repo) = test_repo() else {
            return;
        };

        // Extract context around lines 5-6 of Cargo.toml with 2 lines of context
        let ctx = extract_context(&repo, "Cargo.toml", "@", 5, 6, 2).unwrap();

        assert_eq!(ctx.anchor_start, 5);
        assert_eq!(ctx.anchor_end, 6);
        assert_eq!(ctx.start_line, 3); // 5 - 2 = 3
        assert_eq!(ctx.end_line, 8); // 6 + 2 = 8

        // Check that anchor lines are marked correctly
        for line in &ctx.lines {
            let expected_anchor = line.line_number >= 5 && line.line_number <= 6;
            assert_eq!(
                line.is_anchor, expected_anchor,
                "Line {} should have is_anchor={}, got {}",
                line.line_number, expected_anchor, line.is_anchor
            );
        }
    }

    #[test]
    fn test_extract_context_start_of_file() {
        let Some(repo) = test_repo() else {
            return;
        };

        // Extract context around line 1 with 3 lines of context
        let ctx = extract_context(&repo, "Cargo.toml", "@", 1, 1, 3).unwrap();

        assert_eq!(ctx.anchor_start, 1);
        assert_eq!(ctx.anchor_end, 1);
        assert_eq!(ctx.start_line, 1); // Can't go below 1
        assert_eq!(ctx.end_line, 4); // 1 + 3 = 4

        // First line should be anchor
        assert!(ctx.lines[0].is_anchor);
        assert_eq!(ctx.lines[0].line_number, 1);
    }

    #[test]
    fn test_extract_context_single_line() {
        let Some(repo) = test_repo() else {
            return;
        };

        let ctx = extract_context(&repo, "Cargo.toml", "@", 3, 3, 1).unwrap();

        assert_eq!(ctx.anchor_start, 3);
        assert_eq!(ctx.anchor_end, 3);
        assert_eq!(ctx.start_line, 2);
        assert_eq!(ctx.end_line, 4);

        // Only the middle line should be anchor
        let anchor_count = ctx.lines.iter().filter(|l| l.is_anchor).count();
        assert_eq!(anchor_count, 1);
    }

    #[test]
    fn test_extract_context_zero_context_lines() {
        let Some(repo) = test_repo() else {
            return;
        };

        let ctx = extract_context(&repo, "Cargo.toml", "@", 5, 7, 0).unwrap();

        assert_eq!(ctx.anchor_start, 5);
        assert_eq!(ctx.anchor_end, 7);
        assert_eq!(ctx.start_line, 5);
        assert_eq!(ctx.end_line, 7);
        assert_eq!(ctx.lines.len(), 3);

        // All lines should be anchors
        assert!(ctx.lines.iter().all(|l| l.is_anchor));
    }

    #[test]
    fn test_extract_context_invalid_range() {
        let Some(repo) = test_repo() else {
            return;
        };

        // anchor_start > anchor_end should fail
        let result = extract_context(&repo, "Cargo.toml", "@", 10, 5, 2);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("must be <="));
    }

    #[test]
    fn test_extract_context_zero_line_numbers() {
        let Some(repo) = test_repo() else {
            return;
        };

        // Line 0 is invalid (1-based)
        let result = extract_context(&repo, "Cargo.toml", "@", 0, 5, 2);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("1-based"));
    }

    #[test]
    fn test_extract_context_anchor_start_out_of_bounds() {
        let repo = MockRepo::new("line1\nline2\nline3\n");

        let result = extract_context(&repo, "file.rs", "commit", 4, 4, 1);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("out of bounds"));
    }

    #[test]
    fn test_extract_context_anchor_end_out_of_bounds() {
        let repo = MockRepo::new("line1\nline2\nline3\n");

        let result = extract_context(&repo, "file.rs", "commit", 2, 4, 1);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("out of bounds"));
    }

    #[test]
    fn test_extract_context_nonexistent_file() {
        let Some(repo) = test_repo() else {
            return;
        };

        let result = extract_context(&repo, "nonexistent-file-xyz.txt", "@", 1, 1, 2);
        assert!(result.is_err());
    }

    #[test]
    fn test_format_context() {
        let ctx = CodeContext {
            lines: vec![
                ContextLine {
                    line_number: 41,
                    content: "    fn parse_buffer(buf: &str) {".to_string(),
                    is_anchor: false,
                },
                ContextLine {
                    line_number: 42,
                    content: "        // AGENT NOTE: This buffer".to_string(),
                    is_anchor: true,
                },
                ContextLine {
                    line_number: 43,
                    content: "        let x = buf.len();".to_string(),
                    is_anchor: true,
                },
                ContextLine {
                    line_number: 44,
                    content: "    }".to_string(),
                    is_anchor: false,
                },
            ],
            start_line: 41,
            end_line: 44,
            anchor_start: 42,
            anchor_end: 43,
        };

        let formatted = format_context(&ctx);

        // Check that anchor lines are marked with >
        assert!(formatted.contains("> 42 |"));
        assert!(formatted.contains("> 43 |"));
        // Non-anchor lines should have space prefix
        assert!(formatted.contains("  41 |"));
        assert!(formatted.contains("  44 |"));
    }

    #[test]
    fn test_format_context_empty() {
        let ctx = CodeContext {
            lines: vec![],
            start_line: 0,
            end_line: 0,
            anchor_start: 0,
            anchor_end: 0,
        };

        let formatted = format_context(&ctx);
        assert!(formatted.is_empty());
    }

    #[test]
    fn test_format_context_line_number_width() {
        // Test that line numbers are properly right-aligned
        let ctx = CodeContext {
            lines: vec![
                ContextLine {
                    line_number: 98,
                    content: "line 98".to_string(),
                    is_anchor: false,
                },
                ContextLine {
                    line_number: 99,
                    content: "line 99".to_string(),
                    is_anchor: true,
                },
                ContextLine {
                    line_number: 100,
                    content: "line 100".to_string(),
                    is_anchor: true,
                },
                ContextLine {
                    line_number: 101,
                    content: "line 101".to_string(),
                    is_anchor: false,
                },
            ],
            start_line: 98,
            end_line: 101,
            anchor_start: 99,
            anchor_end: 100,
        };

        let formatted = format_context(&ctx);

        // With max line 101 (3 digits), all numbers should be 3-wide
        // Format is: prefix(1) + space(1) + right-aligned-number(width) + space(1) + pipe
        assert!(
            formatted.contains("   98 |"),
            "Expected '   98 |', got:\n{formatted}"
        );
        assert!(
            formatted.contains(">  99 |"),
            "Expected '>  99 |', got:\n{formatted}"
        );
        assert!(
            formatted.contains("> 100 |"),
            "Expected '> 100 |', got:\n{formatted}"
        );
        assert!(
            formatted.contains("  101 |"),
            "Expected '  101 |', got:\n{formatted}"
        );
    }

    #[test]
    fn test_context_line_equality() {
        let line1 = ContextLine {
            line_number: 10,
            content: "hello".to_string(),
            is_anchor: true,
        };
        let line2 = ContextLine {
            line_number: 10,
            content: "hello".to_string(),
            is_anchor: true,
        };
        let line3 = ContextLine {
            line_number: 10,
            content: "world".to_string(),
            is_anchor: true,
        };

        assert_eq!(line1, line2);
        assert_ne!(line1, line3);
    }

    #[test]
    fn test_code_context_len_and_empty() {
        let empty_ctx = CodeContext {
            lines: vec![],
            start_line: 0,
            end_line: 0,
            anchor_start: 0,
            anchor_end: 0,
        };

        assert!(empty_ctx.is_empty());
        assert_eq!(empty_ctx.len(), 0);

        let ctx = CodeContext {
            lines: vec![ContextLine {
                line_number: 1,
                content: "test".to_string(),
                is_anchor: true,
            }],
            start_line: 1,
            end_line: 1,
            anchor_start: 1,
            anchor_end: 1,
        };

        assert!(!ctx.is_empty());
        assert_eq!(ctx.len(), 1);
    }
}

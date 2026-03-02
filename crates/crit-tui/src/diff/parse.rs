//! Unified diff parser
//!
//! Parses standard unified diff format into structured data.

/// A parsed unified diff
#[derive(Debug, Clone, Default)]
pub struct ParsedDiff {
    pub file_a: Option<String>,
    pub file_b: Option<String>,
    pub hunks: Vec<DiffHunk>,
}

/// Line ranges covered by diff hunks (union of old-side and new-side),
/// merged and sorted. Used to exclude already-displayed lines from orphaned
/// context sections.
#[must_use]
pub fn hunk_exclusion_ranges(hunks: &[DiffHunk]) -> Vec<(i64, i64)> {
    let mut ranges: Vec<(i64, i64)> = Vec::new();
    for h in hunks {
        if h.new_count > 0 {
            ranges.push((
                i64::from(h.new_start),
                i64::from(h.new_start + h.new_count.saturating_sub(1)),
            ));
        }
    }
    ranges.sort_by_key(|r| r.0);
    // Merge overlapping/adjacent ranges
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (s, e) in ranges {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 + 1 {
                last.1 = last.1.max(e);
            } else {
                merged.push((s, e));
            }
        } else {
            merged.push((s, e));
        }
    }
    merged
}

/// A single hunk from a diff
#[derive(Debug, Clone)]
pub struct DiffHunk {
    /// The @@ header line
    pub header: String,
    /// Starting line in old file
    pub old_start: u32,
    /// Number of lines in old file
    pub old_count: u32,
    /// Starting line in new file
    pub new_start: u32,
    /// Number of lines in new file
    pub new_count: u32,
    /// Lines in this hunk
    pub lines: Vec<DiffLine>,
}

/// A single line in a diff hunk
#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    /// Line number in old file (if applicable)
    pub old_line: Option<u32>,
    /// Line number in new file (if applicable)
    pub new_line: Option<u32>,
    /// The line content (without the +/- prefix)
    pub content: String,
}

/// Type of diff line
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

impl ParsedDiff {
    /// Parse a unified diff string
    #[must_use]
    pub fn parse(diff: &str) -> Self {
        let mut result = Self::default();
        let mut lines = diff.lines().peekable();

        // Parse header (--- and +++ lines)
        while let Some(line) = lines.peek() {
            if line.starts_with("---") {
                result.file_a = line.strip_prefix("--- ").map(|s| {
                    // Remove a/ prefix if present
                    s.strip_prefix("a/").unwrap_or(s).to_string()
                });
                lines.next();
            } else if line.starts_with("+++") {
                result.file_b = line.strip_prefix("+++ ").map(|s| {
                    // Remove b/ prefix if present
                    s.strip_prefix("b/").unwrap_or(s).to_string()
                });
                lines.next();
            } else if line.starts_with("@@") {
                break;
            } else {
                lines.next(); // Skip other header lines (diff --git, index, etc.)
            }
        }

        // Parse hunks
        while let Some(line) = lines.next() {
            if line.starts_with("@@") {
                if let Some(hunk) = Self::parse_hunk(line, &mut lines) {
                    result.hunks.push(hunk);
                }
            }
        }

        result
    }

    fn parse_hunk(
        header: &str,
        lines: &mut std::iter::Peekable<std::str::Lines<'_>>,
    ) -> Option<DiffHunk> {
        // Parse @@ -start,count +start,count @@ optional context
        // Example: @@ -1,5 +1,7 @@ fn main() {
        let header_str = header.to_string();

        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.len() < 3 {
            return None;
        }

        let (old_start, old_count) = Self::parse_range(parts[1].trim_start_matches('-'))?;
        let (new_start, new_count) = Self::parse_range(parts[2].trim_start_matches('+'))?;

        let mut hunk = DiffHunk {
            header: header_str,
            old_start,
            old_count,
            new_start,
            new_count,
            lines: Vec::new(),
        };

        let mut old_line = old_start;
        let mut new_line = new_start;

        while let Some(line) = lines.peek() {
            if line.starts_with("@@") || line.starts_with("diff ") {
                break;
            }

            let line = lines.next().unwrap_or_default();

            let (kind, content) = if let Some(content) = line.strip_prefix('+') {
                (DiffLineKind::Added, content)
            } else if let Some(content) = line.strip_prefix('-') {
                (DiffLineKind::Removed, content)
            } else if let Some(content) = line.strip_prefix(' ') {
                (DiffLineKind::Context, content)
            } else if line.is_empty() {
                // Empty context line
                (DiffLineKind::Context, "")
            } else if line.starts_with('\\') {
                // "\ No newline at end of file"
                continue;
            } else {
                // Unknown line format, treat as context
                (DiffLineKind::Context, line)
            };

            let diff_line = match kind {
                DiffLineKind::Added => {
                    let dl = DiffLine {
                        kind,
                        old_line: None,
                        new_line: Some(new_line),
                        content: content.to_string(),
                    };
                    new_line += 1;
                    dl
                }
                DiffLineKind::Removed => {
                    let dl = DiffLine {
                        kind,
                        old_line: Some(old_line),
                        new_line: None,
                        content: content.to_string(),
                    };
                    old_line += 1;
                    dl
                }
                DiffLineKind::Context => {
                    let dl = DiffLine {
                        kind,
                        old_line: Some(old_line),
                        new_line: Some(new_line),
                        content: content.to_string(),
                    };
                    old_line += 1;
                    new_line += 1;
                    dl
                }
            };

            hunk.lines.push(diff_line);
        }

        Some(hunk)
    }

    fn parse_range(s: &str) -> Option<(u32, u32)> {
        if let Some((start, count)) = s.split_once(',') {
            Some((start.parse().ok()?, count.parse().ok()?))
        } else {
            // Single line: "5" means start=5, count=1
            let start = s.parse().ok()?;
            Some((start, 1))
        }
    }

    /// Get total number of lines across all hunks
    #[must_use]
    pub fn total_lines(&self) -> usize {
        self.hunks.iter().map(|h| h.lines.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_diff() {
        let diff = r#"diff --git a/src/main.rs b/src/main.rs
index abc123..def456 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,5 +1,7 @@
 fn main() {
-    println!("Hello");
+    println!("Hello, world!");
+    println!("Goodbye!");
 }
"#;

        let parsed = ParsedDiff::parse(diff);

        assert_eq!(parsed.file_a, Some("src/main.rs".to_string()));
        assert_eq!(parsed.file_b, Some("src/main.rs".to_string()));
        assert_eq!(parsed.hunks.len(), 1);

        let hunk = &parsed.hunks[0];
        assert_eq!(hunk.old_start, 1);
        assert_eq!(hunk.old_count, 5);
        assert_eq!(hunk.new_start, 1);
        assert_eq!(hunk.new_count, 7);

        // Should have: context, removed, added, added, context
        assert_eq!(hunk.lines.len(), 5);
        assert_eq!(hunk.lines[0].kind, DiffLineKind::Context);
        assert_eq!(hunk.lines[1].kind, DiffLineKind::Removed);
        assert_eq!(hunk.lines[2].kind, DiffLineKind::Added);
        assert_eq!(hunk.lines[3].kind, DiffLineKind::Added);
        assert_eq!(hunk.lines[4].kind, DiffLineKind::Context);
    }

    #[test]
    fn test_line_numbers() {
        let diff = r#"--- a/test.txt
+++ b/test.txt
@@ -10,3 +10,4 @@
 context
-removed
+added1
+added2
"#;

        let parsed = ParsedDiff::parse(diff);
        let lines = &parsed.hunks[0].lines;

        // Context line 10
        assert_eq!(lines[0].old_line, Some(10));
        assert_eq!(lines[0].new_line, Some(10));

        // Removed line 11
        assert_eq!(lines[1].old_line, Some(11));
        assert_eq!(lines[1].new_line, None);

        // Added line 11
        assert_eq!(lines[2].old_line, None);
        assert_eq!(lines[2].new_line, Some(11));

        // Added line 12
        assert_eq!(lines[3].old_line, None);
        assert_eq!(lines[3].new_line, Some(12));
    }
}

//! Drift detection for comment anchors across code changes.
//!
//! When a comment is anchored to a specific line in a specific commit, and the code
//! evolves through rebases/amends, we need to calculate where that line "lives" now.
//!
//! This module parses unified diffs and tracks how insertions/deletions shift line numbers.

use anyhow::{bail, Result};

use crate::scm::ScmRepo;

/// Result of drift detection for a line anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftResult {
    /// The anchored line still exists at the same position.
    Unchanged {
        /// The current line number (same as original).
        current_line: u32,
    },
    /// The anchored line moved due to insertions/deletions above it.
    Shifted {
        /// The original line number when the anchor was created.
        original_line: u32,
        /// The current line number after drift.
        current_line: u32,
    },
    /// The anchored line itself was modified (content changed).
    Modified,
    /// The anchored line was deleted.
    Deleted,
}

impl DriftResult {
    /// Get the current line number if the line still exists.
    #[must_use]
    pub const fn current_line(&self) -> Option<u32> {
        match self {
            Self::Unchanged { current_line } | Self::Shifted { current_line, .. } => {
                Some(*current_line)
            }
            Self::Modified | Self::Deleted => None,
        }
    }

    /// Check if the anchor is still valid (line exists and wasn't modified).
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        matches!(self, Self::Unchanged { .. } | Self::Shifted { .. })
    }
}

/// A parsed hunk header from unified diff format.
///
/// Format: `@@ -old_start,old_count +new_start,new_count @@`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkHeader {
    /// Start line in the old file (1-indexed).
    pub old_start: u32,
    /// Number of lines in the old file (0 means no lines, e.g., empty file or pure addition).
    pub old_count: u32,
    /// Start line in the new file (1-indexed).
    pub new_start: u32,
    /// Number of lines in the new file.
    pub new_count: u32,
}

impl HunkHeader {
    /// Parse a hunk header line.
    ///
    /// Handles formats:
    /// - `@@ -1,5 +1,7 @@` (standard)
    /// - `@@ -1 +1 @@` (count defaults to 1)
    /// - `@@ -0,0 +1,3 @@` (new file)
    pub fn parse(line: &str) -> Result<Self> {
        let line = line.trim();
        if !line.starts_with("@@") {
            bail!("Not a hunk header: {line}");
        }

        // Find the second @@ to isolate the range part
        let end_idx = line[2..].find("@@").map(|i| i + 2);
        let range_part = if let Some(idx) = end_idx {
            &line[2..idx]
        } else {
            &line[2..]
        };

        let range_part = range_part.trim();

        // Split on space to get old and new ranges
        let parts: Vec<&str> = range_part.split_whitespace().collect();
        if parts.len() < 2 {
            bail!("Invalid hunk header format: {line}");
        }

        let (old_start, old_count) = Self::parse_range(parts[0], '-')?;
        let (new_start, new_count) = Self::parse_range(parts[1], '+')?;

        Ok(Self {
            old_start,
            old_count,
            new_start,
            new_count,
        })
    }

    /// Parse a range like `-1,5` or `+1` into (start, count).
    fn parse_range(s: &str, prefix: char) -> Result<(u32, u32)> {
        let s = s.trim_start_matches(prefix);

        if let Some((start_str, count_str)) = s.split_once(',') {
            let start: u32 = start_str.parse()?;
            let count: u32 = count_str.parse()?;
            Ok((start, count))
        } else {
            // No comma means count is 1
            let start: u32 = s.parse()?;
            Ok((start, 1))
        }
    }
}

/// A single line change within a hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    /// Line exists in both old and new (context line, starts with space).
    Context,
    /// Line was added (starts with `+`).
    Added,
    /// Line was deleted (starts with `-`).
    Deleted,
}

/// A parsed diff hunk containing the header and line changes.
#[derive(Debug, Clone)]
pub struct Hunk {
    pub header: HunkHeader,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// Parse a hunk from diff lines (header line followed by content lines).
    pub fn parse(lines: &[&str]) -> Result<Self> {
        if lines.is_empty() {
            bail!("Empty hunk");
        }

        let header = HunkHeader::parse(lines[0])?;

        let mut diff_lines = Vec::new();
        for line in &lines[1..] {
            if line.starts_with("@@") {
                // Next hunk started
                break;
            }
            if line.starts_with('-') {
                diff_lines.push(DiffLine::Deleted);
            } else if line.starts_with('+') {
                diff_lines.push(DiffLine::Added);
            } else if line.starts_with(' ') || line.is_empty() {
                // Space prefix or empty line (some diffs omit trailing space)
                diff_lines.push(DiffLine::Context);
            }
            // Skip other lines (e.g., "\ No newline at end of file")
        }

        Ok(Self {
            header,
            lines: diff_lines,
        })
    }
}

fn change_block_contains_addition(lines: &[DiffLine], index: usize) -> bool {
    let mut start = index;
    while start > 0 && !matches!(lines[start - 1], DiffLine::Context) {
        start -= 1;
    }

    let mut end = index + 1;
    while end < lines.len() && !matches!(lines[end], DiffLine::Context) {
        end += 1;
    }

    lines[start..end]
        .iter()
        .any(|line| matches!(line, DiffLine::Added))
}

/// Parse hunks from a unified diff for a single file.
///
/// This extracts all `@@ ... @@` sections and their content.
pub fn parse_hunks(diff: &str) -> Result<Vec<Hunk>> {
    let lines: Vec<&str> = diff.lines().collect();
    let mut hunks = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        if lines[i].starts_with("@@") {
            // Find the end of this hunk (next @@ or end of file section)
            let start = i;
            i += 1;
            while i < lines.len()
                && !lines[i].starts_with("@@")
                && !lines[i].starts_with("diff --git")
            {
                i += 1;
            }

            let hunk = Hunk::parse(&lines[start..i])?;
            hunks.push(hunk);
        } else {
            i += 1;
        }
    }

    Ok(hunks)
}

/// Compute the last old-file line number (1-indexed) that a hunk spans.
///
/// `old_count == 0` marks a pure-addition or new-file hunk that consumes no
/// old lines; its effective end is one line *before* `old_start` (saturating
/// at 0 so `old_start == 0` never underflows), which is exactly what lets a
/// pure-addition hunk be treated as "before" any target line at or after
/// `old_start` (see `bn-2hu`/commit `3df6d2b`).
///
/// For a normal hunk (`old_count > 0`), the end is `old_start + old_count - 1`.
/// `HunkHeader::parse` accepts any `u32` pair, so a header can claim a span
/// that does not fit in `u32` (e.g. `@@ -4294967295,2 +1,1 @@`). Rather than
/// overflow (panic in debug, wrap in release), this returns `None` to signal
/// "this header cannot describe a real hunk" so the caller can report an
/// error instead of computing a wrong or out-of-range line.
///
/// # Postconditions (see the `proofs` module for the Kani proof)
/// - Never panics, for any `u32` inputs.
/// - If it returns `Some(end)`, then `end as u64 + 1 >= old_start as u64`
///   (i.e. `end >= old_start - 1`, stated without underflow).
/// - If it returns `Some(end)` and `old_count > 0`, then
///   `end as u64 == old_start as u64 + old_count as u64 - 1` exactly.
#[must_use]
const fn hunk_old_end(old_start: u32, old_count: u32) -> Option<u32> {
    if old_count == 0 {
        Some(old_start.saturating_sub(1))
    } else {
        match old_start.checked_add(old_count) {
            Some(sum) => sum.checked_sub(1),
            None => None,
        }
    }
}

/// Compute the new value of a drifting line counter after passing through a
/// hunk that lies **entirely before** it, given the hunk's old/new line
/// counts.
///
/// The net effect of such a hunk is `+new_count` then `-old_count` (insertions
/// push the line down, deletions pull it up). Both legs are computed with
/// checked arithmetic: this returns `None` if applying the shift would not
/// fit in `u32` (would overflow past `u32::MAX`, or underflow below 0)
/// instead of silently wrapping or clamping to a value that no longer
/// corresponds to any real line. For any hunk that actually arose from a real
/// diff of a real file, this cannot happen (see `calculate_drift`'s
/// documentation); it is only reachable via a header describing an
/// impossible span.
///
/// # Postconditions (see the `proofs` module for the Kani proof)
/// - Never panics, for any `u32` inputs.
/// - If it returns `Some(shifted)`, then
///   `shifted as i64 == current_line as i64 + new_count as i64 - old_count as i64`
///   exactly.
#[must_use]
const fn shift_for_hunk_before(current_line: u32, old_count: u32, new_count: u32) -> Option<u32> {
    if new_count > old_count {
        current_line.checked_add(new_count - old_count)
    } else {
        current_line.checked_sub(old_count - new_count)
    }
}

/// Advance the per-line `(old_line, new_line)` counters used while walking a
/// hunk's body, given the kind of the line just processed.
///
/// A real hunk body has at most a handful of lines, so this can never
/// overflow for an honest header. But nothing cross-validates a hunk's
/// declared `old_count`/`new_count` against the number of lines actually
/// present in its body (`Hunk::parse` just reads until the next `@@` or `diff
/// --git`), so an adversarial header combined with a long body can drive
/// `old_start`/`new_start` (and therefore these counters) arbitrarily close
/// to `u32::MAX`. This returns `None` rather than overflow (panic in debug,
/// wrap in release) so the walk can be aborted with an error.
///
/// # Postconditions (see the `proofs` module for the Kani proof)
/// - Never panics, for any `u32` inputs and any `DiffLine`.
/// - If it returns `Some((old, new))`, `old`/`new` equal `old_line`/`new_line`
///   plus exactly the increments implied by `diff_line` (0 or 1 each).
#[must_use]
const fn advance_hunk_line(
    old_line: u32,
    new_line: u32,
    diff_line: &DiffLine,
) -> Option<(u32, u32)> {
    match diff_line {
        DiffLine::Context => match (old_line.checked_add(1), new_line.checked_add(1)) {
            (Some(o), Some(n)) => Some((o, n)),
            _ => None,
        },
        DiffLine::Deleted => match old_line.checked_add(1) {
            Some(o) => Some((o, new_line)),
            None => None,
        },
        DiffLine::Added => match new_line.checked_add(1) {
            Some(n) => Some((old_line, n)),
            None => None,
        },
    }
}

/// Calculate drift for a line anchor between two commits.
///
/// # Arguments
///
/// * `repo` - The `JjRepo` wrapper
/// * `file` - Path to the file (relative to repo root)
/// * `original_line` - The line number when the anchor was created (1-indexed)
/// * `original_commit` - The commit where the anchor was created
/// * `current_commit` - The current commit to check against
///
/// # Returns
///
/// A `DriftResult` indicating whether the line is unchanged, shifted, modified, or deleted.
pub fn calculate_drift(
    repo: &dyn ScmRepo,
    file: &str,
    original_line: u32,
    original_commit: &str,
    current_commit: &str,
) -> Result<DriftResult> {
    // Get the diff between original and current commit for this file
    let diff = repo.diff_git_file(original_commit, current_commit, file)?;

    // If diff is empty, file is unchanged
    if diff.trim().is_empty() {
        return Ok(DriftResult::Unchanged {
            current_line: original_line,
        });
    }

    // Parse the hunks
    let hunks = parse_hunks(&diff)?;

    // If no hunks, file is unchanged (diff might have only header)
    if hunks.is_empty() {
        return Ok(DriftResult::Unchanged {
            current_line: original_line,
        });
    }

    // Track how the original line number shifts through each hunk
    let mut current_line = original_line;

    for hunk in &hunks {
        // Check if this hunk affects lines before or at our target line.
        // Special handling for old_count=0 (pure-addition or new-file hunks):
        // When old_count=0, the hunk doesn't consume any old lines, so the
        // effective end is old_start - 1 (no lines spanned in the old file).
        let hunk_old_end =
            hunk_old_end(hunk.header.old_start, hunk.header.old_count).ok_or_else(|| {
                anyhow::anyhow!(
                    "hunk header describes an old-file span that does not fit in u32: \
                     old_start={}, old_count={}",
                    hunk.header.old_start,
                    hunk.header.old_count
                )
            })?;

        if hunk.header.old_start > original_line {
            // Hunk is entirely after our line - no effect
            continue;
        }

        if hunk_old_end < original_line {
            // Hunk is entirely before our line - adjust by net change.
            // This now correctly handles pure-addition hunks (old_count=0)
            // because hunk_old_end will be old_start - 1, allowing the shift.
            current_line =
                shift_for_hunk_before(current_line, hunk.header.old_count, hunk.header.new_count)
                    .ok_or_else(|| {
                    anyhow::anyhow!(
                        "hunk shift does not fit in u32: current_line={current_line}, \
                     old_count={}, new_count={}",
                        hunk.header.old_count,
                        hunk.header.new_count
                    )
                })?;
            continue;
        }

        // The hunk overlaps with our target line
        // We need to walk through the hunk to see exactly what happened
        let mut old_line = hunk.header.old_start;
        let mut new_line = hunk.header.new_start;
        let mut found_as_context = false;

        for (index, diff_line) in hunk.lines.iter().enumerate() {
            match diff_line {
                DiffLine::Context => {
                    if old_line == original_line {
                        // Found our line, it's unchanged in this hunk
                        current_line = new_line;
                        found_as_context = true;
                        break;
                    }
                }
                DiffLine::Deleted => {
                    if old_line == original_line {
                        // A replacement appears in unified diffs as one or more
                        // deletions plus additions in the same contiguous change
                        // block. Treat that as modified, not deleted.
                        if change_block_contains_addition(&hunk.lines, index) {
                            return Ok(DriftResult::Modified);
                        }
                        return Ok(DriftResult::Deleted);
                    }
                }
                DiffLine::Added => {}
            }
            (old_line, new_line) =
                advance_hunk_line(old_line, new_line, diff_line).ok_or_else(|| {
                    anyhow::anyhow!(
                        "hunk line walk overflowed u32 counters: old_line={old_line}, \
                         new_line={new_line}"
                    )
                })?;
        }

        // If we walked through without finding it as context, it might be modified
        // (deleted then re-added in different form)
        if !found_as_context {
            return Ok(DriftResult::Modified);
        }
    }

    // Determine the result
    if current_line == original_line {
        Ok(DriftResult::Unchanged { current_line })
    } else {
        Ok(DriftResult::Shifted {
            original_line,
            current_line,
        })
    }
}

/// Kani proofs for the three pure functions that replaced the unchecked u32
/// arithmetic in `calculate_drift`'s hunk walk (`bn-fx8g`).
///
/// Each harness is complete, not approximate: `kani::any()` ranges over the
/// full `u32` domain with no `kani::assume` narrowing it, so these are
/// unconditional proofs over every possible hunk header / line-counter value,
/// not just the ones a well-formed diff would produce. That is deliberate --
/// the whole point is that `HunkHeader::parse` does not validate its input,
/// so the arithmetic must be safe even for headers no real diff would emit.
#[cfg(kani)]
mod proofs {
    use super::{advance_hunk_line, hunk_old_end, shift_for_hunk_before, DiffLine};

    /// `hunk_old_end` never panics, and when it returns a value that value is
    /// never less than `old_start - 1` (stated as `end + 1 >= old_start` to
    /// avoid underflow in the property itself) and, for `old_count > 0`,
    /// equals `old_start + old_count - 1` exactly. It also always succeeds
    /// (never returns `None`) for `old_count == 0`, since
    /// `old_start.saturating_sub(1)` cannot fail -- a harness that only
    /// constrained the `Some` case would pass vacuously if that branch were
    /// deleted and silently started returning `None` instead.
    #[kani::proof]
    fn hunk_old_end_never_panics_and_is_in_range() {
        let old_start: u32 = kani::any();
        let old_count: u32 = kani::any();

        let result = hunk_old_end(old_start, old_count);

        if old_count == 0 {
            assert!(result == Some(old_start.saturating_sub(1)));
        }

        if let Some(end) = result {
            // end >= old_start - 1, restated without underflow risk.
            assert!(u64::from(end) + 1 >= u64::from(old_start));
            if old_count > 0 {
                assert!(u64::from(end) == u64::from(old_start) + u64::from(old_count) - 1);
            }
        }
    }

    /// `shift_for_hunk_before` never panics, and when it returns a value that
    /// value equals `current_line + new_count - old_count` exactly (computed
    /// in `i64` to state the identity without wraparound). It also always
    /// succeeds when `old_count == new_count` (net shift of zero can never
    /// overflow), guarding against a harness that only pins the `Some` case
    /// and would pass vacuously if that were broken to return `None`.
    #[kani::proof]
    fn shift_for_hunk_before_never_panics_and_is_exact() {
        let current_line: u32 = kani::any();
        let old_count: u32 = kani::any();
        let new_count: u32 = kani::any();

        let result = shift_for_hunk_before(current_line, old_count, new_count);

        if old_count == new_count {
            assert!(result == Some(current_line));
        }

        if let Some(shifted) = result {
            let expected = i64::from(current_line) + i64::from(new_count) - i64::from(old_count);
            assert!(i64::from(shifted) == expected);
        }
    }

    /// `advance_hunk_line` never panics, and when it returns updated counters
    /// they equal the inputs plus exactly the increments implied by the line
    /// kind (Context: both +1; Deleted: old +1; Added: new +1).
    #[kani::proof]
    fn advance_hunk_line_never_panics_and_is_exact() {
        let old_line: u32 = kani::any();
        let new_line: u32 = kani::any();
        let kind: u8 = kani::any();
        kani::assume(kind < 3);
        let diff_line = match kind {
            0 => DiffLine::Context,
            1 => DiffLine::Deleted,
            _ => DiffLine::Added,
        };

        if let Some((new_old_line, new_new_line)) =
            advance_hunk_line(old_line, new_line, &diff_line)
        {
            let (expect_old_delta, expect_new_delta): (u64, u64) = match diff_line {
                DiffLine::Context => (1, 1),
                DiffLine::Deleted => (1, 0),
                DiffLine::Added => (0, 1),
            };
            assert!(u64::from(new_old_line) == u64::from(old_line) + expect_old_delta);
            assert!(u64::from(new_new_line) == u64::from(new_line) + expect_new_delta);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scm::jj::JjScmRepo;
    use crate::scm::{ScmKind, ScmRepo};
    use proptest::prelude::*;
    use std::env;
    use std::path::{Path, PathBuf};

    struct MockRepo {
        root: PathBuf,
        diff: String,
    }

    impl MockRepo {
        fn new(diff: &str) -> Self {
            Self {
                root: PathBuf::from("/tmp/seal-drift-test"),
                diff: diff.to_string(),
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
            Ok(self.diff.clone())
        }

        fn diff_git_file(&self, _from: &str, _to: &str, _file: &str) -> Result<String> {
            Ok(self.diff.clone())
        }

        fn changed_files_between(&self, _from: &str, _to: &str) -> Result<Vec<String>> {
            Ok(vec!["test.rs".to_string()])
        }

        fn file_exists(&self, _rev: &str, _path: &str) -> Result<bool> {
            Ok(true)
        }

        fn show_file(&self, _rev: &str, _path: &str) -> Result<String> {
            Ok(String::new())
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
    fn test_parse_hunk_header_standard() {
        let header = HunkHeader::parse("@@ -1,5 +1,7 @@").unwrap();
        assert_eq!(header.old_start, 1);
        assert_eq!(header.old_count, 5);
        assert_eq!(header.new_start, 1);
        assert_eq!(header.new_count, 7);
    }

    #[test]
    fn test_parse_hunk_header_no_count() {
        let header = HunkHeader::parse("@@ -1 +1 @@").unwrap();
        assert_eq!(header.old_start, 1);
        assert_eq!(header.old_count, 1);
        assert_eq!(header.new_start, 1);
        assert_eq!(header.new_count, 1);
    }

    #[test]
    fn test_parse_hunk_header_new_file() {
        let header = HunkHeader::parse("@@ -0,0 +1,10 @@").unwrap();
        assert_eq!(header.old_start, 0);
        assert_eq!(header.old_count, 0);
        assert_eq!(header.new_start, 1);
        assert_eq!(header.new_count, 10);
    }

    #[test]
    fn test_parse_hunk_header_with_context() {
        // Some diffs include function context after the @@
        let header = HunkHeader::parse("@@ -10,6 +10,8 @@ fn main() {").unwrap();
        assert_eq!(header.old_start, 10);
        assert_eq!(header.old_count, 6);
        assert_eq!(header.new_start, 10);
        assert_eq!(header.new_count, 8);
    }

    #[test]
    fn test_parse_hunks_simple() {
        let diff = r#"diff --git a/test.rs b/test.rs
index 1234567..abcdefg 100644
--- a/test.rs
+++ b/test.rs
@@ -1,3 +1,4 @@
 fn main() {
+    println!("new line");
     println!("hello");
 }
"#;
        let hunks = parse_hunks(diff).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].header.old_start, 1);
        assert_eq!(hunks[0].header.old_count, 3);
        assert_eq!(hunks[0].header.new_start, 1);
        assert_eq!(hunks[0].header.new_count, 4);

        // Context, Added, Context, Context
        assert_eq!(hunks[0].lines.len(), 4);
        assert_eq!(hunks[0].lines[0], DiffLine::Context);
        assert_eq!(hunks[0].lines[1], DiffLine::Added);
        assert_eq!(hunks[0].lines[2], DiffLine::Context);
        assert_eq!(hunks[0].lines[3], DiffLine::Context);
    }

    #[test]
    fn test_parse_hunks_multiple() {
        let diff = r#"diff --git a/test.rs b/test.rs
--- a/test.rs
+++ b/test.rs
@@ -1,3 +1,4 @@
 fn main() {
+    println!("new line");
     println!("hello");
 }
@@ -10,3 +11,4 @@
 fn other() {
+    // comment
     todo!()
 }
"#;
        let hunks = parse_hunks(diff).unwrap();
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].header.old_start, 1);
        assert_eq!(hunks[1].header.old_start, 10);
    }

    #[test]
    fn test_drift_result_current_line() {
        assert_eq!(
            DriftResult::Unchanged { current_line: 5 }.current_line(),
            Some(5)
        );
        assert_eq!(
            DriftResult::Shifted {
                original_line: 5,
                current_line: 7
            }
            .current_line(),
            Some(7)
        );
        assert_eq!(DriftResult::Modified.current_line(), None);
        assert_eq!(DriftResult::Deleted.current_line(), None);
    }

    #[test]
    fn test_drift_result_is_valid() {
        assert!(DriftResult::Unchanged { current_line: 5 }.is_valid());
        assert!(DriftResult::Shifted {
            original_line: 5,
            current_line: 7
        }
        .is_valid());
        assert!(!DriftResult::Modified.is_valid());
        assert!(!DriftResult::Deleted.is_valid());
    }

    /// Test drift calculation using real jj diff from the repo.
    /// This test uses `root()` as the original commit and `@` as current,
    /// which should always work on any jj repo.
    #[test]
    fn test_calculate_drift_real_repo() {
        let Some(repo) = test_repo() else {
            return;
        };

        // Get the current commit to use as both original and current
        // This should result in Unchanged since there's no diff
        let current = repo.current_commit().unwrap();

        // A file that definitely exists
        let result = calculate_drift(&repo, "Cargo.toml", 1, &current, &current);
        assert!(result.is_ok());

        // Same commit should be unchanged
        let drift = result.unwrap();
        assert_eq!(drift, DriftResult::Unchanged { current_line: 1 });
    }

    #[test]
    fn test_calculate_drift_reports_replaced_line_as_modified() {
        let diff = r"diff --git a/test.rs b/test.rs
--- a/test.rs
+++ b/test.rs
@@ -1,4 +1,4 @@
 line1
 line2
-old value
+new value
 line4
";
        let repo = MockRepo::new(diff);

        let drift = calculate_drift(&repo, "test.rs", 3, "old", "new").unwrap();

        assert_eq!(drift, DriftResult::Modified);
    }

    #[test]
    fn test_calculate_drift_keeps_pure_deletion_as_deleted() {
        let diff = r"diff --git a/test.rs b/test.rs
--- a/test.rs
+++ b/test.rs
@@ -1,4 +1,3 @@
 line1
 line2
-removed
 line4
";
        let repo = MockRepo::new(diff);

        let drift = calculate_drift(&repo, "test.rs", 3, "old", "new").unwrap();

        assert_eq!(drift, DriftResult::Deleted);
    }

    #[test]
    fn test_calculate_drift_does_not_confuse_separate_insertion_with_modification() {
        let diff = r"diff --git a/test.rs b/test.rs
--- a/test.rs
+++ b/test.rs
@@ -1,6 +1,6 @@
 line1
-deleted-anchor
 line3
 line4
+separate insertion
 line5
 line6
";
        let repo = MockRepo::new(diff);

        let drift = calculate_drift(&repo, "test.rs", 2, "old", "new").unwrap();

        assert_eq!(drift, DriftResult::Deleted);
    }

    /// Test drift detection with a synthetic diff scenario.
    /// We use the existing `diff_git_file` method output format.
    #[test]
    fn test_drift_unchanged_after_later_changes() {
        // Simulate: changes at line 10-12, checking line 5
        // Line 5 should be unchanged
        let diff = r"diff --git a/test.rs b/test.rs
--- a/test.rs
+++ b/test.rs
@@ -10,3 +10,5 @@
 fn other() {
+    // new comment
+    // another
     todo!()
 }
";
        let hunks = parse_hunks(diff).unwrap();
        assert_eq!(hunks.len(), 1);
        // Hunk starts at line 10, our line 5 is before it
        // So line 5 should be unchanged
    }

    /// Test drift when lines are inserted before the anchor.
    #[test]
    fn test_drift_shifted_by_insertion() {
        // Insert 2 lines at line 3, original anchor at line 5 should become line 7
        let diff = r"diff --git a/test.rs b/test.rs
--- a/test.rs
+++ b/test.rs
@@ -1,5 +1,7 @@
 line1
 line2
+inserted1
+inserted2
 line3
 line4
 line5
";
        let hunks = parse_hunks(diff).unwrap();

        // Verify the hunk structure
        assert_eq!(hunks[0].header.old_count, 5);
        assert_eq!(hunks[0].header.new_count, 7);

        // Net change: +2 lines
        // Original line 5 should shift to line 7
    }

    /// Test drift when lines are deleted before the anchor.
    #[test]
    fn test_drift_shifted_by_deletion() {
        // Delete 2 lines before line 5, anchor should become line 3
        let diff = r"diff --git a/test.rs b/test.rs
--- a/test.rs
+++ b/test.rs
@@ -1,5 +1,3 @@
 line1
-line2
-line3
 line4
 line5
";
        let hunks = parse_hunks(diff).unwrap();

        // Verify the hunk structure
        assert_eq!(hunks[0].header.old_count, 5);
        assert_eq!(hunks[0].header.new_count, 3);
    }

    /// Test drift when the anchored line is deleted.
    #[test]
    fn test_drift_deleted_line() {
        // Line 3 is deleted
        let diff = r"diff --git a/test.rs b/test.rs
--- a/test.rs
+++ b/test.rs
@@ -1,5 +1,4 @@
 line1
 line2
-line3
 line4
 line5
";
        let hunks = parse_hunks(diff).unwrap();

        // Walk through the hunk manually
        // old_line 1 -> Context
        // old_line 2 -> Context
        // old_line 3 -> Deleted <-- our anchor would hit this
        // old_line 4 -> Context
        // old_line 5 -> Context
        assert_eq!(hunks[0].lines[0], DiffLine::Context); // line1
        assert_eq!(hunks[0].lines[1], DiffLine::Context); // line2
        assert_eq!(hunks[0].lines[2], DiffLine::Deleted); // line3 removed
        assert_eq!(hunks[0].lines[3], DiffLine::Context); // line4
        assert_eq!(hunks[0].lines[4], DiffLine::Context); // line5
    }

    /// Test pure-addition hunk (`old_count=0`) at a specific line.
    /// Pure additions should shift lines at or after the insertion point.
    #[test]
    fn test_parse_hunks_pure_addition() {
        // @@ -5,0 +5,3 @@ means: insert 3 lines at position 5 (no old lines consumed)
        let diff = r"diff --git a/test.rs b/test.rs
--- a/test.rs
+++ b/test.rs
@@ -5,0 +5,3 @@
+new1
+new2
+new3
";
        let hunks = parse_hunks(diff).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].header.old_start, 5);
        assert_eq!(hunks[0].header.old_count, 0); // Pure addition
        assert_eq!(hunks[0].header.new_start, 5);
        assert_eq!(hunks[0].header.new_count, 3);

        // All lines should be Added
        assert_eq!(hunks[0].lines.len(), 3);
        assert_eq!(hunks[0].lines[0], DiffLine::Added);
        assert_eq!(hunks[0].lines[1], DiffLine::Added);
        assert_eq!(hunks[0].lines[2], DiffLine::Added);
    }

    /// Test new-file diff: @@ -0,0 +1,N @@
    /// When a file is created, `old_count=0` and `old_start=0`.
    #[test]
    fn test_parse_hunks_new_file() {
        let diff = r#"diff --git a/new.rs b/new.rs
new file mode 100644
--- /dev/null
+++ b/new.rs
@@ -0,0 +1,3 @@
+fn hello() {
+    println!("world");
+}
"#;
        let hunks = parse_hunks(diff).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].header.old_start, 0);
        assert_eq!(hunks[0].header.old_count, 0);
        assert_eq!(hunks[0].header.new_start, 1);
        assert_eq!(hunks[0].header.new_count, 3);

        // All lines should be Added
        assert_eq!(hunks[0].lines.len(), 3);
        for line in &hunks[0].lines {
            assert_eq!(*line, DiffLine::Added);
        }
    }

    /// Test mixed hunks: pure-addition at line 5, then normal edit at line 15.
    /// This verifies the fix for `hunk_old_end` calculation with `old_count=0`.
    #[test]
    fn test_parse_hunks_mixed_pure_and_normal() {
        let diff = r"diff --git a/test.rs b/test.rs
--- a/test.rs
+++ b/test.rs
@@ -5,0 +5,2 @@
+inserted1
+inserted2
@@ -15,3 +17,4 @@
 line15
-removed
+modified
 line16
";
        let hunks = parse_hunks(diff).unwrap();
        assert_eq!(hunks.len(), 2);

        // First hunk: pure addition
        assert_eq!(hunks[0].header.old_count, 0);
        assert_eq!(hunks[0].header.new_count, 2);

        // Second hunk: normal edit
        assert_eq!(hunks[1].header.old_count, 3);
        assert_eq!(hunks[1].header.new_count, 4);
    }

    /// Test drift calculation with pure-addition hunk:
    /// If we have 5 lines total, and insert 3 lines at line 3,
    /// line 3 stays at 3 (insertion happens before), but lines at/after 3 shift down by 3.
    /// This is a semantic test that verifies the `hunk_old_end` fix.
    #[test]
    fn test_drift_pure_addition_before_anchor() {
        // Hunk: @@ -3,0 +3,3 @@ (insert 3 lines before line 3)
        // Original anchor at line 5 should shift to line 8 (+3)
        let diff = r"diff --git a/test.rs b/test.rs
--- a/test.rs
+++ b/test.rs
@@ -1,5 +1,8 @@
 line1
 line2
+inserted1
+inserted2
+inserted3
 line3
 line4
 line5
";
        let hunks = parse_hunks(diff).unwrap();
        assert_eq!(hunks.len(), 1);
        // Hunk spans lines 1-5 in old, becomes 1-8 in new
        // The pure addition is logically at line 3 (before line3)
        // A line at original position 5 should shift to 8
    }

    /// Test drift calculation with pure-addition hunk at insertion point:
    /// If original anchor is at line 5 and we insert 3 lines at line 5,
    /// the line should shift to 8 (+3).
    #[test]
    fn test_drift_pure_addition_at_anchor() {
        let diff = r"diff --git a/test.rs b/test.rs
--- a/test.rs
+++ b/test.rs
@@ -1,5 +1,8 @@
 line1
 line2
 line3
 line4
+inserted1
+inserted2
+inserted3
 line5
";
        let hunks = parse_hunks(diff).unwrap();
        assert_eq!(hunks.len(), 1);
        // Old lines 1-5 map to new lines 1-5 (context) then +3 inserted lines, then line5
        // Original line 5 should become line 8 in the new version
    }

    /// Test that pure-addition at `old_count=0` doesn't break `hunk_old_end` calculation.
    /// This directly tests the fix for the `saturating_sub(1)` bug.
    #[test]
    fn test_hunk_header_pure_addition_old_count_zero() {
        let header = HunkHeader::parse("@@ -5,0 +5,3 @@").unwrap();
        assert_eq!(header.old_start, 5);
        assert_eq!(header.old_count, 0);
        assert_eq!(header.new_start, 5);
        assert_eq!(header.new_count, 3);

        // With the fix, hunk_old_end should be calculated as:
        // if old_count == 0 { old_start - 1 } else { old_start + old_count - 1 }
        // So hunk_old_end = 5 - 1 = 4
        // This means the hunk is "before" lines >= 5, so lines at 5+ shift by +3
    }

    /// A hunk header like `@@ -4294967295,2 +1,1 @@` parses fine (`HunkHeader::parse`
    /// accepts any `u32`), but `old_start + old_count - 1` overflows `u32` when
    /// computing the last old-file line the hunk spans (4294967295 + 2 - 1 does not
    /// fit). Before the fix, this panics in debug builds (and wraps in release).
    /// `calculate_drift` must reject an unrepresentable header instead of panicking
    /// or silently wrapping to a nonsense line number.
    #[test]
    fn test_calculate_drift_hunk_old_end_overflow_does_not_panic() {
        let diff = r"diff --git a/test.rs b/test.rs
--- a/test.rs
+++ b/test.rs
@@ -4294967295,2 +1,1 @@
-a
+b
";
        let repo = MockRepo::new(diff);

        let result = calculate_drift(&repo, "test.rs", 5, "old", "new");

        assert!(
            result.is_err(),
            "a hunk header whose old-end overflows u32 must be rejected as an error, \
             not panic or silently wrap: got {result:?}"
        );
    }

    /// A hunk header claiming `new_count` far larger than `old_count` relative to
    /// `current_line` can overflow the `current_line += new_lines - old_lines` shift
    /// for a hunk entirely before the anchor line. Must not panic or wrap.
    #[test]
    fn test_calculate_drift_shift_before_overflow_does_not_panic() {
        let diff = r"diff --git a/test.rs b/test.rs
--- a/test.rs
+++ b/test.rs
@@ -1,1 +1,4294967295 @@
-a
+b
";
        let repo = MockRepo::new(diff);

        // original_line = 2 is after this hunk (old_start=1, old_count=1, so
        // hunk_old_end=1 < 2), so the "entirely before" shift path is taken:
        // current_line += new_count - old_count, which overflows u32.
        let result = calculate_drift(&repo, "test.rs", 2, "old", "new");

        assert!(
            result.is_err(),
            "a hunk shift that overflows u32 must be rejected as an error, \
             not panic or silently wrap: got {result:?}"
        );
    }

    /// Walking through a hunk's lines increments `old_line` on Context/Deleted
    /// lines and `new_line` on Context/Added lines. The header's declared
    /// `old_count`/`new_count` need not match the actual number of lines in the
    /// hunk body (nothing cross-validates them), so a hunk whose `new_start` is
    /// near `u32::MAX` can overflow `new_line` while walking through a run of
    /// `Added` lines -- entirely independent of the `hunk_old_end` and
    /// before-hunk-shift overflows covered above, and not fixed by guarding
    /// those alone. Must not panic or wrap.
    #[test]
    fn test_calculate_drift_hunk_walk_overflow_does_not_panic() {
        let diff = r"diff --git a/test.rs b/test.rs
--- a/test.rs
+++ b/test.rs
@@ -1,3 +4294967294,2 @@
+x
+y
 a
 b
 c
";
        let repo = MockRepo::new(diff);

        // old_start=1, old_count=3 keep hunk_old_end small (no overflow there);
        // original_line=3 puts the target inside this hunk's old range, so the
        // walk runs. Before it ever reaches old_line==3, the two leading Added
        // lines push new_line (starting at 4294967294) past u32::MAX.
        let result = calculate_drift(&repo, "test.rs", 3, "old", "new");

        assert!(
            result.is_err(),
            "a hunk walk whose line counters overflow u32 must be rejected as an \
             error, not panic or silently wrap: got {result:?}"
        );
    }

    // ------------------------------------------------------------------
    // Property-based test: drift preserves content and order of surviving
    // lines, for hand-built single-hunk diffs derived from a random edit
    // script (keep/delete each old line, insert 0-3 fresh lines in each gap).
    // ------------------------------------------------------------------

    /// An edit script: `keep[i]` says whether old line `i+1` survives, and
    /// `inserts[g]` is how many fresh lines are inserted in gap `g` (gap 0 is
    /// before old line 1, gap `i+1` is right after old line `i+1`, so there
    /// are `keep.len() + 1` gaps).
    fn edit_script_strategy() -> impl proptest::strategy::Strategy<Value = (Vec<bool>, Vec<usize>)>
    {
        prop::collection::vec(proptest::bool::ANY, 1..=8).prop_flat_map(|keep| {
            let gaps = keep.len() + 1;
            prop::collection::vec(0usize..=3, gaps).prop_map(move |inserts| (keep.clone(), inserts))
        })
    }

    /// Build a single-hunk unified diff from an edit script, plus the
    /// old-file and new-file line contents it implies. Old line `i+1`'s
    /// content is the unique string `format!("old{i}")`; each inserted line
    /// gets a unique `format!("new{k}")`, so content identity can be checked
    /// by simple string equality.
    fn build_diff_from_edit_script(
        keep: &[bool],
        inserts: &[usize],
    ) -> (String, Vec<String>, Vec<String>) {
        let old_len = keep.len();
        let old_file: Vec<String> = (1..=old_len).map(|i| format!("old{i}")).collect();

        let mut body = String::new();
        let mut new_file = Vec::new();
        let mut new_count = 0usize;
        let mut fresh = 0usize;

        for gap in 0..=old_len {
            for _ in 0..inserts[gap] {
                let line = format!("new{fresh}");
                fresh += 1;
                body.push('+');
                body.push_str(&line);
                body.push('\n');
                new_file.push(line);
                new_count += 1;
            }
            if gap < old_len {
                let content = &old_file[gap];
                if keep[gap] {
                    body.push(' ');
                    body.push_str(content);
                    body.push('\n');
                    new_file.push(content.clone());
                    new_count += 1;
                } else {
                    body.push('-');
                    body.push_str(content);
                    body.push('\n');
                }
            }
        }

        let header = format!("@@ -1,{old_len} +1,{new_count} @@\n");
        let diff =
            format!("diff --git a/test.rs b/test.rs\n--- a/test.rs\n+++ b/test.rs\n{header}{body}");

        (diff, old_file, new_file)
    }

    proptest! {
        /// Every line `calculate_drift` reports as `Unchanged`/`Shifted` maps to a
        /// line whose content is identical in the new file, and surviving
        /// (kept) lines keep their relative order (current_line strictly
        /// increases as original_line increases among kept lines).
        #[test]
        fn drift_preserves_content_and_order_of_surviving_lines(
            (keep, inserts) in edit_script_strategy()
        ) {
            let (diff, old_file, new_file) = build_diff_from_edit_script(&keep, &inserts);
            let repo = MockRepo::new(&diff);

            let mut prev_current_line: Option<u32> = None;

            for (index, &kept) in keep.iter().enumerate() {
                if !kept {
                    continue;
                }
                let original_line = (index + 1) as u32;

                let drift = calculate_drift(&repo, "test.rs", original_line, "old", "new")
                    .expect("a hunk built from a well-formed edit script must not error");

                let current_line = match drift {
                    DriftResult::Unchanged { current_line } | DriftResult::Shifted { current_line, .. } => current_line,
                    other => panic!(
                        "kept line {original_line} must be Unchanged/Shifted, got {other:?}"
                    ),
                };

                // Content identity: the reported new position holds the same
                // content as the original line did.
                prop_assert_eq!(
                    &new_file[(current_line - 1) as usize],
                    &old_file[index],
                    "line {} drifted to {} but content differs",
                    original_line,
                    current_line
                );

                // Relative order of survivors is preserved.
                if let Some(prev) = prev_current_line {
                    prop_assert!(
                        current_line > prev,
                        "surviving lines must keep relative order: {} -> {} is not after previous {}",
                        original_line,
                        current_line,
                        prev
                    );
                }
                prev_current_line = Some(current_line);
            }
        }
    }
}

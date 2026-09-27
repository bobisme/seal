//! Thread-to-diff mapping, change counting, and thread range analysis.

use crate::db::ThreadSummary;
use crate::diff::{DiffLineKind, ParsedDiff};

use super::{ChangeCounts, ThreadAnchor};

pub fn diff_change_counts(diff: &ParsedDiff) -> ChangeCounts {
    let mut added = 0usize;
    let mut removed = 0usize;
    for hunk in &diff.hunks {
        for line in &hunk.lines {
            match line.kind {
                DiffLineKind::Added => added += 1,
                DiffLineKind::Removed => removed += 1,
                DiffLineKind::Context => {}
            }
        }
    }
    ChangeCounts { added, removed }
}

/// Map threads to display line indices within the diff.
///
/// Threads are placed at their *current* mapped line (see
/// [`ThreadSummary::effective_selection`]), not the line they were stored
/// with, so a thread whose lines shifted during a rebase still lands on the
/// right row. Outdated threads (edited, deleted, or unmappable anchors) are
/// never anchored inline here — there is no line in this target to point
/// at — so they are skipped and left for the caller to show separately
/// (e.g. as a plain comment block), still visible and marked outdated.
#[must_use]
pub fn map_threads_to_diff(diff: &ParsedDiff, threads: &[&ThreadSummary]) -> Vec<ThreadAnchor> {
    let mut anchors = Vec::new();

    // Build maps from line numbers to display line index
    // Check both old and new line numbers since threads could reference either
    let mut new_line_to_display: std::collections::HashMap<u32, usize> =
        std::collections::HashMap::new();
    let mut display_idx = 0;

    for hunk in &diff.hunks {
        display_idx += 1; // hunk header
        for line in &hunk.lines {
            if let Some(new_ln) = line.new_line {
                new_line_to_display.insert(new_ln, display_idx);
            }
            display_idx += 1;
        }
    }

    // Map each thread to its display position
    // Only anchor on new-file line numbers — old-line fallback causes false
    // anchoring when a thread's line number coincidentally matches a removed line
    // in a different commit.
    for thread in threads {
        if thread.outdated {
            continue;
        }
        let (start, end) = thread.effective_selection();
        let start_line = u32::try_from(start.max(0)).unwrap_or(u32::MAX);
        let display_line = new_line_to_display.get(&start_line);

        if let Some(&display_line) = display_line {
            let end_value = end.unwrap_or(start);
            let line_count = usize::try_from((end_value - start + 1).max(1)).unwrap_or(1);

            // Comment block goes after the last line of the range
            let end_line = u32::try_from(end_value.max(0)).unwrap_or(u32::MAX);
            let comment_after_line = new_line_to_display
                .get(&end_line)
                .copied()
                .unwrap_or(display_line);

            anchors.push(ThreadAnchor {
                thread_id: thread.thread_id.clone(),
                display_line,
                comment_after_line,
                line_count,
                status: thread.status.clone(),
                comment_count: thread.comment_count,
                is_expanded: true,
            });
        }
    }

    // Sort by display line
    anchors.sort_by_key(|a| a.display_line);
    anchors
}

pub(super) fn build_thread_ranges(threads: &[&ThreadSummary]) -> Vec<(i64, i64)> {
    threads
        .iter()
        .map(|thread| {
            let (start, end) = thread.effective_selection();
            let end = end.unwrap_or(start);
            (start.min(end), start.max(end))
        })
        .collect()
}

pub(super) fn line_in_thread_ranges(line: Option<i64>, ranges: &[(i64, i64)]) -> bool {
    let Some(line) = line else {
        return false;
    };
    ranges
        .iter()
        .any(|(start, end)| line >= *start && line <= *end)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rebased-like diff: two lines were inserted near the top of a
    /// 10-line file, so everything from `base 3` onward shifts down by 2
    /// (`base 5` moves from line 5 to line 7).
    fn shifted_diff() -> ParsedDiff {
        ParsedDiff::parse(
            "@@ -1,10 +1,12 @@\n\
             \x20base 1\n\
             \x20base 2\n\
             +trunk A\n\
             +trunk B\n\
             \x20base 3\n\
             \x20base 4\n\
             \x20base 5\n\
             \x20base 6\n\
             \x20base 7\n\
             \x20base 8\n\
             \x20base 9\n\
             \x20base 10\n",
        )
    }

    fn positioned(
        thread_id: &str,
        stored_start: i64,
        current_start: Option<i64>,
        drift_status: &str,
        outdated: bool,
    ) -> ThreadSummary {
        ThreadSummary {
            thread_id: thread_id.to_string(),
            file_path: "f.txt".to_string(),
            selection_start: stored_start,
            selection_end: None,
            status: "open".to_string(),
            comment_count: 1,
            current_selection_start: current_start,
            current_selection_end: current_start,
            drift_status: drift_status.to_string(),
            outdated,
        }
    }

    #[test]
    fn map_threads_to_diff_places_shifted_thread_at_its_mapped_line() {
        let diff = shifted_diff();

        // Stored at line 5 ("base 5"), but the anchor mapped it to line 7
        // after the rebase inserted two lines above it.
        let shifted = positioned("th-shift", 5, Some(7), "shifted(+2)", false);
        // Outdated: the anchor commit's line 4 was edited by the rebase, so
        // there is no current line to point at. If the stored line (4) were
        // used instead, this would coincide with a real diff line and wrongly
        // anchor inline.
        let modified = positioned("th-modified", 4, None, "modified", true);
        // Unknown: the anchor commit could not be diffed at all. Stored line
        // 1 ("base 1") is unchanged and would trivially (and wrongly) anchor
        // if the outdated check were skipped.
        let unknown = positioned("th-unknown", 1, None, "unknown", true);

        let threads = [&shifted, &modified, &unknown];
        let anchors = map_threads_to_diff(&diff, &threads);

        // Only the intact, shifted thread is placed inline.
        assert_eq!(
            anchors
                .iter()
                .map(|a| a.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec!["th-shift"]
        );

        // It lands at its *mapped* line 7, not its stored line 5.
        let new_line_to_display: std::collections::HashMap<u32, usize> = {
            let mut map = std::collections::HashMap::new();
            let mut display_idx = 0;
            for hunk in &diff.hunks {
                display_idx += 1;
                for line in &hunk.lines {
                    if let Some(new_ln) = line.new_line {
                        map.insert(new_ln, display_idx);
                    }
                    display_idx += 1;
                }
            }
            map
        };
        assert_eq!(
            anchors[0].display_line, new_line_to_display[&7],
            "thread must anchor at its mapped line (7), not its stored line (5)"
        );
        assert_ne!(
            anchors[0].display_line, new_line_to_display[&5],
            "thread must not anchor at its stale stored line"
        );
    }

    #[test]
    fn map_threads_to_diff_skips_outdated_threads_entirely() {
        let diff = shifted_diff();
        let modified = positioned("th-modified", 4, None, "modified", true);
        let unknown = positioned("th-unknown", 1, None, "unknown", true);

        let anchors = map_threads_to_diff(&diff, &[&modified, &unknown]);

        assert!(
            anchors.is_empty(),
            "outdated threads must never be anchored at a guessed line"
        );
    }

    #[test]
    fn build_thread_ranges_uses_mapped_line_for_intact_threads() {
        let shifted = positioned("th-shift", 5, Some(7), "shifted(+2)", false);
        let outdated = positioned("th-modified", 4, None, "modified", true);

        let ranges = build_thread_ranges(&[&shifted, &outdated]);

        assert_eq!(ranges, vec![(7, 7), (4, 4)]);
    }
}

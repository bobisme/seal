//! Stream layout helpers for the right pane

use std::collections::HashMap;

use crate::db::{Comment, ThreadSummary};
use crate::diff::ParsedDiff;
use crate::layout;
use crate::markdown::render_markdown;
use crate::model::{DiffViewMode, FileCacheEntry, FileEntry};
use crate::text::wrap_text_preserve;

// Re-export for downstream users that were importing from stream::
pub use crate::layout::{
    block_height, BLOCK_LEFT_PAD, BLOCK_MARGIN, BLOCK_PADDING, BLOCK_RIGHT_PAD, BLOCK_SIDE_MARGIN,
    SIDE_BY_SIDE_MIN_WIDTH,
};

pub struct StreamLayout {
    /// Offset where files start (after description block, if any)
    pub description_lines: usize,
    pub file_offsets: Vec<usize>,
    pub total_lines: usize,
}

/// Parameters for [`compute_stream_layout`].
pub struct StreamLayoutParams<'a> {
    pub files: &'a [FileEntry],
    pub file_cache: &'a HashMap<String, FileCacheEntry>,
    pub threads: &'a [ThreadSummary],
    pub all_comments: &'a HashMap<String, Vec<Comment>>,
    pub view_mode: DiffViewMode,
    pub wrap: bool,
    pub content_width: u32,
    pub description: Option<&'a str>,
}

/// Inner width for description/comment block content.
/// Uses the same `comment_block_area` → `comment_content_area` chain as
/// `emit_comment_block` so text wraps at the same column.
const fn block_wrap_width(pane_width: u32) -> usize {
    // comment_block_area: inset COMMENT_H_MARGIN on each side
    let block_w = pane_width.saturating_sub(layout::COMMENT_H_MARGIN * 2);
    // comment_content_area: inset 2 (double bar) + COMMENT_H_PAD on each side
    block_w.saturating_sub(4 + layout::COMMENT_H_PAD * 2) as usize
}

/// Compute height of description block (if present).
#[must_use]
pub fn description_block_height(description: Option<&str>, pane_width: u32) -> usize {
    let Some(desc) = description else {
        return 0;
    };
    if desc.trim().is_empty() {
        return 0;
    }
    let wrap_width = block_wrap_width(pane_width);
    let wrapped = render_markdown(desc, wrap_width);
    block_height(wrapped.len())
}

/// Inner width for diff content (no block bar/margins, just horizontal padding).
const fn diff_inner_width(pane_width: u32) -> u32 {
    layout::diff_inner_width(pane_width)
}

const fn unified_wrap_width(pane_width: u32) -> usize {
    let content_width = diff_inner_width(pane_width)
        .saturating_sub(layout::THREAD_COL_WIDTH + layout::UNIFIED_LINE_NUM_WIDTH);
    let max_content = content_width.saturating_sub(2);
    max_content as usize
}

const fn context_wrap_width(pane_width: u32) -> usize {
    diff_inner_width(pane_width).saturating_sub(layout::CONTEXT_LINE_NUM_WIDTH) as usize
}

const fn side_by_side_wrap_widths(pane_width: u32) -> (usize, usize) {
    let divider_width: u32 = 0;
    let available =
        diff_inner_width(pane_width).saturating_sub(layout::THREAD_COL_WIDTH + divider_width);
    let half_width = available / 2;
    let left = half_width.saturating_sub(layout::SBS_LINE_NUM_WIDTH) as usize;
    let right = half_width.saturating_sub(layout::SBS_LINE_NUM_WIDTH) as usize;
    (left, right)
}

fn wrap_line_count(text: &str, max_width: usize) -> usize {
    if max_width == 0 {
        return 1;
    }
    let lines = wrap_text_preserve(text, max_width);
    lines.len().max(1)
}

#[must_use]
#[allow(clippy::implicit_hasher)] // internal fn, always uses default hasher
pub fn compute_stream_layout(params: &StreamLayoutParams<'_>) -> StreamLayout {
    let StreamLayoutParams {
        files,
        file_cache,
        threads,
        all_comments,
        view_mode,
        wrap,
        content_width,
        description,
    } = *params;

    let description_lines = description_block_height(description, content_width);
    let mut file_offsets = Vec::with_capacity(files.len());
    let mut total = description_lines;

    for file in files {
        file_offsets.push(total);
        total += block_height(1); // file header block

        if let Some(entry) = file_cache.get(&file.path) {
            let file_threads: Vec<&ThreadSummary> = threads
                .iter()
                .filter(|t| t.file_path == file.path)
                .collect();
            let diff_lines = entry.diff.as_ref().map_or_else(
                || {
                    entry.file_content.as_ref().map_or(0, |content| {
                        context_display_count(
                            content.lines.as_slice(),
                            content.start_line,
                            threads,
                            &file.path,
                            wrap,
                            content_width,
                        ) + all_context_extra_lines(
                            content.start_line,
                            content.lines.len(),
                            &file_threads,
                            all_comments,
                            content_width,
                        )
                    })
                },
                |diff| {
                    let anchors = crate::view::map_threads_to_diff(diff, &file_threads);
                    let anchored_ids: std::collections::HashSet<&str> =
                        anchors.iter().map(|a| a.thread_id.as_str()).collect();
                    let anchored_threads: Vec<&ThreadSummary> = file_threads
                        .iter()
                        .filter(|t| anchored_ids.contains(t.thread_id.as_str()))
                        .copied()
                        .collect();
                    let orphaned_threads: Vec<&ThreadSummary> = file_threads
                        .iter()
                        .filter(|t| !anchored_ids.contains(t.thread_id.as_str()))
                        .copied()
                        .collect();
                    // Only intact (non-outdated) orphans get a code preview
                    // window — this must mirror the render-side split in
                    // `view::diff::mod::render_file_with_diff` or the
                    // computed row count and the actual render disagree.
                    let previewable_orphans: Vec<&ThreadSummary> = orphaned_threads
                        .iter()
                        .filter(|t| !t.outdated)
                        .copied()
                        .collect();

                    let mut count = diff_line_count_for_view(diff, view_mode, wrap, content_width)
                        + threads_comment_height(&anchored_threads, all_comments, content_width);

                    if !orphaned_threads.is_empty() {
                        if !previewable_orphans.is_empty() {
                            if let Some(content) = &entry.file_content {
                                let hunk_ranges = crate::diff::hunk_exclusion_ranges(&diff.hunks);
                                count += orphaned_context_display_count(
                                    content.lines.as_slice(),
                                    content.start_line,
                                    &previewable_orphans,
                                    &hunk_ranges,
                                    wrap,
                                    content_width,
                                );
                            }
                        }
                        count +=
                            threads_comment_height(&orphaned_threads, all_comments, content_width);
                    }

                    count
                },
            );

            total += diff_lines.max(1);
        } else {
            total += 1;
        }
    }

    StreamLayout {
        description_lines,
        file_offsets,
        total_lines: total,
    }
}

#[must_use]
pub fn active_file_index(layout: &StreamLayout, scroll: usize) -> usize {
    let mut idx = 0;
    for (i, offset) in layout.file_offsets.iter().enumerate() {
        if *offset <= scroll {
            idx = i;
        } else {
            break;
        }
    }
    idx
}

#[must_use]
pub fn file_scroll_offset(layout: &StreamLayout, index: usize) -> usize {
    layout.file_offsets.get(index).copied().unwrap_or(0)
}

fn diff_line_count_for_view(
    diff: &ParsedDiff,
    view_mode: DiffViewMode,
    wrap: bool,
    content_width: u32,
) -> usize {
    match view_mode {
        DiffViewMode::Unified => {
            if wrap {
                diff_line_count_wrapped(diff, unified_wrap_width(content_width))
            } else {
                diff_line_count(diff)
            }
        }
        DiffViewMode::SideBySide => {
            if wrap {
                let (left_width, right_width) = side_by_side_wrap_widths(content_width);
                side_by_side_line_count_wrapped(diff, left_width, right_width)
            } else {
                side_by_side_line_count(diff)
            }
        }
    }
}

fn diff_line_count(diff: &ParsedDiff) -> usize {
    diff.hunks.iter().map(|h| 1 + h.lines.len()).sum()
}

fn diff_line_count_wrapped(diff: &ParsedDiff, max_width: usize) -> usize {
    let mut count = 0usize;
    for hunk in &diff.hunks {
        count += 1;
        for line in &hunk.lines {
            count += wrap_line_count(&line.content, max_width);
        }
    }
    count
}

fn side_by_side_line_count(diff: &ParsedDiff) -> usize {
    let mut count = 0usize;
    for hunk in &diff.hunks {
        count += 1; // header
        let mut i = 0;
        let lines = &hunk.lines;
        while i < lines.len() {
            match lines[i].kind {
                crate::diff::DiffLineKind::Removed => {
                    let mut removals = 0;
                    while i < lines.len() && lines[i].kind == crate::diff::DiffLineKind::Removed {
                        removals += 1;
                        i += 1;
                    }
                    let mut additions = 0;
                    while i < lines.len() && lines[i].kind == crate::diff::DiffLineKind::Added {
                        additions += 1;
                        i += 1;
                    }
                    count += removals.max(additions);
                }
                crate::diff::DiffLineKind::Context | crate::diff::DiffLineKind::Added => {
                    count += 1;
                    i += 1;
                }
            }
        }
    }
    count
}

fn side_by_side_line_count_wrapped(
    diff: &ParsedDiff,
    left_width: usize,
    right_width: usize,
) -> usize {
    let mut count = 0usize;
    for hunk in &diff.hunks {
        count += 1;
        let mut i = 0;
        let lines = &hunk.lines;
        while i < lines.len() {
            match lines[i].kind {
                crate::diff::DiffLineKind::Context => {
                    count += wrap_line_count(&lines[i].content, left_width);
                    i += 1;
                }
                crate::diff::DiffLineKind::Removed => {
                    let mut removals = Vec::new();
                    while i < lines.len() && lines[i].kind == crate::diff::DiffLineKind::Removed {
                        removals.push(&lines[i]);
                        i += 1;
                    }
                    let mut additions = Vec::new();
                    while i < lines.len() && lines[i].kind == crate::diff::DiffLineKind::Added {
                        additions.push(&lines[i]);
                        i += 1;
                    }
                    let max_len = removals.len().max(additions.len());
                    for idx in 0..max_len {
                        let left_lines = removals
                            .get(idx)
                            .map_or(1, |line| wrap_line_count(&line.content, left_width));
                        let right_lines = additions
                            .get(idx)
                            .map_or(1, |line| wrap_line_count(&line.content, right_width));
                        count += left_lines.max(right_lines);
                    }
                }
                crate::diff::DiffLineKind::Added => {
                    count += wrap_line_count(&lines[i].content, right_width);
                    i += 1;
                }
            }
        }
    }
    count
}

fn comment_block_height(comments: &[Comment], content_width: u32) -> usize {
    if comments.is_empty() {
        return 0;
    }
    let max_width =
        content_width.saturating_sub(BLOCK_SIDE_MARGIN * 2 + 1 + BLOCK_LEFT_PAD + BLOCK_RIGHT_PAD);
    let max_width = max_width as usize;
    let mut content_lines = 2; // thread header line + spacing
    for comment in comments {
        content_lines += 1; // author line
        let wrapped = render_markdown(&comment.body, max_width);
        content_lines += wrapped.len();
    }
    block_height(content_lines).saturating_sub(BLOCK_MARGIN)
}

fn context_display_count(
    lines: &[String],
    start_line: i64,
    threads: &[ThreadSummary],
    file_path: &str,
    wrap: bool,
    content_width: u32,
) -> usize {
    let mut ranges = Vec::new();
    #[allow(clippy::cast_possible_wrap)]
    let end_line = start_line + lines.len() as i64 - 1;
    for thread in threads.iter().filter(|t| t.file_path == file_path) {
        let (eff_start, eff_end) = thread.effective_selection();
        let thread_end = eff_end.unwrap_or(eff_start);
        let start = (eff_start - layout::CONTEXT_LINES).max(start_line);
        let end = (thread_end + layout::CONTEXT_LINES).min(end_line);
        if start <= end {
            ranges.push((start, end));
        }
    }

    if ranges.is_empty() {
        return 1; // "No threads" line
    }

    ranges.sort_by_key(|r| r.0);
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 + 1 {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        } else {
            merged.push((start, end));
        }
    }

    let mut count = 0usize;
    let mut prev_end: Option<i64> = None;
    for (start, end) in merged {
        if let Some(pe) = prev_end {
            if start > pe + 1 {
                count += 1; // separator line
            }
        }
        let max_width = context_wrap_width(content_width);
        for line in start..=end {
            if let Some(text) = lines.get((line - start_line) as usize) {
                if wrap {
                    count += wrap_line_count(text, max_width);
                } else {
                    count += 1;
                }
            } else {
                count += 1;
            }
        }
        prev_end = Some(end);
    }

    count
}

/// Count display lines for orphaned thread context (already-filtered threads).
/// `exclude_ranges` are (start, end) pairs of new-file lines already shown in the diff.
fn orphaned_context_display_count(
    lines: &[String],
    start_line: i64,
    orphaned_threads: &[&ThreadSummary],
    exclude_ranges: &[(i64, i64)],
    wrap: bool,
    content_width: u32,
) -> usize {
    #[allow(clippy::cast_possible_wrap)]
    let end_line = start_line + lines.len() as i64 - 1;
    let mut ranges: Vec<(i64, i64)> = orphaned_threads
        .iter()
        .map(|t| {
            let (eff_start, eff_end) = t.effective_selection();
            let thread_end = eff_end.unwrap_or(eff_start);
            let start = (eff_start - layout::CONTEXT_LINES).max(start_line);
            let end = (thread_end + layout::CONTEXT_LINES).min(end_line);
            (start, end)
        })
        .filter(|(start, end)| start <= end)
        .collect();

    if ranges.is_empty() {
        return 0;
    }

    ranges.sort_by_key(|r| r.0);
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 + 1 {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        } else {
            merged.push((start, end));
        }
    }

    // Clip against diff hunk ranges
    if !exclude_ranges.is_empty() {
        let mut clipped: Vec<(i64, i64)> = Vec::new();
        for (rs, re) in &merged {
            let mut remaining = vec![(*rs, *re)];
            for &(ex_start, ex_end) in exclude_ranges {
                let mut next = Vec::new();
                for (s, e) in remaining {
                    if e < ex_start || s > ex_end {
                        next.push((s, e));
                    } else {
                        if s < ex_start {
                            next.push((s, ex_start - 1));
                        }
                        if e > ex_end {
                            next.push((ex_end + 1, e));
                        }
                    }
                }
                remaining = next;
            }
            clipped.extend(remaining);
        }
        clipped.sort_by_key(|r| r.0);
        merged = clipped;
    }

    let mut count = 0usize;
    let mut prev_end: Option<i64> = None;
    for (start, end) in merged {
        if let Some(pe) = prev_end {
            if start > pe + 1 {
                count += 1; // separator line
            }
        }
        let max_width = context_wrap_width(content_width);
        for line in start..=end {
            if let Some(text) = lines.get((line - start_line) as usize) {
                if wrap {
                    count += wrap_line_count(text, max_width);
                } else {
                    count += 1;
                }
            } else {
                count += 1;
            }
        }
        prev_end = Some(end);
    }

    count
}

/// Count comment block heights for a set of threads.
fn threads_comment_height(
    threads: &[&ThreadSummary],
    all_comments: &HashMap<String, Vec<Comment>>,
    content_width: u32,
) -> usize {
    let mut total = 0;
    for thread in threads {
        if let Some(comments) = all_comments.get(&thread.thread_id) {
            total += comment_block_height(comments, content_width);
        }
    }
    total
}

fn all_context_extra_lines(
    start_line: i64,
    total_lines: usize,
    file_threads: &[&ThreadSummary],
    all_comments: &HashMap<String, Vec<Comment>>,
    content_width: u32,
) -> usize {
    #[allow(clippy::cast_possible_wrap)]
    let end_line = start_line + total_lines as i64 - 1;
    let mut total = 0;
    for thread in file_threads {
        let start = thread.effective_start();
        if start < start_line || start > end_line {
            continue;
        }
        if let Some(comments) = all_comments.get(&thread.thread_id) {
            total += comment_block_height(comments, content_width);
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ThreadSummary;

    fn thread(file_path: &str, start: i64, end: Option<i64>) -> ThreadSummary {
        ThreadSummary {
            thread_id: "th-1".to_string(),
            file_path: file_path.to_string(),
            selection_start: start,
            selection_end: end,
            status: "open".to_string(),
            comment_count: 1,
            current_selection_start: Some(start),
            current_selection_end: end,
            drift_status: "unchanged".to_string(),
            outdated: false,
        }
    }

    #[test]
    fn context_display_count_uses_window_start_line() {
        let lines: Vec<String> = (100..=109).map(|n| format!("line {n}")).collect();
        let threads = vec![thread("src/lib.rs", 105, None)];

        let count = context_display_count(&lines, 100, &threads, "src/lib.rs", false, 120);

        assert_eq!(count, 10);
    }

    #[test]
    fn orphaned_context_count_uses_window_start_line_and_exclusions() {
        let lines: Vec<String> = (100..=109).map(|n| format!("line {n}")).collect();
        let thread = thread("src/lib.rs", 105, None);
        let threads = vec![&thread];

        let unclipped = orphaned_context_display_count(&lines, 100, &threads, &[], false, 120);
        assert_eq!(unclipped, 10);

        let clipped =
            orphaned_context_display_count(&lines, 100, &threads, &[(103, 106)], false, 120);
        assert_eq!(clipped, 7);
    }
}

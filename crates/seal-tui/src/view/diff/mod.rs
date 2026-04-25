//! Diff rendering component
//!
//! Sub-modules:
//! - `text_util`: wrapping, truncation, highlighted-text drawing
//! - `helpers`: block/diff/comment bar draw primitives
//! - `analysis`: thread→diff mapping, change counting
//! - `unified`: unified diff line rendering
//! - `side_by_side`: SBS diff line rendering
//! - `comments`: comment block rendering
//! - `context`: orphaned context building + rendering

mod analysis;
mod comments;
mod context;
mod helpers;
mod side_by_side;
mod text_util;
mod unified;

use crate::render_backend::{buffer_draw_text, buffer_fill_rect, OptimizedBuffer};

use super::components::Rect;
use crate::db::ThreadSummary;
use crate::diff::{DiffLine, DiffLineKind, ParsedDiff};
use crate::layout::{
    block_height, BLOCK_MARGIN, BLOCK_PADDING, SBS_LINE_NUM_WIDTH, THREAD_COL_WIDTH,
    UNIFIED_LINE_NUM_WIDTH,
};
use crate::markdown::{draw_markdown_content, markdown_line_bg, MarkdownContent};
use crate::syntax::{HighlightSpan, Highlighter};
use crate::theme::Theme;

// Re-export public API
pub use analysis::{diff_change_counts, map_threads_to_diff};

use analysis::{build_thread_ranges, line_in_thread_ranges};
use comments::{comment_block_rows, emit_comment_block};
use context::{
    build_context_items, calculate_context_ranges, emit_orphaned_context_section,
    emit_remaining_orphaned_comments, group_context_ranges_by_hunks, render_context_item_block,
    render_context_line_wrapped_row, OrphanedRenderState,
};
use helpers::{
    comment_block_area, comment_content_area, diff_content_width, diff_margin_area,
    draw_block_base_line, draw_block_text_line, draw_file_header_line, draw_plain_line_with_right,
    PlainLineContent,
};
use side_by_side::{render_side_by_side_line_block, render_side_by_side_line_wrapped_row};
use text_util::wrap_content;
use unified::{render_unified_diff_line_block, render_unified_diff_line_wrapped_row};

/// Map from display-line index to the anchors at that position.
type AnchorMap<'a> = std::collections::HashMap<usize, Vec<&'a ThreadAnchor>>;

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

/// Thread anchor info for rendering
#[derive(Debug, Clone)]
pub struct ThreadAnchor {
    pub thread_id: String,
    pub display_line: usize,
    /// Display line after which the comment block should render (end of range)
    pub comment_after_line: usize,
    pub line_count: usize, // How many lines the thread spans
    pub status: String,
    pub comment_count: i64,
    pub is_expanded: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct ChangeCounts {
    pub(super) added: usize,
    pub(super) removed: usize,
}

/// A line to display (either hunk header or diff line)
enum DisplayLine {
    HunkHeader,
    Diff(DiffLine),
}

/// A paired line for side-by-side display
#[derive(Debug, Clone)]
struct SideBySideLine {
    left: Option<SideLine>,
    right: Option<SideLine>,
    is_header: bool,
}

/// One side of a side-by-side line
#[derive(Debug, Clone)]
struct SideLine {
    line_num: u32,
    content: String,
    kind: DiffLineKind,
    display_index: usize,
}

/// Shared rendering context for diff line render functions.
///
/// Bundles the outer-scope parameters that are constant across all lines in a
/// rendering pass, keeping the per-line closure params (`buffer`, `y`, `theme`)
/// separate.
#[allow(dead_code)]
struct LineRenderCtx<'a> {
    area: Rect,
    anchor: Option<&'a ThreadAnchor>,
    show_thread_bar: bool,
    is_cursor: bool,
    is_selected: bool,
}

/// Display item for file context view
enum DisplayItem {
    Separator(#[allow(dead_code)] i64),
    Line { line_num: i64, content: String },
}

/// A range of lines to display
#[derive(Debug, Clone, Copy)]
struct LineRange {
    start: i64,
    end: i64,
}

struct StreamCursor<'a> {
    buffer: &'a mut OptimizedBuffer,
    area: Rect,
    scroll: usize,
    screen_row: usize,
    stream_row: usize,
    diff_cursor: usize,
    theme: &'a Theme,
    max_stream_row: &'a std::cell::Cell<usize>,
    selection: Option<(usize, usize)>,
    cursor_stops: &'a std::cell::RefCell<Vec<usize>>,
}

struct OrphanedContext<'a> {
    sections: Vec<Vec<LineRange>>,
    threads: Vec<&'a ThreadSummary>,
    lines: &'a [String],
    highlights: &'a [Vec<HighlightSpan>],
    /// 1-based line number of `lines[0]`. Used for windowed content.
    start_line: i64,
}

/// Shared rendering context that flows from `render_diff_stream` through all
/// per-file rendering functions. Bundles parameters that are constant across
/// the entire stream.
struct StreamRenderCtx<'a> {
    wrap: bool,
    all_comments: &'a std::collections::HashMap<String, Vec<crate::db::Comment>>,
    thread_positions: &'a std::cell::RefCell<std::collections::HashMap<String, usize>>,
    line_map: &'a std::cell::RefCell<std::collections::HashMap<usize, i64>>,
    highlighter: &'a Highlighter,
}

/// Per-file rendering context for unified/SBS diff functions. Bundles the
/// shared parameters that are constant across all lines in a single file's
/// rendering pass.
struct DiffRenderCtx<'a> {
    line_area: Rect,
    area: Rect,
    threads: &'a [&'a ThreadSummary],
    file_highlights: &'a [Vec<HighlightSpan>],
    wrap: bool,
    all_comments: &'a std::collections::HashMap<String, Vec<crate::db::Comment>>,
    thread_positions: &'a std::cell::RefCell<std::collections::HashMap<String, usize>>,
    line_map: &'a std::cell::RefCell<std::collections::HashMap<usize, i64>>,
    highlighter: &'a Highlighter,
}

impl StreamCursor<'_> {
    fn emit<F>(&mut self, draw: F)
    where
        F: FnOnce(&mut OptimizedBuffer, u32, &Theme),
    {
        if self.stream_row >= self.scroll && self.screen_row < self.area.height as usize {
            let y = self.area.y + self.screen_row as u32;
            draw(self.buffer, y, self.theme);
            self.screen_row += 1;
        }
        self.stream_row += 1;
        self.max_stream_row.set(self.stream_row);
    }

    fn emit_rows<F>(&mut self, rows: usize, mut draw: F)
    where
        F: FnMut(&mut OptimizedBuffer, u32, &Theme, usize),
    {
        for row in 0..rows {
            if self.stream_row >= self.scroll && self.screen_row < self.area.height as usize {
                let y = self.area.y + self.screen_row as u32;
                draw(self.buffer, y, self.theme, row);
                self.screen_row += 1;
            }
            self.stream_row += 1;
        }
        self.max_stream_row.set(self.stream_row);
    }

    /// Check if the diff cursor is at or within [`stream_row`, `stream_row` + rows).
    const fn is_cursor_at(&self, rows: usize) -> bool {
        self.diff_cursor >= self.stream_row && self.diff_cursor < self.stream_row + rows
    }

    /// Check if any row in [`stream_row`, `stream_row` + rows) overlaps with the
    /// visual selection range.
    const fn is_selected_at(&self, rows: usize) -> bool {
        match self.selection {
            Some((start, end)) => {
                let item_end = self.stream_row + rows.saturating_sub(1);
                self.stream_row <= end && item_end >= start
            }
            None => false,
        }
    }

    const fn remaining_rows(&self) -> usize {
        self.area.height.saturating_sub(self.screen_row as u32) as usize
    }

    /// Record the current `stream_row` as a valid cursor stop.
    fn mark_cursor_stop(&self) {
        self.cursor_stops.borrow_mut().push(self.stream_row);
    }
}

// ---------------------------------------------------------------------------
// build_side_by_side_lines (used by stream + SBS rendering)
// ---------------------------------------------------------------------------

fn build_side_by_side_lines(diff: &ParsedDiff) -> Vec<SideBySideLine> {
    let mut result = Vec::new();
    let mut display_index = 0;

    for hunk in &diff.hunks {
        result.push(SideBySideLine {
            left: None,
            right: None,
            is_header: true,
        });
        display_index += 1;

        let mut i = 0;
        let lines = &hunk.lines;

        while i < lines.len() {
            let line = &lines[i];
            match line.kind {
                DiffLineKind::Context => {
                    let line_index = display_index;
                    result.push(SideBySideLine {
                        left: Some(SideLine {
                            line_num: line.old_line.unwrap_or(0),
                            content: line.content.clone(),
                            kind: DiffLineKind::Context,
                            display_index: line_index,
                        }),
                        right: Some(SideLine {
                            line_num: line.new_line.unwrap_or(0),
                            content: line.content.clone(),
                            kind: DiffLineKind::Context,
                            display_index: line_index,
                        }),
                        is_header: false,
                    });
                    i += 1;
                    display_index += 1;
                }
                DiffLineKind::Removed => {
                    let mut removals: Vec<(&DiffLine, usize)> = Vec::new();
                    while i < lines.len() && lines[i].kind == DiffLineKind::Removed {
                        removals.push((&lines[i], display_index));
                        i += 1;
                        display_index += 1;
                    }
                    let mut additions: Vec<(&DiffLine, usize)> = Vec::new();
                    while i < lines.len() && lines[i].kind == DiffLineKind::Added {
                        additions.push((&lines[i], display_index));
                        i += 1;
                        display_index += 1;
                    }
                    let max_len = removals.len().max(additions.len());
                    for j in 0..max_len {
                        let left = removals.get(j).map(|(l, idx)| SideLine {
                            line_num: l.old_line.unwrap_or(0),
                            content: l.content.clone(),
                            kind: DiffLineKind::Removed,
                            display_index: *idx,
                        });
                        let right = additions.get(j).map(|(l, idx)| SideLine {
                            line_num: l.new_line.unwrap_or(0),
                            content: l.content.clone(),
                            kind: DiffLineKind::Added,
                            display_index: *idx,
                        });
                        result.push(SideBySideLine {
                            left,
                            right,
                            is_header: false,
                        });
                    }
                }
                DiffLineKind::Added => {
                    let line_index = display_index;
                    result.push(SideBySideLine {
                        left: None,
                        right: Some(SideLine {
                            line_num: line.new_line.unwrap_or(0),
                            content: line.content.clone(),
                            kind: DiffLineKind::Added,
                            display_index: line_index,
                        }),
                        is_header: false,
                    });
                    i += 1;
                    display_index += 1;
                }
            }
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Public stream rendering
// ---------------------------------------------------------------------------

pub fn render_pinned_header_block(
    buffer: &mut OptimizedBuffer,
    area: Rect,
    file_path: &str,
    theme: &Theme,
    counts: Option<ChangeCounts>,
) -> usize {
    let content_lines = 1usize;
    let height = block_height(content_lines) as u32;
    if area.height < height {
        return 0;
    }

    // Pinned header doesn't participate in cursor tracking
    let dummy_max = std::cell::Cell::new(0);
    let dummy_stops = std::cell::RefCell::new(Vec::new());
    let mut cursor = StreamCursor {
        buffer,
        area: Rect::new(area.x, area.y, area.width, height),
        scroll: 0,
        screen_row: 0,
        stream_row: 0,
        diff_cursor: usize::MAX,
        theme,
        max_stream_row: &dummy_max,
        selection: None,
        cursor_stops: &dummy_stops,
    };

    for _ in 0..BLOCK_MARGIN {
        cursor.emit(|buf, y, _| {
            buffer_fill_rect(buf, area.x, y, area.width, 1, theme.background);
        });
    }
    for _ in 0..BLOCK_PADDING {
        cursor.emit(|buf, y, theme| {
            draw_block_base_line(buf, area, y, theme.panel_bg, theme);
        });
    }
    cursor.emit(|buf, y, theme| {
        draw_file_header_line(buf, area, y, theme, file_path, counts);
    });
    for _ in 0..BLOCK_PADDING {
        cursor.emit(|buf, y, theme| {
            draw_block_base_line(buf, area, y, theme.panel_bg, theme);
        });
    }
    for _ in 0..BLOCK_MARGIN {
        cursor.emit(|buf, y, _| {
            buffer_fill_rect(buf, area.x, y, area.width, 1, theme.background);
        });
    }

    height as usize
}

/// Render a description block at the top of the stream.
/// Uses the same half-block border style as `emit_comment_block`.
fn render_description_block(
    cursor: &mut StreamCursor<'_>,
    area: Rect,
    description: &str,
    highlighter: &Highlighter,
    _theme: &Theme,
) {
    use crate::render_backend::Style;

    let block = comment_block_area(area);
    let padded = comment_content_area(block);
    let content_width = padded.width as usize;
    let content_lines = crate::markdown::render_markdown_with_highlighter(
        description,
        content_width,
        Some(highlighter),
    );

    let top_margin = BLOCK_MARGIN;
    let bottom_margin = BLOCK_MARGIN;
    let content_start = top_margin + BLOCK_PADDING;
    let content_end = content_start + content_lines.len();
    let total_rows = content_end
        .saturating_add(BLOCK_PADDING)
        .saturating_add(bottom_margin);

    for row in 0..total_rows {
        let line_content = if row >= content_start && row < content_end {
            Some(&content_lines[row - content_start])
        } else {
            None
        };
        cursor.emit(|buf, y, theme| {
            let block_bg = theme.panel_bg;
            let border_style = Style::fg(theme.background).with_bg(block_bg);
            let bar_style = Style::fg(theme.background).with_bg(block_bg);
            let rc = block.x + block.width.saturating_sub(1);
            let rc2 = block.x + block.width.saturating_sub(2);
            buffer_fill_rect(buf, area.x, y, area.width, 1, theme.background);
            if row < top_margin {
                // Background only.
            } else if row == top_margin {
                // Top border:  ▛▀…▀▜
                buffer_fill_rect(
                    buf,
                    block.x + 1,
                    y,
                    block.width.saturating_sub(2),
                    1,
                    block_bg,
                );
                buffer_draw_text(buf, block.x + 1, y, "▛", border_style);
                for col in 2..block.width.saturating_sub(2) {
                    buffer_draw_text(buf, block.x + col, y, "▀", border_style);
                }
                buffer_draw_text(buf, rc2, y, "▜", border_style);
            } else if row < content_start {
                // Padding rows: ▌▌ ... ▐▐
                buffer_fill_rect(buf, block.x, y, block.width, 1, block_bg);
                buffer_draw_text(buf, block.x, y, "▌", bar_style);
                buffer_draw_text(buf, block.x + 1, y, "▌", bar_style);
                buffer_draw_text(buf, rc2, y, "▐", bar_style);
                buffer_draw_text(buf, rc, y, "▐", bar_style);
            } else if row < content_end {
                // Content rows: ▌▌ text ▐▐
                buffer_fill_rect(buf, block.x, y, block.width, 1, block_bg);
                buffer_draw_text(buf, block.x, y, "▌", bar_style);
                buffer_draw_text(buf, block.x + 1, y, "▌", bar_style);
                buffer_draw_text(buf, rc2, y, "▐", bar_style);
                buffer_draw_text(buf, rc, y, "▐", bar_style);
                if let Some(line) = line_content {
                    let line_bg = markdown_line_bg(theme, block_bg, line.style);
                    let left_style = line.style.style(theme, line_bg);
                    buffer_fill_rect(buf, padded.x, y, padded.width, 1, line_bg);
                    match &line.content {
                        MarkdownContent::Text(text) => {
                            draw_plain_line_with_right(
                                buf,
                                padded,
                                y,
                                line_bg,
                                &PlainLineContent {
                                    left: text,
                                    right: None,
                                    left_style,
                                    right_style: theme.style_muted_on(line_bg),
                                },
                            );
                        }
                        _ => draw_markdown_content(
                            buf,
                            theme,
                            padded.x,
                            y,
                            padded.width,
                            line_bg,
                            &line.content,
                            line.style,
                        ),
                    }
                }
            } else if row < content_end + BLOCK_PADDING {
                if row == content_end + BLOCK_PADDING - 1 {
                    // Bottom border:  ▙▄…▄▟
                    buffer_fill_rect(
                        buf,
                        block.x + 1,
                        y,
                        block.width.saturating_sub(2),
                        1,
                        block_bg,
                    );
                    buffer_draw_text(buf, block.x + 1, y, "▙", border_style);
                    for col in 2..block.width.saturating_sub(2) {
                        buffer_draw_text(buf, block.x + col, y, "▄", border_style);
                    }
                    buffer_draw_text(buf, rc2, y, "▟", border_style);
                } else {
                    buffer_fill_rect(buf, block.x, y, block.width, 1, block_bg);
                    buffer_draw_text(buf, block.x, y, "▌", bar_style);
                    buffer_draw_text(buf, block.x + 1, y, "▌", bar_style);
                    buffer_draw_text(buf, rc2, y, "▐", bar_style);
                    buffer_draw_text(buf, rc, y, "▐", bar_style);
                }
            }
        });
    }
}

/// Parameters for rendering a diff stream.
pub struct DiffStreamParams<'a> {
    pub files: &'a [crate::model::FileEntry],
    pub file_cache: &'a std::collections::HashMap<String, crate::model::FileCacheEntry>,
    pub threads: &'a [ThreadSummary],
    pub all_comments: &'a std::collections::HashMap<String, Vec<crate::db::Comment>>,
    pub scroll: usize,
    pub diff_cursor: usize,
    pub theme: &'a Theme,
    pub highlighter: &'a Highlighter,
    pub view_mode: crate::model::DiffViewMode,
    pub wrap: bool,
    pub thread_positions: &'a std::cell::RefCell<std::collections::HashMap<String, usize>>,
    pub max_stream_row: &'a std::cell::Cell<usize>,
    pub description: Option<&'a str>,
    pub selection: Option<(usize, usize)>,
    pub line_map: &'a std::cell::RefCell<std::collections::HashMap<usize, i64>>,
    pub cursor_stops: &'a std::cell::RefCell<Vec<usize>>,
}

fn render_file_with_diff(
    cursor: &mut StreamCursor<'_>,
    area: Rect,
    diff: &ParsedDiff,
    entry: &crate::model::FileCacheEntry,
    file_threads: &[&ThreadSummary],
    view_mode: crate::model::DiffViewMode,
    sctx: &StreamRenderCtx<'_>,
) {
    let anchors = map_threads_to_diff(diff, file_threads);
    let anchored_ids: std::collections::HashSet<&str> =
        anchors.iter().map(|a| a.thread_id.as_str()).collect();
    let orphaned_threads: Vec<&&ThreadSummary> = file_threads
        .iter()
        .filter(|t| !anchored_ids.contains(t.thread_id.as_str()))
        .collect();
    let mut orphaned_context: Option<OrphanedContext<'_>> = None;
    if !orphaned_threads.is_empty() {
        if let Some(content) = &entry.file_content {
            let orphaned_deref: Vec<&ThreadSummary> =
                orphaned_threads.iter().map(|t| **t).collect();
            let hunk_ranges = crate::diff::hunk_exclusion_ranges(&diff.hunks);
            #[allow(clippy::cast_possible_wrap)]
            let total_lines = content.start_line + content.lines.len() as i64 - 1;
            let ranges =
                calculate_context_ranges(&orphaned_deref, total_lines as usize, &hunk_ranges);
            let sections = group_context_ranges_by_hunks(ranges, &hunk_ranges);
            if sections.iter().any(|section| !section.is_empty()) {
                orphaned_context = Some(OrphanedContext {
                    sections,
                    threads: orphaned_deref,
                    lines: content.lines.as_slice(),
                    highlights: entry.file_highlighted_lines.as_slice(),
                    start_line: content.start_line,
                });
            }
        }
    }
    let line_area = diff_margin_area(area);
    let ctx = DiffRenderCtx {
        line_area,
        area,
        threads: file_threads,
        file_highlights: &entry.highlighted_lines,
        wrap: sctx.wrap,
        all_comments: sctx.all_comments,
        thread_positions: sctx.thread_positions,
        line_map: sctx.line_map,
        highlighter: sctx.highlighter,
    };

    let emitted_threads = match view_mode {
        crate::model::DiffViewMode::Unified => render_file_diff_unified(
            cursor,
            &diff.hunks,
            &ctx,
            orphaned_context.as_ref(),
            &anchors,
        ),
        crate::model::DiffViewMode::SideBySide => {
            let sbs_lines = build_side_by_side_lines(diff);
            render_file_diff_sbs(
                cursor,
                &sbs_lines,
                &ctx,
                orphaned_context.as_ref(),
                &anchors,
            )
        }
    };

    if let Some(context) = &orphaned_context {
        emit_remaining_orphaned_comments(
            cursor,
            area,
            context,
            sctx.all_comments,
            sctx.thread_positions,
            &emitted_threads,
            sctx.highlighter,
        );
    } else if !orphaned_threads.is_empty() {
        let mut orphaned_sorted = orphaned_threads.clone();
        orphaned_sorted.sort_by_key(|t| t.selection_start);
        for thread in &orphaned_sorted {
            sctx.thread_positions
                .borrow_mut()
                .insert(thread.thread_id.clone(), cursor.stream_row);
            if let Some(comments) = sctx.all_comments.get(&thread.thread_id) {
                let rows = comment_block_rows(thread, comments, area);
                let is_cursor = cursor.is_cursor_at(rows);
                let hl = is_cursor || cursor.is_selected_at(rows);
                emit_comment_block(
                    cursor,
                    area,
                    thread,
                    comments,
                    hl,
                    is_cursor,
                    sctx.highlighter,
                );
            }
        }
    }
}

fn render_file_content_no_diff(
    cursor: &mut StreamCursor<'_>,
    area: Rect,
    content: &crate::model::FileContent,
    file_threads: &[&ThreadSummary],
    file_highlights: &[Vec<HighlightSpan>],
    sctx: &StreamRenderCtx<'_>,
) {
    let line_area = diff_margin_area(area);
    let start_line = content.start_line;
    let thread_ranges = build_thread_ranges(file_threads);
    let display_items =
        build_context_items(content.lines.as_slice(), file_threads, &[], start_line);
    for item in display_items {
        let show_thread_bar = match &item {
            DisplayItem::Line { line_num, .. } => {
                line_in_thread_ranges(Some(*line_num), &thread_ranges)
            }
            DisplayItem::Separator(_) => false,
        };
        match &item {
            DisplayItem::Separator(_) => {
                cursor.emit(|buf, y, theme| {
                    render_context_item_block(
                        buf,
                        line_area,
                        y,
                        &item,
                        theme,
                        show_thread_bar,
                        file_highlights,
                        false,
                        false,
                        start_line,
                    );
                });
            }
            DisplayItem::Line { line_num, content } => {
                cursor.mark_cursor_stop();
                if sctx.wrap {
                    let line_index = (*line_num - start_line) as usize;
                    let highlight = file_highlights.get(line_index);
                    let line_num_width = SBS_LINE_NUM_WIDTH;
                    let content_width =
                        diff_content_width(line_area).saturating_sub(line_num_width) as usize;
                    let wrapped = wrap_content(highlight, content, content_width);
                    let rows = wrapped.len().max(1);
                    let is_cursor = cursor.is_cursor_at(rows);
                    let is_selected = cursor.is_selected_at(rows);
                    cursor.emit_rows(rows, |buf, y, theme, row| {
                        render_context_line_wrapped_row(
                            buf,
                            y,
                            *line_num,
                            theme,
                            &LineRenderCtx {
                                area: line_area,
                                anchor: None,
                                show_thread_bar,
                                is_cursor,
                                is_selected,
                            },
                            &wrapped,
                            row,
                        );
                    });
                } else {
                    let is_cursor = cursor.is_cursor_at(1);
                    let is_selected = cursor.is_selected_at(1);
                    cursor.emit(|buf, y, theme| {
                        render_context_item_block(
                            buf,
                            line_area,
                            y,
                            &item,
                            theme,
                            show_thread_bar,
                            file_highlights,
                            is_cursor,
                            is_selected,
                            start_line,
                        );
                    });
                }
            }
        }

        if let DisplayItem::Line { line_num, .. } = &item {
            for thread in file_threads.iter().filter(|t| {
                let end = t.selection_end.unwrap_or(t.selection_start);
                end == *line_num
            }) {
                sctx.thread_positions
                    .borrow_mut()
                    .entry(thread.thread_id.clone())
                    .or_insert(cursor.stream_row);
                if let Some(comments) = sctx.all_comments.get(&thread.thread_id) {
                    let rows = comment_block_rows(thread, comments, area);
                    let is_cursor = cursor.is_cursor_at(rows);
                    let hl = is_cursor || cursor.is_selected_at(rows);
                    emit_comment_block(
                        cursor,
                        area,
                        thread,
                        comments,
                        hl,
                        is_cursor,
                        sctx.highlighter,
                    );
                }
            }
        }
    }
}

fn render_file_header(
    cursor: &mut StreamCursor<'_>,
    area: Rect,
    file: &crate::model::FileEntry,
    file_cache: &std::collections::HashMap<String, crate::model::FileCacheEntry>,
    theme: &Theme,
) {
    for _ in 0..BLOCK_MARGIN {
        cursor.emit(|buf, y, _| {
            buffer_fill_rect(buf, area.x, y, area.width, 1, theme.background);
        });
    }
    for _ in 0..BLOCK_PADDING {
        cursor.emit(|buf, y, theme| {
            draw_block_base_line(buf, area, y, theme.panel_bg, theme);
        });
    }
    let counts = file_cache
        .get(&file.path)
        .and_then(|entry| entry.diff.as_ref())
        .map(diff_change_counts);
    cursor.emit(|buf, y, theme| {
        draw_file_header_line(buf, area, y, theme, &file.path, counts);
    });
    for _ in 0..BLOCK_PADDING {
        cursor.emit(|buf, y, theme| {
            draw_block_base_line(buf, area, y, theme.panel_bg, theme);
        });
    }
    for _ in 0..BLOCK_MARGIN {
        cursor.emit(|buf, y, _| {
            buffer_fill_rect(buf, area.x, y, area.width, 1, theme.background);
        });
    }
}

struct UnifiedDisplayData<'a> {
    display_lines: Vec<DisplayLine>,
    anchor_map: AnchorMap<'a>,
    comment_map: AnchorMap<'a>,
    thread_ranges: Vec<(i64, i64)>,
}

fn build_unified_display_data<'a>(
    hunks: &[crate::diff::DiffHunk],
    threads: &[&ThreadSummary],
    anchors: &'a [ThreadAnchor],
) -> UnifiedDisplayData<'a> {
    let mut anchor_map: AnchorMap<'_> = std::collections::HashMap::new();
    let mut comment_map: AnchorMap<'_> = std::collections::HashMap::new();
    for anchor in anchors {
        anchor_map
            .entry(anchor.display_line)
            .or_default()
            .push(anchor);
        comment_map
            .entry(anchor.comment_after_line)
            .or_default()
            .push(anchor);
    }

    let thread_ranges = build_thread_ranges(threads);

    let mut display_lines: Vec<DisplayLine> = Vec::new();
    for hunk in hunks {
        display_lines.push(DisplayLine::HunkHeader);
        for line in &hunk.lines {
            display_lines.push(DisplayLine::Diff(line.clone()));
        }
    }

    UnifiedDisplayData {
        display_lines,
        anchor_map,
        comment_map,
        thread_ranges,
    }
}

/// Emit comment blocks after the last line of thread ranges ending at `idx`.
fn try_emit_line_comment(
    cursor: &mut StreamCursor<'_>,
    idx: usize,
    display_data: &UnifiedDisplayData<'_>,
    ctx: &DiffRenderCtx<'_>,
) {
    let Some(anchors) = display_data.comment_map.get(&idx) else {
        return;
    };
    for comment_anchor in anchors {
        ctx.thread_positions
            .borrow_mut()
            .entry(comment_anchor.thread_id.clone())
            .or_insert(cursor.stream_row);
        let Some(thread) = ctx
            .threads
            .iter()
            .find(|t| t.thread_id == comment_anchor.thread_id)
        else {
            continue;
        };
        if let Some(comments) = ctx.all_comments.get(&comment_anchor.thread_id) {
            let rows = comment_block_rows(thread, comments, ctx.area);
            let is_cursor = cursor.is_cursor_at(rows);
            let hl = is_cursor || cursor.is_selected_at(rows);
            emit_comment_block(
                cursor,
                ctx.area,
                thread,
                comments,
                hl,
                is_cursor,
                ctx.highlighter,
            );
        }
    }
}

#[allow(clippy::too_many_lines)]
fn render_unified_display_items(
    cursor: &mut StreamCursor<'_>,
    display_data: &UnifiedDisplayData<'_>,
    ctx: &DiffRenderCtx<'_>,
    orphaned_context: Option<&OrphanedContext<'_>>,
    emitted_threads: &mut std::collections::HashSet<String>,
    last_line_num: &mut Option<i64>,
) -> usize {
    let mut section_idx = 0usize;
    for (idx, display_line) in display_data.display_lines.iter().enumerate() {
        if matches!(display_line, DisplayLine::HunkHeader) {
            if let Some(context) = orphaned_context {
                if let Some(section) = context.sections.get(section_idx) {
                    emit_orphaned_context_section(
                        cursor,
                        ctx.line_area,
                        ctx.area,
                        context,
                        section,
                        ctx.wrap,
                        &mut OrphanedRenderState {
                            all_comments: ctx.all_comments,
                            thread_positions: ctx.thread_positions,
                            emitted_threads,
                            last_line_num,
                            highlighter: ctx.highlighter,
                        },
                    );
                }
            }
            section_idx = section_idx.saturating_add(1);
        }
        let show_thread_bar = match display_line {
            DisplayLine::Diff(line) => {
                line_in_thread_ranges(line.new_line.map(i64::from), &display_data.thread_ranges)
            }
            DisplayLine::HunkHeader => false,
        };
        let anchors_at_line = display_data.anchor_map.get(&idx);
        let anchor = anchors_at_line.and_then(|v: &Vec<&ThreadAnchor>| v.first().copied());
        if let Some(anchors) = anchors_at_line {
            for a in anchors {
                ctx.thread_positions
                    .borrow_mut()
                    .entry(a.thread_id.clone())
                    .or_insert(cursor.stream_row);
            }
        }
        match display_line {
            DisplayLine::HunkHeader => {
                cursor.emit(|buf, y, theme| {
                    render_unified_diff_line_block(
                        buf,
                        y,
                        display_line,
                        theme,
                        &LineRenderCtx {
                            area: ctx.line_area,
                            anchor,
                            show_thread_bar,
                            is_cursor: false,
                            is_selected: false,
                        },
                        ctx.file_highlights.get(idx),
                    );
                });
            }
            DisplayLine::Diff(line) => {
                cursor.mark_cursor_stop();
                // Record new-side line mapping for comment targeting
                if let Some(nl) = line.new_line {
                    let base = cursor.stream_row;
                    let nl_i64 = i64::from(nl);
                    if ctx.wrap {
                        let thread_col_width = THREAD_COL_WIDTH;
                        let line_num_width = UNIFIED_LINE_NUM_WIDTH;
                        let cw = diff_content_width(ctx.line_area)
                            .saturating_sub(thread_col_width + line_num_width);
                        let max_c = cw.saturating_sub(2) as usize;
                        let row_count =
                            wrap_content(ctx.file_highlights.get(idx), &line.content, max_c)
                                .len()
                                .max(1);
                        let mut lm = ctx.line_map.borrow_mut();
                        for r in 0..row_count {
                            lm.insert(base + r, nl_i64);
                        }
                    } else {
                        ctx.line_map.borrow_mut().insert(base, nl_i64);
                    }
                }
                if ctx.wrap {
                    let thread_col_width = THREAD_COL_WIDTH;
                    let line_num_width = UNIFIED_LINE_NUM_WIDTH;
                    let content_width = diff_content_width(ctx.line_area)
                        .saturating_sub(thread_col_width + line_num_width);
                    let max_content = content_width.saturating_sub(2) as usize;
                    let wrapped =
                        wrap_content(ctx.file_highlights.get(idx), &line.content, max_content);
                    let rows = wrapped.len().max(1);
                    let is_cursor = cursor.is_cursor_at(rows);
                    let is_selected = cursor.is_selected_at(rows);
                    cursor.emit_rows(rows, |buf, y, theme, row| {
                        render_unified_diff_line_wrapped_row(
                            buf,
                            y,
                            line,
                            theme,
                            &LineRenderCtx {
                                area: ctx.line_area,
                                anchor,
                                show_thread_bar,
                                is_cursor,
                                is_selected,
                            },
                            &wrapped,
                            row,
                        );
                    });
                } else {
                    let is_cursor = cursor.is_cursor_at(1);
                    let is_selected = cursor.is_selected_at(1);
                    cursor.emit(|buf, y, theme| {
                        render_unified_diff_line_block(
                            buf,
                            y,
                            display_line,
                            theme,
                            &LineRenderCtx {
                                area: ctx.line_area,
                                anchor,
                                show_thread_bar,
                                is_cursor,
                                is_selected,
                            },
                            ctx.file_highlights.get(idx),
                        );
                    });
                }
            }
        }

        try_emit_line_comment(cursor, idx, display_data, ctx);
    }
    section_idx
}

fn render_file_diff_unified(
    cursor: &mut StreamCursor<'_>,
    hunks: &[crate::diff::DiffHunk],
    ctx: &DiffRenderCtx<'_>,
    orphaned_context: Option<&OrphanedContext<'_>>,
    anchors: &[ThreadAnchor],
) -> std::collections::HashSet<String> {
    let mut emitted_threads: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_line_num: Option<i64> = None;

    let display_data = build_unified_display_data(hunks, ctx.threads, anchors);

    let section_idx = render_unified_display_items(
        cursor,
        &display_data,
        ctx,
        orphaned_context,
        &mut emitted_threads,
        &mut last_line_num,
    );

    if let Some(context) = orphaned_context {
        if let Some(section) = context.sections.get(section_idx) {
            emit_orphaned_context_section(
                cursor,
                ctx.line_area,
                ctx.area,
                context,
                section,
                ctx.wrap,
                &mut OrphanedRenderState {
                    all_comments: ctx.all_comments,
                    thread_positions: ctx.thread_positions,
                    emitted_threads: &mut emitted_threads,
                    last_line_num: &mut last_line_num,
                    highlighter: ctx.highlighter,
                },
            );
        }
    }
    emitted_threads
}

/// Build anchor and comment maps for side-by-side rendering.
/// Maps thread anchors to their corresponding SBS line indices.
fn build_sbs_anchor_maps<'a>(
    anchors: &'a [ThreadAnchor],
    threads: &[&ThreadSummary],
    sbs_lines: &[SideBySideLine],
) -> (AnchorMap<'a>, AnchorMap<'a>) {
    let mut sbs_anchor_map: AnchorMap<'_> = std::collections::HashMap::new();
    let mut sbs_comment_map: AnchorMap<'_> = std::collections::HashMap::new();
    for anchor in anchors {
        if let Some(thread) = threads.iter().find(|t| t.thread_id == anchor.thread_id) {
            let start = thread.selection_start as u32;
            let end = thread.selection_end.unwrap_or(thread.selection_start) as u32;
            for (si, sl) in sbs_lines.iter().enumerate() {
                if sl.right.as_ref().is_some_and(|l| l.line_num == start) {
                    sbs_anchor_map.entry(si).or_default().push(anchor);
                }
                if sl.right.as_ref().is_some_and(|l| l.line_num == end) {
                    sbs_comment_map.entry(si).or_default().push(anchor);
                }
            }
        }
    }
    (sbs_anchor_map, sbs_comment_map)
}

/// Render a single SBS line with wrapping support.
fn render_sbs_line(
    cursor: &mut StreamCursor<'_>,
    sbs_line: &SideBySideLine,
    ctx: &LineRenderCtx<'_>,
    wrap: bool,
    file_highlights: &[Vec<HighlightSpan>],
) {
    if wrap && !sbs_line.is_header {
        let thread_col_width = THREAD_COL_WIDTH;
        let divider_width: u32 = 0;
        let line_num_width = SBS_LINE_NUM_WIDTH;
        let available =
            diff_content_width(ctx.area).saturating_sub(thread_col_width + divider_width);
        let half_width = available / 2;
        let left_width = half_width.saturating_sub(line_num_width) as usize;
        let right_width = half_width.saturating_sub(line_num_width) as usize;

        let left_highlights = sbs_line
            .left
            .as_ref()
            .and_then(|line| file_highlights.get(line.display_index));
        let right_highlights = sbs_line
            .right
            .as_ref()
            .and_then(|line| file_highlights.get(line.display_index));

        let left_wrapped = sbs_line
            .left
            .as_ref()
            .map(|line| wrap_content(left_highlights, &line.content, left_width));
        let right_wrapped = sbs_line
            .right
            .as_ref()
            .map(|line| wrap_content(right_highlights, &line.content, right_width));

        let left_rows = left_wrapped.as_ref().map_or(1, Vec::len);
        let right_rows = right_wrapped.as_ref().map_or(1, Vec::len);
        let rows = left_rows.max(right_rows);

        cursor.emit_rows(rows, |buf, y, theme, row| {
            render_side_by_side_line_wrapped_row(
                buf,
                y,
                sbs_line,
                theme,
                ctx,
                (left_wrapped.as_ref(), right_wrapped.as_ref()),
                row,
            );
        });
    } else {
        cursor.emit(|buf, y, theme| {
            render_side_by_side_line_block(buf, y, sbs_line, theme, ctx, file_highlights);
        });
    }
}

#[allow(clippy::too_many_lines)]
fn render_file_diff_sbs(
    cursor: &mut StreamCursor<'_>,
    sbs_lines: &[SideBySideLine],
    ctx: &DiffRenderCtx<'_>,
    orphaned_context: Option<&OrphanedContext<'_>>,
    anchors: &[ThreadAnchor],
) -> std::collections::HashSet<String> {
    let mut emitted_threads: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_line_num: Option<i64> = None;

    let thread_ranges = build_thread_ranges(ctx.threads);
    let (sbs_anchor_map, sbs_comment_map) = build_sbs_anchor_maps(anchors, ctx.threads, sbs_lines);

    let mut section_idx = 0usize;
    for (idx, sbs_line) in sbs_lines.iter().enumerate() {
        if sbs_line.is_header {
            if let Some(context) = orphaned_context {
                if let Some(section) = context.sections.get(section_idx) {
                    emit_orphaned_context_section(
                        cursor,
                        ctx.line_area,
                        ctx.area,
                        context,
                        section,
                        ctx.wrap,
                        &mut OrphanedRenderState {
                            all_comments: ctx.all_comments,
                            thread_positions: ctx.thread_positions,
                            emitted_threads: &mut emitted_threads,
                            last_line_num: &mut last_line_num,
                            highlighter: ctx.highlighter,
                        },
                    );
                }
            }
            section_idx = section_idx.saturating_add(1);
        }
        let show_thread_bar = if sbs_line.is_header {
            false
        } else {
            line_in_thread_ranges(
                sbs_line.right.as_ref().map(|line| i64::from(line.line_num)),
                &thread_ranges,
            )
        };
        let anchors_at_line = sbs_anchor_map.get(&idx);
        let anchor = anchors_at_line.and_then(|v: &Vec<&ThreadAnchor>| v.first().copied());
        if let Some(anchors) = anchors_at_line {
            for a in anchors {
                ctx.thread_positions
                    .borrow_mut()
                    .entry(a.thread_id.clone())
                    .or_insert(cursor.stream_row);
            }
        }
        // Compute wrapped row count for this line (must match render_sbs_line).
        // Used for both line_map recording and cursor/selection highlighting.
        let sbs_rows = if !sbs_line.is_header && ctx.wrap {
            let thread_col_width = THREAD_COL_WIDTH;
            let divider_width: u32 = 0;
            let line_num_width = SBS_LINE_NUM_WIDTH;
            let available =
                diff_content_width(ctx.line_area).saturating_sub(thread_col_width + divider_width);
            let half_width = available / 2;
            let left_w = half_width.saturating_sub(line_num_width) as usize;
            let right_w = half_width.saturating_sub(line_num_width) as usize;
            let left_rows = sbs_line
                .left
                .as_ref()
                .map_or(1, |l| wrap_content(None, &l.content, left_w).len().max(1));
            let right_rows = sbs_line
                .right
                .as_ref()
                .map_or(1, |r| wrap_content(None, &r.content, right_w).len().max(1));
            left_rows.max(right_rows)
        } else {
            1
        };

        // Record new-side line mapping for comment targeting (right side = new)
        if !sbs_line.is_header {
            if let Some(right) = &sbs_line.right {
                let nl = i64::from(right.line_num);
                let base = cursor.stream_row;
                let mut lm = ctx.line_map.borrow_mut();
                for r in 0..sbs_rows {
                    lm.insert(base + r, nl);
                }
            }
        }
        if !sbs_line.is_header {
            cursor.mark_cursor_stop();
        }
        let is_cursor = !sbs_line.is_header && cursor.is_cursor_at(sbs_rows);
        let is_selected = !sbs_line.is_header && cursor.is_selected_at(sbs_rows);
        render_sbs_line(
            cursor,
            sbs_line,
            &LineRenderCtx {
                area: ctx.line_area,
                anchor,
                show_thread_bar,
                is_cursor,
                is_selected,
            },
            ctx.wrap,
            ctx.file_highlights,
        );

        // Emit comment blocks after the last line of the thread range
        if let Some(comment_anchors) = sbs_comment_map.get(&idx) {
            for comment_anchor in comment_anchors {
                ctx.thread_positions
                    .borrow_mut()
                    .entry(comment_anchor.thread_id.clone())
                    .or_insert(cursor.stream_row);
                if let Some(thread) = ctx
                    .threads
                    .iter()
                    .find(|t| t.thread_id == comment_anchor.thread_id)
                {
                    if let Some(comments) = ctx.all_comments.get(&comment_anchor.thread_id) {
                        let rows = comment_block_rows(thread, comments, ctx.area);
                        let is_cursor = cursor.is_cursor_at(rows);
                        let hl = is_cursor || cursor.is_selected_at(rows);
                        emit_comment_block(
                            cursor,
                            ctx.area,
                            thread,
                            comments,
                            hl,
                            is_cursor,
                            ctx.highlighter,
                        );
                    }
                }
            }
        }
    }
    if let Some(context) = orphaned_context {
        if let Some(section) = context.sections.get(section_idx) {
            emit_orphaned_context_section(
                cursor,
                ctx.line_area,
                ctx.area,
                context,
                section,
                ctx.wrap,
                &mut OrphanedRenderState {
                    all_comments: ctx.all_comments,
                    thread_positions: ctx.thread_positions,
                    emitted_threads: &mut emitted_threads,
                    last_line_num: &mut last_line_num,
                    highlighter: ctx.highlighter,
                },
            );
        }
    }
    emitted_threads
}

pub fn render_diff_stream(buffer: &mut OptimizedBuffer, area: Rect, params: &DiffStreamParams<'_>) {
    params.thread_positions.borrow_mut().clear();
    params.line_map.borrow_mut().clear();
    params.cursor_stops.borrow_mut().clear();
    params.max_stream_row.set(0);
    let mut cursor = StreamCursor {
        buffer,
        area,
        scroll: params.scroll,
        screen_row: 0,
        stream_row: 0,
        diff_cursor: params.diff_cursor,
        theme: params.theme,
        max_stream_row: params.max_stream_row,
        selection: params.selection,
        cursor_stops: params.cursor_stops,
    };

    // Render description block if present
    if let Some(desc) = params.description {
        if !desc.trim().is_empty() {
            render_description_block(&mut cursor, area, desc, params.highlighter, params.theme);
        }
    }

    let files = params.files;
    let file_cache = params.file_cache;
    let threads = params.threads;
    let theme = params.theme;
    let view_mode = params.view_mode;
    let sctx = StreamRenderCtx {
        wrap: params.wrap,
        all_comments: params.all_comments,
        thread_positions: params.thread_positions,
        line_map: params.line_map,
        highlighter: params.highlighter,
    };

    for file in files {
        render_file_header(&mut cursor, area, file, file_cache, theme);

        let file_threads: Vec<&ThreadSummary> = threads
            .iter()
            .filter(|t| t.file_path == file.path)
            .collect();

        if let Some(entry) = file_cache.get(&file.path) {
            if let Some(diff) = &entry.diff {
                render_file_with_diff(
                    &mut cursor,
                    area,
                    diff,
                    entry,
                    &file_threads,
                    view_mode,
                    &sctx,
                );
            } else if let Some(content) = &entry.file_content {
                render_file_content_no_diff(
                    &mut cursor,
                    area,
                    content,
                    &file_threads,
                    &entry.highlighted_lines,
                    &sctx,
                );
            } else {
                cursor.emit(|buf, y, theme| {
                    draw_block_text_line(
                        buf,
                        area,
                        y,
                        theme.panel_bg,
                        "No content available",
                        theme.style_muted(),
                        theme,
                    );
                });
            }
        }
    }

    if cursor.remaining_rows() > 0 {
        let remaining_start = area.y + cursor.screen_row as u32;
        let remaining_height = area.height.saturating_sub(cursor.screen_row as u32);
        buffer_fill_rect(
            buffer,
            area.x,
            remaining_start,
            area.width,
            remaining_height,
            theme.background,
        );
    }
}

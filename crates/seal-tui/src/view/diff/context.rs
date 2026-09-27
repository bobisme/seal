//! Orphaned context building, rendering, and `calculate_context_ranges`.

use crate::render_backend::{buffer_draw_text, buffer_fill_rect, OptimizedBuffer, Style};

use crate::db::ThreadSummary;
use crate::layout::{CONTEXT_LINES, SBS_LINE_NUM_WIDTH};
use crate::syntax::{HighlightSpan, Highlighter};
use crate::theme::Theme;
use crate::view::components::Rect;

use super::analysis::{build_thread_ranges, line_in_thread_ranges};
use super::comments::{comment_block_rows, emit_comment_block};
use super::helpers::{
    cursor_bg, cursor_fg, draw_diff_base_line, orphaned_context_width, orphaned_context_x,
    selection_bg,
};
use super::text_util::{
    draw_highlighted_text, draw_wrapped_line, wrap_content, HighlightContent, WrappedLine,
};
use super::{DisplayItem, LineRange, LineRenderCtx, OrphanedContext, StreamCursor};

// --- Context range calculation ---

/// Calculate context ranges around threads, merging overlapping ranges.
/// `exclude_ranges` are line ranges already shown in the diff; context lines
/// that fall inside them are trimmed so the same code doesn't appear twice.
pub(super) fn calculate_context_ranges(
    threads: &[&ThreadSummary],
    total_lines: usize,
    exclude_ranges: &[(i64, i64)],
) -> Vec<LineRange> {
    if threads.is_empty() {
        return Vec::new();
    }

    let mut ranges: Vec<LineRange> = threads
        .iter()
        .map(|t| {
            let (start, end) = t.effective_selection();
            let thread_end = end.unwrap_or(start);
            LineRange {
                start: (start - CONTEXT_LINES).max(1),
                #[allow(clippy::cast_possible_wrap)]
                end: (thread_end + CONTEXT_LINES).min(total_lines as i64),
            }
        })
        .collect();

    ranges.sort_by_key(|r| r.start);

    let mut merged: Vec<LineRange> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut() {
            if range.start <= last.end + 1 {
                last.end = last.end.max(range.end);
            } else {
                merged.push(range);
            }
        } else {
            merged.push(range);
        }
    }

    if exclude_ranges.is_empty() {
        return merged;
    }

    let mut clipped: Vec<LineRange> = Vec::new();
    for range in merged {
        let mut remaining = vec![range];
        for &(ex_start, ex_end) in exclude_ranges {
            let mut next = Vec::new();
            for r in remaining {
                if r.end < ex_start || r.start > ex_end {
                    next.push(r);
                } else {
                    if r.start < ex_start {
                        next.push(LineRange {
                            start: r.start,
                            end: ex_start - 1,
                        });
                    }
                    if r.end > ex_end {
                        next.push(LineRange {
                            start: ex_end + 1,
                            end: r.end,
                        });
                    }
                }
            }
            remaining = next;
        }
        clipped.extend(remaining);
    }
    clipped.sort_by_key(|r| r.start);
    clipped
}

// --- Context item building ---

pub(super) fn build_context_items(
    lines: &[String],
    threads: &[&ThreadSummary],
    exclude_ranges: &[(i64, i64)],
    start_line: i64,
) -> Vec<DisplayItem> {
    #[allow(clippy::cast_possible_wrap)]
    let total_lines = start_line + lines.len() as i64 - 1;
    let ranges = calculate_context_ranges(threads, total_lines as usize, exclude_ranges);
    if ranges.is_empty() {
        return vec![DisplayItem::Separator(0)];
    }

    build_context_items_from_ranges(lines, &ranges, start_line)
}

pub(super) fn build_context_items_from_ranges(
    lines: &[String],
    ranges: &[LineRange],
    start_line: i64,
) -> Vec<DisplayItem> {
    if ranges.is_empty() {
        return Vec::new();
    }

    let mut display_items: Vec<DisplayItem> = Vec::new();
    let mut prev_end: Option<i64> = None;

    for range in ranges {
        if let Some(pe) = prev_end {
            if range.start > pe + 1 {
                let gap = range.start - pe - 1;
                display_items.push(DisplayItem::Separator(gap));
            }
        }

        for line_num in range.start..=range.end {
            let idx = (line_num - start_line) as usize;
            if idx < lines.len() {
                display_items.push(DisplayItem::Line {
                    line_num,
                    content: lines[idx].clone(),
                });
            }
        }

        prev_end = Some(range.end);
    }

    display_items
}

pub(super) fn group_context_ranges_by_hunks(
    ranges: Vec<LineRange>,
    hunk_ranges: &[(i64, i64)],
) -> Vec<Vec<LineRange>> {
    let mut sections: Vec<Vec<LineRange>> = vec![Vec::new(); hunk_ranges.len() + 1];
    if ranges.is_empty() {
        return sections;
    }

    let mut hunk_idx = 0usize;
    for range in ranges {
        while hunk_idx < hunk_ranges.len() && hunk_ranges[hunk_idx].0 <= range.end {
            hunk_idx += 1;
        }
        sections[hunk_idx].push(range);
    }

    sections
}

// --- Context rendering ---

/// Shared mutable state for orphaned context rendering across sections.
pub(super) struct OrphanedRenderState<'a> {
    pub all_comments: &'a std::collections::HashMap<String, Vec<crate::db::Comment>>,
    pub thread_positions: &'a std::cell::RefCell<std::collections::HashMap<String, usize>>,
    pub emitted_threads: &'a mut std::collections::HashSet<String>,
    pub last_line_num: &'a mut Option<i64>,
    pub highlighter: &'a Highlighter,
}

pub(super) fn emit_orphaned_context_section(
    cursor: &mut StreamCursor<'_>,
    area: Rect,
    comment_area: Rect,
    context: &OrphanedContext<'_>,
    ranges: &[LineRange],
    wrap: bool,
    state: &mut OrphanedRenderState<'_>,
) {
    if ranges.is_empty() {
        return;
    }

    let thread_ranges = build_thread_ranges(&context.threads);
    let dt = &cursor.theme.diff;
    cursor.emit(|buf, y, _| {
        draw_diff_base_line(buf, area, y, dt.context_bg);
    });

    let display_items = build_context_items_from_ranges(context.lines, ranges, context.start_line);
    for item in &display_items {
        if let DisplayItem::Line { line_num, .. } = item {
            if let Some(prev) = state.last_line_num.as_ref() {
                for thread in &context.threads {
                    let end = thread.effective_end();
                    if !state.emitted_threads.contains(thread.thread_id.as_str())
                        && end > *prev
                        && end < *line_num
                    {
                        state.emitted_threads.insert(thread.thread_id.clone());
                        state
                            .thread_positions
                            .borrow_mut()
                            .insert(thread.thread_id.clone(), cursor.stream_row);
                        if let Some(comments) = state.all_comments.get(&thread.thread_id) {
                            let rows = comment_block_rows(thread, comments, comment_area);
                            let is_cursor = cursor.is_cursor_at(rows);
                            let hl = is_cursor || cursor.is_selected_at(rows);
                            emit_comment_block(
                                cursor,
                                comment_area,
                                thread,
                                comments,
                                hl,
                                is_cursor,
                                state.highlighter,
                            );
                        }
                    }
                }
            }
        }

        let show_thread_bar = match item {
            DisplayItem::Line { line_num, .. } => {
                line_in_thread_ranges(Some(*line_num), &thread_ranges)
            }
            DisplayItem::Separator(_) => false,
        };
        match item {
            DisplayItem::Separator(_) => {
                cursor.emit(|buf, y, theme| {
                    render_context_item_block(
                        buf,
                        area,
                        y,
                        item,
                        theme,
                        show_thread_bar,
                        context.highlights,
                        false,
                        false,
                        context.start_line,
                    );
                });
            }
            DisplayItem::Line {
                line_num,
                content: line_content,
            } => {
                cursor.mark_cursor_stop();
                if wrap {
                    let line_index = (*line_num - context.start_line) as usize;
                    let highlight = context.highlights.get(line_index);
                    let line_num_width = SBS_LINE_NUM_WIDTH;
                    let cw = orphaned_context_width(area).saturating_sub(line_num_width) as usize;
                    let wrapped = wrap_content(highlight, line_content, cw);
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
                                area,
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
                            area,
                            y,
                            item,
                            theme,
                            show_thread_bar,
                            context.highlights,
                            is_cursor,
                            is_selected,
                            context.start_line,
                        );
                    });
                }

                let end_matches: Vec<&ThreadSummary> = context
                    .threads
                    .iter()
                    .copied()
                    .filter(|thread| {
                        let end = thread.effective_end();
                        end == *line_num
                            && !state.emitted_threads.contains(thread.thread_id.as_str())
                    })
                    .collect();
                for thread in end_matches {
                    state.emitted_threads.insert(thread.thread_id.clone());
                    state
                        .thread_positions
                        .borrow_mut()
                        .insert(thread.thread_id.clone(), cursor.stream_row);
                    if let Some(comments) = state.all_comments.get(&thread.thread_id) {
                        let rows = comment_block_rows(thread, comments, comment_area);
                        let is_cursor = cursor.is_cursor_at(rows);
                        let hl = is_cursor || cursor.is_selected_at(rows);
                        emit_comment_block(
                            cursor,
                            comment_area,
                            thread,
                            comments,
                            hl,
                            is_cursor,
                            state.highlighter,
                        );
                    }
                }
                *state.last_line_num = Some(*line_num);
            }
        }
    }
}

pub(super) fn emit_remaining_orphaned_comments(
    cursor: &mut StreamCursor<'_>,
    comment_area: Rect,
    context: &OrphanedContext<'_>,
    all_comments: &std::collections::HashMap<String, Vec<crate::db::Comment>>,
    thread_positions: &std::cell::RefCell<std::collections::HashMap<String, usize>>,
    emitted_threads: &std::collections::HashSet<String>,
    highlighter: &Highlighter,
) {
    let mut remaining: Vec<&&ThreadSummary> = context
        .threads
        .iter()
        .filter(|t| !emitted_threads.contains(t.thread_id.as_str()))
        .collect();
    remaining.sort_by_key(|t| t.effective_start());
    for thread in remaining {
        thread_positions
            .borrow_mut()
            .insert(thread.thread_id.clone(), cursor.stream_row);
        if let Some(comments) = all_comments.get(&thread.thread_id) {
            let rows = comment_block_rows(thread, comments, comment_area);
            let is_cursor = cursor.is_cursor_at(rows);
            let hl = is_cursor || cursor.is_selected_at(rows);
            emit_comment_block(
                cursor,
                comment_area,
                thread,
                comments,
                hl,
                is_cursor,
                highlighter,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_context_item_block(
    buffer: &mut OptimizedBuffer,
    area: Rect,
    y: u32,
    item: &DisplayItem,
    theme: &Theme,
    _show_thread_bar: bool,
    highlighted_lines: &[Vec<HighlightSpan>],
    is_cursor: bool,
    is_selected: bool,
    start_line: i64,
) {
    let dt = &theme.diff;
    match item {
        DisplayItem::Separator(gap) => {
            draw_diff_base_line(buffer, area, y, dt.context_bg);
            let sep_text = if *gap > 0 {
                format!("··· {gap} lines ···")
            } else {
                "···".to_string()
            };
            let sep_x = orphaned_context_x(area)
                + orphaned_context_width(area).saturating_sub(sep_text.len() as u32) / 2;
            buffer_draw_text(
                buffer,
                sep_x,
                y,
                &sep_text,
                theme.style_muted_on(dt.context_bg),
            );
        }
        DisplayItem::Line { line_num, content } => {
            let bg = cursor_bg(
                selection_bg(dt.context_bg, is_selected, theme),
                is_cursor,
                theme,
            );
            let fg = cursor_fg(dt.context, is_cursor);
            let ln_fg = cursor_fg(dt.line_number, is_cursor);
            draw_diff_base_line(buffer, area, y, bg);

            let ln_str = format!("{line_num:5} ");
            let line_num_width = SBS_LINE_NUM_WIDTH;
            let ln_x = orphaned_context_x(area);
            buffer_fill_rect(buffer, ln_x, y, line_num_width, 1, bg);
            buffer_draw_text(buffer, ln_x, y, &ln_str, Style::fg(ln_fg).with_bg(bg));

            let content_x = ln_x + line_num_width;
            let content_width = orphaned_context_width(area).saturating_sub(line_num_width);
            buffer_fill_rect(buffer, content_x, y, content_width, 1, bg);
            let highlight = highlighted_lines.get((*line_num - start_line) as usize);
            draw_highlighted_text(
                buffer,
                content_x,
                y,
                content_width,
                &HighlightContent {
                    spans: highlight,
                    fallback_text: content,
                    fallback_fg: fg,
                    bg,
                },
            );
        }
    }
}

pub(super) fn render_context_line_wrapped_row(
    buffer: &mut OptimizedBuffer,
    y: u32,
    line_num: i64,
    theme: &Theme,
    ctx: &LineRenderCtx<'_>,
    wrapped: &[WrappedLine],
    row: usize,
) {
    let dt = &theme.diff;
    let is_cursor = ctx.is_cursor;
    let bg = cursor_bg(
        selection_bg(dt.context_bg, ctx.is_selected, theme),
        is_cursor,
        theme,
    );
    let fg = cursor_fg(dt.context, is_cursor);
    let ln_fg = cursor_fg(dt.line_number, is_cursor);
    draw_diff_base_line(buffer, ctx.area, y, bg);

    let ln_str = format!("{line_num:5} ");
    let line_num_width = SBS_LINE_NUM_WIDTH;
    let ln_x = orphaned_context_x(ctx.area);
    buffer_fill_rect(buffer, ln_x, y, line_num_width, 1, bg);
    if row == 0 {
        buffer_draw_text(buffer, ln_x, y, &ln_str, Style::fg(ln_fg).with_bg(bg));
    }

    let content_x = ln_x + line_num_width;
    let content_width = orphaned_context_width(ctx.area).saturating_sub(line_num_width);
    buffer_fill_rect(buffer, content_x, y, content_width, 1, bg);
    if let Some(line_content) = wrapped.get(row) {
        draw_wrapped_line(buffer, content_x, y, content_width, line_content, fg, bg);
    }
}

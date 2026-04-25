//! Review detail screen rendering

use crate::render_backend::{buffer_draw_text, buffer_fill_rect, OptimizedBuffer, Rgba, Style};

use super::components::{
    dim_rect, draw_help_bar, draw_help_bar_with_bg, draw_text_truncated, truncate_path, HotkeyHint,
    Rect,
};
use super::diff::{
    diff_change_counts, render_diff_stream, render_pinned_header_block, DiffStreamParams,
};
use crate::layout::{BLOCK_MARGIN, BLOCK_PADDING, DIFF_MARGIN};
use crate::model::{Focus, LayoutMode, Model, SidebarItem};
use crate::render_backend::color_lerp;
use crate::stream::{block_height, description_block_height};

struct SidebarPadding {
    left: u32,
    right: u32,
}

/// Render the review detail screen
pub fn view(model: &Model, buffer: &mut OptimizedBuffer) {
    let area = Rect::from_size(model.width, model.height);

    let inner = Rect::new(area.x, area.y, area.width, area.height);

    if model.current_review.is_none() {
        draw_loading_splash(model, buffer, inner);
        render_help_bar(model, buffer, area);
        return;
    }

    // Layout based on mode
    match model.layout_mode {
        LayoutMode::Full | LayoutMode::Compact | LayoutMode::Overlay => {
            if model.sidebar_visible {
                let sidebar_width = u32::from(model.layout_mode.sidebar_width());
                let (sidebar_area, diff_area) = inner.split_left(sidebar_width);

                draw_file_sidebar(model, buffer, sidebar_area);
                draw_diff_pane(model, buffer, diff_area);
            } else {
                draw_diff_pane(model, buffer, inner);
            }
        }
        LayoutMode::Single => {
            // Show either sidebar or diff based on focus
            if matches!(model.focus, Focus::FileSidebar) && model.sidebar_visible {
                draw_file_sidebar(model, buffer, inner);
            } else {
                draw_diff_pane(model, buffer, inner);
            }
        }
    }

    // Help bar at bottom
    render_help_bar(model, buffer, area);
}

fn draw_loading_splash(model: &Model, buffer: &mut OptimizedBuffer, area: Rect) {
    let theme = &model.theme;
    buffer_fill_rect(
        buffer,
        area.x,
        area.y,
        area.width,
        area.height,
        theme.background,
    );

    let title = "Loading review...";
    let title_width = title.len() as u32;
    let x = area
        .x
        .saturating_add(area.width.saturating_sub(title_width) / 2);
    let y = area.y.saturating_add(area.height / 2);
    buffer_draw_text(buffer, x, y, title, Style::fg(theme.foreground).with_bold());
}

/// Render a file item in the sidebar
fn draw_sidebar_file_item(
    model: &Model,
    buffer: &mut OptimizedBuffer,
    item: &SidebarItem,
    item_idx: usize,
    y: u32,
    inner: Rect,
    pad: &SidebarPadding,
) {
    if let SidebarItem::File {
        entry,
        file_idx,
        collapsed,
    } = item
    {
        let theme = &model.theme;
        let selected = item_idx == model.sidebar_index;
        let focused = matches!(model.focus, Focus::FileSidebar);

        let row_bg = if selected && focused {
            theme.selection_bg
        } else if selected {
            color_lerp(theme.panel_bg, theme.selection_bg, 0.5)
        } else {
            theme.panel_bg
        };

        if selected {
            buffer_fill_rect(buffer, inner.x, y, inner.width, 1, row_bg);
        }

        let collapse_indicator = if *collapsed { "▸ " } else { "▾ " };
        let (prefix, style) = if *file_idx == model.file_index {
            (collapse_indicator, theme.style_primary().with_bg(row_bg))
        } else {
            (collapse_indicator, theme.style_foreground_on(row_bg))
        };

        let prefix_x = inner.x + pad.left;
        buffer_draw_text(buffer, prefix_x, y, prefix, style);

        // Thread count indicator
        let thread_indicator = if entry.open_threads > 0 {
            format!("{}", entry.open_threads)
        } else if entry.resolved_threads > 0 {
            "✓".to_string()
        } else {
            " ".to_string()
        };

        let indicator_color = if entry.open_threads > 0 {
            theme.warning
        } else {
            theme.success
        };

        let indicator_len = thread_indicator.chars().count() as u32;
        let prefix_width: u32 = 2;
        let filename_width = inner
            .width
            .saturating_sub(prefix_width + indicator_len + pad.left + pad.right);

        let (dir_prefix, filename) = split_sidebar_path(&entry.path, filename_width as usize);
        let text_x = prefix_x + prefix_width;
        if !dir_prefix.is_empty() {
            draw_text_truncated(
                buffer,
                text_x,
                y,
                &dir_prefix,
                filename_width,
                theme.style_muted_on(row_bg),
            );
        }
        let dir_width = dir_prefix.chars().count() as u32;
        let file_width = filename_width.saturating_sub(dir_width);
        if file_width > 0 {
            draw_text_truncated(buffer, text_x + dir_width, y, &filename, file_width, style);
        }

        let indicator_x = inner
            .x
            .saturating_add(inner.width)
            .saturating_sub(pad.right + indicator_len);
        buffer_draw_text(
            buffer,
            indicator_x,
            y,
            &thread_indicator,
            Style::fg(indicator_color).with_bg(row_bg),
        );
    }
}

/// Render a thread item in the sidebar
fn draw_sidebar_thread_item(
    model: &Model,
    buffer: &mut OptimizedBuffer,
    item: &SidebarItem,
    item_idx: usize,
    y: u32,
    inner: Rect,
    pad: &SidebarPadding,
) {
    if let SidebarItem::Thread {
        thread_id,
        status,
        comment_count,
        ..
    } = item
    {
        let theme = &model.theme;
        let is_cursor = item_idx == model.sidebar_index;
        let focused = matches!(model.focus, Focus::FileSidebar);

        let row_bg = if is_cursor && focused {
            theme.selection_bg
        } else if is_cursor {
            color_lerp(theme.panel_bg, theme.selection_bg, 0.5)
        } else {
            theme.panel_bg
        };

        if is_cursor {
            buffer_fill_rect(buffer, inner.x, y, inner.width, 1, row_bg);
        }

        let indent: u32 = 4;
        let thread_x = inner.x + pad.left + indent;

        // Right-aligned comment count indicator
        let count_text = format!("{comment_count}");
        let count_len = count_text.chars().count() as u32;
        let count_color = if status == "open" {
            theme.warning
        } else {
            theme.muted
        };

        let indicator_x = inner
            .x
            .saturating_add(inner.width)
            .saturating_sub(pad.right + count_len);

        let id_width = indicator_x.saturating_sub(thread_x + 1);

        let text_style = if is_cursor {
            theme.style_foreground_on(row_bg)
        } else {
            theme.style_muted_on(row_bg)
        };
        draw_text_truncated(buffer, thread_x, y, thread_id, id_width, text_style);

        buffer_draw_text(
            buffer,
            indicator_x,
            y,
            &count_text,
            Style::fg(count_color).with_bg(row_bg),
        );
    }
}

fn draw_file_sidebar(model: &Model, buffer: &mut OptimizedBuffer, area: Rect) {
    let theme = &model.theme;
    let inner = area;
    buffer_fill_rect(
        buffer,
        inner.x,
        inner.y,
        inner.width,
        inner.height,
        theme.panel_bg,
    );
    let items = model.sidebar_items();

    let pad = SidebarPadding { left: 2, right: 2 };
    let mut y = inner.y + 1;
    let text_x = inner.x + pad.left;
    let text_width = inner.width.saturating_sub(pad.left + pad.right);
    let bottom = inner.y + inner.height.saturating_sub(1);

    // Draw review header info
    if let Some(review) = &model.current_review {
        // Header: "cr-xxxx · status"
        let id_len = review.review_id.len() as u32;
        draw_text_truncated(
            buffer,
            text_x,
            y,
            &review.review_id,
            text_width,
            Style::fg(theme.foreground).with_bold(),
        );
        let sep_x = text_x + id_len;
        if sep_x + 3 < text_x + text_width {
            buffer_draw_text(buffer, sep_x, y, " \u{b7} ", theme.style_muted());
            let status_x = sep_x + 3;
            let status_color = match review.status.as_str() {
                "open" | "merged" => theme.success,
                "abandoned" => theme.muted,
                "approved" => theme.warning,
                _ => theme.foreground,
            };
            draw_text_truncated(
                buffer,
                status_x,
                y,
                &review.status,
                text_width.saturating_sub(id_len + 3),
                Style::fg(status_color),
            );
        }
        y += 1;

        // Title (word-wrapped, bright, non-bold)
        if !review.title.is_empty() {
            y += 1;
            for line in word_wrap_lines(&review.title, text_width as usize) {
                if y >= bottom {
                    break;
                }
                draw_text_truncated(
                    buffer,
                    text_x,
                    y,
                    &line,
                    text_width,
                    theme.style_foreground(),
                );
                y += 1;
            }
        }
        y += 1;

        // Ref and commit ID on separate rows.
        let ref_display = format_ref_for_display(&review.jj_change_id, text_width as usize);
        draw_text_truncated(
            buffer,
            text_x,
            y,
            &ref_display,
            text_width,
            theme.style_muted(),
        );
        y += 1;
        draw_text_truncated(
            buffer,
            text_x,
            y,
            &review.initial_commit,
            text_width,
            theme.style_muted(),
        );
        y += 2;
    }

    if items.is_empty() {
        if y < bottom {
            buffer_draw_text(buffer, text_x, y, "No files", theme.style_muted());
        }
        return;
    }

    let start_index = model.sidebar_scroll.min(items.len());
    for (item_idx, item) in items.iter().enumerate().skip(start_index) {
        if y >= bottom {
            break;
        }

        match item {
            SidebarItem::File { .. } => {
                draw_sidebar_file_item(model, buffer, item, item_idx, y, inner, &pad);
            }
            SidebarItem::Thread { .. } => {
                draw_sidebar_thread_item(model, buffer, item, item_idx, y, inner, &pad);
            }
        }

        y += 1;
    }
}

/// Simple word-wrap: split text into lines that fit within `max_width` characters.
fn word_wrap_lines(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            if word.len() > max_width {
                // Word itself is too long — truncate will handle it
                lines.push(word.to_string());
            } else {
                current = word.to_string();
            }
        } else if current.len() + 1 + word.len() <= max_width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn split_sidebar_path(path: &str, max_width: usize) -> (String, String) {
    let display = truncate_path(path, max_width);
    if let Some((dir, filename)) = display.rsplit_once('/') {
        (format!("{dir}/"), filename.to_string())
    } else {
        (String::new(), display)
    }
}

fn format_ref_for_display(raw: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }

    let branch = if let Some(rest) = raw.strip_prefix("refs/heads/") {
        rest
    } else if let Some(rest) = raw.strip_prefix("refs/remotes/") {
        rest
    } else if let Some(rest) = raw.strip_prefix("refs/tags/") {
        return format_with_prefix("⎇ ", &format!("tag:{rest}"), max_width);
    } else if let Some(rest) = raw.strip_prefix("refs/") {
        rest
    } else {
        raw
    };

    format_with_prefix("⎇ ", branch, max_width)
}

fn format_with_prefix(prefix: &str, body: &str, max_width: usize) -> String {
    let prefix_chars = prefix.chars().count();
    if max_width <= prefix_chars {
        return take_chars(prefix, max_width).to_string();
    }

    let body_width = max_width - prefix_chars;
    let truncated = truncate_middle(body, body_width);
    format!("{prefix}{truncated}")
}

fn truncate_middle(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }

    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "…".to_string();
    }

    let keep = max_chars - 1;
    let head = keep / 2;
    let tail = keep - head;
    let start = take_chars(text, head);
    let end = take_last_chars(text, tail);
    format!("{start}…{end}")
}

fn take_chars(text: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }
    for (count, (idx, _)) in text.char_indices().enumerate() {
        if count == max_chars {
            return &text[..idx];
        }
    }
    text
}

fn take_last_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    text.chars().skip(total - max_chars).collect()
}

fn draw_diff_pane(model: &Model, buffer: &mut OptimizedBuffer, area: Rect) {
    let theme = &model.theme;
    let inner = area;

    let content_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(3),
    );

    let files = model.files_with_threads();
    if files.is_empty() {
        buffer_draw_text(
            buffer,
            inner.x + 2,
            inner.y + 1,
            "No content available",
            theme.style_muted(),
        );
        return;
    }

    let file_title = files
        .get(model.file_index)
        .map_or("No file selected", |f| f.path.as_str());

    let counts = files
        .get(model.file_index)
        .and_then(|file| model.file_cache.get(&file.path))
        .and_then(|entry| entry.diff.as_ref())
        .map(diff_change_counts);

    let description = model
        .current_review
        .as_ref()
        .and_then(|r| r.description.as_deref());

    // Pinned header at the top
    let pinned_height = block_height(1) as u32;
    let pinned_area = Rect::new(
        content_area.x,
        content_area.y,
        content_area.width,
        pinned_height.min(content_area.height),
    );

    // Stream area starts below the pinned header
    let stream_area = Rect::new(
        content_area.x,
        content_area.y + pinned_height,
        content_area.width,
        content_area.height.saturating_sub(pinned_height),
    );

    buffer_fill_rect(
        buffer,
        content_area.x,
        content_area.y,
        content_area.width,
        content_area.height,
        theme.background,
    );

    // Compute visual selection range
    let selection = if model.visual_mode {
        let a = model.visual_anchor;
        let b = model.diff_cursor;
        Some((a.min(b), a.max(b)))
    } else {
        None
    };

    // Render stream content (description block + files) below pinned header
    render_diff_stream(
        buffer,
        stream_area,
        &DiffStreamParams {
            files: &files,
            file_cache: &model.file_cache,
            threads: &model.threads,
            all_comments: &model.all_comments,
            scroll: model.diff_scroll,
            diff_cursor: model.diff_cursor,
            theme,
            highlighter: &model.highlighter,
            view_mode: model.diff_view_mode,
            wrap: model.diff_wrap,
            thread_positions: &model.thread_positions,
            max_stream_row: &model.max_stream_row,
            description,
            selection,
            line_map: &model.line_map,
            cursor_stops: &model.cursor_stops,
        },
    );

    // Render pinned header:
    // - When at top (description visible): show review title
    // - When file header reaches pinned position: show current file header
    // The file header text is at: desc_lines + BLOCK_MARGIN + BLOCK_PADDING
    // (accounting for the file block's margin and padding before the header text)
    let layout_width = stream_area.width.saturating_sub(DIFF_MARGIN * 2);
    let desc_lines = description_block_height(description, layout_width);
    let file_header_offset = desc_lines + BLOCK_MARGIN + BLOCK_PADDING;
    if model.diff_scroll >= file_header_offset {
        // Scrolled past description - show file header
        render_pinned_header_block(buffer, pinned_area, file_title, theme, counts);
    } else if let Some(review) = &model.current_review {
        // At top - show review title
        render_pinned_header_block(buffer, pinned_area, &review.title, theme, None);
    }

    // Bottom margin between content and footer
    if inner.height >= 3 {
        let margin_y = inner.y + inner.height - 3;
        buffer_fill_rect(buffer, inner.x, margin_y, inner.width, 1, theme.background);
    }

    if model.focus == Focus::FileSidebar {
        dim_rect(buffer, inner, 0.7);
    }
}

/// A hotkey hint: label in dim, key in bright
fn render_help_bar(model: &Model, buffer: &mut OptimizedBuffer, area: Rect) {
    let mut footer_x = area.x;
    let mut footer_width = area.width;
    if model.sidebar_visible {
        let sidebar_width = u32::from(model.layout_mode.sidebar_width());
        if sidebar_width < area.width
            && matches!(
                model.layout_mode,
                LayoutMode::Full | LayoutMode::Compact | LayoutMode::Overlay
            )
        {
            footer_x = area.x + sidebar_width;
            footer_width = area.width.saturating_sub(sidebar_width);
        }
    }

    if footer_width == 0 {
        return;
    }

    let mut all_hints: Vec<HotkeyHint> = vec![HotkeyHint::new("Commands", "ctrl+p")];

    match model.focus {
        Focus::FileSidebar => {
            all_hints.extend([
                HotkeyHint::new("Navigate", "j/k"),
                HotkeyHint::new("Open", "Enter"),
                HotkeyHint::new("Sidebar", "s"),
                HotkeyHint::new("Back", "h"),
                HotkeyHint::new("Quit", "q"),
            ]);
        }
        Focus::DiffPane if model.visual_mode => {
            all_hints.extend([
                HotkeyHint::new("Select", "j/k"),
                HotkeyHint::new("Comment", "a"),
                HotkeyHint::new(format!("Comment with {}", model.editor_name), "A"),
                HotkeyHint::new("Exit", "V/Esc"),
            ]);
        }
        Focus::DiffPane => {
            let on_diff_line = model.line_map.borrow().contains_key(&model.diff_cursor);
            if on_diff_line {
                all_hints.push(HotkeyHint::new("Select", "V"));
            }
            all_hints.extend([
                HotkeyHint::new("View", "v"),
                HotkeyHint::new("Wrap", "w"),
                HotkeyHint::new("Open File", "o"),
                HotkeyHint::new("Sidebar", "s"),
                HotkeyHint::new("Back", "Esc"),
                HotkeyHint::new("Quit", "q"),
            ]);
        }
        Focus::ThreadExpanded => {
            all_hints.extend([
                HotkeyHint::new("Resolve", "r"),
                HotkeyHint::new("Collapse", "Esc"),
            ]);
        }
        _ => {
            all_hints.extend([HotkeyHint::new("Back", "Esc"), HotkeyHint::new("Quit", "q")]);
        }
    }

    let footer = Rect::new(footer_x, area.y, footer_width, area.height);
    if let Some(flash) = &model.flash_message {
        // Render flash message in error color instead of normal hints.
        let bg = model.theme.background;
        let y = footer.y + footer.height.saturating_sub(2);
        buffer_fill_rect(buffer, footer.x, y, footer.width, 2, bg);
        let style = Style::fg(model.theme.error).with_bg(bg);
        draw_text_truncated(
            buffer,
            footer.x + 2,
            y,
            flash,
            footer.width.saturating_sub(4),
            style,
        );
    } else if model.focus == Focus::FileSidebar {
        let scale = 0.7;
        let bg = &model.theme.background;
        let dimmed_bg = Rgba::new(bg.r * scale, bg.g * scale, bg.b * scale, bg.a);
        draw_help_bar_with_bg(buffer, footer, &model.theme, &all_hints, dimmed_bg);
    } else {
        draw_help_bar(buffer, footer, &model.theme, &all_hints);
    }
}

#[cfg(test)]
mod tests {
    use super::split_sidebar_path;

    #[test]
    fn split_sidebar_path_preserves_filename() {
        let (dir, file) = split_sidebar_path("crates/wraith-diff/src/lib.rs", 18);

        assert!(dir.ends_with('/'));
        assert_eq!(file, "lib.rs");
    }
}

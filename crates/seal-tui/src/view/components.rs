//! Reusable UI components

use std::borrow::Cow;

use crate::render_backend::{
    buffer_dim_cell_rgb, buffer_draw_box, buffer_draw_text, buffer_fill_rect, BoxStyle,
    OptimizedBuffer, Rgba, Style,
};

use crate::theme::Theme;

/// A rectangular area for layout
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    #[must_use]
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Create from terminal dimensions
    #[must_use]
    pub const fn from_size(width: u16, height: u16) -> Self {
        Self::new(0, 0, width as u32, height as u32)
    }

    /// Inner area after removing border (1 cell on each side)
    #[must_use]
    pub const fn inner(&self) -> Self {
        Self {
            x: self.x + 1,
            y: self.y + 1,
            width: self.width.saturating_sub(2),
            height: self.height.saturating_sub(2),
        }
    }

    /// Split horizontally at a given width from left
    #[must_use]
    pub const fn split_left(&self, width: u32) -> (Self, Self) {
        let left = Self {
            x: self.x,
            y: self.y,
            width,
            height: self.height,
        };
        let right = Self {
            x: self.x + width,
            y: self.y,
            width: self.width.saturating_sub(width),
            height: self.height,
        };
        (left, right)
    }

    /// Split vertically at a given height from top
    #[must_use]
    pub const fn split_top(&self, height: u32) -> (Self, Self) {
        let top = Self {
            x: self.x,
            y: self.y,
            width: self.width,
            height,
        };
        let bottom = Self {
            x: self.x,
            y: self.y + height,
            width: self.width,
            height: self.height.saturating_sub(height),
        };
        (top, bottom)
    }
}

/// Draw a bordered box with optional title
#[allow(dead_code)]
pub fn draw_box(
    buffer: &mut OptimizedBuffer,
    area: Rect,
    border_color: Rgba,
    title: Option<&str>,
    title_color: Rgba,
) {
    buffer_draw_box(
        buffer,
        area.x,
        area.y,
        area.width,
        area.height,
        BoxStyle::rounded(Style::fg(border_color)),
    );

    if let Some(title) = title {
        let title_str = format!(" {title} ");
        buffer_draw_text(
            buffer,
            area.x + 2,
            area.y,
            &title_str,
            Style::fg(title_color).with_bold(),
        );
    }
}

/// Draw a filled rectangle
#[allow(dead_code)]
pub fn fill_rect(buffer: &mut OptimizedBuffer, area: Rect, color: Rgba) {
    buffer_fill_rect(buffer, area.x, area.y, area.width, area.height, color);
}

/// Draw text, truncating if necessary
pub fn draw_text_truncated(
    buffer: &mut OptimizedBuffer,
    x: u32,
    y: u32,
    text: &str,
    max_width: u32,
    style: Style,
) {
    if max_width == 0 {
        return;
    }

    let max_chars = max_width as usize;
    let text = if text.chars().count() > max_chars {
        if max_chars <= 1 {
            take_chars(text, max_chars).to_string()
        } else {
            format!("{}\u{2026}", take_chars(text, max_chars - 1))
        }
    } else {
        text.to_string()
    };

    buffer_draw_text(buffer, x, y, &text, style);
}

/// Draw a horizontal line
#[allow(dead_code)]
pub fn draw_hline(buffer: &mut OptimizedBuffer, x: u32, y: u32, width: u32, color: Rgba) {
    let line = "─".repeat(width as usize);
    buffer_draw_text(buffer, x, y, &line, Style::fg(color));
}

/// Draw a status badge (e.g., "[open]", "[merged]")
#[allow(dead_code)]
pub fn draw_badge(buffer: &mut OptimizedBuffer, x: u32, y: u32, text: &str, fg: Rgba, bg: Rgba) {
    let badge = format!("[{text}]");
    buffer_draw_text(buffer, x, y, &badge, Style::fg(fg).with_bg(bg));
}

/// Format a thread count display
#[must_use]
#[allow(dead_code)]
pub fn format_thread_count(total: i64, open: i64) -> String {
    if total == 0 {
        "0".to_string()
    } else if open == 0 {
        format!("{total}")
    } else {
        format!("{open}/{total}")
    }
}

/// Truncate a path for display, keeping the filename visible
#[must_use]
pub fn truncate_path(path: &str, max_width: usize) -> String {
    if path.chars().count() <= max_width {
        return path.to_string();
    }

    // Try to keep the filename
    if let Some(idx) = path.rfind('/') {
        let filename = &path[idx + 1..];
        let filename_chars = filename.chars().count();
        if filename_chars + 2 <= max_width {
            // "\u{2026}/" + filename
            let available = max_width - filename_chars - 2;
            let prefix = take_tail_chars(&path[..idx], available);
            return format!("{prefix}\u{2026}/{filename}");
        }
    }

    // Just truncate from the end
    let truncated = take_chars(path, max_width.saturating_sub(1));
    format!("{truncated}…")
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

fn take_tail_chars(text: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text;
    }

    let skip = char_count - max_chars;
    for (count, (idx, _)) in text.char_indices().enumerate() {
        if count == skip {
            return &text[idx..];
        }
    }
    text
}

/// A line of content within a block.
pub struct BlockLine<'a> {
    pub text: &'a str,
    pub style: Style,
}

impl<'a> BlockLine<'a> {
    #[must_use]
    pub const fn new(text: &'a str, style: Style) -> Self {
        Self { text, style }
    }
}

/// Draw a block with vertical bar, margins, padding, and content lines.
///
/// Layout per row:
/// ```text
/// [side_margin bg] [bar ┃] [left_pad] content [right_pad] [side_margin bg]
/// ```
///
/// Returns total height consumed.
pub fn draw_block(
    buffer: &mut OptimizedBuffer,
    area: Rect,
    theme: &Theme,
    bg: Rgba,
    lines: &[BlockLine<'_>],
) -> u32 {
    use crate::layout::{
        BLOCK_LEFT_PAD, BLOCK_MARGIN, BLOCK_PADDING, BLOCK_RIGHT_PAD, BLOCK_SIDE_MARGIN,
    };

    let total_height = (BLOCK_MARGIN * 2 + BLOCK_PADDING * 2 + lines.len()) as u32;
    if area.height < total_height {
        return 0;
    }

    let mut y = area.y;

    let draw_margin_line = |buf: &mut OptimizedBuffer, y: u32| {
        buffer_fill_rect(buf, area.x, y, area.width, 1, theme.background);
    };

    let draw_bar_line = |buf: &mut OptimizedBuffer, y: u32| {
        // Side margins
        if BLOCK_SIDE_MARGIN > 0 {
            buffer_fill_rect(buf, area.x, y, BLOCK_SIDE_MARGIN, 1, theme.background);
            buffer_fill_rect(
                buf,
                area.x + area.width.saturating_sub(BLOCK_SIDE_MARGIN),
                y,
                BLOCK_SIDE_MARGIN,
                1,
                theme.background,
            );
        }
        // Content area fill
        let content_x = area.x + BLOCK_SIDE_MARGIN;
        let content_width = area.width.saturating_sub(BLOCK_SIDE_MARGIN * 2);
        buffer_fill_rect(buf, content_x, y, content_width, 1, bg);
        // Bar character
        buffer_draw_text(buf, content_x, y, "\u{2503}", theme.style_muted_on(bg));
    };

    // Top margin
    for _ in 0..BLOCK_MARGIN {
        draw_margin_line(buffer, y);
        y += 1;
    }
    // Top padding
    for _ in 0..BLOCK_PADDING {
        draw_bar_line(buffer, y);
        y += 1;
    }
    // Content lines
    let inner_x = area.x + BLOCK_SIDE_MARGIN + 1 + BLOCK_LEFT_PAD;
    let inner_width = area
        .width
        .saturating_sub(BLOCK_SIDE_MARGIN * 2 + 1 + BLOCK_LEFT_PAD + BLOCK_RIGHT_PAD);
    for line in lines {
        draw_bar_line(buffer, y);
        draw_text_truncated(
            buffer,
            inner_x,
            y,
            line.text,
            inner_width,
            line.style.with_bg(bg),
        );
        y += 1;
    }
    // Bottom padding
    for _ in 0..BLOCK_PADDING {
        draw_bar_line(buffer, y);
        y += 1;
    }
    // Bottom margin
    for _ in 0..BLOCK_MARGIN {
        draw_margin_line(buffer, y);
        y += 1;
    }

    total_height
}

/// Dim the cells in `area` by scaling both fg and bg colors.
pub fn dim_rect(buffer: &mut OptimizedBuffer, area: Rect, scale: f32) {
    for row in area.y..area.y + area.height {
        for col in area.x..area.x + area.width {
            buffer_dim_cell_rgb(buffer, col, row, scale);
        }
    }
}

/// A label + key hint for the help bar.
pub struct HotkeyHint {
    pub label: Cow<'static, str>,
    pub key: &'static str,
}

impl HotkeyHint {
    #[must_use]
    pub fn new(label: impl Into<Cow<'static, str>>, key: &'static str) -> Self {
        Self {
            label: label.into(),
            key,
        }
    }

    #[must_use]
    pub fn width(&self) -> usize {
        self.label.len() + 1 + self.key.len()
    }
}

/// Draw a right-aligned help bar of `[label key]` pairs within `area`.
///
/// The bar is drawn on the second-to-last row of `area`. The last row
/// is filled with `theme.background` as a bottom margin.
pub fn draw_help_bar(
    buffer: &mut OptimizedBuffer,
    area: Rect,
    theme: &Theme,
    hints: &[HotkeyHint],
) {
    draw_help_bar_ext(buffer, area, theme, hints, theme.background, "");
}

/// Draw a help bar with a custom background color.
pub fn draw_help_bar_with_bg(
    buffer: &mut OptimizedBuffer,
    area: Rect,
    theme: &Theme,
    hints: &[HotkeyHint],
    bg: Rgba,
) {
    draw_help_bar_ext(buffer, area, theme, hints, bg, "");
}

/// Draw a help bar with custom bg and an optional left-aligned label.
pub fn draw_help_bar_ext(
    buffer: &mut OptimizedBuffer,
    area: Rect,
    theme: &Theme,
    hints: &[HotkeyHint],
    bg: Rgba,
    left_label: &str,
) {
    let y = area.y + area.height.saturating_sub(2);
    let bottom_y = area.y + area.height.saturating_sub(1);
    buffer_fill_rect(buffer, area.x, bottom_y, area.width, 1, bg);
    buffer_fill_rect(buffer, area.x, y, area.width, 1, bg);

    let padding: u32 = 2;

    // Right-aligned hints
    let separator = "  ";
    let sep_len = separator.len();
    let total_width: usize = if hints.is_empty() {
        0
    } else {
        hints.iter().map(HotkeyHint::width).sum::<usize>() + hints.len().saturating_sub(1) * sep_len
    };

    let x_start = if area.width > 0 && !hints.is_empty() {
        if (total_width as u32) + padding <= area.width {
            area.x + area.width - total_width as u32 - padding
        } else {
            area.x + padding.min(area.width)
        }
    } else {
        area.x + area.width
    };

    // Left label (truncated to not overlap with hints)
    if !left_label.is_empty() {
        let label_x = area.x + padding;
        let max_width = x_start.saturating_sub(label_x + 1);
        if max_width > 0 {
            draw_text_truncated(
                buffer,
                label_x,
                y,
                left_label,
                max_width,
                theme.style_muted(),
            );
        }
    }

    let dim = theme.style_muted();
    let bright = theme.style_foreground();

    let mut x = x_start;
    for (i, hint) in hints.iter().enumerate() {
        if i > 0 {
            buffer_draw_text(buffer, x, y, separator, dim);
            x += sep_len as u32;
        }
        buffer_draw_text(buffer, x, y, &hint.label, dim);
        x += hint.label.len() as u32;
        buffer_draw_text(buffer, x, y, " ", dim);
        x += 1;
        buffer_draw_text(buffer, x, y, hint.key, bright);
        x += hint.key.len() as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_path;

    #[test]
    fn truncate_path_keeps_tail_directories() {
        let path = "crates/wraith-diff/src/render/some_file.rs";
        let truncated = truncate_path(path, 24);

        assert!(truncated.ends_with("some_file.rs"));
        assert!(truncated.contains("render") || truncated.contains("…/"));
    }
}

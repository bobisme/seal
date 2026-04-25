//! Minimal markdown rendering for review descriptions and comments.

use std::sync::OnceLock;

use crate::render_backend::{buffer_draw_text, color_lerp, OptimizedBuffer, Rgba, Style};
use crate::syntax::{HighlightSpan, Highlighter};
use crate::text::{wrap_text, wrap_text_preserve};
use crate::theme::Theme;

fn markdown_highlighter() -> &'static Highlighter {
    static HIGHLIGHTER: OnceLock<Highlighter> = OnceLock::new();
    HIGHLIGHTER.get_or_init(Highlighter::new)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarkdownStyle {
    Body,
    Heading,
    Quote,
    List,
    Code,
    CodeMeta,
}

impl MarkdownStyle {
    #[must_use]
    pub const fn style(self, theme: &Theme, bg: Rgba) -> Style {
        match self {
            Self::Body | Self::List | Self::Code => theme.style_foreground_on(bg),
            Self::Heading => theme.style_primary_on(bg).with_bold(),
            Self::Quote | Self::CodeMeta => theme.style_muted_on(bg),
        }
    }
}

#[must_use]
pub fn markdown_line_bg(theme: &Theme, base_bg: Rgba, style: MarkdownStyle) -> Rgba {
    match style {
        MarkdownStyle::Code | MarkdownStyle::CodeMeta => {
            color_lerp(base_bg, theme.background, 0.28)
        }
        _ => base_bg,
    }
}

#[derive(Clone, Debug)]
pub struct MarkdownSpan {
    pub text: String,
    pub bold: bool,
    pub code: bool,
}

#[derive(Clone, Debug)]
pub enum MarkdownContent {
    Text(String),
    Styled {
        spans: Vec<MarkdownSpan>,
        fallback: String,
    },
    Highlighted {
        spans: Vec<HighlightSpan>,
        fallback: String,
    },
}

#[derive(Clone, Debug)]
pub struct MarkdownLine {
    pub content: MarkdownContent,
    pub style: MarkdownStyle,
}

impl MarkdownLine {
    #[must_use]
    pub const fn plain(text: String, style: MarkdownStyle) -> Self {
        Self {
            content: MarkdownContent::Text(text),
            style,
        }
    }

    #[must_use]
    pub fn fallback_text(&self) -> &str {
        match &self.content {
            MarkdownContent::Text(text) => text,
            MarkdownContent::Styled { fallback, .. }
            | MarkdownContent::Highlighted { fallback, .. } => fallback,
        }
    }
}

#[must_use]
pub fn render_markdown(text: &str, max_width: usize) -> Vec<MarkdownLine> {
    render_markdown_with_highlighter(text, max_width, None)
}

#[must_use]
pub fn render_markdown_with_highlighter(
    text: &str,
    max_width: usize,
    highlighter: Option<&Highlighter>,
) -> Vec<MarkdownLine> {
    if max_width == 0 {
        return Vec::new();
    }

    let highlighter = match highlighter {
        Some(highlighter) => highlighter,
        None => markdown_highlighter(),
    };
    let mut code_highlighter = None;
    let mut in_code_block = false;
    let mut pending_blank_after_code = false;
    let mut lines = Vec::new();

    for raw_line in text.split('\n') {
        if pending_blank_after_code {
            if !raw_line.trim().is_empty() && !last_line_is_blank(&lines) {
                lines.push(MarkdownLine::plain(String::new(), MarkdownStyle::Body));
            }
            pending_blank_after_code = false;
        }

        let trimmed = raw_line.trim_end();
        if let Some(fence_info) = trimmed.strip_prefix("```") {
            if in_code_block {
                in_code_block = false;
                code_highlighter = None;
                pending_blank_after_code = true;
            } else {
                if !last_line_is_blank(&lines) {
                    lines.push(MarkdownLine::plain(String::new(), MarkdownStyle::Body));
                }
                in_code_block = true;
                let fence_info = fence_info.trim();
                code_highlighter =
                    highlighter.for_fence_info((!fence_info.is_empty()).then_some(fence_info));
                if !fence_info.is_empty() {
                    lines.push(MarkdownLine::plain(
                        format!("[{fence_info}]"),
                        MarkdownStyle::CodeMeta,
                    ));
                }
            }
            continue;
        }

        if in_code_block {
            push_code_line(&mut lines, raw_line, max_width, code_highlighter.as_mut());
            continue;
        }

        if raw_line.trim().is_empty() {
            lines.push(MarkdownLine::plain(String::new(), MarkdownStyle::Body));
            continue;
        }

        if let Some(heading) = parse_heading(raw_line) {
            push_wrapped_plain(
                &mut lines,
                &heading,
                max_width,
                MarkdownStyle::Heading,
                None,
            );
            continue;
        }

        if let Some((prefix, body, continuation)) = parse_list_item(raw_line) {
            push_wrapped_plain(
                &mut lines,
                &body,
                max_width,
                MarkdownStyle::List,
                Some((prefix, continuation)),
            );
            continue;
        }

        if let Some(body) = raw_line.trim_start().strip_prefix('>') {
            push_wrapped_plain(
                &mut lines,
                body.trim_start(),
                max_width,
                MarkdownStyle::Quote,
                Some(("> ".to_string(), "  ".to_string())),
            );
            continue;
        }

        push_wrapped_plain(&mut lines, raw_line, max_width, MarkdownStyle::Body, None);
    }

    lines
}

pub fn draw_markdown_content(
    buffer: &mut OptimizedBuffer,
    theme: &Theme,
    x: u32,
    y: u32,
    width: u32,
    bg: Rgba,
    content: &MarkdownContent,
    style: MarkdownStyle,
) {
    let max_chars = width as usize;
    match content {
        MarkdownContent::Text(text) => {
            let text = truncate_chars(text, max_chars);
            buffer_draw_text(buffer, x, y, text, style.style(theme, bg));
        }
        MarkdownContent::Styled { spans, fallback } => {
            if spans.is_empty() {
                let text = truncate_chars(fallback, max_chars);
                buffer_draw_text(buffer, x, y, text, style.style(theme, bg));
                return;
            }

            let mut col = x;
            let mut chars_drawn = 0usize;
            for span in spans {
                if chars_drawn >= max_chars {
                    break;
                }
                let remaining = max_chars - chars_drawn;
                let text = truncate_chars(&span.text, remaining);
                if text.is_empty() {
                    continue;
                }

                let mut span_style = if span.code {
                    theme.style_primary_on(bg)
                } else {
                    style.style(theme, bg)
                };
                if span.bold {
                    span_style = span_style.with_bold();
                }
                buffer_draw_text(buffer, col, y, text, span_style);
                let drawn = text.chars().count();
                col += drawn as u32;
                chars_drawn += drawn;
            }
        }
        MarkdownContent::Highlighted { spans, fallback } => {
            if spans.is_empty() {
                let text = truncate_chars(fallback, max_chars);
                buffer_draw_text(buffer, x, y, text, style.style(theme, bg));
                return;
            }

            let mut col = x;
            let mut chars_drawn = 0usize;
            for span in spans {
                if chars_drawn >= max_chars {
                    break;
                }
                let remaining = max_chars - chars_drawn;
                let text = truncate_chars(&span.text, remaining);
                if text.is_empty() {
                    continue;
                }

                let mut span_style = Style::fg(span.fg).with_bg(bg);
                if span.bold {
                    span_style = span_style.with_bold();
                }
                buffer_draw_text(buffer, col, y, text, span_style);
                let drawn = text.chars().count();
                col += drawn as u32;
                chars_drawn += drawn;
            }
        }
    }
}

fn push_code_line(
    out: &mut Vec<MarkdownLine>,
    raw_line: &str,
    max_width: usize,
    highlighter: Option<&mut crate::syntax::FileHighlighter<'_>>,
) {
    if let Some(highlighter) = highlighter {
        let spans = highlighter.highlight_line(raw_line);
        for wrapped in wrap_highlighted_line(&spans, max_width) {
            let fallback = wrapped
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>();
            out.push(MarkdownLine {
                content: MarkdownContent::Highlighted {
                    spans: wrapped,
                    fallback,
                },
                style: MarkdownStyle::Code,
            });
        }
        return;
    }

    for line in wrap_text_preserve(raw_line, max_width) {
        out.push(MarkdownLine::plain(line, MarkdownStyle::Code));
    }
}

fn push_wrapped_plain(
    out: &mut Vec<MarkdownLine>,
    text: &str,
    max_width: usize,
    style: MarkdownStyle,
    prefix: Option<(String, String)>,
) {
    let content = parse_inline_markdown(text);

    if let Some((first_prefix, continuation_prefix)) = prefix {
        let available = max_width
            .saturating_sub(first_prefix.chars().count())
            .max(1);
        let wrapped = wrap_markdown_content(&content, available);
        if wrapped.is_empty() {
            out.push(MarkdownLine::plain(first_prefix, style));
            return;
        }

        for (index, line) in wrapped.into_iter().enumerate() {
            let prefix = if index == 0 {
                first_prefix.as_str()
            } else {
                continuation_prefix.as_str()
            };
            out.push(MarkdownLine {
                content: prepend_prefix(line, prefix),
                style,
            });
        }
        return;
    }

    for line in wrap_markdown_content(&content, max_width) {
        out.push(MarkdownLine {
            content: line,
            style,
        });
    }
}

fn parse_inline_markdown(text: &str) -> MarkdownContent {
    if !text.contains('`') && !text.contains('*') && !text.contains('_') {
        return MarkdownContent::Text(text.to_string());
    }

    let mut spans = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    let mut bold = false;
    let mut emphasis = false;
    let mut code = false;

    while i < chars.len() {
        if i + 1 < chars.len()
            && chars[i] == '*'
            && chars[i + 1] == '*'
            && (bold || (!code && has_token_ahead(&chars, i + 2, "**")))
        {
            push_span(&mut spans, &mut current, bold || emphasis, code);
            bold = !bold;
            i += 2;
            continue;
        }

        if chars[i] == '`' && (code || has_char_ahead(&chars, i + 1, '`')) {
            push_span(&mut spans, &mut current, bold || emphasis, code);
            code = !code;
            i += 1;
            continue;
        }

        if !code
            && (chars[i] == '*' || chars[i] == '_')
            && (emphasis || has_char_ahead(&chars, i + 1, chars[i]))
        {
            push_span(&mut spans, &mut current, bold || emphasis, code);
            emphasis = !emphasis;
            i += 1;
            continue;
        }

        current.push(chars[i]);
        i += 1;
    }

    push_span(&mut spans, &mut current, bold || emphasis, code);
    if spans.is_empty() {
        return MarkdownContent::Text(text.to_string());
    }

    let fallback = spans
        .iter()
        .map(|span| span.text.as_str())
        .collect::<String>();
    let any_style = spans.iter().any(|span| span.bold || span.code);
    if any_style {
        MarkdownContent::Styled { spans, fallback }
    } else {
        MarkdownContent::Text(fallback)
    }
}

fn push_span(spans: &mut Vec<MarkdownSpan>, current: &mut String, bold: bool, code: bool) {
    if current.is_empty() {
        return;
    }

    spans.push(MarkdownSpan {
        text: std::mem::take(current),
        bold,
        code,
    });
}

fn has_token_ahead(chars: &[char], start: usize, token: &str) -> bool {
    let token_chars: Vec<char> = token.chars().collect();
    chars[start..]
        .windows(token_chars.len())
        .any(|window| window == token_chars.as_slice())
}

fn has_char_ahead(chars: &[char], start: usize, ch: char) -> bool {
    chars[start..].contains(&ch)
}

fn last_line_is_blank(lines: &[MarkdownLine]) -> bool {
    lines
        .last()
        .is_some_and(|line| line.fallback_text().trim().is_empty())
}

fn wrap_markdown_content(content: &MarkdownContent, max_width: usize) -> Vec<MarkdownContent> {
    match content {
        MarkdownContent::Text(text) => wrap_text(text, max_width)
            .into_iter()
            .map(MarkdownContent::Text)
            .collect(),
        MarkdownContent::Styled { spans, .. } => wrap_markdown_spans(spans, max_width)
            .into_iter()
            .map(|line_spans| {
                let fallback = line_spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>();
                MarkdownContent::Styled {
                    spans: line_spans,
                    fallback,
                }
            })
            .collect(),
        MarkdownContent::Highlighted { spans, .. } => wrap_highlighted_line(spans, max_width)
            .into_iter()
            .map(|line_spans| {
                let fallback = line_spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>();
                MarkdownContent::Highlighted {
                    spans: line_spans,
                    fallback,
                }
            })
            .collect(),
    }
}

fn prepend_prefix(content: MarkdownContent, prefix: &str) -> MarkdownContent {
    if prefix.is_empty() {
        return content;
    }

    match content {
        MarkdownContent::Text(text) => MarkdownContent::Text(format!("{prefix}{text}")),
        MarkdownContent::Styled {
            mut spans,
            fallback,
        } => {
            spans.insert(
                0,
                MarkdownSpan {
                    text: prefix.to_string(),
                    bold: false,
                    code: false,
                },
            );
            MarkdownContent::Styled {
                spans,
                fallback: format!("{prefix}{fallback}"),
            }
        }
        MarkdownContent::Highlighted { spans: _, fallback } => {
            MarkdownContent::Text(format!("{prefix}{fallback}"))
        }
    }
}

fn parse_heading(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|ch| *ch == '#').count();
    if hashes == 0 {
        return None;
    }

    let body = trimmed[hashes..].trim_start();
    if body.is_empty() {
        None
    } else {
        Some(body.to_string())
    }
}

fn parse_list_item(line: &str) -> Option<(String, String, String)> {
    let trimmed = line.trim_start();
    for marker in ["- ", "* ", "+ "] {
        if let Some(body) = trimmed.strip_prefix(marker) {
            return Some((
                marker.to_string(),
                body.trim_start().to_string(),
                "  ".to_string(),
            ));
        }
    }

    let digit_count = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digit_count == 0 {
        return None;
    }

    let suffix = &trimmed[digit_count..];
    let body = suffix.strip_prefix(". ")?;
    let prefix = &trimmed[..digit_count + 2];
    let continuation = " ".repeat(prefix.chars().count());
    Some((
        prefix.to_string(),
        body.trim_start().to_string(),
        continuation,
    ))
}

fn split_at_char(text: &str, max_chars: usize) -> (&str, &str) {
    if max_chars == 0 {
        return ("", text);
    }
    for (count, (idx, _)) in text.char_indices().enumerate() {
        if count == max_chars {
            return (&text[..idx], &text[idx..]);
        }
    }
    (text, "")
}

fn truncate_chars(text: &str, max_chars: usize) -> &str {
    split_at_char(text, max_chars).0
}

fn wrap_markdown_spans(spans: &[MarkdownSpan], max_width: usize) -> Vec<Vec<MarkdownSpan>> {
    if max_width == 0 {
        return Vec::new();
    }

    let tokens = tokenize_markdown_spans(spans);
    let mut lines: Vec<Vec<MarkdownSpan>> = Vec::new();
    let mut current: Vec<MarkdownSpan> = Vec::new();
    let mut width = 0usize;

    for token in tokens {
        if token.text.is_empty() {
            continue;
        }

        let token_width = token.text.chars().count();
        if token.is_whitespace {
            if current.is_empty() {
                continue;
            }
            if width + token_width <= max_width {
                width += push_markdown_piece(&mut current, &token.text, token.bold, token.code);
            } else if !current.is_empty() {
                trim_trailing_whitespace(&mut current);
                lines.push(current);
                current = Vec::new();
                width = 0;
            }
            continue;
        }

        let mut remaining = token.text.as_str();
        loop {
            if remaining.is_empty() {
                break;
            }

            let available = max_width.saturating_sub(width);
            if available == 0 {
                trim_trailing_whitespace(&mut current);
                lines.push(current);
                current = Vec::new();
                width = 0;
                continue;
            }

            let remaining_width = remaining.chars().count();
            if !current.is_empty() && remaining_width > available {
                trim_trailing_whitespace(&mut current);
                lines.push(current);
                current = Vec::new();
                width = 0;
                continue;
            }

            let piece = if remaining_width > max_width {
                truncate_chars(remaining, available.max(1))
            } else {
                remaining
            };

            width += push_markdown_piece(&mut current, piece, token.bold, token.code);
            remaining = &remaining[piece.len()..];

            if width >= max_width {
                trim_trailing_whitespace(&mut current);
                lines.push(current);
                current = Vec::new();
                width = 0;
            }
        }
    }

    trim_trailing_whitespace(&mut current);
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }

    lines
}

#[derive(Clone, Debug)]
struct MarkdownToken {
    text: String,
    bold: bool,
    code: bool,
    is_whitespace: bool,
}

fn tokenize_markdown_spans(spans: &[MarkdownSpan]) -> Vec<MarkdownToken> {
    let mut tokens = Vec::new();
    for span in spans {
        let mut current = String::new();
        let mut current_is_whitespace = None;

        for ch in span.text.chars() {
            let is_whitespace = ch.is_whitespace();
            match current_is_whitespace {
                Some(existing) if existing == is_whitespace => current.push(ch),
                Some(existing) => {
                    tokens.push(MarkdownToken {
                        text: std::mem::take(&mut current),
                        bold: span.bold,
                        code: span.code,
                        is_whitespace: existing,
                    });
                    current.push(ch);
                    current_is_whitespace = Some(is_whitespace);
                }
                None => {
                    current.push(ch);
                    current_is_whitespace = Some(is_whitespace);
                }
            }
        }

        if let Some(is_whitespace) = current_is_whitespace {
            tokens.push(MarkdownToken {
                text: current,
                bold: span.bold,
                code: span.code,
                is_whitespace,
            });
        }
    }
    tokens
}

fn push_markdown_piece(
    current: &mut Vec<MarkdownSpan>,
    text: &str,
    bold: bool,
    code: bool,
) -> usize {
    if text.is_empty() {
        return 0;
    }

    current.push(MarkdownSpan {
        text: text.to_string(),
        bold,
        code,
    });
    text.chars().count()
}

fn trim_trailing_whitespace(spans: &mut Vec<MarkdownSpan>) {
    while let Some(last) = spans.last_mut() {
        let trimmed = last.text.trim_end_matches(char::is_whitespace).to_string();
        if trimmed.is_empty() {
            spans.pop();
        } else {
            last.text = trimmed;
            break;
        }
    }
}

fn wrap_highlighted_line(spans: &[HighlightSpan], max_width: usize) -> Vec<Vec<HighlightSpan>> {
    if max_width == 0 {
        return Vec::new();
    }

    let mut lines: Vec<Vec<HighlightSpan>> = Vec::new();
    let mut current: Vec<HighlightSpan> = Vec::new();
    let mut width = 0usize;

    for span in spans {
        let mut remaining = span.text.as_str();
        while !remaining.is_empty() {
            let available = max_width.saturating_sub(width);
            if available == 0 {
                lines.push(current);
                current = Vec::new();
                width = 0;
                continue;
            }

            let (chunk, rest) = split_at_char(remaining, available);
            if !chunk.is_empty() {
                current.push(HighlightSpan {
                    text: chunk.to_string(),
                    fg: span.fg,
                    bold: span.bold,
                    italic: span.italic,
                });
                width += chunk.chars().count();
            }

            remaining = rest;
            if width >= max_width {
                lines.push(current);
                current = Vec::new();
                width = 0;
            }
        }
    }

    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_markdown_formats_code_fences() {
        let lines = render_markdown("Before\n```rust\nfn main() {}\n```\nAfter", 40);

        assert_eq!(lines.len(), 6);
        assert!(lines[1].fallback_text().is_empty());
        assert!(matches!(lines[2].style, MarkdownStyle::CodeMeta));
        assert!(matches!(
            lines[3].content,
            MarkdownContent::Highlighted { .. }
        ));
        assert!(lines[4].fallback_text().is_empty());
    }

    #[test]
    fn test_render_markdown_formats_lists() {
        let lines = render_markdown("- first item wraps nicely", 10);

        assert!(lines.len() >= 2);
        assert_eq!(lines[0].fallback_text(), "- first");
        assert!(lines[1].fallback_text().starts_with("  "));
    }

    #[test]
    fn test_render_markdown_formats_inline_strong_and_code() {
        let lines = render_markdown("Use `seal sync` for **rebuilds**", 80);

        match &lines[0].content {
            MarkdownContent::Styled { spans, fallback } => {
                assert_eq!(fallback, "Use seal sync for rebuilds");
                let code_text: String = spans
                    .iter()
                    .filter(|span| span.code)
                    .map(|span| span.text.as_str())
                    .collect();
                assert_eq!(code_text, "seal sync");
                assert!(spans
                    .iter()
                    .any(|span| span.bold && span.text == "rebuilds"));
            }
            other => panic!("expected styled content, got {other:?}"),
        }
    }

    #[test]
    fn test_render_markdown_wraps_styled_content_on_word_boundaries() {
        let lines = render_markdown("alpha **beta** gamma", 10);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].fallback_text(), "alpha beta");
        assert_eq!(lines[1].fallback_text(), "gamma");
    }

    #[test]
    fn test_code_lines_use_darker_background() {
        let theme = Theme::default();
        let code_bg = markdown_line_bg(&theme, theme.panel_bg, MarkdownStyle::Code);

        assert_ne!(code_bg, theme.panel_bg);
    }
}

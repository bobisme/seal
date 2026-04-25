//! Syntax highlighting module using syntect
//!
//! Provides syntax highlighting for code displayed in diffs and file views.
//! Integrates with the theme system for consistent colors.

use std::path::Path;
use std::str::FromStr;

use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color, FontStyle, ScopeSelectors, StyleModifier, Theme as SyntectTheme, ThemeItem, ThemeSet,
    ThemeSettings,
};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use crate::render_backend::{color_from_hex, Rgba};
use crate::theme::Theme;

/// Highlighted text span with color information
#[derive(Debug, Clone)]
pub struct HighlightSpan {
    pub text: String,
    pub fg: Rgba,
    pub bold: bool,
    pub italic: bool,
}

/// Syntax highlighter with loaded syntaxes and theme
pub struct Highlighter {
    syntax_set: SyntaxSet,
    theme: SyntectTheme,
}

impl Highlighter {
    /// Create a new highlighter with the default theme.
    ///
    /// # Panics
    ///
    /// Panics if the bundled syntect theme set contains no themes.
    #[must_use]
    pub fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();

        // Use base16-ocean.dark as default (similar to Tokyo Night)
        let theme = theme_set
            .themes
            .get("base16-ocean.dark")
            .cloned()
            .unwrap_or_else(|| {
                theme_set
                    .themes
                    .values()
                    .next()
                    .expect("bundled syntect theme set is non-empty")
                    .clone()
            });

        Self { syntax_set, theme }
    }

    /// Create a highlighter with a specific syntect theme name.
    ///
    /// # Panics
    ///
    /// Panics if the bundled syntect theme set contains no themes.
    #[must_use]
    pub fn with_theme(theme_name: &str) -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();

        let theme = theme_set
            .themes
            .get(theme_name)
            .cloned()
            .unwrap_or_else(|| {
                theme_set
                    .themes
                    .get("base16-ocean.dark")
                    .cloned()
                    .unwrap_or_else(|| {
                        theme_set
                            .themes
                            .values()
                            .next()
                            .expect("bundled syntect theme set is non-empty")
                            .clone()
                    })
            });

        Self { syntax_set, theme }
    }

    /// Create a highlighter using the active UI theme's syntax colors.
    #[must_use]
    pub fn from_ui_theme(theme: &Theme) -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme = syntect_theme_from_ui_theme(theme);
        Self { syntax_set, theme }
    }

    /// Get syntax reference for a file path (by extension)
    fn syntax_for_path(&self, path: &str) -> Option<&SyntaxReference> {
        let path = Path::new(path);

        // First try by extension
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if let Some(syntax) = self.syntax_set.find_syntax_by_extension(ext) {
                return Some(syntax);
            }
        }

        // Try by filename (for things like Makefile, Dockerfile)
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            // Check common filenames
            match name {
                "Makefile" | "makefile" | "GNUmakefile" => {
                    return self.syntax_set.find_syntax_by_extension("make");
                }
                "Dockerfile" => {
                    return self.syntax_set.find_syntax_by_extension("dockerfile");
                }
                "Cargo.toml" | "Cargo.lock" => {
                    return self.syntax_set.find_syntax_by_extension("toml");
                }
                _ => {}
            }
        }

        None
    }

    /// Highlight a single line of code, returning spans with colors
    ///
    /// Returns None if the syntax couldn't be determined or highlighting failed.
    pub fn highlight_line(&self, line: &str, file_path: &str) -> Option<Vec<HighlightSpan>> {
        let syntax = self.syntax_for_path(file_path)?;
        let mut highlighter = HighlightLines::new(syntax, &self.theme);

        let ranges = highlighter.highlight_line(line, &self.syntax_set).ok()?;

        Some(
            ranges
                .into_iter()
                .map(|(style, text)| HighlightSpan {
                    text: text.to_string(),
                    fg: syntect_color_to_rgba(style.foreground),
                    bold: style.font_style.contains(FontStyle::BOLD),
                    italic: style.font_style.contains(FontStyle::ITALIC),
                })
                .collect(),
        )
    }

    /// Create a stateful highlighter for a file (maintains state across lines)
    #[must_use]
    pub fn for_file(&self, file_path: &str) -> Option<FileHighlighter<'_>> {
        let syntax = self.syntax_for_path(file_path)?;
        Some(FileHighlighter {
            highlighter: HighlightLines::new(syntax, &self.theme),
            syntax_set: &self.syntax_set,
        })
    }

    /// Create a stateful highlighter for a fenced markdown code block.
    #[must_use]
    pub fn for_fence_info(&self, fence_info: Option<&str>) -> Option<FileHighlighter<'_>> {
        let language = fence_info?.split_whitespace().next()?;
        let lower = language.to_ascii_lowercase();
        let path_hint = match lower.as_str() {
            "rust" | "rs" => "snippet.rs".to_string(),
            "python" | "py" => "snippet.py".to_string(),
            "javascript" | "js" => "snippet.js".to_string(),
            "typescript" | "ts" => "snippet.ts".to_string(),
            "tsx" => "snippet.tsx".to_string(),
            "jsx" => "snippet.jsx".to_string(),
            "json" => "snippet.json".to_string(),
            "toml" => "snippet.toml".to_string(),
            "yaml" | "yml" => "snippet.yaml".to_string(),
            "shell" | "sh" | "bash" | "zsh" => "snippet.sh".to_string(),
            "diff" | "patch" => "snippet.diff".to_string(),
            "html" => "snippet.html".to_string(),
            "css" => "snippet.css".to_string(),
            "sql" => "snippet.sql".to_string(),
            "markdown" | "md" => "snippet.md".to_string(),
            _ => format!("snippet.{lower}"),
        };

        self.for_file(&path_hint)
    }

    /// List available theme names
    #[must_use]
    pub fn available_themes() -> Vec<&'static str> {
        vec![
            "base16-ocean.dark",
            "base16-eighties.dark",
            "base16-mocha.dark",
            "base16-ocean.light",
            "InspiredGitHub",
            "Solarized (dark)",
            "Solarized (light)",
        ]
    }
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
}

fn syntect_theme_from_ui_theme(theme: &Theme) -> SyntectTheme {
    SyntectTheme {
        name: Some(format!("{}-syntax", theme.name)),
        author: None,
        settings: ThemeSettings {
            foreground: Some(rgba_to_syntect_color(theme.foreground)),
            background: Some(rgba_to_syntect_color(theme.background)),
            caret: Some(rgba_to_syntect_color(theme.cursor)),
            ..ThemeSettings::default()
        },
        scopes: vec![
            scope_item("comment", theme.syntax.comment, Some(FontStyle::ITALIC)),
            scope_item(
                "keyword, storage.modifier",
                theme.syntax.keyword,
                Some(FontStyle::BOLD),
            ),
            scope_item(
                "entity.name.function, support.function, meta.function-call, variable.function",
                theme.syntax.function,
                None,
            ),
            scope_item(
                "entity.name.type, storage.type, support.type",
                theme.syntax.type_name,
                None,
            ),
            scope_item("string", theme.syntax.string, None),
            scope_item("constant.numeric", theme.syntax.number, None),
            scope_item("keyword.operator", theme.syntax.operator, None),
            scope_item("punctuation", theme.syntax.punctuation, None),
            scope_item(
                "constant.language, constant.character.escape, constant.other",
                theme.syntax.constant,
                None,
            ),
            scope_item(
                "entity.other.attribute-name, meta.attribute, punctuation.definition.attribute",
                theme.syntax.attribute,
                None,
            ),
            scope_item("variable, support.variable", theme.syntax.variable, None),
        ],
    }
}

fn scope_item(selector: &str, color: Rgba, font_style: Option<FontStyle>) -> ThemeItem {
    ThemeItem {
        scope: ScopeSelectors::from_str(selector).expect("valid syntax scope selector"),
        style: StyleModifier {
            foreground: Some(rgba_to_syntect_color(color)),
            background: None,
            font_style,
        },
    }
}

fn rgba_to_syntect_color(color: Rgba) -> Color {
    Color {
        r: (color.r * 255.0).round() as u8,
        g: (color.g * 255.0).round() as u8,
        b: (color.b * 255.0).round() as u8,
        a: (color.a * 255.0).round() as u8,
    }
}

/// Stateful highlighter for a single file
///
/// This maintains state across lines, which is important for multi-line
/// constructs like strings and comments.
pub struct FileHighlighter<'a> {
    highlighter: HighlightLines<'a>,
    syntax_set: &'a SyntaxSet,
}

impl FileHighlighter<'_> {
    /// Highlight the next line, maintaining state from previous lines
    pub fn highlight_line(&mut self, line: &str) -> Vec<HighlightSpan> {
        self.highlighter
            .highlight_line(line, self.syntax_set)
            .map_or_else(
                |_| {
                    vec![HighlightSpan {
                        text: line.to_string(),
                        fg: Rgba::WHITE,
                        bold: false,
                        italic: false,
                    }]
                },
                |ranges| {
                    ranges
                        .into_iter()
                        .map(|(style, text)| HighlightSpan {
                            text: text.to_string(),
                            fg: syntect_color_to_rgba(style.foreground),
                            bold: style.font_style.contains(FontStyle::BOLD),
                            italic: style.font_style.contains(FontStyle::ITALIC),
                        })
                        .collect()
                },
            )
    }
}

/// Convert syntect `Color` to backend `Rgba`.
fn syntect_color_to_rgba(color: Color) -> Rgba {
    Rgba::new(
        f32::from(color.r) / 255.0,
        f32::from(color.g) / 255.0,
        f32::from(color.b) / 255.0,
        f32::from(color.a) / 255.0,
    )
}

/// Syntax theme colors that integrate with the UI theme
///
/// These can be customized per-theme to ensure syntax colors
/// look good with the theme's background and other colors.
#[derive(Debug, Clone)]
pub struct SyntaxColors {
    /// Keywords (if, else, fn, let, etc.)
    pub keyword: Rgba,
    /// Function/method names
    pub function: Rgba,
    /// Type names (String, Vec, etc.)
    pub type_name: Rgba,
    /// String literals
    pub string: Rgba,
    /// Number literals
    pub number: Rgba,
    /// Comments
    pub comment: Rgba,
    /// Operators (+, -, =, etc.)
    pub operator: Rgba,
    /// Punctuation (brackets, semicolons, etc.)
    pub punctuation: Rgba,
    /// Variables and identifiers
    pub variable: Rgba,
    /// Constants and statics
    pub constant: Rgba,
    /// Attributes/decorators (@, #[])
    pub attribute: Rgba,
}

impl Default for SyntaxColors {
    fn default() -> Self {
        Self::tokyo_night()
    }
}

impl SyntaxColors {
    /// Tokyo Night inspired syntax colors
    #[must_use]
    pub fn tokyo_night() -> Self {
        Self {
            keyword: color_from_hex("#bb9af7").unwrap_or(Rgba::WHITE), // purple
            function: color_from_hex("#7aa2f7").unwrap_or(Rgba::WHITE), // blue
            type_name: color_from_hex("#2ac3de").unwrap_or(Rgba::WHITE), // cyan
            string: color_from_hex("#9ece6a").unwrap_or(Rgba::WHITE),  // green
            number: color_from_hex("#ff9e64").unwrap_or(Rgba::WHITE),  // orange
            comment: color_from_hex("#565f89").unwrap_or(Rgba::WHITE), // gray
            operator: color_from_hex("#89ddff").unwrap_or(Rgba::WHITE), // light cyan
            punctuation: color_from_hex("#a9b1d6").unwrap_or(Rgba::WHITE), // light gray
            variable: color_from_hex("#c0caf5").unwrap_or(Rgba::WHITE), // foreground
            constant: color_from_hex("#ff9e64").unwrap_or(Rgba::WHITE), // orange
            attribute: color_from_hex("#bb9af7").unwrap_or(Rgba::WHITE), // purple
        }
    }

    /// Light theme syntax colors
    #[must_use]
    pub fn light() -> Self {
        Self {
            keyword: color_from_hex("#5c21a5").unwrap_or(Rgba::BLACK), // purple
            function: color_from_hex("#0550ae").unwrap_or(Rgba::BLACK), // blue
            type_name: color_from_hex("#0969da").unwrap_or(Rgba::BLACK), // cyan
            string: color_from_hex("#0a3069").unwrap_or(Rgba::BLACK),  // dark blue
            number: color_from_hex("#953800").unwrap_or(Rgba::BLACK),  // orange
            comment: color_from_hex("#6e7781").unwrap_or(Rgba::BLACK), // gray
            operator: color_from_hex("#0550ae").unwrap_or(Rgba::BLACK), // blue
            punctuation: color_from_hex("#24292f").unwrap_or(Rgba::BLACK), // dark
            variable: color_from_hex("#24292f").unwrap_or(Rgba::BLACK), // foreground
            constant: color_from_hex("#953800").unwrap_or(Rgba::BLACK), // orange
            attribute: color_from_hex("#5c21a5").unwrap_or(Rgba::BLACK), // purple
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_highlight_rust() {
        let highlighter = Highlighter::new();
        let spans = highlighter
            .highlight_line("fn main() {", "test.rs")
            .expect("Should highlight Rust");

        assert!(!spans.is_empty());
        // Should have multiple spans for different tokens
        assert!(spans.len() > 1);
    }

    #[test]
    fn test_highlight_python() {
        let highlighter = Highlighter::new();
        let spans = highlighter
            .highlight_line("def hello():", "test.py")
            .expect("Should highlight Python");

        assert!(!spans.is_empty());
    }

    #[test]
    fn test_file_highlighter_state() {
        let highlighter = Highlighter::new();
        let mut file_hl = highlighter
            .for_file("test.rs")
            .expect("Should get highlighter");

        // Multi-line string should maintain state
        let spans1 = file_hl.highlight_line("let s = \"hello");
        let spans2 = file_hl.highlight_line("world\";");

        assert!(!spans1.is_empty());
        assert!(!spans2.is_empty());
    }

    #[test]
    fn test_highlight_has_different_colors() {
        let highlighter = Highlighter::new();
        let spans = highlighter
            .highlight_line("let x = 42;", "test.rs")
            .expect("Should highlight");

        // Print for debugging
        for span in &spans {
            eprintln!(
                "'{}' -> ({:.2}, {:.2}, {:.2})",
                span.text, span.fg.r, span.fg.g, span.fg.b
            );
        }

        // Different tokens should have different colors
        // "let" is a keyword, "42" is a number - they should differ
        assert!(spans.len() >= 2, "Should have multiple spans");

        // Find the "let" and "42" spans
        let let_span = spans.iter().find(|s| s.text.trim() == "let");
        let num_span = spans.iter().find(|s| s.text.trim() == "42");

        if let (Some(let_s), Some(num_s)) = (let_span, num_span) {
            // They should have different colors
            assert!(
                let_s.fg != num_s.fg,
                "Keyword and number should have different colors: let={:?}, 42={:?}",
                let_s.fg,
                num_s.fg
            );
        }
    }

    #[test]
    fn test_fence_info_maps_common_languages() {
        let highlighter = Highlighter::new();
        let mut file_hl = highlighter
            .for_fence_info(Some("rust"))
            .expect("Should resolve rust fence");

        let spans = file_hl.highlight_line("fn main() {}");
        assert!(!spans.is_empty());
    }
}

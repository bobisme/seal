//! seal-tui - GitHub-style code review TUI for seal
//!
//! Uses Elm Architecture (Model/Message/Update/View) with ftui rendering.
//! Replaces the standalone botseal-ui with direct `SealServices` integration.

#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::unnecessary_wraps)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::literal_string_with_formatting_args)]

pub mod command;
pub mod config;
pub mod core_client;
pub mod db;
pub mod diff;
pub mod input;
pub mod layout;
pub mod markdown;
pub mod message;
pub mod model;
pub mod render_backend;
pub mod stream;
pub mod syntax;
pub mod text;
pub mod theme;
pub mod update;
pub mod vcs;
pub mod view;

pub use core_client::CoreClient;
pub use db::SealClient;
pub use message::Message;
pub use model::{Focus, LayoutMode, Model, Screen};
pub use syntax::{HighlightSpan, Highlighter};
pub use theme::Theme;
pub use update::update;
pub use view::view;

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::{load_ui_config, save_ui_config};
use crate::input::map_event_to_message;
use crate::model::{CommentRequest, DiffViewMode, EditorRequest};
use crate::render_backend::{enable_raw_mode, Event, RawModeGuard, Renderer, RendererOptions};
use crate::render_backend::{event_from_ftui, rgba_to_packed, OptimizedBuffer};
use crate::render_backend::{
    Cell as OtCell, CellContent as OtCellContent, TextAttributes as OtTextAttributes,
};
use crate::stream::SIDE_BY_SIDE_MIN_WIDTH;
use crate::theme::{load_built_in_theme, load_theme_from_path};

use seal_core::core::SealServices;

use ftui_core::terminal_session::{SessionOptions as FtuiSessionOptions, TerminalSession};
use ftui_render::buffer::Buffer as FtuiBuffer;
use ftui_render::cell::{
    Cell as FtuiCell, CellAttrs as FtuiCellAttrs, CellContent as FtuiCellContent,
    StyleFlags as FtuiStyleFlags,
};
use ftui_render::diff::BufferDiff as FtuiBufferDiff;
use ftui_render::presenter::{Presenter as FtuiPresenter, TerminalCapabilities};

/// Run the TUI application with direct `SealServices` integration.
///
/// # Errors
///
/// Returns an error if the terminal cannot be initialized or an I/O error occurs.
pub fn run(repo_root: &Path, services: SealServices) -> Result<()> {
    let ctx = services.context().clone();
    let client: Box<dyn SealClient> = Box::new(CoreClient::new(ctx, repo_root));

    // Load theme
    let mut config = load_ui_config()?.unwrap_or_default();
    let theme_override = std::env::var("BOTSEAL_UI_THEME")
        .or_else(|_| std::env::var("BOTCRIT_UI_THEME"))
        .ok();
    let theme_selection = theme_override.clone().or_else(|| config.theme.clone());

    let default_theme =
        load_built_in_theme("default-dark").unwrap_or_else(|| crate::theme::ThemeLoadResult {
            theme: Theme::default(),
            syntax_theme: None,
        });

    let mut selected_builtin: Option<String> = None;
    let (theme, _syntax_theme) = if let Some(selection) = theme_selection {
        if let Some(loaded) = load_built_in_theme(&selection) {
            selected_builtin = Some(selection);
            (loaded.theme, loaded.syntax_theme)
        } else {
            let path = Path::new(&selection);
            if path.exists() {
                let loaded = load_theme_from_path(path)
                    .with_context(|| format!("Failed to load theme: {}", path.display()))?;
                (loaded.theme, loaded.syntax_theme)
            } else if theme_override.is_some() {
                anyhow::bail!("Unknown theme: {selection}");
            } else {
                (default_theme.theme, default_theme.syntax_theme)
            }
        }
    } else {
        (default_theme.theme, default_theme.syntax_theme)
    };

    if theme_override.is_some() {
        if let Some(name) = selected_builtin {
            config.theme = Some(name);
            save_ui_config(&config)?;
        }
    }

    // Initial terminal size
    let (width, height) = (80, 24);

    // Create model
    let mut model = Model::new(width, height, config);
    model.theme = theme;
    model.highlighter = Highlighter::from_ui_theme(&model.theme);

    apply_default_diff_view(&mut model);

    // Store repo path for display in header
    model.repo_path = Some(repo_root.display().to_string());

    // Load initial data
    model.reviews = client.list_reviews(None).unwrap_or_default();

    // Initialize renderer
    let options = RendererOptions {
        use_alt_screen: false,
        hide_cursor: false,
        enable_mouse: false,
        query_capabilities: false,
    };
    let mut renderer = Renderer::new_with_options(width.into(), height.into(), options)
        .context("Failed to initialize renderer")?;
    let mut wrap_guard = Some(AutoWrapGuard::new().context("Failed to disable line wrap")?);
    let mut cursor_guard = Some(CursorGuard::new().context("Failed to hide cursor")?);
    renderer.set_background(model.theme.background);

    let mut raw_guard: Option<RawModeGuard> = None;
    let mut ftui_presenter = FtuiPresenter::new(std::io::stdout(), TerminalCapabilities::detect());
    let mut ftui_prev = FtuiBuffer::new(width, height);
    let mut ftui_next = FtuiBuffer::new(width, height);
    let mut terminal_session = Some(
        TerminalSession::new(FtuiSessionOptions {
            alternate_screen: true,
            mouse_capture: true,
            bracketed_paste: true,
            focus_events: true,
            ..Default::default()
        })
        .context("Failed to initialize ftui terminal session")?,
    );
    terminal_session
        .as_ref()
        .expect("ftui session initialized")
        .hide_cursor()
        .context("Failed to hide cursor via ftui terminal session")?;
    if let Ok((term_width, term_height)) = terminal_session
        .as_ref()
        .expect("ftui session initialized")
        .size()
    {
        if term_width != model.width || term_height != model.height {
            model.resize(term_width, term_height);
            renderer
                .resize(term_width.into(), term_height.into())
                .context("Failed to apply initial ftui terminal size")?;
            ftui_prev = FtuiBuffer::new(term_width, term_height);
            ftui_next = FtuiBuffer::new(term_width, term_height);
        }
    }

    let repo_path = Some(repo_root.to_path_buf());

    // Main loop
    loop {
        renderer.invalidate();
        model.needs_redraw = false;

        renderer.clear();
        view(&model, renderer.buffer());
        bridge_buffer_to_ftui(renderer.buffer(), &mut ftui_next);
        let diff = FtuiBufferDiff::compute(&ftui_prev, &ftui_next);
        ftui_presenter
            .present(&ftui_next, &diff)
            .context("Failed to present ftui frame")?;
        ftui_presenter
            .hide_cursor()
            .context("Failed to keep cursor hidden")?;
        std::mem::swap(&mut ftui_prev, &mut ftui_next);

        if model.should_quit {
            break;
        }

        handle_data_loading(&mut model, client.as_ref(), repo_path.as_deref());

        // Poll for input
        let polled = terminal_session
            .as_ref()
            .expect("ftui session available")
            .poll_event(Duration::from_millis(100))
            .context("Failed polling ftui terminal events")?;
        if polled {
            let ft_event = terminal_session
                .as_ref()
                .expect("ftui session available")
                .read_event()
                .context("Failed reading ftui terminal event")?;
            if let Some(ft_event) = ft_event {
                if let Some(event) = event_from_ftui(ft_event) {
                    let resized_to = if let Event::Resize(resize) = &event {
                        Some((resize.width, resize.height))
                    } else {
                        None
                    };
                    process_event(
                        &event,
                        &mut model,
                        &mut EventContext {
                            renderer: &mut renderer,
                            raw_guard: &mut raw_guard,
                            wrap_guard: &mut wrap_guard,
                            cursor_guard: &mut cursor_guard,
                            client: Some(client.as_ref()),
                            repo_path: repo_path.as_deref(),
                            options,
                            terminal_session: &mut terminal_session,
                        },
                    )?;
                    if let Some((width, height)) = resized_to {
                        ftui_prev = FtuiBuffer::new(width, height);
                        ftui_next = FtuiBuffer::new(width, height);
                    }
                }
            }
        }
    }

    Ok(())
}

fn bridge_buffer_to_ftui(src: &OptimizedBuffer, dst: &mut FtuiBuffer) {
    let width = src.width().min(u32::from(dst.width())) as u16;
    let height = src.height().min(u32::from(dst.height())) as u16;
    dst.clear();

    for y in 0..height {
        for x in 0..width {
            if let Some(cell) = src.get(u32::from(x), u32::from(y)) {
                dst.set_raw(x, y, convert_backend_cell(cell));
            }
        }
    }
}

fn convert_backend_cell(cell: &OtCell) -> FtuiCell {
    let mut flags = FtuiStyleFlags::empty();
    if cell.attributes.contains(OtTextAttributes::BOLD) {
        flags |= FtuiStyleFlags::BOLD;
    }
    if cell.attributes.contains(OtTextAttributes::DIM) {
        flags |= FtuiStyleFlags::DIM;
    }
    if cell.attributes.contains(OtTextAttributes::ITALIC) {
        flags |= FtuiStyleFlags::ITALIC;
    }
    if cell.attributes.contains(OtTextAttributes::UNDERLINE) {
        flags |= FtuiStyleFlags::UNDERLINE;
    }
    if cell.attributes.contains(OtTextAttributes::BLINK) {
        flags |= FtuiStyleFlags::BLINK;
    }
    if cell.attributes.contains(OtTextAttributes::INVERSE) {
        flags |= FtuiStyleFlags::REVERSE;
    }
    if cell.attributes.contains(OtTextAttributes::HIDDEN) {
        flags |= FtuiStyleFlags::HIDDEN;
    }
    if cell.attributes.contains(OtTextAttributes::STRIKETHROUGH) {
        flags |= FtuiStyleFlags::STRIKETHROUGH;
    }
    let attrs = FtuiCellAttrs::new(
        flags,
        cell.attributes
            .link_id()
            .unwrap_or(FtuiCellAttrs::LINK_ID_NONE),
    );

    let content = match cell.content {
        OtCellContent::Char(c) => FtuiCellContent::from_char(c),
        OtCellContent::Empty => FtuiCellContent::EMPTY,
        OtCellContent::Continuation => FtuiCellContent::CONTINUATION,
        OtCellContent::Grapheme(_) => FtuiCellContent::from_char('\u{FFFD}'),
    };

    FtuiCell {
        content,
        fg: rgba_to_packed(cell.fg),
        bg: rgba_to_packed(cell.bg),
        attrs,
    }
}

struct EventContext<'a> {
    renderer: &'a mut Renderer,
    raw_guard: &'a mut Option<RawModeGuard>,
    wrap_guard: &'a mut Option<AutoWrapGuard>,
    cursor_guard: &'a mut Option<CursorGuard>,
    client: Option<&'a dyn SealClient>,
    repo_path: Option<&'a Path>,
    options: RendererOptions,
    terminal_session: &'a mut Option<TerminalSession>,
}

fn process_event(event: &Event, model: &mut Model, ctx: &mut EventContext<'_>) -> Result<()> {
    let msg = map_event_to_message(model, event);
    let resize = if let Message::Resize { width, height } = &msg {
        Some((*width, *height))
    } else {
        None
    };
    update(model, msg);

    if let Some((width, height)) = resize {
        ctx.renderer
            .resize(width.into(), height.into())
            .context("Failed to resize renderer")?;
        model.needs_redraw = true;
    }

    if let Some(request) = model.pending_editor_request.take() {
        ctx.terminal_session.take();
        let (prev_width, prev_height) = ctx.renderer.size();
        let prev_width = prev_width as u16;
        let prev_height = prev_height as u16;
        drop(std::mem::replace(
            ctx.renderer,
            Renderer::new_with_options(1, 1, ctx.options).expect("renderer"),
        ));
        ctx.raw_guard.take();
        ctx.wrap_guard.take();
        ctx.cursor_guard.take();

        let _ = open_file_in_editor(ctx.repo_path, request);

        *ctx.raw_guard = Some(enable_raw_mode().context("Failed to enable raw mode")?);
        let (width, height) = {
            let session = TerminalSession::new(FtuiSessionOptions {
                alternate_screen: true,
                mouse_capture: true,
                bracketed_paste: true,
                focus_events: true,
                ..Default::default()
            })
            .context("Failed to reinitialize ftui terminal session")?;
            session
                .hide_cursor()
                .context("Failed to hide cursor via ftui terminal session")?;
            let size = session.size().unwrap_or((prev_width, prev_height));
            *ctx.terminal_session = Some(session);
            size
        };
        *ctx.renderer = Renderer::new_with_options(width.into(), height.into(), ctx.options)
            .context("Failed to initialize renderer")?;
        ctx.renderer.set_background(model.theme.background);
        *ctx.wrap_guard = Some(AutoWrapGuard::new().context("Failed to disable line wrap")?);
        *ctx.cursor_guard = Some(CursorGuard::new().context("Failed to hide cursor")?);
        model.resize(width, height);
        model.needs_redraw = true;
        ctx.renderer.invalidate();
    }

    if let Some(request) = model.pending_comment_request.take() {
        ctx.terminal_session.take();
        let (prev_width, prev_height) = ctx.renderer.size();
        let prev_width = prev_width as u16;
        let prev_height = prev_height as u16;
        drop(std::mem::replace(
            ctx.renderer,
            Renderer::new_with_options(1, 1, ctx.options).expect("renderer"),
        ));
        ctx.raw_guard.take();
        ctx.wrap_guard.take();
        ctx.cursor_guard.take();

        let comment_result = run_comment_editor(ctx.repo_path, &request);

        if let Ok(Some(body)) = &comment_result {
            if let Some(client) = ctx.client.as_ref() {
                let persist_result = persist_comment(*client, ctx.repo_path, &request, body);
                if persist_result.is_ok() {
                    reload_review_data(model, *client, ctx.repo_path);
                }
            }
        }

        *ctx.raw_guard = Some(enable_raw_mode().context("Failed to enable raw mode")?);
        let (width, height) = {
            let session = TerminalSession::new(FtuiSessionOptions {
                alternate_screen: true,
                mouse_capture: true,
                bracketed_paste: true,
                focus_events: true,
                ..Default::default()
            })
            .context("Failed to reinitialize ftui terminal session")?;
            session
                .hide_cursor()
                .context("Failed to hide cursor via ftui terminal session")?;
            let size = session.size().unwrap_or((prev_width, prev_height));
            *ctx.terminal_session = Some(session);
            size
        };
        *ctx.renderer = Renderer::new_with_options(width.into(), height.into(), ctx.options)
            .context("Failed to initialize renderer")?;
        ctx.renderer.set_background(model.theme.background);
        *ctx.wrap_guard = Some(AutoWrapGuard::new().context("Failed to disable line wrap")?);
        *ctx.cursor_guard = Some(CursorGuard::new().context("Failed to hide cursor")?);
        model.resize(width, height);
        model.needs_redraw = true;
        ctx.renderer.invalidate();
    }

    // Handle inline editor submission
    if let Some(submission) = model.pending_comment_submission.take() {
        if let Some(client) = ctx.client.as_ref() {
            let persist_result = persist_comment(
                *client,
                ctx.repo_path,
                &submission.request,
                &submission.body,
            );
            match persist_result {
                Ok(()) => reload_review_data(model, *client, ctx.repo_path),
                Err(e) => {
                    model.flash_message = Some(format!("Comment failed: {e}"));
                }
            }
        }
        model.needs_redraw = true;
    }

    Ok(())
}

struct AutoWrapGuard;

impl AutoWrapGuard {
    fn new() -> std::io::Result<Self> {
        let mut out = std::io::stdout();
        out.write_all(b"\x1b[?7l")?;
        out.flush()?;
        Ok(Self)
    }
}

impl Drop for AutoWrapGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[?7h");
        let _ = out.flush();
    }
}

struct CursorGuard;

impl CursorGuard {
    fn new() -> std::io::Result<Self> {
        let mut out = std::io::stdout();
        out.write_all(b"\x1b[?25l")?;
        out.flush()?;
        Ok(Self)
    }
}

impl Drop for CursorGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[?25h");
        let _ = out.flush();
    }
}

fn apply_default_diff_view(model: &mut Model) {
    if let Some(value) = model.config.default_diff_view.as_deref() {
        if let Some(mode) = parse_diff_view_mode(value) {
            model.diff_view_mode = mode;
        }
        return;
    }

    if should_default_side_by_side(model) {
        model.diff_view_mode = DiffViewMode::SideBySide;
    }
}

fn parse_diff_view_mode(value: &str) -> Option<DiffViewMode> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "unified" | "unify" | "uni" => Some(DiffViewMode::Unified),
        "side-by-side" | "side_by_side" | "sidebyside" | "sbs" => Some(DiffViewMode::SideBySide),
        _ => None,
    }
}

fn should_default_side_by_side(model: &Model) -> bool {
    let diff_pane_width = match model.layout_mode {
        LayoutMode::Full | LayoutMode::Compact => {
            if model.sidebar_visible {
                model
                    .width
                    .saturating_sub(model.layout_mode.sidebar_width())
            } else {
                model.width
            }
        }
        LayoutMode::Overlay | LayoutMode::Single => model.width,
    };

    u32::from(diff_pane_width) >= SIDE_BY_SIDE_MIN_WIDTH
}

fn open_file_in_editor(repo_path: Option<&Path>, request: EditorRequest) -> Result<()> {
    let Some(repo_root) = repo_path else {
        return Ok(());
    };

    let file_path = repo_root.join(&request.file_path);
    if !file_path.exists() {
        return Ok(());
    }

    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut cmd = Command::new(editor);
    if let Some(line) = request.line {
        cmd.arg(format!("+{line}"));
    }
    cmd.arg(file_path);
    let _ = cmd.status();
    Ok(())
}

fn run_comment_editor(
    _repo_path: Option<&Path>,
    request: &CommentRequest,
) -> Result<Option<String>> {
    use std::io::Read;

    let dir = std::env::temp_dir();
    let tmp_path = dir.join(format!("seal-comment-{}.md", std::process::id()));

    {
        let mut f =
            std::fs::File::create(&tmp_path).context("Failed to create temp file for comment")?;

        writeln!(f, "# File: {}", request.file_path)?;
        let line_range = match request.end_line {
            Some(end) if end != request.start_line => format!("{}-{}", request.start_line, end),
            _ => request.start_line.to_string(),
        };
        writeln!(f, "# Lines: {line_range}")?;
        if let Some(thread_id) = &request.thread_id {
            writeln!(f, "# Thread: {thread_id}")?;
        }
        if !request.existing_comments.is_empty() {
            writeln!(f, "#")?;
            writeln!(f, "# Existing comments:")?;
            for c in &request.existing_comments {
                writeln!(f, "# {}: {}", c.author, c.body)?;
            }
        }
        writeln!(f, "#")?;
        writeln!(
            f,
            "# Write your comment below. Lines starting with # are ignored."
        )?;
        writeln!(f, "# Save and exit to submit. Leave empty to cancel.")?;
        writeln!(f)?;
        f.flush()?;
    }

    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());

    let status = Command::new(&editor).arg(&tmp_path).status();

    let body = if let Ok(exit) = status {
        if exit.success() {
            let mut content = String::new();
            std::fs::File::open(&tmp_path)
                .and_then(|mut f| f.read_to_string(&mut content))
                .context("Failed to read temp file after editor")?;

            let body: String = content
                .lines()
                .filter(|line| !line.starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();

            if body.is_empty() {
                None
            } else {
                Some(body)
            }
        } else {
            None
        }
    } else {
        None
    };

    let _ = std::fs::remove_file(&tmp_path);

    Ok(body)
}

fn persist_comment(
    client: &dyn SealClient,
    _repo_path: Option<&Path>,
    request: &CommentRequest,
    body: &str,
) -> Result<()> {
    if let Some(thread_id) = &request.thread_id {
        client.reply(thread_id, body)?;
    } else {
        client.comment(
            &request.review_id,
            &request.file_path,
            request.start_line,
            request.end_line,
            body,
        )?;
    }
    Ok(())
}

/// Build file cache entries from data returned by seal.
fn populate_file_cache(model: &mut Model, files: Vec<crate::db::FileData>) {
    model.file_cache.clear();

    for file_data in files.into_iter().filter(|f| !f.path.starts_with(".seal/")) {
        let diff = file_data
            .diff
            .as_deref()
            .map(crate::diff::ParsedDiff::parse);

        let file_content = file_data.content.map(|c| crate::model::FileContent {
            lines: c.lines,
            start_line: c.start_line,
        });

        let (highlighted_lines, file_highlighted_lines) = compute_cache_highlights(
            diff.as_ref(),
            file_content.as_ref(),
            &file_data.path,
            &model.highlighter,
        );

        model.file_cache.insert(
            file_data.path,
            crate::model::FileCacheEntry {
                diff,
                file_content,
                highlighted_lines,
                file_highlighted_lines,
            },
        );
    }

    model.sync_active_file_cache();
}

pub(crate) fn rehighlight_file_cache(
    file_cache: &mut std::collections::HashMap<String, crate::model::FileCacheEntry>,
    highlighter: &Highlighter,
) {
    for (path, entry) in file_cache.iter_mut() {
        let (highlighted_lines, file_highlighted_lines) = compute_cache_highlights(
            entry.diff.as_ref(),
            entry.file_content.as_ref(),
            path,
            highlighter,
        );
        entry.highlighted_lines = highlighted_lines;
        entry.file_highlighted_lines = file_highlighted_lines;
    }
}

fn reload_review_data(model: &mut Model, client: &dyn SealClient, _repo_path: Option<&Path>) {
    let Some(review) = &model.current_review else {
        return;
    };
    let review_id = review.review_id.clone();
    if let Ok(Some(data)) = client.load_review_data(&review_id) {
        model.current_review = Some(data.detail);
        model.threads = data.threads;
        model.all_comments = data.comments;
        populate_file_cache(model, data.files);
    }
}

fn handle_data_loading(model: &mut Model, client: &dyn SealClient, _repo_path: Option<&Path>) {
    if model.screen == Screen::ReviewDetail && model.current_review.is_none() {
        let reviews = model.filtered_reviews();
        if let Some(review) = reviews.get(model.list_index) {
            let review_id = review.review_id.clone();
            if let Ok(Some(data)) = client.load_review_data(&review_id) {
                model.current_review = Some(data.detail);
                model.threads = data.threads;
                model.all_comments = data.comments;
                populate_file_cache(model, data.files);
            }
        }
    }

    if model.screen == Screen::ReviewDetail && model.current_review.is_some() {
        model.sync_active_file_cache();
    }

    ensure_default_expanded_thread(model);
}

fn ensure_default_expanded_thread(model: &mut Model) {
    if model.expanded_thread.is_some() {
        return;
    }

    if let Some(thread) = model.threads_for_current_file().first() {
        model.expanded_thread = Some(thread.thread_id.clone());
        return;
    }

    if let Some(thread) = model.threads.first() {
        model.expanded_thread = Some(thread.thread_id.clone());
    }
}

fn compute_diff_highlights(
    diff: &crate::diff::ParsedDiff,
    file_path: &str,
    highlighter: &Highlighter,
) -> Vec<Vec<HighlightSpan>> {
    let mut result = Vec::new();

    let Some(mut file_hl) = highlighter.for_file(file_path) else {
        return result;
    };

    for hunk in &diff.hunks {
        result.push(Vec::new());

        for line in &hunk.lines {
            let spans = file_hl.highlight_line(&line.content);
            result.push(spans);
        }
    }

    result
}

fn compute_file_highlights(
    lines: &[String],
    file_path: &str,
    highlighter: &Highlighter,
) -> Vec<Vec<HighlightSpan>> {
    let Some(mut file_hl) = highlighter.for_file(file_path) else {
        return Vec::new();
    };

    lines
        .iter()
        .map(|line| file_hl.highlight_line(line))
        .collect()
}

fn compute_cache_highlights(
    diff: Option<&crate::diff::ParsedDiff>,
    file_content: Option<&crate::model::FileContent>,
    file_path: &str,
    highlighter: &Highlighter,
) -> (Vec<Vec<HighlightSpan>>, Vec<Vec<HighlightSpan>>) {
    let highlighted_lines = if let Some(parsed) = diff {
        compute_diff_highlights(parsed, file_path, highlighter)
    } else if let Some(content) = file_content {
        compute_file_highlights(&content.lines, file_path, highlighter)
    } else {
        Vec::new()
    };

    let file_highlighted_lines = if diff.is_some() {
        if let Some(content) = file_content {
            compute_file_highlights(&content.lines, file_path, highlighter)
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    (highlighted_lines, file_highlighted_lines)
}

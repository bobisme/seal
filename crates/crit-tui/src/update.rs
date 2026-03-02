//! State update logic (Elm Architecture)

use crate::command::{command_id_to_message, get_commands};
use crate::layout::visible_stream_rows;
use crate::message::Message;
use crate::model::{
    CommentRequest, DiffViewMode, EditorRequest, Focus, InlineEditor, Model, PaletteMode,
    PendingCommentSubmission, ReviewFilter, Screen,
};
use crate::stream::{
    active_file_index, compute_stream_layout, file_scroll_offset, StreamLayoutParams,
};
use crate::{config, theme, Highlighter};

fn update_list_nav(model: &mut Model, msg: &Message) {
    match msg {
        Message::ListUp => {
            let count = model.filtered_reviews().len();
            if count > 0 && model.list_index > 0 {
                model.list_index -= 1;
                // Adjust scroll if needed
                if model.list_index < model.list_scroll {
                    model.list_scroll = model.list_index;
                }
            }
            model.needs_redraw = true;
        }

        Message::ListDown => {
            let count = model.filtered_reviews().len();
            if count > 0 && model.list_index < count - 1 {
                model.list_index += 1;
                // Adjust scroll if needed
                let visible = model.list_visible_height();
                if model.list_index >= model.list_scroll + visible {
                    model.list_scroll = model.list_index - visible + 1;
                }
            }
            model.needs_redraw = true;
        }

        Message::ListPageUp => {
            let visible = model.list_visible_height();
            model.list_index = model.list_index.saturating_sub(visible);
            model.list_scroll = model.list_scroll.saturating_sub(visible);
            model.needs_redraw = true;
        }

        Message::ListPageDown => {
            let count = model.filtered_reviews().len();
            let visible = model.list_visible_height();
            let max_index = count.saturating_sub(1);
            let max_scroll = count.saturating_sub(visible);

            model.list_index = (model.list_index + visible).min(max_index);
            model.list_scroll = (model.list_scroll + visible).min(max_scroll);
            model.needs_redraw = true;
        }

        Message::ListTop => {
            model.list_index = 0;
            model.list_scroll = 0;
            model.needs_redraw = true;
        }

        Message::ListBottom => {
            let count = model.filtered_reviews().len();
            if count > 0 {
                model.list_index = count - 1;
                let visible = model.list_visible_height();
                model.list_scroll = count.saturating_sub(visible);
            }
            model.needs_redraw = true;
        }
        _ => {}
    }
}

fn update_cursor(model: &mut Model, msg: &Message) {
    let stops = model.cursor_stops.borrow();

    match msg {
        Message::CursorDown => {
            // Jump to the next cursor stop after the current position
            if let Some(&next) = stops.iter().find(|&&s| s > model.diff_cursor) {
                drop(stops);
                model.diff_cursor = next;
            } else {
                drop(stops);
            }
        }
        Message::CursorUp => {
            // Jump to the previous cursor stop before the current position
            if let Some(&prev) = stops.iter().rev().find(|&&s| s < model.diff_cursor) {
                drop(stops);
                model.diff_cursor = prev;
            } else {
                drop(stops);
            }
        }
        Message::CursorTop => {
            if let Some(&first) = stops.first() {
                drop(stops);
                model.diff_cursor = first;
            } else {
                drop(stops);
                model.diff_cursor = 0;
            }
        }
        Message::CursorBottom => {
            if let Some(&last) = stops.last() {
                drop(stops);
                model.diff_cursor = last;
            } else {
                drop(stops);
                model.diff_cursor = model.max_stream_row.get().saturating_sub(1);
            }
        }
        _ => {
            drop(stops);
        }
    }

    center_cursor_scroll(model);
    update_active_file_from_scroll(model);
}

/// Center the viewport around the cursor position.
/// When at the top or bottom of the stream, clamps scroll appropriately.
fn center_cursor_scroll(model: &mut Model) {
    let visible = visible_stream_rows(model.height);
    if visible == 0 {
        return;
    }
    let half = visible / 2;
    model.diff_scroll = model.diff_cursor.saturating_sub(half);
    clamp_diff_scroll(model);
}

/// After a scroll operation, snap cursor to the nearest cursor stop.
fn snap_cursor_to_nearest_stop(model: &mut Model) {
    let stops = model.cursor_stops.borrow();
    if stops.is_empty() {
        return;
    }
    let pos = stops.partition_point(|&s| s <= model.diff_cursor);
    let candidate = if pos > 0 { stops[pos - 1] } else { stops[0] };
    drop(stops);
    model.diff_cursor = candidate;
}

fn update_scroll(model: &mut Model, msg: &Message) {
    let max_row = model.max_stream_row.get().saturating_sub(1);

    match msg {
        Message::ScrollUp => {
            model.diff_cursor = model.diff_cursor.saturating_sub(1);
            snap_cursor_to_nearest_stop(model);
        }
        Message::ScrollDown => {
            model.diff_cursor = (model.diff_cursor + 1).min(max_row);
            snap_cursor_to_nearest_stop(model);
        }
        Message::ScrollTop => {
            model.diff_cursor = 0;
            snap_cursor_to_nearest_stop(model);
        }
        Message::ScrollBottom => {
            model.diff_cursor = max_row;
            snap_cursor_to_nearest_stop(model);
        }
        Message::ScrollHalfPageUp => {
            let page = visible_stream_rows(model.height);
            let half = page.max(1) / 2;
            model.diff_cursor = model.diff_cursor.saturating_sub(half.max(1));
            snap_cursor_to_nearest_stop(model);
        }
        Message::ScrollHalfPageDown => {
            let page = visible_stream_rows(model.height);
            let half = page.max(1) / 2;
            model.diff_cursor = (model.diff_cursor + half.max(1)).min(max_row);
            snap_cursor_to_nearest_stop(model);
        }
        Message::ScrollTenUp => {
            model.diff_cursor = model.diff_cursor.saturating_sub(10);
            snap_cursor_to_nearest_stop(model);
        }
        Message::ScrollTenDown => {
            model.diff_cursor = (model.diff_cursor + 10).min(max_row);
            snap_cursor_to_nearest_stop(model);
        }
        Message::PageUp => {
            let page = visible_stream_rows(model.height);
            model.diff_cursor = model.diff_cursor.saturating_sub(page);
            snap_cursor_to_nearest_stop(model);
        }
        Message::PageDown => {
            let page = visible_stream_rows(model.height);
            model.diff_cursor = (model.diff_cursor + page).min(max_row);
            snap_cursor_to_nearest_stop(model);
        }
        _ => {}
    }
    center_cursor_scroll(model);
    update_active_file_from_scroll(model);
}

fn update_thread_nav(model: &mut Model, msg: Message) {
    match msg {
        Message::NextThread => {
            // Only navigate through threads visible in the diff
            let threads = model.visible_threads_for_current_file();
            if let Some(current) = &model.expanded_thread {
                // Find next thread after current
                if let Some(pos) = threads.iter().position(|t| &t.thread_id == current) {
                    if pos + 1 < threads.len() {
                        model.expanded_thread = Some(threads[pos + 1].thread_id.clone());
                    }
                } else {
                    // Current thread not in visible list, start from first
                    if let Some(first) = threads.first() {
                        model.expanded_thread = Some(first.thread_id.clone());
                    }
                }
            } else if let Some(first) = threads.first() {
                model.expanded_thread = Some(first.thread_id.clone());
            }
            center_on_thread(model);
            update_active_file_from_scroll(model);
        }

        Message::PrevThread => {
            // Only navigate through threads visible in the diff
            let threads = model.visible_threads_for_current_file();
            if let Some(current) = &model.expanded_thread {
                if let Some(pos) = threads.iter().position(|t| &t.thread_id == current) {
                    if pos > 0 {
                        model.expanded_thread = Some(threads[pos - 1].thread_id.clone());
                    }
                } else {
                    // Current thread not in visible list, start from last
                    if let Some(last) = threads.last() {
                        model.expanded_thread = Some(last.thread_id.clone());
                    }
                }
            } else if let Some(last) = threads.last() {
                model.expanded_thread = Some(last.thread_id.clone());
            }
            center_on_thread(model);
            update_active_file_from_scroll(model);
        }

        Message::ExpandThread(id) => {
            model.expanded_thread = Some(id);
            model.focus = Focus::ThreadExpanded;
            center_on_thread(model);
            update_active_file_from_scroll(model);
        }

        Message::CollapseThread => {
            model.expanded_thread = None;
            model.focus = Focus::DiffPane;
            update_active_file_from_scroll(model);
        }
        _ => {}
    }
}

fn update_command_palette(model: &mut Model, msg: Message) {
    match msg {
        Message::ShowCommandPalette => {
            model.command_palette_mode = PaletteMode::Commands;
            model.command_palette_commands = get_commands();
            model.command_palette_input.clear();
            model.command_palette_selection = 0;
            model.previous_focus = Some(model.focus);
            model.focus = Focus::CommandPalette;
            model.needs_redraw = true;
        }
        Message::HideCommandPalette => {
            // Revert theme preview if we were in theme picker mode
            if model.command_palette_mode == PaletteMode::Themes {
                if let Some(original) = model.pre_palette_theme.take() {
                    update(model, Message::ApplyTheme(original));
                }
            }
            model.command_palette_mode = PaletteMode::Commands;
            model.focus = model.previous_focus.take().unwrap_or(Focus::DiffPane);
            model.needs_redraw = true;
        }
        Message::CommandPaletteNext => {
            let count = match model.command_palette_mode {
                PaletteMode::Commands => model.command_palette_commands.len(),
                PaletteMode::Themes => filter_theme_names(&model.command_palette_input).len(),
            };
            if count > 0 {
                model.command_palette_selection = (model.command_palette_selection + 1) % count;
            }
            preview_selected_theme(model);
            model.needs_redraw = true;
        }
        Message::CommandPalettePrev => {
            let count = match model.command_palette_mode {
                PaletteMode::Commands => model.command_palette_commands.len(),
                PaletteMode::Themes => filter_theme_names(&model.command_palette_input).len(),
            };
            if count > 0 {
                model.command_palette_selection =
                    (model.command_palette_selection + count - 1) % count;
            }
            preview_selected_theme(model);
            model.needs_redraw = true;
        }
        Message::CommandPaletteUpdateInput(input) => {
            model.command_palette_input.push_str(&input);
            model.command_palette_selection = 0;
            if model.command_palette_mode == PaletteMode::Commands {
                model.command_palette_commands = filter_commands(&model.command_palette_input);
            }
            preview_selected_theme(model);
            model.needs_redraw = true;
        }
        Message::CommandPaletteInputBackspace => {
            model.command_palette_input.pop();
            model.command_palette_selection = 0;
            if model.command_palette_mode == PaletteMode::Commands {
                model.command_palette_commands = filter_commands(&model.command_palette_input);
            }
            preview_selected_theme(model);
            model.needs_redraw = true;
        }
        Message::CommandPaletteDeleteWord => {
            delete_last_word(&mut model.command_palette_input);
            model.command_palette_selection = 0;
            if model.command_palette_mode == PaletteMode::Commands {
                model.command_palette_commands = filter_commands(&model.command_palette_input);
            }
            preview_selected_theme(model);
            model.needs_redraw = true;
        }
        Message::CommandPaletteExecute => {
            match model.command_palette_mode {
                PaletteMode::Commands => {
                    let commands = model.command_palette_commands.clone();
                    if let Some(command) = commands.get(model.command_palette_selection) {
                        update(model, Message::HideCommandPalette);
                        let msg = command_id_to_message(command.id);
                        update(model, msg);
                    }
                }
                PaletteMode::Themes => {
                    let theme_names = filter_theme_names(&model.command_palette_input);
                    if let Some(name) = theme_names.get(model.command_palette_selection) {
                        let name = name.to_string();
                        // Clear saved theme so HideCommandPalette won't revert
                        model.pre_palette_theme = None;
                        update(model, Message::HideCommandPalette);
                        update(model, Message::ApplyTheme(name));
                    }
                }
            }
        }
        _ => {}
    }
}

fn update_comment(model: &mut Model, msg: Message) {
    match msg {
        Message::EnterCommentMode => {
            model.comment_input.clear();
            model.comment_target_line = None;
            model.focus = Focus::Commenting;
            model.needs_redraw = true;
        }
        Message::CommentInput(text) => {
            if let Some(editor) = &mut model.inline_editor {
                for c in text.chars() {
                    editor.insert_char(c);
                }
            }
        }
        Message::CommentInputBackspace => {
            if let Some(editor) = &mut model.inline_editor {
                editor.backspace();
            }
        }
        Message::CommentNewline => {
            if let Some(editor) = &mut model.inline_editor {
                editor.newline();
            }
        }
        Message::CommentCursorUp => {
            if let Some(editor) = &mut model.inline_editor {
                editor.cursor_up();
            }
        }
        Message::CommentCursorDown => {
            if let Some(editor) = &mut model.inline_editor {
                editor.cursor_down();
            }
        }
        Message::CommentCursorLeft => {
            if let Some(editor) = &mut model.inline_editor {
                editor.cursor_left();
            }
        }
        Message::CommentCursorRight => {
            if let Some(editor) = &mut model.inline_editor {
                editor.cursor_right();
            }
        }
        Message::CommentHome => {
            if let Some(editor) = &mut model.inline_editor {
                editor.home();
            }
        }
        Message::CommentEnd => {
            if let Some(editor) = &mut model.inline_editor {
                editor.end();
            }
        }
        Message::CommentWordLeft => {
            if let Some(editor) = &mut model.inline_editor {
                editor.word_left();
            }
        }
        Message::CommentWordRight => {
            if let Some(editor) = &mut model.inline_editor {
                editor.word_right();
            }
        }
        Message::CommentDeleteWord => {
            if let Some(editor) = &mut model.inline_editor {
                editor.delete_word();
            }
        }
        Message::CommentClearLine => {
            if let Some(editor) = &mut model.inline_editor {
                editor.clear_line();
            }
        }
        Message::SaveComment => {
            if let Some(editor) = model.inline_editor.take() {
                let body = editor.body();
                if !body.is_empty() {
                    model.pending_comment_submission = Some(PendingCommentSubmission {
                        request: editor.request,
                        body,
                    });
                }
            }
            model.visual_mode = false;
            model.focus = Focus::DiffPane;
        }
        Message::CancelComment => {
            model.inline_editor = None;
            model.comment_input.clear();
            model.comment_target_line = None;
            model.visual_mode = false;
            model.focus = Focus::DiffPane;
        }
        _ => {}
    }

    // Keep editor scroll in sync with cursor
    if let Some(editor) = &mut model.inline_editor {
        // Estimate viewport height (will be refined during render, but 6 is a safe default)
        editor.ensure_visible(6);
    }
    model.needs_redraw = true;
}

fn update_file_sidebar(model: &mut Model, msg: &Message) {
    match msg {
        Message::NextFile => {
            let items = model.sidebar_items();
            if !items.is_empty() && model.sidebar_index < items.len() - 1 {
                model.sidebar_index += 1;
                sync_file_index_from_sidebar(model);
                ensure_sidebar_visible(model);
            }
        }

        Message::PrevFile => {
            if model.sidebar_index > 0 {
                model.sidebar_index -= 1;
                sync_file_index_from_sidebar(model);
                ensure_sidebar_visible(model);
            }
        }

        Message::SidebarTop => {
            if !model.sidebar_items().is_empty() {
                model.sidebar_index = 0;
                sync_file_index_from_sidebar(model);
                ensure_sidebar_visible(model);
            }
        }

        Message::SidebarBottom => {
            let items = model.sidebar_items();
            if !items.is_empty() {
                model.sidebar_index = items.len() - 1;
                sync_file_index_from_sidebar(model);
                ensure_sidebar_visible(model);
            }
        }

        Message::SelectFile(idx) => {
            let file_count = model.files_with_threads().len();
            if *idx < file_count {
                model.focus = Focus::FileSidebar;
                if let Some(pos) = model
                    .sidebar_items()
                    .iter()
                    .position(|item| matches!(item, crate::model::SidebarItem::File { file_idx, .. } if *file_idx == *idx))
                {
                    model.sidebar_index = pos;
                }
                jump_to_file(model, *idx);
                ensure_sidebar_visible(model);
            }
        }

        Message::ClickSidebarItem(idx) => {
            let items = model.sidebar_items();
            if let Some(item) = items.get(*idx) {
                model.sidebar_index = *idx;
                match item {
                    crate::model::SidebarItem::File { file_idx, .. } => {
                        model.focus = Focus::FileSidebar;
                        jump_to_file(model, *file_idx);
                    }
                    crate::model::SidebarItem::Thread { .. } => {
                        sync_file_index_from_sidebar(model);
                        model.focus = Focus::DiffPane;
                        model.needs_redraw = true;
                    }
                }
                ensure_sidebar_visible(model);
            }
        }

        Message::SidebarSelect => {
            let items = model.sidebar_items();
            if let Some(item) = items.get(model.sidebar_index) {
                match item {
                    crate::model::SidebarItem::File {
                        entry,
                        file_idx,
                        collapsed,
                    } => {
                        // Toggle collapse state
                        if *collapsed {
                            model.collapsed_files.remove(&entry.path);
                        } else {
                            model.collapsed_files.insert(entry.path.clone());
                        }
                        // Clamp sidebar_index to new tree size
                        let new_len = model.sidebar_items().len();
                        if new_len > 0 && model.sidebar_index >= new_len {
                            model.sidebar_index = new_len - 1;
                        }
                        ensure_sidebar_visible(model);
                        // Also select this file
                        let target = *file_idx;
                        jump_to_file(model, target);
                    }
                    crate::model::SidebarItem::Thread { .. } => {
                        // Sync already centers on thread via sync_file_index_from_sidebar;
                        // Enter additionally switches focus to the diff pane
                        sync_file_index_from_sidebar(model);
                        model.focus = Focus::DiffPane;
                    }
                }
            }
        }
        _ => {}
    }
}

fn update_navigation(model: &mut Model, msg: &Message) {
    match msg {
        Message::SelectReview(id) => {
            if let Some(index) = model
                .filtered_reviews()
                .iter()
                .position(|review| &review.review_id == id)
            {
                model.list_index = index;
                let visible = model.list_visible_height().max(1);
                if model.list_index < model.list_scroll {
                    model.list_scroll = model.list_index;
                } else if model.list_index >= model.list_scroll + visible {
                    model.list_scroll = model.list_index.saturating_sub(visible.saturating_sub(1));
                }
            }
            // Switch to review detail screen
            model.screen = Screen::ReviewDetail;
            model.focus = Focus::DiffPane;
            model.file_index = 0;
            model.sidebar_index = 0;
            model.sidebar_scroll = 0;
            model.collapsed_files.clear();
            model.diff_scroll = 0;
            model.diff_cursor = 0;
            model.expanded_thread = None;
            model.current_review = None; // Clear to trigger reload
            model.current_diff = None;
            model.current_file_content = None;
            model.highlighted_lines.clear();
            model.file_cache.clear();
            model.threads.clear();
            model.all_comments.clear();
            model.needs_redraw = true;
            // Note: caller should load review details from DB
        }

        Message::Back => match model.screen {
            Screen::ReviewDetail => {
                model.screen = Screen::ReviewList;
                model.focus = Focus::ReviewList;
                model.visual_mode = false;
                model.current_review = None;
                model.current_diff = None;
                model.current_file_content = None;
                model.highlighted_lines.clear();
                model.file_cache.clear();
                model.threads.clear();
                model.all_comments.clear();
                model.needs_redraw = true;
            }
            Screen::ReviewList => {
                // Already at top level, could quit or no-op
            }
        },
        _ => {}
    }
}

fn update_view_filter(model: &mut Model, msg: &Message) {
    match msg {
        Message::CycleStatusFilter => {
            model.filter = match model.filter {
                ReviewFilter::All => ReviewFilter::Open,
                ReviewFilter::Open => ReviewFilter::Closed,
                ReviewFilter::Closed => ReviewFilter::All,
            };
            model.list_index = 0;
            model.list_scroll = 0;
            model.needs_redraw = true;
        }

        Message::ToggleDiffView => {
            model.diff_view_mode = match model.diff_view_mode {
                DiffViewMode::Unified => DiffViewMode::SideBySide,
                DiffViewMode::SideBySide => DiffViewMode::Unified,
            };
            model.needs_redraw = true;
            update_active_file_from_scroll(model);
        }

        Message::ToggleSidebar => {
            model.sidebar_visible = !model.sidebar_visible;
            if !model.sidebar_visible && matches!(model.focus, Focus::FileSidebar) {
                model.focus = Focus::DiffPane;
            }
            model.needs_redraw = true;
            update_active_file_from_scroll(model);
        }

        Message::ToggleDiffWrap => {
            model.diff_wrap = !model.diff_wrap;
            model.needs_redraw = true;
            update_active_file_from_scroll(model);
        }

        Message::OpenFileInEditor => {
            let files = model.files_with_threads();
            if let Some(file) = files.get(model.file_index) {
                let line = model
                    .expanded_thread
                    .as_ref()
                    .and_then(|thread_id| model.threads.iter().find(|t| t.thread_id == *thread_id))
                    .and_then(|thread| {
                        // Only use line number if thread is for the current file
                        if thread.file_path == file.path && thread.selection_start > 0 {
                            Some(thread.selection_start as u32)
                        } else {
                            None
                        }
                    });
                model.pending_editor_request = Some(EditorRequest {
                    file_path: file.path.clone(),
                    line,
                });
            }
        }
        _ => {}
    }
}

fn update_system_theme(model: &mut Model, msg: &Message) {
    match msg {
        Message::Resize { width, height } => {
            model.resize(*width, *height);
            model.needs_redraw = true;
            update_active_file_from_scroll(model);
        }

        Message::Quit => {
            model.should_quit = true;
        }

        Message::ShowThemePicker => {
            model.pre_palette_theme = model.config.theme.clone();
            model.command_palette_mode = PaletteMode::Themes;
            model.command_palette_input.clear();
            let theme_names = filter_theme_names(&model.command_palette_input);
            model.command_palette_selection = theme_names
                .iter()
                .position(|&name| name == model.theme.name)
                .unwrap_or(0);
            model.previous_focus = Some(model.focus);
            model.focus = Focus::CommandPalette;
            model.needs_redraw = true;
        }

        Message::ApplyTheme(theme_name) => {
            if let Some(loaded) = theme::load_built_in_theme(theme_name) {
                model.theme = loaded.theme;
                if let Some(syntax_theme) = loaded.syntax_theme {
                    model.highlighter = Highlighter::with_theme(&syntax_theme);
                } else if theme_name.to_lowercase().contains("light") {
                    model.highlighter = Highlighter::with_theme("base16-ocean.light");
                } else {
                    model.highlighter = Highlighter::with_theme("base16-ocean.dark");
                }
                model.config.theme = Some(theme_name.clone());
                let _ = config::save_ui_config(&model.config);
                model.needs_redraw = true;
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_lines)]
pub fn update(model: &mut Model, msg: Message) {
    // Clear transient flash message on any user-initiated action.
    if model.flash_message.is_some()
        && !matches!(msg, Message::Tick | Message::Resize { .. } | Message::Noop)
    {
        model.flash_message = None;
        model.needs_redraw = true;
    }

    match msg {
        Message::ListUp
        | Message::ListDown
        | Message::ListPageUp
        | Message::ListPageDown
        | Message::ListTop
        | Message::ListBottom => {
            update_list_nav(model, &msg);
        }

        Message::CursorUp | Message::CursorDown | Message::CursorTop | Message::CursorBottom => {
            update_cursor(model, &msg);
        }

        Message::VisualToggle => {
            if model.visual_mode {
                model.visual_mode = false;
            } else {
                model.visual_mode = true;
                model.visual_anchor = model.diff_cursor;
            }
            model.needs_redraw = true;
        }

        Message::ScrollUp
        | Message::ScrollDown
        | Message::ScrollTop
        | Message::ScrollBottom
        | Message::ScrollHalfPageUp
        | Message::ScrollHalfPageDown
        | Message::ScrollTenUp
        | Message::ScrollTenDown
        | Message::PageUp
        | Message::PageDown => {
            update_scroll(model, &msg);
        }

        Message::NextThread
        | Message::PrevThread
        | Message::ExpandThread(_)
        | Message::CollapseThread => {
            update_thread_nav(model, msg);
        }

        Message::ShowCommandPalette
        | Message::HideCommandPalette
        | Message::CommandPaletteNext
        | Message::CommandPalettePrev
        | Message::CommandPaletteUpdateInput(_)
        | Message::CommandPaletteInputBackspace
        | Message::CommandPaletteDeleteWord
        | Message::CommandPaletteExecute => {
            update_command_palette(model, msg);
        }

        Message::StartComment => {
            handle_start_comment_inline(model);
        }

        Message::StartCommentExternal => {
            handle_start_comment_external(model);
        }

        Message::EnterCommentMode
        | Message::CommentInput(_)
        | Message::CommentInputBackspace
        | Message::CommentNewline
        | Message::CommentCursorUp
        | Message::CommentCursorDown
        | Message::CommentCursorLeft
        | Message::CommentCursorRight
        | Message::CommentHome
        | Message::CommentEnd
        | Message::CommentWordLeft
        | Message::CommentWordRight
        | Message::CommentDeleteWord
        | Message::CommentClearLine
        | Message::SaveComment
        | Message::CancelComment => {
            update_comment(model, msg);
        }

        Message::SelectReview(_) | Message::Back => {
            update_navigation(model, &msg);
        }

        Message::NextFile
        | Message::PrevFile
        | Message::SidebarTop
        | Message::SidebarBottom
        | Message::SelectFile(_)
        | Message::ClickSidebarItem(_)
        | Message::SidebarSelect => {
            update_file_sidebar(model, &msg);
        }

        Message::ToggleFocus => {
            model.focus = match model.focus {
                Focus::ReviewList => Focus::ReviewList,
                Focus::DiffPane => Focus::FileSidebar,
                Focus::CommandPalette => model.previous_focus.take().unwrap_or(Focus::DiffPane),
                Focus::FileSidebar | Focus::ThreadExpanded | Focus::Commenting => Focus::DiffPane,
            };
        }

        Message::ResolveThread(_id) | Message::ReopenThread(_id) => {
            // TODO: Write to event log
        }

        Message::CycleStatusFilter
        | Message::ToggleDiffView
        | Message::ToggleSidebar
        | Message::ToggleDiffWrap
        | Message::OpenFileInEditor => {
            update_view_filter(model, &msg);
        }

        Message::SearchActivate => {
            model.search_active = true;
            model.needs_redraw = true;
        }
        Message::SearchInput(ref text) => {
            model.search_input.push_str(text);
            model.list_index = 0;
            model.list_scroll = 0;
            model.needs_redraw = true;
        }
        Message::SearchBackspace => {
            model.search_input.pop();
            model.list_index = 0;
            model.list_scroll = 0;
            model.needs_redraw = true;
        }
        Message::SearchDeleteWord => {
            delete_last_word(&mut model.search_input);
            model.list_index = 0;
            model.list_scroll = 0;
            model.needs_redraw = true;
        }
        Message::SearchClearLine => {
            model.search_input.clear();
            model.list_index = 0;
            model.list_scroll = 0;
            model.needs_redraw = true;
        }
        Message::SearchClear => {
            model.search_input.clear();
            model.search_active = false;
            model.list_index = 0;
            model.list_scroll = 0;
            model.needs_redraw = true;
        }

        Message::Resize { .. }
        | Message::Quit
        | Message::ShowThemePicker
        | Message::ApplyTheme(_) => {
            update_system_theme(model, &msg);
        }

        Message::Tick | Message::Noop => {}
    }
}

/// Build a `CommentRequest` from the current model state (visual selection or expanded thread).
fn build_comment_request(model: &mut Model) -> Option<CommentRequest> {
    let review = model.current_review.as_ref()?;
    let review_id = review.review_id.clone();
    let files = model.files_with_threads();
    let file = files.get(model.file_index)?;
    let file_path = file.path.clone();

    if model.visual_mode {
        let sel_start = model.visual_anchor.min(model.diff_cursor);
        let sel_end = model.visual_anchor.max(model.diff_cursor);

        let line_map = model.line_map.borrow();
        let mut min_line = i64::MAX;
        let mut max_line = i64::MIN;
        for row in sel_start..=sel_end {
            if let Some(&new_line) = line_map.get(&row) {
                min_line = min_line.min(new_line);
                max_line = max_line.max(new_line);
            }
        }
        drop(line_map);

        if min_line > max_line {
            return None;
        }

        let end_line = if max_line == min_line {
            None
        } else {
            Some(max_line)
        };

        Some(CommentRequest {
            review_id,
            file_path,
            start_line: min_line,
            end_line,
            thread_id: None,
            existing_comments: Vec::new(),
        })
    } else {
        let line_map = model.line_map.borrow();
        if let Some(&new_line) = line_map.get(&model.diff_cursor) {
            return Some(CommentRequest {
                review_id,
                file_path,
                start_line: new_line,
                end_line: None,
                thread_id: None,
                existing_comments: Vec::new(),
            });
        }
        drop(line_map);

        // Find the thread whose rendered position is closest to (and at or
        // before) the cursor, so pressing 'a' inside a comment block targets it.
        let thread_id = {
            let positions = model.thread_positions.borrow();
            let mut best: Option<(usize, String)> = None;
            for thread in model.threads.iter().filter(|t| t.file_path == file_path) {
                if let Some(&pos) = positions.get(&thread.thread_id) {
                    if pos <= model.diff_cursor
                        && best.as_ref().is_none_or(|(best_pos, _)| pos > *best_pos)
                    {
                        best = Some((pos, thread.thread_id.clone()));
                    }
                }
            }
            best.map(|(_, id)| id)
        }?;
        let thread = model.threads.iter().find(|t| t.thread_id == thread_id)?;
        let existing_comments = model
            .all_comments
            .get(&thread_id)
            .cloned()
            .unwrap_or_default();

        Some(CommentRequest {
            review_id,
            file_path: thread.file_path.clone(),
            start_line: thread.selection_start,
            end_line: thread.selection_end,
            thread_id: Some(thread_id),
            existing_comments,
        })
    }
}

/// Open inline multi-line comment editor (a key).
fn handle_start_comment_inline(model: &mut Model) {
    if let Some(request) = build_comment_request(model) {
        model.inline_editor = Some(InlineEditor::new(request));
        model.focus = Focus::Commenting;
        model.needs_redraw = true;
    }
}

/// Open $EDITOR for commenting (Shift+A key).
fn handle_start_comment_external(model: &mut Model) {
    if let Some(request) = build_comment_request(model) {
        model.pending_comment_request = Some(request);
        model.needs_redraw = true;
    }
}

fn sync_file_index_from_sidebar(model: &mut Model) {
    let items = model.sidebar_items();
    if let Some(item) = items.get(model.sidebar_index) {
        match item {
            crate::model::SidebarItem::File { file_idx, .. } => {
                jump_to_file(model, *file_idx);
            }
            crate::model::SidebarItem::Thread {
                file_idx,
                thread_id,
                ..
            } => {
                let target = *file_idx;
                let tid = thread_id.clone();
                if target != model.file_index {
                    jump_to_file(model, target);
                }
                model.expanded_thread = Some(tid);
                center_on_thread(model);
                model.needs_redraw = true;
            }
        }
    }
}

fn jump_to_file(model: &mut Model, index: usize) {
    model.file_index = index;
    model.expanded_thread = None;

    let layout = stream_layout(model);
    model.diff_scroll = file_scroll_offset(&layout, index);
    model.sync_active_file_cache();
    model.needs_redraw = true;
}

fn update_active_file_from_scroll(model: &mut Model) {
    let layout = stream_layout(model);
    let active = active_file_index(&layout, model.diff_scroll);
    if active != model.file_index {
        model.file_index = active;
        model.sync_active_file_cache();
    }
    sync_sidebar_from_active(model);
    model.needs_redraw = true;
}

fn sync_sidebar_from_active(model: &mut Model) {
    let items = model.sidebar_items();
    let mut target = active_thread_from_scroll(model).and_then(|thread_id| {
        items.iter().position(|item| match item {
            crate::model::SidebarItem::Thread { thread_id: id, .. } => id == &thread_id,
            crate::model::SidebarItem::File { .. } => false,
        })
    });

    if target.is_none() {
        if let Some(thread_id) = &model.expanded_thread {
            target = items.iter().position(|item| match item {
                crate::model::SidebarItem::Thread { thread_id: id, .. } => id == thread_id,
                crate::model::SidebarItem::File { .. } => false,
            });
        }
    }

    if target.is_none() {
        target = items.iter().position(|item| match item {
            crate::model::SidebarItem::File { file_idx, .. } => *file_idx == model.file_index,
            crate::model::SidebarItem::Thread { .. } => false,
        });
    }

    if let Some(index) = target {
        model.sidebar_index = index;
        ensure_sidebar_visible(model);
    }
}

fn active_thread_from_scroll(model: &Model) -> Option<String> {
    let positions = model.thread_positions.borrow();
    if positions.is_empty() {
        return None;
    }

    let files = model.files_with_threads();
    let file = files.get(model.file_index)?;

    let view_height = visible_stream_rows(model.height);
    let view_end = model.diff_scroll.saturating_add(view_height);

    let mut in_view: Option<(usize, &str)> = None;
    let mut above: Option<(usize, &str)> = None;

    for thread in model.threads.iter().filter(|t| t.file_path == file.path) {
        if let Some(&pos) = positions.get(&thread.thread_id) {
            if pos >= model.diff_scroll && pos <= view_end {
                if in_view.is_none_or(|(best, _)| pos < best) {
                    in_view = Some((pos, thread.thread_id.as_str()));
                }
            } else if pos < model.diff_scroll && above.is_none_or(|(best, _)| pos > best) {
                above = Some((pos, thread.thread_id.as_str()));
            }
        }
    }

    if let Some((_, id)) = in_view {
        return Some(id.to_string());
    }
    if let Some((_, id)) = above {
        return Some(id.to_string());
    }

    None
}

fn ensure_sidebar_visible(model: &mut Model) {
    let items_len = model.sidebar_items().len();
    let visible = sidebar_visible_rows(model);
    if items_len == 0 || visible == 0 {
        model.sidebar_scroll = 0;
        return;
    }

    let max_scroll = items_len.saturating_sub(visible);
    if model.sidebar_scroll > max_scroll {
        model.sidebar_scroll = max_scroll;
    }

    if model.sidebar_index < model.sidebar_scroll {
        model.sidebar_scroll = model.sidebar_index;
    } else if model.sidebar_index >= model.sidebar_scroll + visible {
        model.sidebar_scroll = model
            .sidebar_index
            .saturating_sub(visible.saturating_sub(1));
    }

    if model.sidebar_scroll > max_scroll {
        model.sidebar_scroll = max_scroll;
    }
}

const fn sidebar_visible_rows(model: &Model) -> usize {
    let mut start = 1usize;
    if model.current_review.is_some() {
        start = start.saturating_add(5);
    }
    let bottom = model.height.saturating_sub(1) as usize;
    if start >= bottom {
        return 0;
    }
    bottom - start
}

fn center_on_thread(model: &mut Model) {
    let Some(thread_id) = model.expanded_thread.clone() else {
        return;
    };
    // Use positions captured during the last render pass
    let positions = model.thread_positions.borrow();
    if let Some(&stream_row) = positions.get(&thread_id) {
        drop(positions);
        model.diff_cursor = stream_row;
        let view_height = visible_stream_rows(model.height);
        let center = view_height / 2;
        model.diff_scroll = stream_row.saturating_sub(center);
    } else {
        drop(positions);
        // Thread not anchored in the diff (line outside hunk range).
        // Scroll to the end of the file's section as a fallback.
        let layout = stream_layout(model);
        let files = model.files_with_threads();
        if let Some(thread) = model.threads.iter().find(|t| t.thread_id == thread_id) {
            if let Some(file_index) = files.iter().position(|f| f.path == thread.file_path) {
                let file_end = layout
                    .file_offsets
                    .get(file_index + 1)
                    .copied()
                    .unwrap_or(layout.total_lines);
                let view_height = visible_stream_rows(model.height);
                let center = view_height / 2;
                model.diff_scroll = file_end.saturating_sub(center);
            }
        }
    }
}

fn stream_layout(model: &Model) -> crate::stream::StreamLayout {
    let files = model.files_with_threads();
    let width = diff_content_width(model);
    let description = model
        .current_review
        .as_ref()
        .and_then(|r| r.description.as_deref());
    compute_stream_layout(&StreamLayoutParams {
        files: &files,
        file_cache: &model.file_cache,
        threads: &model.threads,
        all_comments: &model.all_comments,
        view_mode: model.diff_view_mode,
        wrap: model.diff_wrap,
        content_width: width,
        description,
    })
}

fn clamp_diff_scroll(model: &mut Model) {
    let layout = stream_layout(model);
    let visible = visible_stream_rows(model.height);
    let max_scroll = layout.total_lines.saturating_sub(visible);
    if model.diff_scroll > max_scroll {
        model.diff_scroll = max_scroll;
    }
}

fn diff_content_width(model: &Model) -> u32 {
    /// Must match `DIFF_MARGIN` in diff.rs.
    const DIFF_MARGIN: u32 = 2;
    let total_width = u32::from(model.width);
    let pane_width = match model.layout_mode {
        crate::model::LayoutMode::Full
        | crate::model::LayoutMode::Compact
        | crate::model::LayoutMode::Overlay => {
            if model.sidebar_visible {
                total_width.saturating_sub(u32::from(model.layout_mode.sidebar_width()))
            } else {
                total_width
            }
        }
        crate::model::LayoutMode::Single => total_width,
    };
    pane_width.saturating_sub(DIFF_MARGIN * 2)
}

/// If the theme picker is active, apply the currently highlighted theme as a preview.
fn preview_selected_theme(model: &mut Model) {
    if model.command_palette_mode != PaletteMode::Themes {
        return;
    }
    let theme_names = filter_theme_names(&model.command_palette_input);
    if let Some(&name) = theme_names.get(model.command_palette_selection) {
        if let Some(loaded) = theme::load_built_in_theme(name) {
            model.theme = loaded.theme;
            if let Some(syntax_theme) = loaded.syntax_theme {
                model.highlighter = Highlighter::with_theme(&syntax_theme);
            }
        }
    }
}

fn filter_theme_names(query: &str) -> Vec<&'static str> {
    let names = theme::built_in_theme_names();
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if terms.is_empty() {
        return names;
    }
    names
        .into_iter()
        .filter(|name| {
            let name_lower = name.to_lowercase();
            terms.iter().all(|term| name_lower.contains(term.as_str()))
        })
        .collect()
}

fn delete_last_word(s: &mut String) {
    // Trim trailing whitespace
    while s.ends_with(' ') {
        s.pop();
    }
    // Delete until whitespace or empty
    while !s.is_empty() && !s.ends_with(' ') {
        s.pop();
    }
}

fn filter_commands(query: &str) -> Vec<crate::command::CommandSpec> {
    let commands = get_commands();
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if terms.is_empty() {
        return commands;
    }
    commands
        .into_iter()
        .filter(|cmd| {
            let name_lower = cmd.name.to_lowercase();
            let cat_lower = cmd.category.to_lowercase();
            terms
                .iter()
                .all(|term| name_lower.contains(term.as_str()) || cat_lower.contains(term.as_str()))
        })
        .collect()
}

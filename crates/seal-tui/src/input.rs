//! Input mapping: events → messages.
//!
//! Pure-ish functions that translate keyboard, mouse, and resize events
//! into the application's `Message` type.

use std::time::{Duration, Instant};

use crate::render_backend::{
    Event, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::message::Message;
use crate::model::{Focus, LayoutMode, Model, Screen};
use crate::stream::description_block_height;

pub fn map_event_to_message(model: &mut Model, event: &Event) -> Message {
    match event {
        Event::Key(key) => {
            // Check for Ctrl+C to quit
            if key.modifiers.contains(KeyModifiers::CTRL) && key.code == KeyCode::Char('c') {
                return Message::Quit;
            }

            if key.modifiers.contains(KeyModifiers::CTRL) && key.code == KeyCode::Char('p') {
                return Message::ShowCommandPalette;
            }

            if model.focus == Focus::CommandPalette {
                return map_command_palette_key(key.code, key.modifiers);
            }

            match model.screen {
                Screen::ReviewList => map_review_list_key(key.code, key.modifiers, model),
                Screen::ReviewDetail => map_review_detail_key(model, key.code, key.modifiers),
            }
        }
        Event::Resize(resize) => Message::Resize {
            width: resize.width,
            height: resize.height,
        },
        Event::Mouse(mouse) => match model.screen {
            Screen::ReviewList => map_review_list_mouse(model, *mouse),
            Screen::ReviewDetail => map_review_detail_mouse(model, *mouse),
        },
        Event::Paste(_) | Event::FocusGained | Event::FocusLost => Message::Noop,
    }
}

fn map_review_list_key(key: KeyCode, modifiers: KeyModifiers, model: &Model) -> Message {
    // When search is active, route chars to search input
    if model.search_active {
        if modifiers.contains(KeyModifiers::CTRL) {
            return match key {
                KeyCode::Char('w') => Message::SearchDeleteWord,
                KeyCode::Char('u') => Message::SearchClearLine,
                _ => Message::Noop,
            };
        }
        return match key {
            KeyCode::Esc => Message::SearchClear,
            KeyCode::Backspace => Message::SearchBackspace,
            KeyCode::Enter => {
                // Select current review from filtered results
                let reviews = model.filtered_reviews();
                reviews
                    .get(model.list_index)
                    .map_or(Message::Noop, |review| {
                        Message::SelectReview(review.review_id.clone())
                    })
            }
            KeyCode::Char(c) => Message::SearchInput(c.to_string()),
            _ => Message::Noop,
        };
    }

    match key {
        KeyCode::Char('q') => Message::Quit,
        KeyCode::Char('j') | KeyCode::Down => Message::ListDown,
        KeyCode::Char('k') | KeyCode::Up => Message::ListUp,
        KeyCode::Char('g') | KeyCode::Home => Message::ListTop,
        KeyCode::Char('G') | KeyCode::End => Message::ListBottom,
        KeyCode::PageUp => Message::ListPageUp,
        KeyCode::PageDown => Message::ListPageDown,
        KeyCode::Enter | KeyCode::Char('l') => {
            let reviews = model.filtered_reviews();
            reviews
                .get(model.list_index)
                .map_or(Message::Noop, |review| {
                    Message::SelectReview(review.review_id.clone())
                })
        }
        KeyCode::Char('s') => Message::CycleStatusFilter,
        KeyCode::Char('/') => Message::SearchActivate,
        _ => Message::Noop,
    }
}

fn map_review_list_mouse(model: &mut Model, mouse: MouseEvent) -> Message {
    if model.focus == Focus::CommandPalette {
        return Message::Noop;
    }

    if mouse.is_scroll() {
        let direction = match mouse.kind {
            MouseEventKind::ScrollUp => -1,
            MouseEventKind::ScrollDown => 1,
            _ => return Message::Noop,
        };
        if !should_handle_scroll(&mut model.last_list_scroll, direction) {
            return Message::Noop;
        }
        return match mouse.kind {
            MouseEventKind::ScrollUp => Message::ListUp,
            MouseEventKind::ScrollDown => Message::ListDown,
            _ => Message::Noop,
        };
    }

    if mouse.button != MouseButton::Left {
        return Message::Noop;
    }

    if !matches!(mouse.kind, MouseEventKind::Press | MouseEventKind::Release) {
        return Message::Noop;
    }

    // Must match review_list.rs: HEADER_HEIGHT(5) + SEARCH_HEIGHT(2)
    let header_height = 7u32;
    let footer_height = 2u32;
    let height = u32::from(model.height);
    if height <= header_height + footer_height {
        return Message::Noop;
    }

    let list_start = header_height;
    let list_end = height.saturating_sub(footer_height);
    if mouse.y < list_start || mouse.y >= list_end {
        return Message::Noop;
    }

    let row = (mouse.y - list_start) as usize;
    // Each review item is 2 lines tall (ITEM_HEIGHT)
    let index = model.list_scroll + row / 2;
    let reviews = model.filtered_reviews();
    if let Some(review) = reviews.get(index) {
        return Message::SelectReview(review.review_id.clone());
    }

    Message::Noop
}

fn map_review_detail_mouse(model: &mut Model, mouse: MouseEvent) -> Message {
    if model.focus == Focus::CommandPalette || model.focus == Focus::Commenting {
        return Message::Noop;
    }

    let sidebar_rect = match model.layout_mode {
        LayoutMode::Full | LayoutMode::Compact | LayoutMode::Overlay => {
            if model.sidebar_visible {
                Some((
                    0u32,
                    0u32,
                    u32::from(model.layout_mode.sidebar_width()),
                    u32::from(model.height),
                ))
            } else {
                None
            }
        }
        LayoutMode::Single => {
            if !model.sidebar_visible || !matches!(model.focus, Focus::FileSidebar) {
                None
            } else {
                Some((0u32, 0u32, u32::from(model.width), u32::from(model.height)))
            }
        }
    };

    if mouse.is_scroll() {
        let direction = match mouse.kind {
            MouseEventKind::ScrollUp => -1,
            MouseEventKind::ScrollDown => 1,
            _ => return Message::Noop,
        };
        if let Some((x, y, width, height)) = sidebar_rect {
            if mouse.x >= x
                && mouse.x < x.saturating_add(width)
                && mouse.y >= y
                && mouse.y < y.saturating_add(height)
            {
                if !should_handle_scroll(&mut model.last_sidebar_scroll, direction) {
                    return Message::Noop;
                }
                return match mouse.kind {
                    MouseEventKind::ScrollUp => Message::PrevFile,
                    MouseEventKind::ScrollDown => Message::NextFile,
                    _ => Message::Noop,
                };
            }
        }

        return match mouse.kind {
            MouseEventKind::ScrollUp => Message::ScrollUp,
            MouseEventKind::ScrollDown => Message::ScrollDown,
            _ => Message::Noop,
        };
    }

    if mouse.button != MouseButton::Left {
        return Message::Noop;
    }

    if !matches!(mouse.kind, MouseEventKind::Press | MouseEventKind::Release) {
        return Message::Noop;
    }

    let Some((sidebar_x, sidebar_y, sidebar_width, sidebar_height)) = sidebar_rect else {
        return Message::Noop;
    };

    if mouse.x < sidebar_x
        || mouse.x >= sidebar_x.saturating_add(sidebar_width)
        || mouse.y < sidebar_y
        || mouse.y >= sidebar_y.saturating_add(sidebar_height)
    {
        return Message::Noop;
    }

    let mut list_start = sidebar_y + 1;
    if model.current_review.is_some() {
        list_start = list_start.saturating_add(5);
    }
    let bottom = sidebar_y + sidebar_height.saturating_sub(1);
    if list_start >= bottom || mouse.y < list_start || mouse.y >= bottom {
        return Message::Noop;
    }

    let row = (mouse.y - list_start) as usize;
    let index = model.sidebar_scroll.saturating_add(row);
    let items = model.sidebar_items();
    if items.get(index).is_some() {
        return Message::ClickSidebarItem(index);
    }

    Message::Noop
}

fn should_handle_scroll(last: &mut Option<(Instant, i8)>, direction: i8) -> bool {
    const DEBOUNCE: Duration = Duration::from_millis(5);
    let now = Instant::now();
    if let Some((prev_at, prev_dir)) = last {
        if *prev_dir == direction && now.duration_since(*prev_at) < DEBOUNCE {
            return false;
        }
    }
    *last = Some((now, direction));
    true
}

fn map_review_detail_key(model: &Model, key: KeyCode, modifiers: KeyModifiers) -> Message {
    if modifiers.contains(KeyModifiers::CTRL) {
        match key {
            KeyCode::Char('j') => return Message::ScrollTenDown,
            KeyCode::Char('k') => return Message::ScrollTenUp,
            _ => {}
        }
    }

    match model.focus {
        Focus::FileSidebar => match key {
            KeyCode::Char('q') => Message::Quit,
            KeyCode::Esc | KeyCode::Char('h') => Message::Back,
            KeyCode::Tab | KeyCode::Char('l') => Message::ToggleFocus,
            KeyCode::Char('j') | KeyCode::Down => Message::NextFile,
            KeyCode::Char('k') | KeyCode::Up => Message::PrevFile,
            KeyCode::Char('g') | KeyCode::Home => Message::SidebarTop,
            KeyCode::Char('G') | KeyCode::End => Message::SidebarBottom,
            KeyCode::Enter => Message::SidebarSelect,
            KeyCode::Char('s') => Message::ToggleSidebar,
            _ => Message::Noop,
        },
        Focus::DiffPane if model.visual_mode => match key {
            KeyCode::Char('j') | KeyCode::Down => Message::CursorDown,
            KeyCode::Char('k') | KeyCode::Up => Message::CursorUp,
            KeyCode::Char('g') | KeyCode::Home => Message::CursorTop,
            KeyCode::Char('G') | KeyCode::End => Message::CursorBottom,
            KeyCode::Char('a') => Message::StartComment,
            KeyCode::Char('A') => Message::StartCommentExternal,
            KeyCode::Char('V') | KeyCode::Esc => Message::VisualToggle,
            _ => Message::Noop,
        },
        Focus::DiffPane => {
            let description_visible = description_scroll_active(model);
            match key {
                KeyCode::Char('q') => Message::Quit,
                KeyCode::Esc => Message::Back,
                KeyCode::Tab | KeyCode::Char('h') => Message::ToggleFocus,
                KeyCode::Char('j') | KeyCode::Down => {
                    if description_visible {
                        Message::ScrollDown
                    } else {
                        Message::CursorDown
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if description_visible {
                        Message::ScrollUp
                    } else {
                        Message::CursorUp
                    }
                }
                KeyCode::Char('g') | KeyCode::Home => {
                    if description_visible {
                        Message::ScrollTop
                    } else {
                        Message::CursorTop
                    }
                }
                KeyCode::Char('G') | KeyCode::End => {
                    if description_visible {
                        Message::ScrollBottom
                    } else {
                        Message::CursorBottom
                    }
                }
                KeyCode::Char('n') => Message::NextThread,
                KeyCode::Char('p' | 'N') => Message::PrevThread,
                KeyCode::Char('v') => Message::ToggleDiffView,
                KeyCode::Char('w') => Message::ToggleDiffWrap,
                KeyCode::Char('o') => Message::OpenFileInEditor,
                KeyCode::Char('u') => Message::ScrollHalfPageUp,
                KeyCode::Char('d') => Message::ScrollHalfPageDown,
                KeyCode::Char('b') | KeyCode::PageUp => Message::PageUp,
                KeyCode::Char('f') | KeyCode::PageDown => Message::PageDown,
                KeyCode::Char('s') => Message::ToggleSidebar,
                KeyCode::Enter => model
                    .expanded_thread
                    .as_ref()
                    .map_or(Message::NextThread, |id| Message::ExpandThread(id.clone())),
                KeyCode::Char('a') => Message::StartComment,
                KeyCode::Char('A') => Message::StartCommentExternal,
                KeyCode::Char('V') => Message::VisualToggle,
                KeyCode::Char('[') => Message::PrevFile,
                KeyCode::Char(']') => Message::NextFile,
                _ => Message::Noop,
            }
        }
        Focus::ThreadExpanded => match key {
            KeyCode::Esc => Message::CollapseThread,
            KeyCode::Char('j') | KeyCode::Down => Message::ScrollDown,
            KeyCode::Char('k') | KeyCode::Up => Message::ScrollUp,
            KeyCode::Char('g') | KeyCode::Home => Message::ScrollTop,
            KeyCode::Char('G') | KeyCode::End => Message::ScrollBottom,
            KeyCode::Char('r' | 'R') => model
                .expanded_thread
                .as_ref()
                .map_or(Message::Noop, |id| Message::ResolveThread(id.clone())),
            _ => Message::Noop,
        },
        Focus::Commenting => {
            if modifiers.contains(KeyModifiers::CTRL) {
                return match key {
                    KeyCode::Char('s') => Message::SaveComment,
                    KeyCode::Char('w') => Message::CommentDeleteWord,
                    KeyCode::Char('u') => Message::CommentClearLine,
                    KeyCode::Char('a') => Message::CommentHome,
                    KeyCode::Char('e') => Message::CommentEnd,
                    KeyCode::Char('b') => Message::CommentCursorLeft,
                    KeyCode::Char('f') => Message::CommentCursorRight,
                    _ => Message::Noop,
                };
            }
            if modifiers.contains(KeyModifiers::ALT) {
                return match key {
                    KeyCode::Char('b') => Message::CommentWordLeft,
                    KeyCode::Char('f') => Message::CommentWordRight,
                    _ => Message::Noop,
                };
            }
            match key {
                KeyCode::Esc => Message::CancelComment,
                KeyCode::Enter => Message::CommentNewline,
                KeyCode::Up => Message::CommentCursorUp,
                KeyCode::Down => Message::CommentCursorDown,
                KeyCode::Left => Message::CommentCursorLeft,
                KeyCode::Right => Message::CommentCursorRight,
                KeyCode::Home => Message::CommentHome,
                KeyCode::End => Message::CommentEnd,
                KeyCode::Backspace => Message::CommentInputBackspace,
                KeyCode::Char(c) => Message::CommentInput(c.to_string()),
                _ => Message::Noop,
            }
        }
        _ => Message::Noop,
    }
}

fn description_scroll_active(model: &Model) -> bool {
    let description = model
        .current_review
        .as_ref()
        .and_then(|review| review.description.as_deref());
    let description_lines = description_block_height(description, diff_content_width(model));
    description_lines > 0 && model.diff_scroll < description_lines
}

fn diff_content_width(model: &Model) -> u32 {
    const DIFF_MARGIN: u32 = 2;
    let total_width = u32::from(model.width);
    let pane_width = match model.layout_mode {
        LayoutMode::Full | LayoutMode::Compact | LayoutMode::Overlay => {
            if model.sidebar_visible {
                total_width.saturating_sub(u32::from(model.layout_mode.sidebar_width()))
            } else {
                total_width
            }
        }
        LayoutMode::Single => total_width,
    };
    pane_width.saturating_sub(DIFF_MARGIN * 2)
}

fn map_command_palette_key(key: KeyCode, modifiers: KeyModifiers) -> Message {
    if modifiers.contains(KeyModifiers::CTRL) {
        return match key {
            KeyCode::Char('w') => Message::CommandPaletteDeleteWord,
            _ => Message::Noop,
        };
    }
    match key {
        KeyCode::Esc => Message::HideCommandPalette,
        KeyCode::Up => Message::CommandPalettePrev,
        KeyCode::Down => Message::CommandPaletteNext,
        KeyCode::Enter => Message::CommandPaletteExecute,
        KeyCode::Char(c) => Message::CommandPaletteUpdateInput(c.to_string()),
        KeyCode::Backspace => Message::CommandPaletteInputBackspace,
        _ => Message::Noop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UiConfig;

    #[test]
    fn diff_pane_j_scrolls_when_description_visible() {
        let mut model = Model::new(120, 40, UiConfig::default());
        model.screen = Screen::ReviewDetail;
        model.focus = Focus::DiffPane;
        model.current_review = Some(crate::db::ReviewDetail {
            review_id: "cr-1".to_string(),
            jj_change_id: "main".to_string(),
            scm_kind: "git".to_string(),
            scm_anchor: "main".to_string(),
            initial_commit: "abc123".to_string(),
            final_commit: None,
            title: "Title".to_string(),
            description: Some("Line one\n\nLine two\n\nLine three".to_string()),
            author: "alice".to_string(),
            created_at: "2026-03-10T00:00:00Z".to_string(),
            status: "open".to_string(),
            status_changed_at: None,
            status_changed_by: None,
            abandon_reason: None,
            thread_count: 0,
            open_thread_count: 0,
        });

        let msg = map_review_detail_key(&model, KeyCode::Char('j'), KeyModifiers::empty());
        assert!(matches!(msg, Message::ScrollDown));
    }
}

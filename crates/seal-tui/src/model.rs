//! Application state model

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::command::CommandSpec;
use crate::config::UiConfig;
use crate::db::{Comment, ReviewDetail, ReviewSummary, ThreadDetail, ThreadSummary};
use crate::diff::ParsedDiff;
use crate::syntax::{HighlightSpan, Highlighter};
use crate::theme::Theme;

/// File content for displaying context when no diff is available.
///
/// When populated from seal's windowed content, `start_line` indicates
/// the 1-based line number of the first element in `lines`.
/// When populated from a full file read, `start_line` is 1.
#[derive(Debug, Clone)]
pub struct FileContent {
    pub lines: Vec<String>,
    /// 1-based line number of `lines[0]`. Defaults to 1 for full files.
    pub start_line: i64,
}

/// Cached data for a file in the review stream
pub struct FileCacheEntry {
    pub diff: Option<ParsedDiff>,
    pub file_content: Option<FileContent>,
    pub highlighted_lines: Vec<Vec<HighlightSpan>>,
    /// Syntax highlights indexed by file line number (for orphaned thread context).
    /// Only populated when both `diff` and `file_content` are present.
    pub file_highlighted_lines: Vec<Vec<HighlightSpan>>,
}

/// Current screen/view
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    #[default]
    ReviewList,
    ReviewDetail,
}

/// Which pane has focus
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    ReviewList,
    FileSidebar,
    DiffPane,
    ThreadExpanded,
    CommandPalette,
    Commenting,
}

/// What the command palette is showing
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaletteMode {
    #[default]
    Commands,
    Themes,
}

/// Responsive layout mode based on terminal width
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// >= 120 cols: full sidebar + diff
    Full,
    /// 90-119 cols: compact sidebar + diff
    Compact,
    /// 70-89 cols: overlay sidebar (toggleable)
    Overlay,
    /// < 70 cols: single pane mode
    Single,
}

/// Diff view mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffViewMode {
    /// Traditional unified diff (default)
    #[default]
    Unified,
    /// Side-by-side diff (old left, new right)
    SideBySide,
}

#[derive(Debug, Clone)]
pub struct EditorRequest {
    pub file_path: String,
    pub line: Option<u32>,
}

/// Request to open $EDITOR for writing a comment.
#[derive(Debug, Clone)]
pub struct CommentRequest {
    /// Review being commented on
    pub review_id: String,
    /// File the comment targets
    pub file_path: String,
    /// Start line (new-side, 1-based)
    pub start_line: i64,
    /// End line (new-side, 1-based); None means single line
    pub end_line: Option<i64>,
    /// If Some, add comment to existing thread; if None, create new thread
    pub thread_id: Option<String>,
    /// Existing comments for context in the editor temp file
    pub existing_comments: Vec<Comment>,
}

/// A comment ready to be persisted (from the inline editor).
#[derive(Debug, Clone)]
pub struct PendingCommentSubmission {
    pub request: CommentRequest,
    pub body: String,
}

/// A thread status change ready to be persisted.
#[derive(Debug, Clone)]
pub struct PendingThreadStatusChange {
    pub thread_id: String,
    pub action: ThreadStatusAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadStatusAction {
    Resolve,
    Reopen,
}

/// In-TUI multi-line comment editor state.
#[derive(Debug, Clone)]
pub struct InlineEditor {
    /// Lines of text (always at least one)
    pub lines: Vec<String>,
    /// Cursor row (0-indexed into lines)
    pub cursor_row: usize,
    /// Cursor column (0-indexed, character position in current line)
    pub cursor_col: usize,
    /// Vertical scroll offset for the text area
    pub scroll: usize,
    /// The comment request this editor is for
    pub request: CommentRequest,
}

impl InlineEditor {
    #[must_use]
    pub fn new(request: CommentRequest) -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            scroll: 0,
            request,
        }
    }

    /// Insert a character at the cursor position.
    pub fn insert_char(&mut self, c: char) {
        let line = &mut self.lines[self.cursor_row];
        let byte_idx = char_to_byte_index(line, self.cursor_col);
        line.insert(byte_idx, c);
        self.cursor_col += 1;
    }

    /// Insert a newline, splitting the current line.
    pub fn newline(&mut self) {
        let line = &self.lines[self.cursor_row];
        let byte_idx = char_to_byte_index(line, self.cursor_col);
        let rest = self.lines[self.cursor_row][byte_idx..].to_string();
        self.lines[self.cursor_row].truncate(byte_idx);
        self.cursor_row += 1;
        self.lines.insert(self.cursor_row, rest);
        self.cursor_col = 0;
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_row];
            let byte_idx = char_to_byte_index(line, self.cursor_col - 1);
            let end_byte = char_to_byte_index(line, self.cursor_col);
            line.drain(byte_idx..end_byte);
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            // Merge with previous line
            let current = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
            self.lines[self.cursor_row].push_str(&current);
        }
    }

    pub fn cursor_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.clamp_col();
        }
    }

    pub fn cursor_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.clamp_col();
        }
    }

    pub fn cursor_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
        }
    }

    pub fn cursor_right(&mut self) {
        let line_len = self.lines[self.cursor_row].chars().count();
        if self.cursor_col < line_len {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    pub const fn home(&mut self) {
        self.cursor_col = 0;
    }

    pub fn end(&mut self) {
        self.cursor_col = self.lines[self.cursor_row].chars().count();
    }

    /// Move cursor one word to the left (Alt+B).
    pub fn word_left(&mut self) {
        if self.cursor_col == 0 {
            return;
        }
        let line = &self.lines[self.cursor_row];
        let byte_idx = char_to_byte_index(line, self.cursor_col);
        let before = &line[..byte_idx];
        let trimmed = before.trim_end();
        let word_start = trimmed
            .rfind(|c: char| c.is_whitespace())
            .map_or(0, |i| i + 1);
        self.cursor_col = before[..word_start].chars().count();
    }

    /// Move cursor one word to the right (Alt+F).
    pub fn word_right(&mut self) {
        let line = &self.lines[self.cursor_row];
        let line_len = line.chars().count();
        if self.cursor_col >= line_len {
            return;
        }
        let byte_idx = char_to_byte_index(line, self.cursor_col);
        let after = &line[byte_idx..];
        // Skip non-whitespace, then skip whitespace
        let skip_word = after
            .find(|c: char| c.is_whitespace())
            .unwrap_or(after.len());
        let rest = &after[skip_word..];
        let skip_space = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        self.cursor_col += after[..skip_word + skip_space].chars().count();
    }

    /// Delete the word before the cursor (Ctrl+W).
    pub fn delete_word(&mut self) {
        if self.cursor_col == 0 {
            return;
        }
        let line = &self.lines[self.cursor_row];
        let byte_idx = char_to_byte_index(line, self.cursor_col);
        let before = &line[..byte_idx];
        let trimmed = before.trim_end();
        // Find start of last word
        let word_start = trimmed
            .rfind(|c: char| c.is_whitespace())
            .map_or(0, |i| i + 1);
        let new_col = before[..word_start].chars().count();
        let start_byte = char_to_byte_index(&self.lines[self.cursor_row], new_col);
        self.lines[self.cursor_row].drain(start_byte..byte_idx);
        self.cursor_col = new_col;
    }

    /// Clear from cursor to start of line (Ctrl+U).
    pub fn clear_line(&mut self) {
        let line = &self.lines[self.cursor_row];
        let byte_idx = char_to_byte_index(line, self.cursor_col);
        self.lines[self.cursor_row].drain(..byte_idx);
        self.cursor_col = 0;
    }

    /// Get the full body text.
    #[must_use]
    pub fn body(&self) -> String {
        self.lines.join("\n").trim().to_string()
    }

    /// Ensure scroll keeps cursor visible given a viewport height.
    pub const fn ensure_visible(&mut self, viewport_height: usize) {
        if viewport_height == 0 {
            return;
        }
        if self.cursor_row < self.scroll {
            self.scroll = self.cursor_row;
        } else if self.cursor_row >= self.scroll + viewport_height {
            self.scroll = self.cursor_row - viewport_height + 1;
        }
    }

    fn clamp_col(&mut self) {
        let line_len = self.lines[self.cursor_row].chars().count();
        if self.cursor_col > line_len {
            self.cursor_col = line_len;
        }
    }
}

/// Convert a character index to a byte index in a string.
fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map_or(s.len(), |(byte_idx, _)| byte_idx)
}

impl LayoutMode {
    /// Determine layout mode from terminal width
    #[must_use]
    pub const fn from_width(width: u16) -> Self {
        match width {
            w if w >= 130 => Self::Full,
            w if w >= 100 => Self::Compact,
            w if w >= 80 => Self::Overlay,
            _ => Self::Single,
        }
    }

    /// Get sidebar width for this layout mode
    #[must_use]
    pub const fn sidebar_width(self) -> u16 {
        match self {
            Self::Full => 34,
            Self::Compact => 30,
            Self::Overlay => 28,
            Self::Single => 0,
        }
    }
}

/// Filter for review list
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewFilter {
    #[default]
    All,
    Open,
    Closed,
}

/// Application state
#[allow(clippy::struct_excessive_bools)] // TUI state inherently needs many boolean flags
pub struct Model {
    // === Screen state ===
    pub screen: Screen,
    pub focus: Focus,
    pub previous_focus: Option<Focus>,

    // === Data ===
    pub reviews: Vec<ReviewSummary>,
    pub current_review: Option<ReviewDetail>,
    pub threads: Vec<ThreadSummary>,
    pub current_thread: Option<ThreadDetail>,
    pub all_comments: HashMap<String, Vec<Comment>>,
    /// Parsed diff for the currently selected file
    pub current_diff: Option<ParsedDiff>,
    /// File content for context when no diff available
    pub current_file_content: Option<FileContent>,
    /// Cache for all files in the review stream
    pub file_cache: HashMap<String, FileCacheEntry>,
    /// Syntax highlighter
    pub highlighter: Highlighter,
    /// Cached highlighted lines for current diff (indexed by display line)
    pub highlighted_lines: Vec<Vec<HighlightSpan>>,

    // === UI state ===
    /// Selected index in review list
    pub list_index: usize,
    /// Scroll offset in review list
    pub list_scroll: usize,
    /// Selected file index in sidebar
    pub file_index: usize,
    /// Selected index in the flat sidebar tree
    pub sidebar_index: usize,
    /// Scroll offset for sidebar tree
    pub sidebar_scroll: usize,
    /// Files whose thread children are collapsed
    pub collapsed_files: HashSet<String>,
    /// Scroll offset in diff pane
    pub diff_scroll: usize,
    /// Line cursor position in diff pane (stream row index)
    pub diff_cursor: usize,
    /// Currently expanded thread ID
    pub expanded_thread: Option<String>,
    /// Review list filter
    pub filter: ReviewFilter,
    /// Show sidebar in overlay mode
    pub sidebar_visible: bool,
    /// Diff view mode (unified or side-by-side)
    pub diff_view_mode: DiffViewMode,
    /// Wrap diff lines when enabled
    pub diff_wrap: bool,
    /// Pending editor launch request
    pub pending_editor_request: Option<EditorRequest>,
    /// Pending comment-via-$EDITOR request (Shift+A)
    pub pending_comment_request: Option<CommentRequest>,
    /// Inline comment editor state (a)
    pub inline_editor: Option<InlineEditor>,
    /// Comment ready for persistence (from inline editor submit)
    pub pending_comment_submission: Option<PendingCommentSubmission>,
    /// Thread status change ready for persistence.
    pub pending_thread_status_change: Option<PendingThreadStatusChange>,

    // === Command Palette ===
    pub command_palette_input: String,
    pub command_palette_selection: usize,
    pub command_palette_commands: Vec<CommandSpec>,
    pub command_palette_mode: PaletteMode,

    // === Visual Selection ===
    /// Whether visual line selection mode is active (Shift+V)
    pub visual_mode: bool,
    /// Anchor stream row where visual mode was entered
    pub visual_anchor: usize,

    // === Commenting State ===
    pub comment_input: String,
    pub comment_target_line: Option<u32>,

    // === Layout ===
    pub width: u16,
    pub height: u16,
    pub layout_mode: LayoutMode,

    // === Theme ===
    pub theme: Theme,
    /// Theme name before opening the picker (for revert on Esc)
    pub pre_palette_theme: Option<String>,
    pub config: UiConfig,

    // === Render-computed data ===
    /// Thread positions captured during rendering (`thread_id` → `stream_row`)
    pub thread_positions: RefCell<HashMap<String, usize>>,
    /// Total stream rows from the last render pass (for cursor clamping)
    pub max_stream_row: Cell<usize>,
    /// Diff line mapping captured during rendering: `stream_row` → new-side line number.
    /// Populated for every diff line (including all wrapped rows).
    pub line_map: RefCell<HashMap<usize, i64>>,
    /// Sorted list of stream rows that are valid cursor stops (one per logical item).
    /// Populated during rendering; used by cursor navigation to skip wrapped/padding rows.
    pub cursor_stops: RefCell<Vec<usize>>,

    // === Review list search ===
    pub search_input: String,
    pub search_active: bool,

    // === Repo path for display ===
    pub repo_path: Option<String>,

    // === Cached editor name for help bar ===
    pub editor_name: String,

    // === Flash message (transient error/status) ===
    /// Shown in the help bar area until the next keypress.
    pub flash_message: Option<String>,

    // === Control ===
    pub should_quit: bool,
    /// Flag indicating the view needs a full redraw
    pub needs_redraw: bool,

    // === Input state ===
    pub last_list_scroll: Option<(Instant, i8)>,
    pub last_sidebar_scroll: Option<(Instant, i8)>,

    // === Pending CLI navigation targets ===
    pub pending_review: Option<String>,
    pub pending_file: Option<String>,
    pub pending_thread: Option<String>,
}

impl Model {
    /// Create a new model
    #[must_use]
    pub fn new(width: u16, height: u16, config: UiConfig) -> Self {
        Self {
            screen: Screen::default(),
            focus: Focus::default(),
            previous_focus: None,
            reviews: Vec::new(),
            current_review: None,
            threads: Vec::new(),
            current_thread: None,
            all_comments: HashMap::new(),
            current_diff: None,
            current_file_content: None,
            file_cache: HashMap::new(),
            highlighter: Highlighter::new(),
            highlighted_lines: Vec::new(),
            list_index: 0,
            list_scroll: 0,
            file_index: 0,
            sidebar_index: 0,
            sidebar_scroll: 0,
            collapsed_files: HashSet::new(),
            diff_scroll: 0,
            diff_cursor: 0,
            expanded_thread: None,
            filter: ReviewFilter::default(),
            sidebar_visible: true,
            diff_view_mode: DiffViewMode::default(),
            diff_wrap: true,
            pending_editor_request: None,
            pending_comment_request: None,
            inline_editor: None,
            pending_comment_submission: None,
            pending_thread_status_change: None,
            command_palette_input: String::new(),
            command_palette_selection: 0,
            command_palette_commands: Vec::new(),
            command_palette_mode: PaletteMode::default(),
            visual_mode: false,
            visual_anchor: 0,
            comment_input: String::new(),
            comment_target_line: None,
            width,
            height,
            layout_mode: LayoutMode::from_width(width),
            theme: Theme::default(),
            pre_palette_theme: None,
            config,
            thread_positions: RefCell::new(HashMap::new()),
            max_stream_row: Cell::new(0),
            line_map: RefCell::new(HashMap::new()),
            cursor_stops: RefCell::new(Vec::new()),
            search_input: String::new(),
            search_active: false,
            repo_path: None,
            editor_name: std::env::var("EDITOR")
                .or_else(|_| std::env::var("VISUAL"))
                .ok()
                .and_then(|e| e.rsplit('/').next().map(String::from))
                .unwrap_or_else(|| "Editor".to_string()),
            flash_message: None,
            should_quit: false,
            needs_redraw: true,
            last_list_scroll: None,
            last_sidebar_scroll: None,
            pending_review: None,
            pending_file: None,
            pending_thread: None,
        }
    }

    /// Get filtered reviews based on current filter and search query
    #[must_use]
    pub fn filtered_reviews(&self) -> Vec<&ReviewSummary> {
        let status_filtered: Vec<&ReviewSummary> = match self.filter {
            ReviewFilter::All => self.reviews.iter().collect(),
            ReviewFilter::Open => self.reviews.iter().filter(|r| r.status == "open").collect(),
            ReviewFilter::Closed => self.reviews.iter().filter(|r| r.status != "open").collect(),
        };
        if self.search_input.is_empty() {
            return status_filtered;
        }
        let query = self.search_input.to_lowercase();
        status_filtered
            .into_iter()
            .filter(|r| {
                r.title.to_lowercase().contains(&query)
                    || r.review_id.to_lowercase().contains(&query)
                    || r.author.to_lowercase().contains(&query)
            })
            .collect()
    }

    /// Get unique files from threads and the diff file cache for the sidebar.
    #[must_use]
    pub fn files_with_threads(&self) -> Vec<FileEntry> {
        use std::collections::HashMap;

        let mut files: HashMap<String, (usize, usize)> = HashMap::new();

        for thread in &self.threads {
            let entry = files.entry(thread.file_path.clone()).or_insert((0, 0));
            if thread.status == "open" {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        }

        // Include cached files that have diffs but no threads.
        for path in self.file_cache.keys() {
            files.entry(path.clone()).or_insert((0, 0));
        }

        let mut result: Vec<_> = files
            .into_iter()
            .map(|(path, (open, resolved))| FileEntry {
                path,
                open_threads: open,
                resolved_threads: resolved,
            })
            .collect();

        result.sort_by(|a, b| a.path.cmp(&b.path));
        result
    }

    /// Get threads for the currently selected file
    #[must_use]
    pub fn threads_for_current_file(&self) -> Vec<&ThreadSummary> {
        let files = self.files_with_threads();
        let Some(file) = files.get(self.file_index) else {
            return Vec::new();
        };

        self.threads
            .iter()
            .filter(|t| t.file_path == file.path)
            .collect()
    }

    /// Get threads that are visible in the current diff (all threads for the file)
    #[must_use]
    pub fn visible_threads_for_current_file(&self) -> Vec<&ThreadSummary> {
        self.threads_for_current_file()
    }

    /// Build a flat list of sidebar items: files with their threads as children
    #[must_use]
    pub fn sidebar_items(&self) -> Vec<SidebarItem> {
        let files = self.files_with_threads();
        let mut items = Vec::new();

        for (file_idx, file) in files.iter().enumerate() {
            let collapsed = self.collapsed_files.contains(&file.path);
            items.push(SidebarItem::File {
                entry: file.clone(),
                file_idx,
                collapsed,
            });
            if !collapsed {
                // Add threads belonging to this file, sorted by their
                // position in the diff stream so the sidebar order matches
                // what the user sees in the diff pane.  Fall back to
                // selection_start for threads not yet positioned.
                let positions = self.thread_positions.borrow();
                let mut file_threads: Vec<&ThreadSummary> = self
                    .threads
                    .iter()
                    .filter(|t| t.file_path == file.path)
                    .collect();
                file_threads
                    .sort_by_key(|t| positions.get(&t.thread_id).copied().unwrap_or(usize::MAX));

                for thread in file_threads {
                    items.push(SidebarItem::Thread {
                        thread_id: thread.thread_id.clone(),
                        status: thread.status.clone(),
                        comment_count: thread.comment_count,
                        file_idx,
                    });
                }
            }
        }

        items
    }

    /// Handle terminal resize
    pub const fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.layout_mode = LayoutMode::from_width(width);
    }

    /// Get the visible height for the review list (accounting for chrome)
    #[must_use]
    pub const fn list_visible_height(&self) -> usize {
        // Account for header block (5) + search bar (2) + help bar (2)
        // Each item is 2 lines tall
        let available = self.height.saturating_sub(9) as usize;
        available / 2
    }

    /// Sync current file fields from the file cache
    pub fn sync_active_file_cache(&mut self) {
        let files = self.files_with_threads();
        let Some(file) = files.get(self.file_index) else {
            self.current_diff = None;
            self.current_file_content = None;
            self.highlighted_lines.clear();
            return;
        };

        if let Some(entry) = self.file_cache.get(&file.path) {
            self.current_diff = entry.diff.clone();
            self.current_file_content = entry.file_content.clone();
            self.highlighted_lines = entry.highlighted_lines.clone();
        } else {
            self.current_diff = None;
            self.current_file_content = None;
            self.highlighted_lines.clear();
        }
    }
}

/// File entry for sidebar display
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub open_threads: usize,
    pub resolved_threads: usize,
}

/// An item in the sidebar tree (file or thread)
#[derive(Debug, Clone)]
pub enum SidebarItem {
    File {
        entry: FileEntry,
        /// Index into `files_with_threads()` for selection matching
        file_idx: usize,
        /// Whether this file's threads are collapsed
        collapsed: bool,
    },
    Thread {
        thread_id: String,
        status: String,
        comment_count: i64,
        /// Parent file index for selection matching
        file_idx: usize,
    },
}

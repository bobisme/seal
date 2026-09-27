//! Shared types for review data and the `SealClient` trait.

use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Summary of a review for list views.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub review_id: String,
    pub title: String,
    pub author: String,
    pub status: String,
    pub thread_count: i64,
    pub open_thread_count: i64,
    #[serde(default)]
    pub reviewers: Vec<String>,
}

/// Full details of a review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewDetail {
    pub review_id: String,
    pub jj_change_id: String,
    pub scm_kind: String,
    pub scm_anchor: String,
    pub initial_commit: String,
    pub final_commit: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub author: String,
    pub created_at: String,
    pub status: String,
    pub status_changed_at: Option<String>,
    pub status_changed_by: Option<String>,
    pub abandon_reason: Option<String>,
    pub thread_count: i64,
    pub open_thread_count: i64,
}

/// Summary of a thread for list views.
///
/// `selection_start`/`selection_end` are the line(s) the thread was created
/// on. After a rebase or retarget those lines can drift, so readers that
/// need to *place* the thread (in the diff, in a context window, in sort
/// order) should go through [`ThreadSummary::effective_selection`] rather
/// than reading `selection_start`/`selection_end` directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub file_path: String,
    pub selection_start: i64,
    pub selection_end: Option<i64>,
    pub status: String,
    pub comment_count: i64,
    /// Where the anchor currently maps to in the review's target commit,
    /// when it is still intact (`Unchanged` or `Shifted`). `None` when the
    /// thread is outdated (see [`ThreadSummary::outdated`]).
    #[serde(default)]
    pub current_selection_start: Option<i64>,
    #[serde(default)]
    pub current_selection_end: Option<i64>,
    /// Short drift label from `seal_core::anchor::AnchorLocation::drift_label`
    /// (e.g. `unchanged`, `shifted(+2)`, `modified`, `deleted`, `unknown`).
    #[serde(default)]
    pub drift_status: String,
    /// True when the anchored lines were edited, removed, or could not be
    /// mapped at all. An outdated thread must still be shown, marked
    /// outdated, never hidden or placed at a guessed line.
    #[serde(default)]
    pub outdated: bool,
    /// Small code preview for this thread, read from its own anchor commit
    /// rather than the review's current target. Only ever set for a
    /// `modified`/`deleted` thread (never `unknown`, which has no commit
    /// that can be trusted to read), and only when the read succeeded — see
    /// [`OutdatedPreview`]. `None` means: render the plain comment block,
    /// same as before this field existed.
    #[serde(default)]
    pub outdated_preview: Option<OutdatedPreview>,
}

/// Code preview for an outdated thread, read from the thread's own anchor
/// commit (`commit_hash`) once at review load — mirrors the CLI's
/// `context_source` (`crates/seal-cli/src/cli/commands/reviews.rs`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutdatedPreview {
    /// Full commit hash the preview was read from (the thread's anchor
    /// commit, i.e. `commit_hash` on the underlying core thread).
    pub commit: String,
    /// 1-based line number of `lines[0]`.
    pub start_line: i64,
    /// The text lines in the window: the anchored range plus a little
    /// context, already clamped to a small size regardless of how large
    /// the stored anchor range claims to be (see `MAX_PREVIEW_ANCHOR_LINES`
    /// in `core_client`).
    pub lines: Vec<String>,
    /// First line of the anchored range within `lines` (absolute line
    /// number, same numbering as `start_line`).
    pub anchor_start: i64,
    /// Last line of the anchored range, inclusive. May be less than the
    /// thread's own stored/mapped end when the range was clamped.
    pub anchor_end: i64,
}

impl ThreadSummary {
    /// The line(s) to use when placing this thread: the mapped current
    /// position when the anchor is intact, otherwise the originally stored
    /// position (stable for sorting/labeling; never used to anchor an
    /// outdated thread inline in the diff — callers must check `outdated`
    /// before doing that).
    #[must_use]
    pub fn effective_selection(&self) -> (i64, Option<i64>) {
        if self.outdated {
            (self.selection_start, self.selection_end)
        } else {
            (
                self.current_selection_start.unwrap_or(self.selection_start),
                self.current_selection_end.or(self.selection_end),
            )
        }
    }

    /// First line of [`Self::effective_selection`].
    #[must_use]
    pub fn effective_start(&self) -> i64 {
        self.effective_selection().0
    }

    /// Last line of [`Self::effective_selection`] (equal to the start for a
    /// single-line selection).
    #[must_use]
    pub fn effective_end(&self) -> i64 {
        let (start, end) = self.effective_selection();
        end.unwrap_or(start)
    }
}

/// Full details of a thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadDetail {
    pub thread_id: String,
    pub review_id: String,
    pub file_path: String,
    pub selection_type: String,
    pub selection_start: i64,
    pub selection_end: Option<i64>,
    pub commit_hash: String,
    pub author: String,
    pub created_at: String,
    pub status: String,
    pub status_changed_at: Option<String>,
    pub status_changed_by: Option<String>,
    pub resolve_reason: Option<String>,
    pub reopen_reason: Option<String>,
}

/// A single comment in a thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub comment_id: String,
    pub author: String,
    pub body: String,
    pub created_at: String,
}

/// Per-file diff and content data from seal.
pub struct FileData {
    pub path: String,
    /// Unified diff text for this file (if available).
    pub diff: Option<String>,
    /// Windowed file content for orphaned thread context.
    pub content: Option<FileContentData>,
}

/// Windowed file content returned by seal for orphaned threads.
pub struct FileContentData {
    /// 1-based line number of the first line in `lines`.
    pub start_line: i64,
    pub lines: Vec<String>,
}

/// Bundle of review data loaded in one call.
pub struct ReviewData {
    pub detail: ReviewDetail,
    pub threads: Vec<ThreadSummary>,
    pub comments: HashMap<String, Vec<Comment>>,
    /// Per-file diffs and content (populated when `--include-diffs` is used).
    pub files: Vec<FileData>,
}

/// Trait for loading review data from any backend.
pub trait SealClient {
    /// List reviews, optionally filtered by status.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend query fails.
    fn list_reviews(&self, status: Option<&str>) -> Result<Vec<ReviewSummary>>;

    /// Load full review data (detail, threads, comments) for a review.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend query fails.
    fn load_review_data(&self, review_id: &str) -> Result<Option<ReviewData>>;

    /// Add a comment to a review on specific lines (auto-creates thread).
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI call fails.
    fn comment(
        &self,
        review_id: &str,
        file_path: &str,
        start_line: i64,
        end_line: Option<i64>,
        body: &str,
    ) -> Result<()>;

    /// Reply to an existing thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI call fails.
    fn reply(&self, thread_id: &str, body: &str) -> Result<()>;

    /// Resolve an open thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend update fails.
    fn resolve_thread(&self, thread_id: &str) -> Result<()>;

    /// Reopen a resolved thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend update fails.
    fn reopen_thread(&self, thread_id: &str) -> Result<()>;
}

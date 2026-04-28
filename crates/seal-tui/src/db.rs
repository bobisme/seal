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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub file_path: String,
    pub selection_start: i64,
    pub selection_end: Option<i64>,
    pub status: String,
    pub comment_count: i64,
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

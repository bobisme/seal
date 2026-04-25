//! Query API for the projection database.
//!
//! Provides structured access to reviews, threads, and comments
//! with optional filtering. All result types implement Serialize
//! for structured output.

use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension, Row};
use serde::Serialize;

use super::ProjectionDb;

// ============================================================================
// Query Result Types
// ============================================================================

/// Summary of a review for list views.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewSummary {
    pub review_id: String,
    pub jj_change_id: String,
    pub scm_kind: String,
    pub scm_anchor: String,
    pub title: String,
    pub author: String,
    pub status: String,
    pub thread_count: i64,
    pub open_thread_count: i64,
    pub reviewers: Vec<String>,
}

/// Full details of a review.
#[derive(Debug, Clone, Serialize)]
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
    pub reviewers: Vec<String>,
    pub votes: Vec<ReviewerVote>,
}

/// A reviewer's vote on a review.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewerVote {
    pub reviewer: String,
    pub vote: String,
    pub reason: Option<String>,
    pub voted_at: String,
}

/// Summary of a thread for list views.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub file_path: String,
    pub selection_start: i64,
    pub selection_end: Option<i64>,
    pub status: String,
    pub comment_count: i64,
}

/// Full details of a thread with comments.
#[derive(Debug, Clone, Serialize)]
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
    pub comments: Vec<Comment>,
}

/// A single comment in a thread.
#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    pub comment_id: String,
    pub author: String,
    pub body: String,
    pub created_at: String,
}

/// A review awaiting the agent's vote.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewAwaitingVote {
    pub review_id: String,
    pub title: String,
    pub author: String,
    pub status: String,
    pub open_thread_count: i64,
    pub requested_at: String,
    /// "fresh" if reviewer has never voted, "re-review" if they voted but author re-requested.
    pub request_status: String,
}

/// A thread with new responses since the agent's last comment.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadWithNewResponses {
    pub thread_id: String,
    pub review_id: String,
    pub review_title: String,
    pub file_path: String,
    pub selection_start: i64,
    pub status: String,
    pub my_last_comment_at: String,
    pub new_response_count: i64,
    pub latest_response_at: String,
}

/// An open thread on a review I authored (feedback to address).
#[derive(Debug, Clone, Serialize)]
pub struct OpenThreadOnMyReview {
    pub thread_id: String,
    pub review_id: String,
    pub review_title: String,
    pub file_path: String,
    pub selection_start: i64,
    pub thread_author: String,
    pub comment_count: i64,
    pub latest_comment_at: String,
}

/// Complete inbox summary for an agent.
#[derive(Debug, Clone, Serialize)]
pub struct InboxSummary {
    pub reviews_awaiting_vote: Vec<ReviewAwaitingVote>,
    pub threads_with_new_responses: Vec<ThreadWithNewResponses>,
    pub open_threads_on_my_reviews: Vec<OpenThreadOnMyReview>,
}

// ============================================================================
// Query Functions
// ============================================================================

impl ProjectionDb {
    /// List reviews with optional filtering.
    ///
    /// Returns reviews sorted by creation date (newest first).
    pub fn list_reviews(
        &self,
        status: Option<&str>,
        author: Option<&str>,
    ) -> Result<Vec<ReviewSummary>> {
        self.list_reviews_filtered(status, author, None, false)
    }

    /// List reviews with extended filtering options.
    ///
    /// - `needs_reviewer`: Only return reviews where this agent is a requested reviewer
    /// - `has_unresolved`: Only return reviews with open_thread_count > 0
    pub fn list_reviews_filtered(
        &self,
        status: Option<&str>,
        author: Option<&str>,
        needs_reviewer: Option<&str>,
        has_unresolved: bool,
    ) -> Result<Vec<ReviewSummary>> {
        let mut sql = String::from(
            "SELECT DISTINCT v.review_id, v.jj_change_id, v.scm_kind, v.scm_anchor, v.title, v.author, v.status, v.thread_count, v.open_thread_count
             FROM v_reviews_summary v",
        );
        let mut param_values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        // Join with review_reviewers if filtering by reviewer
        if needs_reviewer.is_some() {
            sql.push_str(" JOIN review_reviewers rr ON v.review_id = rr.review_id");
        }

        sql.push_str(" WHERE 1=1");

        if let Some(s) = status {
            sql.push_str(" AND v.status = ?");
            param_values.push(Box::new(s.to_string()));
        }
        if let Some(a) = author {
            sql.push_str(" AND v.author = ?");
            param_values.push(Box::new(a.to_string()));
        }
        if let Some(r) = needs_reviewer {
            sql.push_str(" AND rr.reviewer = ?");
            param_values.push(Box::new(r.to_string()));
        }
        if has_unresolved {
            sql.push_str(" AND v.open_thread_count > 0");
        }

        sql.push_str(" ORDER BY v.created_at DESC");

        let params: Vec<&dyn rusqlite::ToSql> = param_values
            .iter()
            .map(std::convert::AsRef::as_ref)
            .collect();

        let mut stmt = self
            .conn
            .prepare(&sql)
            .context("Failed to prepare list_reviews query")?;

        let rows = stmt
            .query_map(params.as_slice(), |row| {
                Ok(ReviewSummary {
                    review_id: row.get(0)?,
                    jj_change_id: row.get(1)?,
                    scm_kind: row.get(2)?,
                    scm_anchor: row.get(3)?,
                    title: row.get(4)?,
                    author: row.get(5)?,
                    status: row.get(6)?,
                    thread_count: row.get(7)?,
                    open_thread_count: row.get(8)?,
                    reviewers: Vec::new(), // populated below
                })
            })
            .context("Failed to execute list_reviews query")?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.context("Failed to read review row")?);
        }

        // Batch-fetch reviewers for all returned reviews
        if !results.is_empty() {
            let mut reviewer_stmt = self
                .conn
                .prepare(
                    "SELECT reviewer FROM review_reviewers WHERE review_id = ? ORDER BY requested_at",
                )
                .context("Failed to prepare reviewers query")?;

            for review in &mut results {
                let reviewers: Vec<String> = reviewer_stmt
                    .query_map(params![review.review_id], |row| row.get(0))
                    .context("Failed to query reviewers")?
                    .collect::<Result<Vec<_>, _>>()
                    .context("Failed to read reviewers")?;
                review.reviewers = reviewers;
            }
        }

        Ok(results)
    }

    /// Get detailed information about a single review.
    ///
    /// Returns `None` if the review doesn't exist.
    pub fn get_review(&self, review_id: &str) -> Result<Option<ReviewDetail>> {
        // Get the review with thread counts
        let review_row: Option<ReviewDetailRow> = self
            .conn
            .query_row(
                "SELECT 
                    r.review_id, r.jj_change_id, r.scm_kind, r.scm_anchor, r.initial_commit, r.final_commit,
                    r.title, r.description, r.author, r.created_at, r.status,
                    r.status_changed_at, r.status_changed_by, r.abandon_reason,
                    COALESCE(s.thread_count, 0), COALESCE(s.open_thread_count, 0)
                 FROM reviews r
                 LEFT JOIN v_reviews_summary s ON s.review_id = r.review_id
                 WHERE r.review_id = ?",
                params![review_id],
                ReviewDetailRow::from_row,
            )
            .optional()
            .context("Failed to query review")?;

        let Some(row) = review_row else {
            return Ok(None);
        };

        // Get the reviewers
        let mut stmt = self
            .conn
            .prepare(
                "SELECT reviewer FROM review_reviewers WHERE review_id = ? ORDER BY requested_at",
            )
            .context("Failed to prepare reviewers query")?;

        let reviewers: Vec<String> = stmt
            .query_map(params![review_id], |row| row.get(0))
            .context("Failed to query reviewers")?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to read reviewers")?;

        // Get the votes
        let votes = self.get_votes(review_id)?;

        Ok(Some(ReviewDetail {
            review_id: row.review_id,
            jj_change_id: row.jj_change_id,
            scm_kind: row.scm_kind,
            scm_anchor: row.scm_anchor,
            initial_commit: row.initial_commit,
            final_commit: row.final_commit,
            title: row.title,
            description: row.description,
            author: row.author,
            created_at: row.created_at,
            status: row.status,
            status_changed_at: row.status_changed_at,
            status_changed_by: row.status_changed_by,
            abandon_reason: row.abandon_reason,
            thread_count: row.thread_count,
            open_thread_count: row.open_thread_count,
            reviewers,
            votes,
        }))
    }

    /// List threads for a review with optional filtering.
    ///
    /// Returns threads sorted by file path, then line number.
    pub fn list_threads(
        &self,
        review_id: &str,
        status: Option<&str>,
        file: Option<&str>,
    ) -> Result<Vec<ThreadSummary>> {
        let mut sql = String::from(
            "SELECT thread_id, file_path, selection_start, selection_end, effective_status, comment_count
             FROM v_threads_detail
             WHERE review_id = ?",
        );
        let mut param_values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        param_values.push(Box::new(review_id.to_string()));

        if let Some(s) = status {
            sql.push_str(" AND effective_status = ?");
            param_values.push(Box::new(s.to_string()));
        }
        if let Some(f) = file {
            sql.push_str(" AND file_path = ?");
            param_values.push(Box::new(f.to_string()));
        }

        sql.push_str(" ORDER BY file_path, selection_start");

        let params: Vec<&dyn rusqlite::ToSql> = param_values
            .iter()
            .map(std::convert::AsRef::as_ref)
            .collect();

        let mut stmt = self
            .conn
            .prepare(&sql)
            .context("Failed to prepare list_threads query")?;

        let rows = stmt
            .query_map(params.as_slice(), |row| {
                Ok(ThreadSummary {
                    thread_id: row.get(0)?,
                    file_path: row.get(1)?,
                    selection_start: row.get(2)?,
                    selection_end: row.get(3)?,
                    status: row.get(4)?,
                    comment_count: row.get(5)?,
                })
            })
            .context("Failed to execute list_threads query")?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.context("Failed to read thread row")?);
        }
        Ok(results)
    }

    /// Find an existing open thread at a specific file and line.
    ///
    /// Returns the thread_id if a thread exists at the location, or None.
    /// For single-line threads, matches exact `selection_start`.
    /// For range threads, matches if line falls within `[selection_start, selection_end]`.
    /// Only returns open threads (not resolved ones).
    pub fn find_thread_at_location(
        &self,
        review_id: &str,
        file_path: &str,
        line: i64,
    ) -> Result<Option<String>> {
        let result: Option<String> = self
            .conn
            .query_row(
                "SELECT thread_id FROM threads
                 WHERE review_id = ? AND file_path = ? AND status = 'open'
                   AND selection_start <= ?
                   AND COALESCE(selection_end, selection_start) >= ?
                 LIMIT 1",
                rusqlite::params![review_id, file_path, line, line],
                |row| row.get(0),
            )
            .optional()
            .context("Failed to query for existing thread")?;

        Ok(result)
    }

    /// Get detailed information about a single thread with its comments.
    ///
    /// Returns `None` if the thread doesn't exist.
    pub fn get_thread(&self, thread_id: &str) -> Result<Option<ThreadDetail>> {
        let thread_row: Option<ThreadDetailRow> = self
            .conn
            .query_row(
                "SELECT 
                    thread_id, review_id, file_path, selection_type,
                    selection_start, selection_end, commit_hash, author,
                    created_at, status, status_changed_at, status_changed_by,
                    resolve_reason, reopen_reason
                 FROM threads
                 WHERE thread_id = ?",
                params![thread_id],
                ThreadDetailRow::from_row,
            )
            .optional()
            .context("Failed to query thread")?;

        let Some(row) = thread_row else {
            return Ok(None);
        };

        // Get comments for this thread
        let comments = self.list_comments(thread_id)?;

        Ok(Some(ThreadDetail {
            thread_id: row.thread_id,
            review_id: row.review_id,
            file_path: row.file_path,
            selection_type: row.selection_type,
            selection_start: row.selection_start,
            selection_end: row.selection_end,
            commit_hash: row.commit_hash,
            author: row.author,
            created_at: row.created_at,
            status: row.status,
            status_changed_at: row.status_changed_at,
            status_changed_by: row.status_changed_by,
            resolve_reason: row.resolve_reason,
            reopen_reason: row.reopen_reason,
            comments,
        }))
    }

    /// Get all votes for a review.
    ///
    /// Returns votes sorted by vote time (oldest first).
    pub fn get_votes(&self, review_id: &str) -> Result<Vec<ReviewerVote>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT reviewer, vote, reason, voted_at
                 FROM reviewer_votes
                 WHERE review_id = ?
                 ORDER BY voted_at ASC",
            )
            .context("Failed to prepare get_votes query")?;

        let rows = stmt
            .query_map(params![review_id], |row| {
                Ok(ReviewerVote {
                    reviewer: row.get(0)?,
                    vote: row.get(1)?,
                    reason: row.get(2)?,
                    voted_at: row.get(3)?,
                })
            })
            .context("Failed to execute get_votes query")?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.context("Failed to read vote row")?);
        }
        Ok(results)
    }

    /// Check if a review has any blocking votes.
    ///
    /// Returns true if there is at least one "block" vote.
    pub fn has_blocking_votes(&self, review_id: &str) -> Result<bool> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM reviewer_votes WHERE review_id = ? AND vote = 'block'",
                params![review_id],
                |row| row.get(0),
            )
            .context("Failed to check for blocking votes")?;

        Ok(count > 0)
    }

    /// Check if a review has blocking votes from reviewers other than the specified one.
    ///
    /// Used for auto-approval logic: when a reviewer votes LGTM, we only auto-approve
    /// if no OTHER reviewers have blocking votes.
    pub fn has_blocking_votes_from_others(
        &self,
        review_id: &str,
        exclude_reviewer: &str,
    ) -> Result<bool> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM reviewer_votes WHERE review_id = ? AND vote = 'block' AND reviewer != ?",
                params![review_id, exclude_reviewer],
                |row| row.get(0),
            )
            .context("Failed to check for blocking votes from others")?;

        Ok(count > 0)
    }

    /// List all comments for a thread.
    ///
    /// Returns comments sorted by creation time (oldest first).
    pub fn list_comments(&self, thread_id: &str) -> Result<Vec<Comment>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT comment_id, author, body, created_at
                 FROM comments
                 WHERE thread_id = ?
                 ORDER BY created_at ASC",
            )
            .context("Failed to prepare list_comments query")?;

        let rows = stmt
            .query_map(params![thread_id], |row| {
                Ok(Comment {
                    comment_id: row.get(0)?,
                    author: row.get(1)?,
                    body: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })
            .context("Failed to execute list_comments query")?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.context("Failed to read comment row")?);
        }
        Ok(results)
    }

    /// Get the next comment number for a thread.
    ///
    /// Returns the next sequential number to use for a comment ID (e.g., 1, 2, 3...).
    /// Returns `None` if the thread doesn't exist.
    pub fn get_next_comment_number(&self, thread_id: &str) -> Result<Option<u32>> {
        let result: Option<i64> = self
            .conn
            .query_row(
                "SELECT next_comment_number FROM threads WHERE thread_id = ?",
                params![thread_id],
                |row| row.get(0),
            )
            .optional()
            .context("Failed to query next_comment_number")?;

        Ok(result.map(|n| n as u32))
    }

    // ========================================================================
    // Inbox Queries
    // ========================================================================

    /// Get complete inbox summary for an agent.
    pub fn get_inbox(&self, agent: &str) -> Result<InboxSummary> {
        Ok(InboxSummary {
            reviews_awaiting_vote: self.get_reviews_awaiting_vote(agent)?,
            threads_with_new_responses: self.get_threads_with_new_responses(agent)?,
            open_threads_on_my_reviews: self.get_open_threads_on_my_reviews(agent)?,
        })
    }

    /// Get reviews where the agent is a requested reviewer but hasn't voted,
    /// or where they voted but the author re-requested review.
    ///
    /// Only includes open/approved reviews (not merged/abandoned).
    /// Returns `request_status` = 'fresh' for never voted, 're-review' for re-requested after vote.
    pub fn get_reviews_awaiting_vote(&self, agent: &str) -> Result<Vec<ReviewAwaitingVote>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT
                    r.review_id, r.title, r.author, r.status,
                    COALESCE(s.open_thread_count, 0) as open_thread_count,
                    rr.requested_at,
                    CASE
                        WHEN v.vote IS NULL THEN 'fresh'
                        ELSE 're-review'
                    END as request_status
                 FROM review_reviewers rr
                 JOIN reviews r ON r.review_id = rr.review_id
                 LEFT JOIN v_reviews_summary s ON s.review_id = r.review_id
                 LEFT JOIN reviewer_votes v ON v.review_id = rr.review_id AND v.reviewer = rr.reviewer
                 WHERE rr.reviewer = ?
                   AND r.status IN ('open', 'approved')
                   AND (v.vote IS NULL OR rr.requested_at > v.voted_at)
                 ORDER BY rr.requested_at DESC",
            )
            .context("Failed to prepare reviews_awaiting_vote query")?;

        let rows = stmt
            .query_map(params![agent], |row| {
                Ok(ReviewAwaitingVote {
                    review_id: row.get(0)?,
                    title: row.get(1)?,
                    author: row.get(2)?,
                    status: row.get(3)?,
                    open_thread_count: row.get(4)?,
                    requested_at: row.get(5)?,
                    request_status: row.get(6)?,
                })
            })
            .context("Failed to execute reviews_awaiting_vote query")?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.context("Failed to read review row")?);
        }
        Ok(results)
    }

    /// Get threads where the agent has commented but there are newer comments from others.
    ///
    /// Only includes open threads on open/approved reviews.
    pub fn get_threads_with_new_responses(
        &self,
        agent: &str,
    ) -> Result<Vec<ThreadWithNewResponses>> {
        let mut stmt = self
            .conn
            .prepare(
                "WITH my_last_comment AS (
                    SELECT thread_id, MAX(created_at) as last_at
                    FROM comments
                    WHERE author = ?
                    GROUP BY thread_id
                ),
                new_responses AS (
                    SELECT 
                        c.thread_id,
                        COUNT(*) as new_count,
                        MAX(c.created_at) as latest_at
                    FROM comments c
                    JOIN my_last_comment m ON m.thread_id = c.thread_id
                    WHERE c.author != ? AND c.created_at > m.last_at
                    GROUP BY c.thread_id
                )
                SELECT 
                    t.thread_id, t.review_id, r.title, t.file_path,
                    t.selection_start, t.status,
                    m.last_at, n.new_count, n.latest_at
                FROM threads t
                JOIN reviews r ON r.review_id = t.review_id
                JOIN my_last_comment m ON m.thread_id = t.thread_id
                JOIN new_responses n ON n.thread_id = t.thread_id
                WHERE t.status = 'open'
                  AND r.status IN ('open', 'approved')
                ORDER BY n.latest_at DESC",
            )
            .context("Failed to prepare threads_with_new_responses query")?;

        let rows = stmt
            .query_map(params![agent, agent], |row| {
                Ok(ThreadWithNewResponses {
                    thread_id: row.get(0)?,
                    review_id: row.get(1)?,
                    review_title: row.get(2)?,
                    file_path: row.get(3)?,
                    selection_start: row.get(4)?,
                    status: row.get(5)?,
                    my_last_comment_at: row.get(6)?,
                    new_response_count: row.get(7)?,
                    latest_response_at: row.get(8)?,
                })
            })
            .context("Failed to execute threads_with_new_responses query")?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.context("Failed to read thread row")?);
        }
        Ok(results)
    }

    /// Get open threads on reviews where the agent is the author.
    ///
    /// This shows feedback that the agent needs to address.
    /// Only includes open/approved reviews.
    pub fn get_open_threads_on_my_reviews(&self, agent: &str) -> Result<Vec<OpenThreadOnMyReview>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT 
                    t.thread_id, t.review_id, r.title, t.file_path,
                    t.selection_start, t.author,
                    COUNT(c.comment_id) as comment_count,
                    MAX(c.created_at) as latest_comment_at
                 FROM threads t
                 JOIN reviews r ON r.review_id = t.review_id
                 LEFT JOIN comments c ON c.thread_id = t.thread_id
                 WHERE r.author = ?
                   AND r.status IN ('open', 'approved')
                   AND t.status = 'open'
                 GROUP BY t.thread_id
                 ORDER BY MAX(c.created_at) DESC NULLS LAST, t.created_at DESC",
            )
            .context("Failed to prepare open_threads_on_my_reviews query")?;

        let rows = stmt
            .query_map(params![agent], |row| {
                Ok(OpenThreadOnMyReview {
                    thread_id: row.get(0)?,
                    review_id: row.get(1)?,
                    review_title: row.get(2)?,
                    file_path: row.get(3)?,
                    selection_start: row.get(4)?,
                    thread_author: row.get(5)?,
                    comment_count: row.get(6)?,
                    latest_comment_at: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
                })
            })
            .context("Failed to execute open_threads_on_my_reviews query")?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.context("Failed to read thread row")?);
        }
        Ok(results)
    }
}

// ============================================================================
// Internal Row Types (for query mapping)
// ============================================================================

/// Internal type for reading review details from the database.
struct ReviewDetailRow {
    review_id: String,
    jj_change_id: String,
    scm_kind: String,
    scm_anchor: String,
    initial_commit: String,
    final_commit: Option<String>,
    title: String,
    description: Option<String>,
    author: String,
    created_at: String,
    status: String,
    status_changed_at: Option<String>,
    status_changed_by: Option<String>,
    abandon_reason: Option<String>,
    thread_count: i64,
    open_thread_count: i64,
}

impl ReviewDetailRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            review_id: row.get(0)?,
            jj_change_id: row.get(1)?,
            scm_kind: row.get(2)?,
            scm_anchor: row.get(3)?,
            initial_commit: row.get(4)?,
            final_commit: row.get(5)?,
            title: row.get(6)?,
            description: row.get(7)?,
            author: row.get(8)?,
            created_at: row.get(9)?,
            status: row.get(10)?,
            status_changed_at: row.get(11)?,
            status_changed_by: row.get(12)?,
            abandon_reason: row.get(13)?,
            thread_count: row.get(14)?,
            open_thread_count: row.get(15)?,
        })
    }
}

/// Internal type for reading thread details from the database.
struct ThreadDetailRow {
    thread_id: String,
    review_id: String,
    file_path: String,
    selection_type: String,
    selection_start: i64,
    selection_end: Option<i64>,
    commit_hash: String,
    author: String,
    created_at: String,
    status: String,
    status_changed_at: Option<String>,
    status_changed_by: Option<String>,
    resolve_reason: Option<String>,
    reopen_reason: Option<String>,
}

impl ThreadDetailRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            thread_id: row.get(0)?,
            review_id: row.get(1)?,
            file_path: row.get(2)?,
            selection_type: row.get(3)?,
            selection_start: row.get(4)?,
            selection_end: row.get(5)?,
            commit_hash: row.get(6)?,
            author: row.get(7)?,
            created_at: row.get(8)?,
            status: row.get(9)?,
            status_changed_at: row.get(10)?,
            status_changed_by: row.get(11)?,
            resolve_reason: row.get(12)?,
            reopen_reason: row.get(13)?,
        })
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{
        CodeSelection, CommentAdded, Event, EventEnvelope, ReviewAbandoned, ReviewCreated,
        ReviewMerged, ReviewerVoted, ReviewersRequested, ThreadCreated, ThreadResolved, VoteType,
    };
    use crate::projection::apply_event;
    use chrono::{DateTime, Duration, Utc};

    fn setup_db() -> ProjectionDb {
        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db
    }

    fn make_review(review_id: &str, author: &str, title: &str) -> EventEnvelope {
        EventEnvelope::new(
            author,
            Event::ReviewCreated(ReviewCreated {
                review_id: review_id.to_string(),
                jj_change_id: format!("change-{review_id}"),
                scm_kind: Some("jj".to_string()),
                scm_anchor: Some(format!("change-{review_id}")),
                initial_commit: format!("commit-{review_id}"),
                title: title.to_string(),
                description: Some(format!("Description for {review_id}")),
            }),
        )
    }

    fn make_thread(thread_id: &str, review_id: &str, file: &str, line: u32) -> EventEnvelope {
        EventEnvelope::new(
            "thread_author",
            Event::ThreadCreated(ThreadCreated {
                thread_id: thread_id.to_string(),
                review_id: review_id.to_string(),
                file_path: file.to_string(),
                selection: CodeSelection::line(line),
                commit_hash: "abc123".to_string(),
            }),
        )
    }

    fn make_thread_range(
        thread_id: &str,
        review_id: &str,
        file: &str,
        start: u32,
        end: u32,
    ) -> EventEnvelope {
        EventEnvelope::new(
            "thread_author",
            Event::ThreadCreated(ThreadCreated {
                thread_id: thread_id.to_string(),
                review_id: review_id.to_string(),
                file_path: file.to_string(),
                selection: CodeSelection::range(start, end),
                commit_hash: "abc123".to_string(),
            }),
        )
    }

    fn make_comment(comment_id: &str, thread_id: &str, body: &str) -> EventEnvelope {
        EventEnvelope::new(
            "commenter",
            Event::CommentAdded(CommentAdded {
                comment_id: comment_id.to_string(),
                thread_id: thread_id.to_string(),
                body: body.to_string(),
            }),
        )
    }

    // ========================================================================
    // list_reviews tests
    // ========================================================================

    #[test]
    fn test_list_reviews_empty() {
        let db = setup_db();
        let reviews = db.list_reviews(None, None).unwrap();
        assert!(reviews.is_empty());
    }

    #[test]
    fn test_list_reviews_all() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "First review")).unwrap();
        apply_event(&db, &make_review("cr-002", "bob", "Second review")).unwrap();

        let reviews = db.list_reviews(None, None).unwrap();
        assert_eq!(reviews.len(), 2);
    }

    #[test]
    fn test_list_reviews_filter_by_status() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Open review")).unwrap();
        apply_event(&db, &make_review("cr-002", "alice", "Will be merged")).unwrap();

        // Merge the second review
        apply_event(
            &db,
            &EventEnvelope::new(
                "merger",
                Event::ReviewMerged(crate::events::ReviewMerged {
                    review_id: "cr-002".to_string(),
                    final_commit: "final".to_string(),
                }),
            ),
        )
        .unwrap();

        let open_reviews = db.list_reviews(Some("open"), None).unwrap();
        assert_eq!(open_reviews.len(), 1);
        assert_eq!(open_reviews[0].review_id, "cr-001");

        let merged_reviews = db.list_reviews(Some("merged"), None).unwrap();
        assert_eq!(merged_reviews.len(), 1);
        assert_eq!(merged_reviews[0].review_id, "cr-002");
    }

    #[test]
    fn test_list_reviews_filter_by_author() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Alice's review")).unwrap();
        apply_event(&db, &make_review("cr-002", "bob", "Bob's review")).unwrap();
        apply_event(&db, &make_review("cr-003", "alice", "Another Alice review")).unwrap();

        let alice_reviews = db.list_reviews(None, Some("alice")).unwrap();
        assert_eq!(alice_reviews.len(), 2);

        let bob_reviews = db.list_reviews(None, Some("bob")).unwrap();
        assert_eq!(bob_reviews.len(), 1);
        assert_eq!(bob_reviews[0].review_id, "cr-002");
    }

    #[test]
    fn test_list_reviews_filter_combined() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Open by alice")).unwrap();
        apply_event(&db, &make_review("cr-002", "alice", "Merged by alice")).unwrap();
        apply_event(&db, &make_review("cr-003", "bob", "Open by bob")).unwrap();

        // Merge cr-002
        apply_event(
            &db,
            &EventEnvelope::new(
                "merger",
                Event::ReviewMerged(crate::events::ReviewMerged {
                    review_id: "cr-002".to_string(),
                    final_commit: "final".to_string(),
                }),
            ),
        )
        .unwrap();

        let results = db.list_reviews(Some("open"), Some("alice")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].review_id, "cr-001");
    }

    #[test]
    fn test_list_reviews_with_thread_counts() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Review with threads")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 10)).unwrap();
        apply_event(&db, &make_thread("th-002", "cr-001", "src/lib.rs", 20)).unwrap();

        // Resolve one thread
        apply_event(
            &db,
            &EventEnvelope::new(
                "resolver",
                Event::ThreadResolved(ThreadResolved {
                    thread_id: "th-001".to_string(),
                    reason: None,
                }),
            ),
        )
        .unwrap();

        let reviews = db.list_reviews(None, None).unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].thread_count, 2);
        assert_eq!(reviews[0].open_thread_count, 1);
    }

    #[test]
    fn test_list_reviews_includes_reviewers() {
        let db = setup_db();

        apply_event(
            &db,
            &make_review("cr-001", "alice", "Review with reviewers"),
        )
        .unwrap();
        apply_event(
            &db,
            &make_review("cr-002", "alice", "Review without reviewers"),
        )
        .unwrap();

        // Add reviewers to cr-001
        apply_event(
            &db,
            &EventEnvelope::new(
                "alice",
                Event::ReviewersRequested(ReviewersRequested {
                    review_id: "cr-001".to_string(),
                    reviewers: vec!["bob".to_string(), "charlie".to_string()],
                }),
            ),
        )
        .unwrap();

        let reviews = db.list_reviews(None, None).unwrap();
        assert_eq!(reviews.len(), 2);

        // Find cr-001 (may not be first due to ordering)
        let r1 = reviews.iter().find(|r| r.review_id == "cr-001").unwrap();
        assert_eq!(r1.reviewers.len(), 2);
        assert!(r1.reviewers.contains(&"bob".to_string()));
        assert!(r1.reviewers.contains(&"charlie".to_string()));

        // cr-002 should have empty reviewers
        let r2 = reviews.iter().find(|r| r.review_id == "cr-002").unwrap();
        assert!(r2.reviewers.is_empty());
    }

    // ========================================================================
    // get_review tests
    // ========================================================================

    #[test]
    fn test_get_review_not_found() {
        let db = setup_db();
        let result = db.get_review("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_get_review_basic() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Test review")).unwrap();

        let review = db.get_review("cr-001").unwrap().unwrap();
        assert_eq!(review.review_id, "cr-001");
        assert_eq!(review.title, "Test review");
        assert_eq!(review.author, "alice");
        assert_eq!(review.status, "open");
        assert_eq!(review.jj_change_id, "change-cr-001");
        assert!(review.description.is_some());
    }

    #[test]
    fn test_get_review_with_reviewers() {
        let db = setup_db();

        apply_event(
            &db,
            &make_review("cr-001", "alice", "Review with reviewers"),
        )
        .unwrap();
        apply_event(
            &db,
            &EventEnvelope::new(
                "alice",
                Event::ReviewersRequested(ReviewersRequested {
                    review_id: "cr-001".to_string(),
                    reviewers: vec!["bob".to_string(), "charlie".to_string()],
                }),
            ),
        )
        .unwrap();

        let review = db.get_review("cr-001").unwrap().unwrap();
        assert_eq!(review.reviewers.len(), 2);
        assert!(review.reviewers.contains(&"bob".to_string()));
        assert!(review.reviewers.contains(&"charlie".to_string()));
    }

    #[test]
    fn test_get_review_with_threads() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 10)).unwrap();
        apply_event(&db, &make_thread("th-002", "cr-001", "src/lib.rs", 20)).unwrap();

        let review = db.get_review("cr-001").unwrap().unwrap();
        assert_eq!(review.thread_count, 2);
        assert_eq!(review.open_thread_count, 2);
    }

    // ========================================================================
    // list_threads tests
    // ========================================================================

    #[test]
    fn test_list_threads_empty() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Empty review")).unwrap();

        let threads = db.list_threads("cr-001", None, None).unwrap();
        assert!(threads.is_empty());
    }

    #[test]
    fn test_list_threads_all() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 10)).unwrap();
        apply_event(&db, &make_thread("th-002", "cr-001", "src/lib.rs", 20)).unwrap();

        let threads = db.list_threads("cr-001", None, None).unwrap();
        assert_eq!(threads.len(), 2);
    }

    #[test]
    fn test_list_threads_filter_by_status() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 10)).unwrap();
        apply_event(&db, &make_thread("th-002", "cr-001", "src/lib.rs", 20)).unwrap();

        // Resolve first thread
        apply_event(
            &db,
            &EventEnvelope::new(
                "resolver",
                Event::ThreadResolved(ThreadResolved {
                    thread_id: "th-001".to_string(),
                    reason: None,
                }),
            ),
        )
        .unwrap();

        let open_threads = db.list_threads("cr-001", Some("open"), None).unwrap();
        assert_eq!(open_threads.len(), 1);
        assert_eq!(open_threads[0].thread_id, "th-002");

        let resolved_threads = db.list_threads("cr-001", Some("resolved"), None).unwrap();
        assert_eq!(resolved_threads.len(), 1);
        assert_eq!(resolved_threads[0].thread_id, "th-001");
    }

    #[test]
    fn test_list_threads_filter_by_file() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 10)).unwrap();
        apply_event(&db, &make_thread("th-002", "cr-001", "src/main.rs", 50)).unwrap();
        apply_event(&db, &make_thread("th-003", "cr-001", "src/lib.rs", 20)).unwrap();

        let main_threads = db
            .list_threads("cr-001", None, Some("src/main.rs"))
            .unwrap();
        assert_eq!(main_threads.len(), 2);

        let lib_threads = db.list_threads("cr-001", None, Some("src/lib.rs")).unwrap();
        assert_eq!(lib_threads.len(), 1);
    }

    #[test]
    fn test_list_threads_with_comment_counts() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 10)).unwrap();
        apply_event(&db, &make_comment("th-001.1", "th-001", "First comment")).unwrap();
        apply_event(&db, &make_comment("th-001.2", "th-001", "Second comment")).unwrap();

        let threads = db.list_threads("cr-001", None, None).unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].comment_count, 2);
    }

    #[test]
    fn test_list_threads_sorted_by_file_and_line() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 100)).unwrap();
        apply_event(&db, &make_thread("th-002", "cr-001", "src/lib.rs", 20)).unwrap();
        apply_event(&db, &make_thread("th-003", "cr-001", "src/main.rs", 10)).unwrap();

        let threads = db.list_threads("cr-001", None, None).unwrap();
        assert_eq!(threads.len(), 3);
        // Should be sorted by file, then line
        assert_eq!(threads[0].file_path, "src/lib.rs");
        assert_eq!(threads[1].file_path, "src/main.rs");
        assert_eq!(threads[1].selection_start, 10);
        assert_eq!(threads[2].file_path, "src/main.rs");
        assert_eq!(threads[2].selection_start, 100);
    }

    // ========================================================================
    // get_thread tests
    // ========================================================================

    #[test]
    fn test_get_thread_not_found() {
        let db = setup_db();
        let result = db.get_thread("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_get_thread_basic() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 42)).unwrap();

        let thread = db.get_thread("th-001").unwrap().unwrap();
        assert_eq!(thread.thread_id, "th-001");
        assert_eq!(thread.review_id, "cr-001");
        assert_eq!(thread.file_path, "src/main.rs");
        assert_eq!(thread.selection_start, 42);
        assert_eq!(thread.status, "open");
        assert!(thread.comments.is_empty());
    }

    #[test]
    fn test_get_thread_with_comments() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 10)).unwrap();
        apply_event(&db, &make_comment("th-001.1", "th-001", "First comment")).unwrap();
        apply_event(&db, &make_comment("th-001.2", "th-001", "Second comment")).unwrap();

        let thread = db.get_thread("th-001").unwrap().unwrap();
        assert_eq!(thread.comments.len(), 2);
        assert_eq!(thread.comments[0].body, "First comment");
        assert_eq!(thread.comments[1].body, "Second comment");
    }

    #[test]
    fn test_get_thread_resolved() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 10)).unwrap();
        apply_event(
            &db,
            &EventEnvelope::new(
                "resolver",
                Event::ThreadResolved(ThreadResolved {
                    thread_id: "th-001".to_string(),
                    reason: Some("Fixed the issue".to_string()),
                }),
            ),
        )
        .unwrap();

        let thread = db.get_thread("th-001").unwrap().unwrap();
        assert_eq!(thread.status, "resolved");
        assert_eq!(thread.resolve_reason, Some("Fixed the issue".to_string()));
        assert_eq!(thread.status_changed_by, Some("resolver".to_string()));
    }

    // ========================================================================
    // list_comments tests
    // ========================================================================

    #[test]
    fn test_list_comments_empty() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 10)).unwrap();

        let comments = db.list_comments("th-001").unwrap();
        assert!(comments.is_empty());
    }

    #[test]
    fn test_list_comments_ordered_by_time() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 10)).unwrap();
        apply_event(&db, &make_comment("th-001.1", "th-001", "First")).unwrap();
        apply_event(&db, &make_comment("th-001.2", "th-001", "Second")).unwrap();
        apply_event(&db, &make_comment("th-001.3", "th-001", "Third")).unwrap();

        let comments = db.list_comments("th-001").unwrap();
        assert_eq!(comments.len(), 3);
        assert_eq!(comments[0].body, "First");
        assert_eq!(comments[1].body, "Second");
        assert_eq!(comments[2].body, "Third");
    }

    #[test]
    fn test_list_comments_only_for_specified_thread() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 10)).unwrap();
        apply_event(&db, &make_thread("th-002", "cr-001", "src/lib.rs", 20)).unwrap();
        apply_event(&db, &make_comment("th-001.1", "th-001", "Thread 1 comment")).unwrap();
        apply_event(&db, &make_comment("th-002.1", "th-002", "Thread 2 comment")).unwrap();

        let comments_1 = db.list_comments("th-001").unwrap();
        assert_eq!(comments_1.len(), 1);
        assert_eq!(comments_1[0].body, "Thread 1 comment");

        let comments_2 = db.list_comments("th-002").unwrap();
        assert_eq!(comments_2.len(), 1);
        assert_eq!(comments_2[0].body, "Thread 2 comment");
    }

    // ========================================================================
    // has_blocking_votes_from_others tests
    // ========================================================================

    fn make_vote(reviewer: &str, review_id: &str, vote: VoteType) -> EventEnvelope {
        EventEnvelope::new(
            reviewer,
            Event::ReviewerVoted(ReviewerVoted {
                review_id: review_id.to_string(),
                vote,
                reason: None,
            }),
        )
    }

    #[test]
    fn test_has_blocking_votes_from_others_no_votes() {
        let db = setup_db();
        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();

        // No votes at all → no blocks from others
        assert!(!db.has_blocking_votes_from_others("cr-001", "bob").unwrap());
    }

    #[test]
    fn test_has_blocking_votes_from_others_only_own_block() {
        let db = setup_db();
        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_vote("bob", "cr-001", VoteType::Block)).unwrap();

        // Bob blocks, but we exclude bob → no blocks from others
        assert!(!db.has_blocking_votes_from_others("cr-001", "bob").unwrap());
    }

    #[test]
    fn test_has_blocking_votes_from_others_other_reviewer_blocks() {
        let db = setup_db();
        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_vote("charlie", "cr-001", VoteType::Block)).unwrap();

        // Charlie blocks, we exclude bob → charlie's block counts
        assert!(db.has_blocking_votes_from_others("cr-001", "bob").unwrap());
    }

    #[test]
    fn test_has_blocking_votes_from_others_mixed_votes() {
        let db = setup_db();
        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_vote("bob", "cr-001", VoteType::Lgtm)).unwrap();
        apply_event(&db, &make_vote("charlie", "cr-001", VoteType::Lgtm)).unwrap();

        // Both LGTM → no blocks from others
        assert!(!db.has_blocking_votes_from_others("cr-001", "bob").unwrap());
    }

    #[test]
    fn test_has_blocking_votes_from_others_self_lgtm_other_blocks() {
        let db = setup_db();
        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_vote("bob", "cr-001", VoteType::Lgtm)).unwrap();
        apply_event(&db, &make_vote("charlie", "cr-001", VoteType::Block)).unwrap();

        // Bob LGTM, Charlie blocks → blocks from others exist
        assert!(db.has_blocking_votes_from_others("cr-001", "bob").unwrap());
    }

    #[test]
    fn test_has_blocking_votes_from_others_self_block_other_lgtm() {
        let db = setup_db();
        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_vote("bob", "cr-001", VoteType::Block)).unwrap();
        apply_event(&db, &make_vote("charlie", "cr-001", VoteType::Lgtm)).unwrap();

        // Bob blocks, Charlie LGTM, exclude bob → no blocks from others
        assert!(!db.has_blocking_votes_from_others("cr-001", "bob").unwrap());
    }

    // ========================================================================
    // get_reviews_awaiting_vote tests (inbox with re-review status)
    // ========================================================================

    fn make_reviewers_requested(
        review_id: &str,
        reviewers: Vec<&str>,
        author: &str,
    ) -> EventEnvelope {
        EventEnvelope::new(
            author,
            Event::ReviewersRequested(ReviewersRequested {
                review_id: review_id.to_string(),
                reviewers: reviewers.into_iter().map(String::from).collect(),
            }),
        )
    }

    fn make_reviewers_requested_at(
        review_id: &str,
        reviewers: Vec<&str>,
        author: &str,
        ts: DateTime<Utc>,
    ) -> EventEnvelope {
        EventEnvelope {
            ts,
            author: author.to_string(),
            event: Event::ReviewersRequested(ReviewersRequested {
                review_id: review_id.to_string(),
                reviewers: reviewers.into_iter().map(String::from).collect(),
            }),
        }
    }

    fn make_vote_at(
        reviewer: &str,
        review_id: &str,
        vote: VoteType,
        ts: DateTime<Utc>,
    ) -> EventEnvelope {
        EventEnvelope {
            ts,
            author: reviewer.to_string(),
            event: Event::ReviewerVoted(ReviewerVoted {
                review_id: review_id.to_string(),
                vote,
                reason: None,
            }),
        }
    }

    #[test]
    fn test_reviews_awaiting_vote_fresh_status() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(
            &db,
            &make_reviewers_requested("cr-001", vec!["bob"], "alice"),
        )
        .unwrap();

        // Bob hasn't voted → should show in inbox with 'fresh' status
        let awaiting = db.get_reviews_awaiting_vote("bob").unwrap();
        assert_eq!(awaiting.len(), 1);
        assert_eq!(awaiting[0].review_id, "cr-001");
        assert_eq!(awaiting[0].request_status, "fresh");
    }

    #[test]
    fn test_reviews_awaiting_vote_disappears_after_vote() {
        let db = setup_db();

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(
            &db,
            &make_reviewers_requested("cr-001", vec!["bob"], "alice"),
        )
        .unwrap();
        apply_event(&db, &make_vote("bob", "cr-001", VoteType::Lgtm)).unwrap();

        // Bob voted → should NOT show in inbox
        let awaiting = db.get_reviews_awaiting_vote("bob").unwrap();
        assert!(awaiting.is_empty());
    }

    #[test]
    fn test_reviews_awaiting_vote_rereview_status() {
        let db = setup_db();

        let t0 = Utc::now();
        let t1 = t0 + Duration::hours(1);
        let t2 = t0 + Duration::hours(2);

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(
            &db,
            &make_reviewers_requested_at("cr-001", vec!["bob"], "alice", t0),
        )
        .unwrap();
        apply_event(&db, &make_vote_at("bob", "cr-001", VoteType::Lgtm, t1)).unwrap();

        // Bob voted at t1 → not in inbox
        let awaiting = db.get_reviews_awaiting_vote("bob").unwrap();
        assert!(awaiting.is_empty());

        // Alice re-requests at t2 (after bob's vote)
        apply_event(
            &db,
            &make_reviewers_requested_at("cr-001", vec!["bob"], "alice", t2),
        )
        .unwrap();

        // Bob should now see review with 're-review' status
        let awaiting = db.get_reviews_awaiting_vote("bob").unwrap();
        assert_eq!(awaiting.len(), 1);
        assert_eq!(awaiting[0].review_id, "cr-001");
        assert_eq!(awaiting[0].request_status, "re-review");
    }

    #[test]
    fn test_reviews_awaiting_vote_rerequest_before_vote_still_fresh() {
        let db = setup_db();

        let t0 = Utc::now();
        let t1 = t0 + Duration::hours(1);

        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(
            &db,
            &make_reviewers_requested_at("cr-001", vec!["bob"], "alice", t0),
        )
        .unwrap();
        // Re-request at t1 but bob never voted
        apply_event(
            &db,
            &make_reviewers_requested_at("cr-001", vec!["bob"], "alice", t1),
        )
        .unwrap();

        // Bob never voted → still 'fresh' (the re-request just updated timestamp)
        let awaiting = db.get_reviews_awaiting_vote("bob").unwrap();
        assert_eq!(awaiting.len(), 1);
        assert_eq!(awaiting[0].request_status, "fresh");
    }

    #[test]
    fn test_reviews_awaiting_vote_only_shows_actionable_reviews() {
        let db = setup_db();

        // Create multiple reviews
        apply_event(&db, &make_review("cr-001", "alice", "Open review")).unwrap();
        apply_event(&db, &make_review("cr-002", "alice", "Merged review")).unwrap();
        apply_event(&db, &make_review("cr-003", "alice", "Abandoned review")).unwrap();

        // Request bob on all
        apply_event(
            &db,
            &make_reviewers_requested("cr-001", vec!["bob"], "alice"),
        )
        .unwrap();
        apply_event(
            &db,
            &make_reviewers_requested("cr-002", vec!["bob"], "alice"),
        )
        .unwrap();
        apply_event(
            &db,
            &make_reviewers_requested("cr-003", vec!["bob"], "alice"),
        )
        .unwrap();

        // Merge cr-002
        apply_event(
            &db,
            &EventEnvelope::new(
                "merger",
                Event::ReviewMerged(ReviewMerged {
                    review_id: "cr-002".to_string(),
                    final_commit: "final".to_string(),
                }),
            ),
        )
        .unwrap();

        // Abandon cr-003
        apply_event(
            &db,
            &EventEnvelope::new(
                "abandoner",
                Event::ReviewAbandoned(ReviewAbandoned {
                    review_id: "cr-003".to_string(),
                    reason: Some("not needed".to_string()),
                }),
            ),
        )
        .unwrap();

        // Bob should only see cr-001 (open review)
        let awaiting = db.get_reviews_awaiting_vote("bob").unwrap();
        assert_eq!(awaiting.len(), 1);
        assert_eq!(awaiting[0].review_id, "cr-001");
    }

    // ========================================================================
    // bd-16n: Auto-resolve threads when review is merged
    // ========================================================================

    #[test]
    fn test_bd_16n_merged_review_threads_not_counted_as_open() {
        let db = setup_db();

        // Create review with threads
        apply_event(&db, &make_review("cr-001", "alice", "Review with threads")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 10)).unwrap();
        apply_event(&db, &make_thread("th-002", "cr-001", "src/lib.rs", 20)).unwrap();

        // Before merge: 2 threads, 2 open
        let review = db.get_review("cr-001").unwrap().unwrap();
        assert_eq!(review.thread_count, 2);
        assert_eq!(review.open_thread_count, 2);

        // Merge the review WITHOUT resolving threads
        apply_event(
            &db,
            &EventEnvelope::new(
                "merger",
                Event::ReviewMerged(ReviewMerged {
                    review_id: "cr-001".to_string(),
                    final_commit: "final".to_string(),
                }),
            ),
        )
        .unwrap();

        // After merge: 2 threads, but 0 open (they're effectively resolved)
        let review = db.get_review("cr-001").unwrap().unwrap();
        assert_eq!(review.thread_count, 2);
        assert_eq!(
            review.open_thread_count, 0,
            "Threads on merged reviews should not count as open"
        );
    }

    #[test]
    fn test_bd_16n_merged_review_threads_effective_status() {
        let db = setup_db();

        // Create review with a thread
        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 10)).unwrap();

        // Before merge: thread shows as open
        let threads = db.list_threads("cr-001", Some("open"), None).unwrap();
        assert_eq!(threads.len(), 1, "Thread should be open before merge");
        assert_eq!(threads[0].status, "open");

        // Merge the review
        apply_event(
            &db,
            &EventEnvelope::new(
                "merger",
                Event::ReviewMerged(ReviewMerged {
                    review_id: "cr-001".to_string(),
                    final_commit: "final".to_string(),
                }),
            ),
        )
        .unwrap();

        // After merge: filtering by "open" returns nothing
        let open_threads = db.list_threads("cr-001", Some("open"), None).unwrap();
        assert!(
            open_threads.is_empty(),
            "No threads should appear open after merge"
        );

        // After merge: filtering by "resolved" returns the thread
        let resolved_threads = db.list_threads("cr-001", Some("resolved"), None).unwrap();
        assert_eq!(
            resolved_threads.len(),
            1,
            "Thread should appear resolved after merge"
        );
        assert_eq!(resolved_threads[0].status, "resolved");
    }

    #[test]
    fn test_bd_16n_abandoned_review_threads_not_counted_as_open() {
        let db = setup_db();

        // Create review with threads
        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 10)).unwrap();

        // Before abandon: 1 open thread
        let review = db.get_review("cr-001").unwrap().unwrap();
        assert_eq!(review.open_thread_count, 1);

        // Abandon the review
        apply_event(
            &db,
            &EventEnvelope::new(
                "abandoner",
                Event::ReviewAbandoned(ReviewAbandoned {
                    review_id: "cr-001".to_string(),
                    reason: Some("not needed".to_string()),
                }),
            ),
        )
        .unwrap();

        // After abandon: 0 open threads
        let review = db.get_review("cr-001").unwrap().unwrap();
        assert_eq!(
            review.open_thread_count, 0,
            "Threads on abandoned reviews should not count as open"
        );
    }

    // ========================================================================
    // find_thread_at_location range matching tests (bd-3aw)
    // ========================================================================

    #[test]
    fn test_find_thread_at_location_exact_single_line() {
        let db = setup_db();
        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(&db, &make_thread("th-001", "cr-001", "src/main.rs", 10)).unwrap();

        // Exact match
        let found = db
            .find_thread_at_location("cr-001", "src/main.rs", 10)
            .unwrap();
        assert_eq!(found, Some("th-001".to_string()));

        // Adjacent lines should NOT match
        let not_found = db
            .find_thread_at_location("cr-001", "src/main.rs", 9)
            .unwrap();
        assert_eq!(not_found, None);
        let not_found = db
            .find_thread_at_location("cr-001", "src/main.rs", 11)
            .unwrap();
        assert_eq!(not_found, None);
    }

    #[test]
    fn test_find_thread_at_location_within_range() {
        let db = setup_db();
        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(
            &db,
            &make_thread_range("th-001", "cr-001", "src/main.rs", 10, 20),
        )
        .unwrap();

        // Line within the range
        let found = db
            .find_thread_at_location("cr-001", "src/main.rs", 15)
            .unwrap();
        assert_eq!(found, Some("th-001".to_string()));

        // Boundary: start of range
        let found = db
            .find_thread_at_location("cr-001", "src/main.rs", 10)
            .unwrap();
        assert_eq!(found, Some("th-001".to_string()));

        // Boundary: end of range
        let found = db
            .find_thread_at_location("cr-001", "src/main.rs", 20)
            .unwrap();
        assert_eq!(found, Some("th-001".to_string()));
    }

    #[test]
    fn test_find_thread_at_location_outside_range() {
        let db = setup_db();
        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(
            &db,
            &make_thread_range("th-001", "cr-001", "src/main.rs", 10, 20),
        )
        .unwrap();

        // Just before range
        let not_found = db
            .find_thread_at_location("cr-001", "src/main.rs", 9)
            .unwrap();
        assert_eq!(not_found, None);

        // Just after range
        let not_found = db
            .find_thread_at_location("cr-001", "src/main.rs", 21)
            .unwrap();
        assert_eq!(not_found, None);
    }

    #[test]
    fn test_find_thread_at_location_resolved_range_not_matched() {
        let db = setup_db();
        apply_event(&db, &make_review("cr-001", "alice", "Review")).unwrap();
        apply_event(
            &db,
            &make_thread_range("th-001", "cr-001", "src/main.rs", 10, 20),
        )
        .unwrap();

        // Resolve the thread
        apply_event(
            &db,
            &EventEnvelope::new(
                "resolver",
                Event::ThreadResolved(crate::events::ThreadResolved {
                    thread_id: "th-001".to_string(),
                    reason: Some("Fixed".to_string()),
                }),
            ),
        )
        .unwrap();

        // Line within range should NOT match (thread is resolved)
        let not_found = db
            .find_thread_at_location("cr-001", "src/main.rs", 15)
            .unwrap();
        assert_eq!(not_found, None);
    }
}

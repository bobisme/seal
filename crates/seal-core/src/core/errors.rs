//! Typed error types for the seal-core service layer.

use thiserror::Error;

/// Result type alias for core service operations.
pub type CoreResult<T> = Result<T, CoreError>;

/// Errors that can occur in the seal-core service layer.
#[derive(Debug, Error)]
pub enum CoreError {
    /// The seal repository is not initialized.
    #[error("Not a seal repository at {path}. Run 'seal init' first.")]
    NotInitialized { path: String },

    /// The data format is v1 and needs migration.
    #[error("Repository uses v1 format. Run 'seal migrate' first.")]
    V1NeedsMigration,

    /// A review was not found.
    #[error("Review not found: {review_id}")]
    ReviewNotFound { review_id: String },

    /// A thread was not found.
    #[error("Thread not found: {thread_id}")]
    ThreadNotFound { thread_id: String },

    /// Operation not allowed because the review is not in the expected status.
    #[error("Review {review_id} has status '{actual}', expected '{expected}'")]
    InvalidReviewStatus {
        review_id: String,
        actual: String,
        expected: String,
    },

    /// Only the original review author may change its target.
    #[error(
        "Only review author '{review_author}' can retarget {review_id}; current agent is '{actor}'.\n  To fix: rerun as the review author with '--agent {review_author}', or create a new review."
    )]
    ReviewAuthorRequired {
        review_id: String,
        review_author: String,
        actor: String,
    },

    /// A review cannot move between SCM backends.
    #[error(
        "Cannot retarget {review_id} with the '{actual}' backend; the review uses '{expected}'.\n  To fix: run this command from a workspace that uses the review's SCM backend."
    )]
    ReviewScmMismatch {
        review_id: String,
        actual: String,
        expected: String,
    },

    /// An SCM anchor needed for retargeting does not resolve.
    #[error(
        "Cannot retarget {review_id}: failed to resolve the {target_name} SCM anchor '{anchor}': {reason}\n  To fix: restore that revision in this repository, or create a new review at the current target."
    )]
    RetargetAnchorUnresolved {
        review_id: String,
        target_name: &'static str,
        anchor: String,
        reason: String,
    },

    /// The explicit `--base` given to a retarget does not resolve.
    #[error(
        "Cannot retarget {review_id}: failed to resolve --base {base}: {reason}\n  To fix: pass a revision this repository knows, such as a commit hash, a branch name, or main."
    )]
    RetargetBaseUnresolved {
        review_id: String,
        base: String,
        reason: String,
    },

    /// The proposed target is already the effective review target.
    #[error(
        "Review {review_id} already targets commit {target_commit}.\n  To fix: commit the new work, then run 'seal reviews retarget {review_id}' again."
    )]
    ReviewTargetUnchanged {
        review_id: String,
        target_commit: String,
    },

    /// The proposed target does not preserve the review's existing range.
    #[error(
        "Cannot retarget {review_id}: commit {target_commit} does not descend from review base {base_commit}.\n  To fix: switch to the review's feature history, or pass the fork point with 'seal reviews retarget {review_id} --base <rev>'."
    )]
    InvalidRetargetRange {
        review_id: String,
        base_commit: String,
        target_commit: String,
    },

    /// The file does not exist at the given commit.
    #[error("File does not exist in review {review_id} at {commit}: {file_path}")]
    FileNotFound {
        review_id: String,
        commit: String,
        file_path: String,
    },

    /// A code selection used invalid line numbers.
    #[error("Invalid code selection: {reason}")]
    InvalidCodeSelection { reason: String },

    /// An internal storage or database error.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

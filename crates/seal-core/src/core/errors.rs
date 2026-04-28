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

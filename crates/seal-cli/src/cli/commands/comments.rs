//! Implementation of `seal comments` subcommands.

use anyhow::{bail, Result};
use std::path::Path;

use crate::cli::commands::helpers::{
    ensure_initialized, open_services, resolve_review_thread_commit, review_not_found_error,
    thread_not_found_error,
};
use crate::cli::commands::threads::parse_line_selection;
use crate::output::{Formatter, OutputFormat};
use seal_core::scm::ScmRepo;

/// Add a comment to a thread.
#[tracing::instrument(skip(repo_root, message, format))]
pub fn run_comments_add(
    repo_root: &Path,
    thread_id: &str,
    message: &str,
    author: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(repo_root)?;

    let services = open_services(repo_root)?;

    // Verify thread exists (service will check review status too)
    if services.threads().get_optional(thread_id)?.is_none() {
        return Err(thread_not_found_error(repo_root, thread_id));
    }

    let result = services
        .comments()
        .add_to_thread(thread_id, message, author)?;

    let author_str = seal_core::events::get_agent_identity(author)?;
    let output = serde_json::json!({
        "comment_id": result.comment_id,
        "thread_id": thread_id,
        "author": author_str,
        "body": message,
    });

    let formatter = Formatter::new(format);
    formatter.print(&output)?;

    Ok(())
}

/// Add a comment to a review, auto-creating a thread if needed.
///
/// This is the simplified comment workflow for agents:
/// - If a thread already exists at the file+line, adds comment to it
/// - If no thread exists, creates one and adds the comment
///
/// # Arguments
/// * `seal_root` - Path to main repo (where .seal/ lives)
/// * `workspace_root` - Path to current workspace (for jj @ resolution)
#[tracing::instrument(skip(seal_root, scm, message, format))]
pub fn run_comment(
    seal_root: &Path,
    scm: &dyn ScmRepo,
    review_id: &str,
    file: &str,
    line: &str,
    message: &str,
    author: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(seal_root)?;

    let services = open_services(seal_root)?;

    // Verify review exists and is open
    let Some(review) = services.reviews().get_optional(review_id)? else {
        return Err(review_not_found_error(seal_root, review_id));
    };

    if review.status != "open" {
        bail!(
            "Cannot comment on review with status '{}': {}",
            review.status,
            review_id
        );
    }

    // Parse line selection
    let selection = parse_line_selection(line)?;
    let start_line = i64::from(selection.start_line());

    // Check if file exists when a new thread would be needed
    let needs_new_thread = services
        .threads()
        .find_at_location(review_id, file, start_line)?
        .is_none();
    if needs_new_thread {
        let commit_hash = resolve_review_thread_commit(scm, &review);
        if !scm.file_exists(&commit_hash, file)? {
            bail!("File does not exist in review {review_id} at {commit_hash}: {file}");
        }
    }

    // Resolve commit for thread creation
    let commit_hash = resolve_review_thread_commit(scm, &review);

    // Use core service to add comment (handles thread creation if needed)
    let result = services.comments().add_to_review(
        review_id,
        file,
        selection,
        message,
        commit_hash,
        author,
    )?;

    let author_str = seal_core::events::get_agent_identity(author)?;

    // Output result
    let output = serde_json::json!({
        "comment_id": result.comment_id,
        "thread_id": result.thread_id,
        "review_id": review_id,
        "file": file,
        "line": start_line,
        "author": author_str,
        "body": message,
    });

    let formatter = Formatter::new(format);
    formatter.print(&output)?;

    Ok(())
}

/// List comments for a thread.
pub fn run_comments_list(repo_root: &Path, thread_id: &str, format: OutputFormat) -> Result<()> {
    ensure_initialized(repo_root)?;

    let services = open_services(repo_root)?;

    // Verify thread exists
    if services.threads().get_optional(thread_id)?.is_none() {
        return Err(thread_not_found_error(repo_root, thread_id));
    }

    let comments = services.comments().list(thread_id)?;

    let formatter = Formatter::new(format);
    formatter.print_list(
        &comments,
        "No comments yet",
        "comments",
        &["seal reply <thread_id> \"...\""],
    )?;

    Ok(())
}

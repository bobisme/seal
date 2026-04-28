//! Implementation of `seal status` and `seal diff` commands.

use anyhow::Result;
use serde::Serialize;
use std::path::Path;

use crate::cli::commands::helpers::{ensure_initialized, open_services, review_not_found_error};
use crate::output::{Formatter, OutputFormat};
use seal_core::jj::drift::{calculate_drift, DriftResult};
use seal_core::projection::ThreadSummary;
use seal_core::scm::{git_diff_changed_paths, ScmRepo};
use seal_core::sealignore::{AllFilesIgnoredError, SealIgnore};

/// Thread status with drift information.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadStatusEntry {
    pub thread_id: String,
    pub file_path: String,
    pub original_line: i64,
    pub current_line: Option<i64>,
    pub drift_status: String,
    pub status: String,
    pub comment_count: i64,
}

/// Review status with threads and drift information.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewStatus {
    pub review_id: String,
    pub title: String,
    pub status: String,
    pub total_threads: usize,
    pub open_threads: usize,
    pub threads_with_drift: usize,
    pub threads: Vec<ThreadStatusEntry>,
}

/// Show status of reviews with drift detection.
///
/// # Arguments
/// * `seal_root` - Path to main repo (where .seal/ lives)
/// * `workspace_root` - Path to current workspace (for jj @ resolution)
pub fn run_status(
    seal_root: &Path,
    scm: &dyn ScmRepo,
    review_id: Option<&str>,
    unresolved_only: bool,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(seal_root)?;

    let services = open_services(seal_root)?;
    let current_commit = scm.current_commit()?;

    // Get reviews to process
    let reviews = if let Some(rid) = review_id {
        match services.reviews().get_optional(rid)? {
            Some(r) => vec![r],
            None => return Err(review_not_found_error(seal_root, rid)),
        }
    } else {
        // Get all open reviews
        services
            .reviews()
            .list(Some("open"), None)?
            .into_iter()
            .filter_map(|rs| {
                services
                    .reviews()
                    .get_optional(&rs.review_id)
                    .ok()
                    .flatten()
            })
            .collect()
    };

    let mut statuses = Vec::new();

    for review in reviews {
        // Get threads for this review
        let status_filter = if unresolved_only { Some("open") } else { None };
        let threads = services
            .threads()
            .list(&review.review_id, status_filter, None)?;

        let mut thread_entries = Vec::new();
        let mut drift_count = 0;

        for thread in &threads {
            // Calculate drift for this thread
            let thread_detail = services.threads().get_optional(&thread.thread_id)?;
            let drift_result = if let Some(td) = &thread_detail {
                calculate_drift(
                    scm,
                    &td.file_path,
                    td.selection_start as u32,
                    &td.commit_hash,
                    &current_commit,
                )
                .unwrap_or(DriftResult::Unchanged {
                    current_line: td.selection_start as u32,
                })
            } else {
                DriftResult::Unchanged {
                    current_line: thread.selection_start as u32,
                }
            };

            let (current_line, drift_status) = match &drift_result {
                DriftResult::Unchanged { current_line } => {
                    (Some(i64::from(*current_line)), "unchanged".to_string())
                }
                DriftResult::Shifted {
                    current_line,
                    original_line,
                } => {
                    drift_count += 1;
                    let delta = i64::from(*current_line) - i64::from(*original_line);
                    let direction = if delta > 0 { "+" } else { "" };
                    (
                        Some(i64::from(*current_line)),
                        format!("shifted({direction}{delta})"),
                    )
                }
                DriftResult::Modified => {
                    drift_count += 1;
                    (None, "modified".to_string())
                }
                DriftResult::Deleted => {
                    drift_count += 1;
                    (None, "deleted".to_string())
                }
            };

            thread_entries.push(ThreadStatusEntry {
                thread_id: thread.thread_id.clone(),
                file_path: thread.file_path.clone(),
                original_line: thread.selection_start,
                current_line,
                drift_status,
                status: thread.status.clone(),
                comment_count: thread.comment_count,
            });
        }

        let open_count = threads.iter().filter(|t| t.status == "open").count();

        statuses.push(ReviewStatus {
            review_id: review.review_id.clone(),
            title: review.title.clone(),
            status: review.status.clone(),
            total_threads: threads.len(),
            open_threads: open_count,
            threads_with_drift: drift_count,
            threads: thread_entries,
        });
    }

    // Build context-aware empty message
    let empty_msg = if review_id.is_some() {
        "Review has no threads"
    } else if unresolved_only {
        "No open reviews with unresolved threads"
    } else {
        "No open reviews"
    };

    let formatter = Formatter::new(format);
    formatter.print_list(
        &statuses,
        empty_msg,
        "reviews",
        &["seal review <id>", "seal threads list <id>"],
    )?;

    Ok(())
}

/// Show diff for a review.
///
/// # Arguments
/// * `seal_root` - Path to main repo (where .seal/ lives)
/// * `workspace_root` - Path to current workspace (for jj @ resolution)
pub fn run_diff(
    seal_root: &Path,
    scm: &dyn ScmRepo,
    review_id: &str,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(seal_root)?;

    let services = open_services(seal_root)?;

    // Get the review
    let Some(review) = services.reviews().get_optional(review_id)? else {
        return Err(review_not_found_error(seal_root, review_id));
    };

    // Get target commit: resolve the review's change_id to its current commit
    // - For merged reviews: use final_commit
    // - For open/approved: resolve jj_change_id to current commit
    // We resolve this FIRST so we can get its parent, which handles rewrites correctly
    let target_commit = review
        .final_commit
        .clone()
        .or_else(|| scm.commit_for_anchor(&review.scm_anchor).ok())
        .or_else(|| scm.commit_for_anchor(&review.jj_change_id).ok())
        .unwrap_or_else(|| review.initial_commit.clone());

    // Get the base commit: parent of target_commit (not initial_commit)
    // This shows ALL files changed in the review, even after rewrites
    let base_commit = scm
        .parent_commit(&target_commit)
        .unwrap_or_else(|_| review.initial_commit.clone());

    // Get the diff between base and target
    let diff = scm.diff_git(&base_commit, &target_commit)?;

    // Get changed files from the diff and filter with sealignore
    let all_files = extract_changed_files_from_diff(&diff);
    let sealignore = SealIgnore::load(seal_root);
    let (changed_files, ignored_count) = sealignore.filter_files(all_files);

    // Check if all files were ignored
    if changed_files.is_empty() && ignored_count > 0 {
        return Err(AllFilesIgnoredError {
            ignored_count,
            has_sealignore: SealIgnore::has_sealignore_file(seal_root),
        }
        .into());
    }

    // Get threads for context
    let threads = services.threads().list(review_id, None, None)?;

    // Build structured output
    let result = serde_json::json!({
        "review_id": review_id,
        "base_commit": base_commit,
        "initial_commit": review.initial_commit,
        "target_commit": target_commit,
        "changed_files": changed_files,
        "thread_count": threads.len(),
        "threads_by_file": group_threads_by_file(&threads),
        "diff": diff,
    });

    let formatter = Formatter::new(format);
    formatter.print(&result)?;

    Ok(())
}

/// Extract file names from a git diff output.
fn extract_changed_files_from_diff(diff: &str) -> Vec<String> {
    git_diff_changed_paths(diff)
}

/// Group threads by file path.
fn group_threads_by_file(threads: &[ThreadSummary]) -> serde_json::Value {
    let mut by_file: std::collections::HashMap<String, Vec<&ThreadSummary>> =
        std::collections::HashMap::new();

    for thread in threads {
        by_file
            .entry(thread.file_path.clone())
            .or_default()
            .push(thread);
    }

    serde_json::json!(by_file
        .into_iter()
        .map(|(file, threads)| {
            serde_json::json!({
                "file": file,
                "threads": threads.iter().map(|t| serde_json::json!({
                    "thread_id": t.thread_id,
                    "line": t.selection_start,
                    "status": t.status,
                })).collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_changed_files_from_diff_allows_spaces() {
        let diff = "\
diff --git a/src/has space.rs b/src/has space.rs
--- a/src/has space.rs
+++ b/src/has space.rs
@@ -1 +1 @@
-old
+new
";

        assert_eq!(
            extract_changed_files_from_diff(diff),
            vec!["src/has space.rs".to_string()]
        );
    }
}

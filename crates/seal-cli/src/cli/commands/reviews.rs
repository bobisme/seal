//! Implementation of `seal reviews` subcommands.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use std::path::Path;

use crate::cli::commands::helpers::{ensure_initialized, open_services};
use crate::output::{Formatter, OutputFormat};
use seal_core::events::VoteType;
use seal_core::projection::{ReviewDetail, ThreadSummary};
use seal_core::scm::ScmRepo;
use seal_core::sealignore::{AllFilesIgnoredError, SealIgnore};

/// Parse a --since value into a `DateTime`.
/// Supports:
/// - ISO 8601 timestamps: "2026-01-27T23:00:00Z"
/// - Relative durations: "1h", "2d", "30m", "1w"
pub fn parse_since(value: &str) -> Result<DateTime<Utc>> {
    // Try ISO 8601 first
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Ok(dt.with_timezone(&Utc));
    }

    // Try relative duration
    let value = value.trim().to_lowercase();
    if let Some(num_str) = value.strip_suffix('h') {
        let hours: i64 = num_str.parse().context("Invalid hours")?;
        return Ok(Utc::now() - Duration::hours(hours));
    }
    if let Some(num_str) = value.strip_suffix('d') {
        let days: i64 = num_str.parse().context("Invalid days")?;
        return Ok(Utc::now() - Duration::days(days));
    }
    if let Some(num_str) = value.strip_suffix('m') {
        let mins: i64 = num_str.parse().context("Invalid minutes")?;
        return Ok(Utc::now() - Duration::minutes(mins));
    }
    if let Some(num_str) = value.strip_suffix('w') {
        let weeks: i64 = num_str.parse().context("Invalid weeks")?;
        return Ok(Utc::now() - Duration::weeks(weeks));
    }

    bail!(
        "Invalid --since format. Use ISO 8601 (2026-01-27T23:00:00Z) or relative (1h, 2d, 30m, 1w)"
    )
}

/// Create a new review for the current jj change.
///
/// # Arguments
/// * `seal_root` - Path to main repo (where .seal/ lives)
/// * `workspace_root` - Path to current workspace (for jj @ resolution)
#[tracing::instrument(skip(seal_root, scm, format, description, reviewers), fields(title = %title))]
pub fn run_reviews_create(
    seal_root: &Path,
    scm: &dyn ScmRepo,
    title: String,
    description: Option<String>,
    reviewers: Option<String>,
    author: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(seal_root)?;

    let change_id = scm
        .current_anchor()
        .context("Failed to get current SCM anchor")?;
    let commit_id = scm
        .current_commit()
        .context("Failed to get current commit")?;

    // Check if there are any non-ignored files to review
    let parent_commit = scm.parent_commit(&commit_id)?;
    let all_files = scm.changed_files_between(&parent_commit, &commit_id)?;
    let sealignore = SealIgnore::load(seal_root);
    let (reviewable_files, ignored_count) = sealignore.filter_files(all_files);

    if reviewable_files.is_empty() {
        if ignored_count > 0 {
            // All files were ignored
            return Err(AllFilesIgnoredError {
                ignored_count,
                has_sealignore: SealIgnore::has_sealignore_file(seal_root),
            }
            .into());
        }
        // No files changed at all
        bail!("No files changed in this commit. Nothing to review.");
    }

    // Parse reviewers before creating the review
    let reviewer_list: Option<Vec<String>> = reviewers.map(|r| {
        r.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });

    // Use core service to create the review (handles event construction + log append)
    let services = open_services(seal_root)?;
    let review_id = services.reviews().create(
        scm,
        title.clone(),
        description,
        reviewer_list.clone(),
        author,
    )?;

    // Resolve author for output (same logic the service uses internally)
    let author_str = seal_core::events::get_agent_identity(author)?;

    // Output the result
    let scm_anchor = change_id.clone();
    let mut result = serde_json::json!({
        "review_id": review_id,
        "jj_change_id": change_id,
        "scm_kind": scm.kind().as_str(),
        "scm_anchor": scm_anchor,
        "initial_commit": commit_id,
        "title": title,
        "author": author_str,
    });
    if let Some(ref reviewers) = reviewer_list {
        result["reviewers"] = serde_json::json!(reviewers);
    }

    let formatter = Formatter::new(format);
    formatter.print(&result)?;

    // Add next steps for non-JSON formats (agents need guidance on what to do next)
    if format != OutputFormat::Json {
        println!();
        println!("Next:");
        if reviewer_list.is_none() {
            println!("  seal reviews request {review_id} --reviewers <name>");
        }
        println!("  seal comment {review_id} --file <path> --line <n> \"feedback\"");
        println!("  seal review {review_id}");
    }

    Ok(())
}

/// List reviews with optional filters.
pub fn run_reviews_list(
    seal_root: &Path,
    status: Option<&str>,
    author: Option<&str>,
    needs_reviewer: Option<&str>,
    has_unresolved: bool,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(seal_root)?;

    let services = open_services(seal_root)?;
    let reviews =
        services
            .reviews()
            .list_filtered(status, author, needs_reviewer, has_unresolved)?;

    // Build context-aware empty message
    let empty_msg = if needs_reviewer.is_some() {
        "No reviews need your attention"
    } else if has_unresolved {
        "No reviews have unresolved threads"
    } else if status.is_some() || author.is_some() {
        "No reviews match the filters"
    } else {
        "No reviews yet"
    };

    let formatter = Formatter::new(format);
    formatter.print_list(
        &reviews,
        empty_msg,
        "reviews",
        &["seal reviews show <id>", "seal lgtm <id> -m \"...\""],
    )?;

    Ok(())
}

/// Show details for a specific review.
pub fn run_reviews_show(repo_root: &Path, review_id: &str, format: OutputFormat) -> Result<()> {
    use crate::cli::commands::helpers::get_review;

    ensure_initialized(repo_root)?;

    let review = get_review(repo_root, review_id)?;

    let formatter = Formatter::new(format);
    formatter.print(&review)?;

    Ok(())
}

/// Request reviewers for a review.
#[tracing::instrument(skip(repo_root, format))]
pub fn run_reviews_request(
    repo_root: &Path,
    review_id: &str,
    reviewers: &str,
    author: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(repo_root)?;

    let reviewer_list: Vec<String> = reviewers
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if reviewer_list.is_empty() {
        bail!("No reviewers specified");
    }

    let services = open_services(repo_root)?;
    services
        .reviews()
        .request_reviewers(review_id, reviewer_list.clone(), author)?;

    let result = serde_json::json!({
        "review_id": review_id,
        "reviewers": reviewer_list,
    });

    let formatter = Formatter::new(format);
    formatter.print(&result)?;

    Ok(())
}

/// Approve a review.
pub fn run_reviews_approve(
    repo_root: &Path,
    review_id: &str,
    author: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(repo_root)?;

    let services = open_services(repo_root)?;

    // Verify review exists locally and is open
    let review = services.reviews().get(review_id)?;
    if review.status != "open" {
        bail!(
            "Cannot approve review with status '{}': {}",
            review.status,
            review_id
        );
    }

    services.reviews().approve(review_id, author)?;

    let result = serde_json::json!({
        "review_id": review_id,
        "status": "approved",
    });

    let formatter = Formatter::new(format);
    formatter.print(&result)?;

    Ok(())
}

/// Abandon a review.
pub fn run_reviews_abandon(
    repo_root: &Path,
    review_id: &str,
    reason: Option<String>,
    author: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(repo_root)?;

    let services = open_services(repo_root)?;

    // Verify review exists locally and is not already abandoned/merged
    let review = services.reviews().get(review_id)?;
    if review.status == "abandoned" {
        bail!("Review is already abandoned: {review_id}");
    }
    if review.status == "merged" {
        bail!("Cannot abandon merged review: {review_id}");
    }

    services
        .reviews()
        .abandon(review_id, reason.clone(), author)?;

    let result = serde_json::json!({
        "review_id": review_id,
        "status": "abandoned",
        "reason": reason,
    });

    let formatter = Formatter::new(format);
    formatter.print(&result)?;

    Ok(())
}

/// Mark a review as merged.
///
/// # Arguments
/// * `seal_root` - Path to main repo (where .seal/ lives)
/// * `workspace_root` - Path to current workspace (for jj @ resolution)
/// * `self_approve` - If true, auto-approve open reviews before merging
pub fn run_reviews_merge(
    seal_root: &Path,
    scm: &dyn ScmRepo,
    review_id: &str,
    commit: Option<String>,
    self_approve: bool,
    author: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(seal_root)?;

    let services = open_services(seal_root)?;

    // Verify review exists locally (need to write to its log)
    let review = services.reviews().get(review_id)?;
    if review.status == "merged" {
        bail!("Review is already merged: {review_id}");
    }
    if review.status == "abandoned" {
        bail!("Cannot merge abandoned review: {review_id}");
    }
    if review.status == "open" && !self_approve {
        bail!(
            "Cannot merge unapproved review: {review_id}. Approve it first, or use --self-approve."
        );
    }
    if review.status == "open" && self_approve {
        // Auto-approve the review first
        services.reviews().approve(review_id, author)?;
    }

    // Re-open the database to get fresh state after any approval
    let services = open_services(seal_root)?;

    // Check for blocking votes (not yet in service layer, use db directly)
    if services.db().has_blocking_votes(review_id)? {
        let votes = services.db().get_votes(review_id)?;
        let blockers: Vec<_> = votes
            .iter()
            .filter(|v| v.vote == "block")
            .map(|v| {
                if let Some(reason) = &v.reason {
                    format!("  - {} ({})", v.reviewer, reason)
                } else {
                    format!("  - {}", v.reviewer)
                }
            })
            .collect();

        bail!(
            "Cannot merge review with blocking votes:\n{}\n\nReviewers must change their vote with 'seal --agent <their-name> lgtm {}' before merging.",
            blockers.join("\n"),
            review_id
        );
    }

    // Get final commit hash - either provided or auto-detected from active backend.
    let final_commit = match commit {
        Some(c) => c,
        None => scm
            .current_commit()
            .context("Failed to get current commit for merge")?,
    };

    services
        .reviews()
        .mark_merged(review_id, final_commit.clone(), author)?;

    let result = serde_json::json!({
        "review_id": review_id,
        "status": "merged",
        "final_commit": final_commit,
    });

    let formatter = Formatter::new(format);
    formatter.print(&result)?;

    Ok(())
}

/// Vote LGTM on a review.
#[tracing::instrument(skip(repo_root, format, message))]
pub fn run_lgtm(
    repo_root: &Path,
    review_id: &str,
    message: Option<String>,
    author: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    run_vote(
        repo_root,
        review_id,
        VoteType::Lgtm,
        message,
        author,
        format,
    )
}

/// Block a review (request changes).
#[tracing::instrument(skip(repo_root, format, reason))]
pub fn run_block(
    repo_root: &Path,
    review_id: &str,
    reason: String,
    author: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    run_vote(
        repo_root,
        review_id,
        VoteType::Block,
        Some(reason),
        author,
        format,
    )
}

/// Internal vote handler.
fn run_vote(
    repo_root: &Path,
    review_id: &str,
    vote: VoteType,
    reason: Option<String>,
    author: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(repo_root)?;

    let services = open_services(repo_root)?;

    // Verify review exists locally and is open or approved (approved reviews can still
    // receive votes, e.g., to change a block to lgtm after issues are fixed)
    let review = services.reviews().get(review_id)?;
    if review.status == "merged" {
        bail!("Cannot vote on merged review: {review_id}");
    }
    if review.status == "abandoned" {
        bail!("Cannot vote on abandoned review: {review_id}");
    }
    let review_status = review.status;

    // Resolve author identity for output and auto-approve check
    let author_str = seal_core::events::get_agent_identity(author)?;

    services
        .reviews()
        .vote(review_id, vote, reason.clone(), author)?;

    // Auto-approve on LGTM if review is open and no blocking votes from others
    let auto_approved = if vote == VoteType::Lgtm && review_status == "open" {
        // Re-sync to see our newly recorded vote
        let services = open_services(repo_root)?;
        let has_blocks = services
            .db()
            .has_blocking_votes_from_others(review_id, &author_str)?;
        if has_blocks {
            false
        } else {
            // Auto-approve the review
            services.reviews().approve(review_id, author)?;
            true
        }
    } else {
        false
    };

    let mut result = serde_json::json!({
        "review_id": review_id,
        "vote": vote.to_string(),
        "reason": reason,
        "voter": author_str,
    });
    if auto_approved {
        result["auto_approved"] = serde_json::json!(true);
    }

    let formatter = Formatter::new(format);
    formatter.print(&result)?;

    Ok(())
}

/// Show full review with all threads and comments.
///
/// # Arguments
/// * `seal_root` - Path to main repo (where .seal/ lives)
/// * `workspace_root` - Path to current workspace (for jj @ resolution)
/// * `since` - Optional filter to only show activity after this time
#[tracing::instrument(skip(seal_root, scm, format))]
pub fn run_review(
    seal_root: &Path,
    scm: &dyn ScmRepo,
    review_id: &str,
    context_lines: u32,
    since: Option<DateTime<Utc>>,
    include_diffs: bool,
    format: OutputFormat,
) -> Result<()> {
    use seal_core::jj::context::{extract_context, format_context};

    ensure_initialized(seal_root)?;

    let services = open_services(seal_root)?;
    let review = services.reviews().get(review_id)?;

    // For JSON output, build a complete structure
    if matches!(format, OutputFormat::Json) {
        let threads = services.threads().list(review_id, None, None)?;
        let mut threads_with_comments = Vec::new();

        // Determine commit for context (same logic as text/pretty output)
        let commit_ref = review
            .final_commit
            .clone()
            .or_else(|| scm.commit_for_anchor(&review.scm_anchor).ok())
            .or_else(|| scm.commit_for_anchor(&review.jj_change_id).ok())
            .unwrap_or_else(|| review.initial_commit.clone());

        // Pre-fetch file contents for all thread files (one show_file per unique file)
        let mut file_cache: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if context_lines > 0 || include_diffs {
            for thread in &threads {
                if !file_cache.contains_key(&thread.file_path) {
                    if let Ok(contents) = scm.show_file(&commit_ref, &thread.file_path) {
                        file_cache.insert(thread.file_path.clone(), contents);
                    }
                }
            }
        }

        for thread in &threads {
            let comments = services.comments().list(&thread.thread_id)?;
            // Filter comments by since if provided
            let filtered_comments: Vec<_> = if let Some(since_dt) = since {
                comments
                    .into_iter()
                    .filter(|c| {
                        DateTime::parse_from_rfc3339(&c.created_at)
                            .map(|dt| dt.with_timezone(&Utc) >= since_dt)
                            .unwrap_or(true)
                    })
                    .collect()
            } else {
                comments
            };

            // Skip threads with no activity since the cutoff
            if since.is_some() && filtered_comments.is_empty() {
                continue;
            }

            // Extract code context from cached file contents
            let anchor_start = thread.selection_start as u32;
            let anchor_end = thread.selection_end.unwrap_or(thread.selection_start) as u32;

            let context_value = if context_lines > 0 {
                file_cache
                    .get(&thread.file_path)
                    .and_then(|contents| {
                        extract_context_from_str(contents, anchor_start, anchor_end, context_lines)
                    })
                    .and_then(|ctx| serde_json::to_value(&ctx).ok())
            } else {
                None
            };

            threads_with_comments.push(serde_json::json!({
                "thread_id": thread.thread_id,
                "file_path": thread.file_path,
                "selection_start": thread.selection_start,
                "selection_end": thread.selection_end,
                "status": thread.status,
                "context": context_value,
                "comments": filtered_comments,
            }));
        }

        let mut result = serde_json::json!({
            "review": review,
            "threads": threads_with_comments,
        });

        // Include per-file diffs when requested
        if include_diffs {
            let files_value =
                build_file_diffs(scm, &review, &threads, &commit_ref, &file_cache, seal_root);
            result["files"] = files_value;
        }

        let formatter = Formatter::new(format);
        formatter.print(&result)?;
        return Ok(());
    }

    // Text/pretty output: human-readable format
    let status_symbol = match review.status.as_str() {
        "open" => "○",
        "approved" => "◐",
        "merged" => "●",
        "abandoned" => "✗",
        _ => "?",
    };

    println!("{} {} · {}", status_symbol, review.review_id, review.title);
    println!(
        "  Status: {} | Author: {} | Created: {}",
        review.status,
        review.author,
        &review.created_at[..10]
    );

    if let Some(desc) = &review.description {
        println!("\n  {desc}");
    }

    // Show votes if any
    if !review.votes.is_empty() {
        println!("\n  Votes:");
        for vote in &review.votes {
            let icon = if vote.vote == "lgtm" { "✓" } else { "✗" };
            let reason = vote.reason.as_deref().unwrap_or("");
            if reason.is_empty() {
                println!("    {} {} ({})", icon, vote.reviewer, vote.vote);
            } else {
                println!("    {} {} ({}): {}", icon, vote.reviewer, vote.vote, reason);
            }
        }
    }

    // Get threads grouped by file
    let threads = services.threads().list(review_id, None, None)?;

    // Determine commit for context/diff rendering
    let commit_ref = review
        .final_commit
        .clone()
        .or_else(|| scm.commit_for_anchor(&review.scm_anchor).ok())
        .or_else(|| scm.commit_for_anchor(&review.jj_change_id).ok())
        .unwrap_or_else(|| review.initial_commit.clone());

    // Pre-fetch file contents for thread files when include_diffs is requested.
    let mut file_cache: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    if include_diffs {
        for thread in &threads {
            if !file_cache.contains_key(&thread.file_path) {
                if let Ok(contents) = scm.show_file(&commit_ref, &thread.file_path) {
                    file_cache.insert(thread.file_path.clone(), contents);
                }
            }
        }
    }

    if threads.is_empty() {
        if include_diffs {
            print_file_diffs_text(scm, &review, &threads, &commit_ref, &file_cache, seal_root);
        } else {
            println!("\n  No threads yet. Use seal diff {review_id} to view changes.");
        }
        return Ok(());
    }

    // Show filter notice if --since is active
    if let Some(since_dt) = since {
        println!(
            "\n  [Showing activity since {}]",
            since_dt.format("%Y-%m-%d %H:%M")
        );
    }

    // Group threads by file, filtering by since if provided
    let mut threads_by_file: std::collections::BTreeMap<String, Vec<_>> =
        std::collections::BTreeMap::new();
    let mut total_new_comments = 0;

    for thread in &threads {
        // Get comments and filter by since
        let comments = services.comments().list(&thread.thread_id)?;
        let filtered_comments: Vec<_> = if let Some(since_dt) = since {
            comments
                .into_iter()
                .filter(|c| {
                    DateTime::parse_from_rfc3339(&c.created_at)
                        .map(|dt| dt.with_timezone(&Utc) >= since_dt)
                        .unwrap_or(true)
                })
                .collect()
        } else {
            comments
        };

        // Skip threads with no new activity when filtering
        if since.is_some() && filtered_comments.is_empty() {
            continue;
        }

        total_new_comments += filtered_comments.len();
        threads_by_file
            .entry(thread.file_path.clone())
            .or_default()
            .push((thread.clone(), filtered_comments));
    }

    if since.is_some() && threads_by_file.is_empty() {
        println!("\n  No new activity since the specified time.");
        return Ok(());
    }

    for (file, file_threads) in threads_by_file {
        println!("\n━━━ {file} ━━━");

        for (thread, comments) in file_threads {
            let status_icon = if thread.status == "open" {
                "○"
            } else {
                "✓"
            };
            let line_info = match thread.selection_end {
                Some(end) if end != thread.selection_start => {
                    format!("lines {}-{}", thread.selection_start, end)
                }
                _ => format!("line {}", thread.selection_start),
            };

            let new_indicator = if since.is_some() {
                format!(" [+{}]", comments.len())
            } else {
                String::new()
            };

            println!(
                "\n  {} {} ({}){}",
                status_icon, thread.thread_id, line_info, new_indicator
            );

            // Show code context if requested
            if context_lines > 0 {
                let anchor_start = thread.selection_start as u32;
                let anchor_end = thread.selection_end.unwrap_or(thread.selection_start) as u32;

                if let Ok(ctx) = extract_context(
                    scm,
                    &file,
                    &commit_ref,
                    anchor_start,
                    anchor_end,
                    context_lines,
                ) {
                    // Indent the context
                    for line in format_context(&ctx).lines() {
                        println!("  {line}");
                    }
                }
            }

            // Show comments (already filtered)
            for comment in comments {
                println!(
                    "\n    ▸ {} ({}):",
                    comment.author,
                    &comment.created_at[..10]
                );
                for line in comment.body.lines() {
                    println!("       {line}");
                }
            }
        }
    }

    if since.is_some() {
        println!("\n  [{total_new_comments} new comment(s)]");
    }

    if include_diffs {
        print_file_diffs_text(scm, &review, &threads, &commit_ref, &file_cache, seal_root);
    }

    println!();
    Ok(())
}

fn print_file_diffs_text(
    scm: &dyn ScmRepo,
    review: &ReviewDetail,
    threads: &[ThreadSummary],
    commit_ref: &str,
    file_cache: &std::collections::HashMap<String, String>,
    seal_root: &Path,
) {
    let files_value = build_file_diffs(scm, review, threads, commit_ref, file_cache, seal_root);
    let Some(files) = files_value.as_array() else {
        return;
    };

    if files.is_empty() {
        println!("\n  No diff files found.");
        return;
    }

    println!("\n  Diffs:");
    for file in files {
        let path = file
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<unknown>");

        println!("\n━━━ {path} (diff) ━━━");

        if let Some(diff_text) = file.get("diff").and_then(serde_json::Value::as_str) {
            if diff_text.trim().is_empty() {
                println!("  (no textual diff)");
            } else {
                for line in diff_text.lines() {
                    println!("  {line}");
                }
            }
            continue;
        }

        if let Some(content) = file.get("content") {
            println!("  no diff available; content window: {content}");
        } else {
            println!("  (no diff available)");
        }
    }
}

/// Show inbox - reviews and threads needing the agent's attention.
#[tracing::instrument(skip(repo_root, format))]
pub fn run_inbox(repo_root: &Path, agent: &str, format: OutputFormat) -> Result<()> {
    ensure_initialized(repo_root)?;

    let services = open_services(repo_root)?;
    let inbox = services.inbox().get(agent)?;

    if matches!(format, OutputFormat::Json) {
        let formatter = Formatter::new(format);
        formatter.print(&inbox)?;
        return Ok(());
    }

    // Text/pretty output
    let total_items = inbox.reviews_awaiting_vote.len()
        + inbox.threads_with_new_responses.len()
        + inbox.open_threads_on_my_reviews.len();

    if total_items == 0 {
        println!("Inbox empty - no items need your attention");
        return Ok(());
    }

    println!("Inbox for {} ({} items)", agent, total_items);
    println!();

    // Section 1: Reviews awaiting vote
    if !inbox.reviews_awaiting_vote.is_empty() {
        println!(
            "Reviews awaiting your vote ({}):",
            inbox.reviews_awaiting_vote.len()
        );
        for r in &inbox.reviews_awaiting_vote {
            let threads_info = if r.open_thread_count > 0 {
                format!(" [{} open threads]", r.open_thread_count)
            } else {
                String::new()
            };
            let status_indicator = if r.request_status == "re-review" {
                " [re-review]"
            } else {
                ""
            };
            println!(
                "  {} · {} by {}{}{}",
                r.review_id, r.title, r.author, threads_info, status_indicator
            );
        }
        println!();
    }

    // Section 2: Threads with new responses
    if !inbox.threads_with_new_responses.is_empty() {
        println!(
            "Threads with new responses ({}):",
            inbox.threads_with_new_responses.len()
        );
        for t in &inbox.threads_with_new_responses {
            println!(
                "  {} · {}:{} (+{} new)",
                t.thread_id, t.file_path, t.selection_start, t.new_response_count
            );
            println!("    in {} ({})", t.review_id, t.review_title);
        }
        println!();
    }

    // Section 3: Open threads on my reviews
    if !inbox.open_threads_on_my_reviews.is_empty() {
        println!(
            "Open feedback on your reviews ({}):",
            inbox.open_threads_on_my_reviews.len()
        );
        for t in &inbox.open_threads_on_my_reviews {
            let comments_info = if t.comment_count > 0 {
                format!(" ({} comments)", t.comment_count)
            } else {
                String::new()
            };
            println!(
                "  {} · {}:{} by {}{}",
                t.thread_id, t.file_path, t.selection_start, t.thread_author, comments_info
            );
            println!("    in {} ({})", t.review_id, t.review_title);
        }
        println!();
    }

    Ok(())
}

// ============================================================================
// File diff helpers for --include-diffs
// ============================================================================

/// Extract code context from pre-fetched file content (avoids subprocess call).
fn extract_context_from_str(
    contents: &str,
    anchor_start: u32,
    anchor_end: u32,
    context_lines: u32,
) -> Option<seal_core::jj::context::CodeContext> {
    use seal_core::jj::context::{CodeContext, ContextLine};

    if anchor_start == 0 || anchor_end == 0 || anchor_start > anchor_end {
        return None;
    }

    let file_lines: Vec<&str> = contents.lines().collect();
    let total_lines = file_lines.len() as u32;
    if total_lines == 0 {
        return None;
    }

    let anchor_start = anchor_start.min(total_lines);
    let anchor_end = anchor_end.min(total_lines);
    let start_line = anchor_start.saturating_sub(context_lines).max(1);
    let end_line = (anchor_end + context_lines).min(total_lines);

    let mut lines = Vec::new();
    for line_num in start_line..=end_line {
        let idx = (line_num - 1) as usize;
        let content = file_lines.get(idx).unwrap_or(&"").to_string();
        let is_anchor = line_num >= anchor_start && line_num <= anchor_end;
        lines.push(ContextLine {
            line_number: line_num,
            content,
            is_anchor,
        });
    }

    Some(CodeContext {
        lines,
        start_line,
        end_line,
        anchor_start,
        anchor_end,
    })
}

/// Content window around an orphaned thread's anchor.
#[derive(serde::Serialize)]
struct ContentWindow {
    start_line: u32,
    lines: Vec<String>,
}

/// Build per-file diff data for files that have threads.
///
/// Uses a single `jj diff --git` call and splits the output by file in Rust,
/// avoiding N subprocess spawns. Orphaned thread content is fetched only for
/// files that need it.
///
/// Returns a JSON array of `{ path, diff, content }` objects.
/// - `diff`: unified diff text for files with changes, null otherwise
/// - `content`: windowed file content for orphaned threads, null otherwise
fn build_file_diffs(
    scm: &dyn ScmRepo,
    review: &ReviewDetail,
    threads: &[ThreadSummary],
    target_commit: &str,
    file_cache: &std::collections::HashMap<String, String>,
    seal_root: &Path,
) -> serde_json::Value {
    // Collect unique files that have threads
    let files_with_threads: std::collections::BTreeSet<String> =
        threads.iter().map(|t| t.file_path.clone()).collect();

    // Resolve base commit
    let base_commit = scm
        .parent_commit(target_commit)
        .unwrap_or_else(|_| review.initial_commit.clone());

    // Single diff call — split into per-file diffs in Rust
    let full_diff = scm
        .diff_git(&base_commit, target_commit)
        .unwrap_or_default();
    let diffs_by_file = split_diff_by_file(&full_diff);

    // All files: union of files with threads and files with diffs, filtered by sealignore
    let sealignore = SealIgnore::load(seal_root);
    let all_files: Vec<String> = {
        let mut files = files_with_threads;
        for key in diffs_by_file.keys() {
            files.insert((*key).to_string());
        }
        files
            .into_iter()
            .filter(|f| !sealignore.is_ignored(f))
            .collect()
    };

    let mut file_entries = Vec::new();

    for file_path in &all_files {
        let diff = diffs_by_file
            .get(file_path.as_str())
            .map(std::string::ToString::to_string);

        // Get threads for this file
        let file_threads: Vec<&ThreadSummary> = threads
            .iter()
            .filter(|t| &t.file_path == file_path)
            .collect();

        // Check for orphaned threads (selection_start not in any diff hunk)
        let content = if file_threads.is_empty() {
            None
        } else if let Some(ref diff_text) = diff {
            let hunks = parse_hunk_ranges(diff_text);
            let has_orphan = file_threads.iter().any(|t| {
                let line = t.selection_start as u32;
                !hunks.iter().any(|h| line >= h.0 && line <= h.1)
            });

            if has_orphan {
                build_content_window_from_cache(file_cache, file_path, &file_threads)
            } else {
                None
            }
        } else {
            // No diff at all — all threads are orphaned
            build_content_window_from_cache(file_cache, file_path, &file_threads)
        };

        file_entries.push(serde_json::json!({
            "path": file_path,
            "diff": diff,
            "content": content,
        }));
    }

    serde_json::json!(file_entries)
}

/// Split a full git-format diff into per-file sections.
///
/// Each `diff --git a/path b/path` header starts a new file section.
/// Returns a map from file path to the complete diff section for that file.
fn split_diff_by_file(full_diff: &str) -> std::collections::HashMap<&str, &str> {
    let mut result = std::collections::HashMap::new();
    let mut current_file: Option<&str> = None;
    let mut current_start: usize = 0;

    for (byte_offset, line) in line_byte_offsets(full_diff) {
        if line.starts_with("diff --git") {
            // Flush previous file
            if let Some(file) = current_file {
                let section = &full_diff[current_start..byte_offset];
                if !section.trim().is_empty() {
                    result.insert(file, section);
                }
            }
            // Parse new file path: "diff --git a/path b/path"
            current_file = line
                .split_whitespace()
                .nth(3)
                .map(|s| s.trim_start_matches("b/"));
            current_start = byte_offset;
        }
    }

    // Flush last file
    if let Some(file) = current_file {
        let section = &full_diff[current_start..];
        if !section.trim().is_empty() {
            result.insert(file, section);
        }
    }

    result
}

/// Iterate over lines with their byte offsets in the original string.
fn line_byte_offsets(s: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0;
    s.lines().map(move |line| {
        let start = offset;
        // +1 for the newline (or 0 if at end without trailing newline)
        offset += line.len() + 1;
        (start, line)
    })
}

/// Parse unified diff hunk headers to extract new-side line ranges.
///
/// Looks for `@@ ... +start,count @@` or `@@ ... +start @@` patterns
/// and returns `(start_line, end_line)` tuples (1-based, inclusive).
fn parse_hunk_ranges(diff: &str) -> Vec<(u32, u32)> {
    let mut ranges = Vec::new();
    for line in diff.lines() {
        if !line.starts_with("@@") {
            continue;
        }
        // Format: @@ -old_start,old_count +new_start,new_count @@
        // or:     @@ -old_start,old_count +new_start @@
        if let Some(plus_pos) = line.find('+') {
            let after_plus = &line[plus_pos + 1..];
            let end = after_plus.find([' ', '@']).unwrap_or(after_plus.len());
            let range_str = &after_plus[..end];

            if let Some((start_str, count_str)) = range_str.split_once(',') {
                if let (Ok(start), Ok(count)) = (start_str.parse::<u32>(), count_str.parse::<u32>())
                {
                    if count > 0 {
                        ranges.push((start, start + count - 1));
                    }
                }
            } else if let Ok(start) = range_str.parse::<u32>() {
                // Single line hunk: +start (count=1 implied)
                ranges.push((start, start));
            }
        }
    }
    ranges
}

/// Build a windowed content region covering all thread anchors in a file.
///
/// Uses 20-line padding around the min/max thread selections.
/// Reads from the pre-fetched file cache to avoid subprocess calls.
fn build_content_window_from_cache(
    file_cache: &std::collections::HashMap<String, String>,
    file_path: &str,
    threads: &[&ThreadSummary],
) -> Option<ContentWindow> {
    let contents = file_cache.get(file_path)?;
    let file_lines: Vec<&str> = contents.lines().collect();
    let total = file_lines.len() as u32;

    if total == 0 || threads.is_empty() {
        return None;
    }

    // Find the min/max lines across all threads
    let min_line = threads
        .iter()
        .map(|t| t.selection_start as u32)
        .min()
        .unwrap_or(1);
    let max_line = threads
        .iter()
        .map(|t| t.selection_end.unwrap_or(t.selection_start) as u32)
        .max()
        .unwrap_or(min_line);

    let padding = 20u32;
    let start = min_line.saturating_sub(padding).max(1);
    let end = (max_line + padding).min(total);

    let lines: Vec<String> = ((start - 1) as usize..end as usize)
        .map(|i| file_lines.get(i).unwrap_or(&"").to_string())
        .collect();

    Some(ContentWindow {
        start_line: start,
        lines,
    })
}

#[cfg(test)]
mod diff_tests {
    use super::*;

    #[test]
    fn test_parse_hunk_ranges_standard() {
        let diff = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,5 +10,8 @@ fn main() {
 some context
+added line
@@ -30,3 +33,6 @@ fn other() {
 more context";

        let ranges = parse_hunk_ranges(diff);
        assert_eq!(ranges, vec![(10, 17), (33, 38)]);
    }

    #[test]
    fn test_parse_hunk_ranges_single_line() {
        let diff = "@@ -5,1 +5 @@ fn foo() {\n+new line\n";
        let ranges = parse_hunk_ranges(diff);
        assert_eq!(ranges, vec![(5, 5)]);
    }

    #[test]
    fn test_parse_hunk_ranges_zero_count() {
        // Deletion-only hunk: +start,0 means no new lines
        let diff = "@@ -10,3 +10,0 @@ fn bar() {\n-removed\n";
        let ranges = parse_hunk_ranges(diff);
        assert!(ranges.is_empty());
    }

    #[test]
    fn test_parse_hunk_ranges_no_hunks() {
        let diff = "diff --git a/file b/file\n--- a/file\n+++ b/file\n";
        let ranges = parse_hunk_ranges(diff);
        assert!(ranges.is_empty());
    }

    #[test]
    fn test_split_diff_by_file_multiple_files() {
        let diff = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
+    println!(\"hello\");
 }
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -5,2 +5,3 @@
 pub fn foo() {
+    42
 }
";

        let result = split_diff_by_file(diff);
        assert_eq!(result.len(), 2);
        assert!(result.contains_key("src/main.rs"));
        assert!(result.contains_key("src/lib.rs"));
        assert!(result["src/main.rs"].contains("println"));
        assert!(result["src/lib.rs"].contains("42"));
        // Each section should NOT contain the other file's content
        assert!(!result["src/main.rs"].contains("pub fn foo"));
        assert!(!result["src/lib.rs"].contains("fn main"));
    }

    #[test]
    fn test_split_diff_by_file_single() {
        let diff =
            "diff --git a/foo.rs b/foo.rs\n--- a/foo.rs\n+++ b/foo.rs\n@@ -1 +1 @@\n-old\n+new\n";
        let result = split_diff_by_file(diff);
        assert_eq!(result.len(), 1);
        assert!(result.contains_key("foo.rs"));
    }

    #[test]
    fn test_split_diff_by_file_empty() {
        let result = split_diff_by_file("");
        assert!(result.is_empty());
    }
}

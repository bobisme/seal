//! Implementation of `seal threads` subcommands.

use anyhow::{bail, Context, Result};
use std::path::Path;

use crate::cli::commands::helpers::{
    ensure_initialized, open_services, resolve_review_thread_commit, review_not_found_error,
    thread_not_found_error,
};
use crate::output::{Formatter, OutputFormat};
use seal_core::events::CodeSelection;
use seal_core::jj::context::{extract_context, format_context};
use seal_core::scm::ScmRepo;

/// Create a new comment thread on a file.
///
/// # Arguments
/// * `seal_root` - Path to main repo (where .seal/ lives)
/// * `workspace_root` - Path to current workspace (for jj @ resolution)
pub fn run_threads_create(
    seal_root: &Path,
    scm: &dyn ScmRepo,
    review_id: &str,
    file: &str,
    lines: &str,
    author: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(seal_root)?;

    let services = open_services(seal_root)?;

    // Verify review exists
    let Some(review) = services.reviews().get_optional(review_id)? else {
        return Err(review_not_found_error(seal_root, review_id));
    };

    if review.status != "open" && review.status != "approved" {
        bail!(
            "Cannot create thread on review with status '{}': {}",
            review.status,
            review_id
        );
    }

    // Parse line selection
    let selection = parse_line_selection(lines)?;

    // Resolve review commit anchor (not current workspace commit).
    let commit_hash = resolve_review_thread_commit(scm, &review);

    // Verify file exists at the review's commit anchor.
    if !scm.file_exists(&commit_hash, file)? {
        bail!("File does not exist in review {review_id} at {commit_hash}: {file}");
    }

    // Use core service to create the thread
    let thread_id = services.threads().create(
        review_id,
        file,
        selection.clone(),
        commit_hash.clone(),
        author,
    )?;

    let author_str = seal_core::events::get_agent_identity(author)?;

    // Output result
    let result = serde_json::json!({
        "thread_id": thread_id,
        "review_id": review_id,
        "file_path": file,
        "selection_start": selection.start_line(),
        "selection_end": selection.end_line(),
        "commit_hash": commit_hash,
        "author": author_str,
    });

    let formatter = Formatter::new(format);
    formatter.print(&result)?;

    Ok(())
}

/// List threads for a review with optional filters.
pub fn run_threads_list(
    repo_root: &Path,
    review_id: &str,
    status: Option<&str>,
    file: Option<&str>,
    verbose: bool,
    since: Option<chrono::DateTime<chrono::Utc>>,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(repo_root)?;

    let services = open_services(repo_root)?;

    // Verify review exists
    if services.reviews().get_optional(review_id)?.is_none() {
        return Err(review_not_found_error(repo_root, review_id));
    }

    let threads = services.threads().list(review_id, status, file)?;

    // Filter threads by --since (only those with recent comments)
    let threads: Vec<_> = if let Some(since_dt) = since {
        threads
            .into_iter()
            .filter(|t| {
                let comments = services.comments().list(&t.thread_id).unwrap_or_default();
                comments.iter().any(|c| {
                    chrono::DateTime::parse_from_rfc3339(&c.created_at)
                        .map(|dt| dt.with_timezone(&chrono::Utc) >= since_dt)
                        .unwrap_or(true)
                })
            })
            .collect()
    } else {
        threads
    };

    // Build context-aware empty message
    let empty_msg = if since.is_some() {
        "No threads with activity since the specified time"
    } else if status.is_some() || file.is_some() {
        "No threads match the filters"
    } else {
        "No threads yet"
    };

    if verbose && !threads.is_empty() {
        // Verbose mode: show first comment for each thread
        for thread in &threads {
            let line_range = match thread.selection_end {
                Some(end) if end != thread.selection_start => {
                    format!("{}:{}-{}", thread.file_path, thread.selection_start, end)
                }
                _ => format!("{}:{}", thread.file_path, thread.selection_start),
            };

            let status_icon = if thread.status == "open" {
                "○"
            } else {
                "✓"
            };

            println!(
                "{} {} {} ({}, {} comment{})",
                status_icon,
                thread.thread_id,
                line_range,
                thread.status,
                thread.comment_count,
                if thread.comment_count == 1 { "" } else { "s" }
            );

            // Get first comment if any
            let comments = services.comments().list(&thread.thread_id)?;
            if let Some(first) = comments.first() {
                // Truncate body to first line or 80 chars
                let preview: String = first
                    .body
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect();
                let ellipsis = if first.body.len() > 80 || first.body.contains('\n') {
                    "..."
                } else {
                    ""
                };
                println!("    {}: {}{}", first.author, preview, ellipsis);
            }
        }
    } else {
        let formatter = Formatter::new(format);
        formatter.print_list(
            &threads,
            empty_msg,
            "threads",
            &["seal threads show <id>", "seal threads resolve <id>"],
        )?;
    }

    Ok(())
}

/// Show details for a specific thread with optional context.
///
/// # Arguments
/// * `seal_root` - Path to main repo (where .seal/ lives)
/// * `workspace_root` - Path to current workspace (for jj @ resolution)
pub fn run_threads_show(
    seal_root: &Path,
    scm: &dyn ScmRepo,
    thread_id: &str,
    context_lines: u32,
    use_current: bool,
    conversation: bool,
    use_color: bool,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(seal_root)?;

    let services = open_services(seal_root)?;
    let thread = services.threads().get_optional(thread_id)?;

    match thread {
        Some(t) => {
            // If context requested, extract it (use workspace for jj context)
            let code_context = if context_lines > 0 {
                let anchor_start = t.selection_start as u32;
                let anchor_end = t.selection_end.unwrap_or(t.selection_start) as u32;

                // Use current commit or original commit based on flag
                let commit_ref = if use_current {
                    scm.current_commit()
                        .unwrap_or_else(|_| t.commit_hash.clone())
                } else {
                    t.commit_hash.clone()
                };

                match extract_context(
                    scm,
                    &t.file_path,
                    &commit_ref,
                    anchor_start,
                    anchor_end,
                    context_lines,
                ) {
                    Ok(ctx) => Some(ctx),
                    Err(e) => {
                        // Context extraction failed, but we can still show the thread
                        tracing::warn!("could not extract context: {}", e);
                        None
                    }
                }
            } else {
                None
            };

            // Build output based on format
            if conversation {
                // Conversation format: human-readable with timestamps
                print_conversation(&t, code_context.as_ref(), use_color);
            } else if matches!(format, OutputFormat::Json) {
                // For JSON, include context as structured data
                let mut result = serde_json::to_value(&t)?;
                if let Some(ctx) = code_context {
                    result["code_context"] = serde_json::to_value(&ctx)?;
                }
                let formatter = Formatter::new(format);
                formatter.print(&result)?;
            } else {
                // For text/pretty, print thread details then context
                let formatter = Formatter::new(format);
                formatter.print(&t)?;

                if let Some(ctx) = code_context {
                    println!("\nCode context:");
                    print!("{}", format_context(&ctx));
                }
            }
        }
        None => {
            return Err(thread_not_found_error(seal_root, thread_id));
        }
    }

    Ok(())
}

/// ANSI color codes for terminal output
mod colors {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const BLUE: &str = "\x1b[34m";
    pub const MAGENTA: &str = "\x1b[35m";
    pub const CYAN: &str = "\x1b[36m";
}

/// Format and print a thread as a human-readable conversation.
fn print_conversation(
    thread: &seal_core::projection::ThreadDetail,
    code_context: Option<&seal_core::jj::context::CodeContext>,
    use_color: bool,
) {
    // Color helpers
    let c = |color: &str, text: &str| -> String {
        if use_color {
            format!("{}{}{}", color, text, colors::RESET)
        } else {
            text.to_string()
        }
    };

    let bold = |text: &str| -> String {
        if use_color {
            format!("{}{}{}", colors::BOLD, text, colors::RESET)
        } else {
            text.to_string()
        }
    };

    // Header with thread info
    let status_indicator = if thread.status == "resolved" {
        c(colors::GREEN, "[RESOLVED]")
    } else {
        c(colors::YELLOW, "[OPEN]")
    };

    let line_range = match thread.selection_end {
        Some(end) if end != thread.selection_start => {
            format!("lines {}-{}", thread.selection_start, end)
        }
        _ => format!("line {}", thread.selection_start),
    };

    println!(
        "{} {} on {} ({})",
        bold(&format!("Thread {}", thread.thread_id)),
        status_indicator,
        c(colors::CYAN, &thread.file_path),
        c(colors::DIM, &line_range)
    );
    println!("{}", "=".repeat(60));

    // Show code context if available
    if let Some(ctx) = code_context {
        println!();
        print!("{}", format_context(ctx));
        println!("{}", "-".repeat(60));
    }

    // Thread creation
    println!(
        "\n{} started this thread ({})",
        c(colors::BLUE, &thread.author),
        c(colors::DIM, &format_timestamp(&thread.created_at))
    );

    // Comments as conversation
    for comment in &thread.comments {
        println!();
        println!(
            "{} ({})",
            c(colors::MAGENTA, &comment.author),
            c(colors::DIM, &format_timestamp(&comment.created_at))
        );
        // Indent the body for readability
        for line in comment.body.lines() {
            println!("  {line}");
        }
    }

    // Status changes
    if thread.status == "resolved" {
        if let Some(ref changed_by) = thread.status_changed_by {
            println!();
            let timestamp = thread
                .status_changed_at
                .as_deref()
                .map_or_else(|| "unknown time".to_string(), format_timestamp);
            print!(
                "{} {} ({})",
                c(colors::GREEN, changed_by),
                c(colors::GREEN, "resolved this thread"),
                c(colors::DIM, &timestamp)
            );
            if let Some(ref reason) = thread.resolve_reason {
                print!(": {reason}");
            }
            println!();
        }
    }

    println!();
}

/// Format an ISO timestamp to a more readable form.
fn format_timestamp(iso_timestamp: &str) -> String {
    // Parse ISO 8601 format: 2026-01-25T12:34:56.789Z or 2026-01-25T12:34:56Z
    // Return a more readable format: "Jan 25, 12:34"
    if let Some(datetime_part) = iso_timestamp.split('T').nth(1) {
        if let Some(time_part) = datetime_part.split('.').next() {
            // Get just HH:MM
            let time_short = time_part.split(':').take(2).collect::<Vec<_>>().join(":");
            // Get the date part
            if let Some(date_part) = iso_timestamp.split('T').next() {
                let parts: Vec<&str> = date_part.split('-').collect();
                if parts.len() == 3 {
                    let month = match parts[1] {
                        "01" => "Jan",
                        "02" => "Feb",
                        "03" => "Mar",
                        "04" => "Apr",
                        "05" => "May",
                        "06" => "Jun",
                        "07" => "Jul",
                        "08" => "Aug",
                        "09" => "Sep",
                        "10" => "Oct",
                        "11" => "Nov",
                        "12" => "Dec",
                        _ => parts[1],
                    };
                    let day = parts[2].trim_start_matches('0');
                    return format!("{month} {day}, {time_short}");
                }
            }
        }
    }
    // Fallback: return as-is
    iso_timestamp.to_string()
}

/// Resolve a thread (or all threads matching criteria).
/// Supports batch resolve: pass multiple thread IDs to resolve them all at once.
pub fn run_threads_resolve(
    repo_root: &Path,
    thread_ids: &[String],
    all: bool,
    file: Option<&str>,
    reason: Option<String>,
    author: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(repo_root)?;

    if !all && thread_ids.is_empty() {
        bail!("Either specify thread_id(s) or use --all");
    }

    if all && !thread_ids.is_empty() {
        bail!("Cannot specify both thread_id(s) and --all");
    }

    let services = open_services(repo_root)?;

    let mut resolved_count = 0;
    let mut resolved_ids = Vec::new();

    if all {
        // Resolve all open threads, optionally filtered by file
        // The CLI doesn't pass review_id to resolve, so --all resolves across all reviews.

        // Get all threads and filter to open ones
        let all_reviews = services.reviews().list(None, None)?;
        for review in all_reviews {
            let threads = services
                .threads()
                .list(&review.review_id, Some("open"), file)?;
            for thread in threads {
                services
                    .threads()
                    .resolve(&thread.thread_id, reason.clone(), author)?;
                resolved_ids.push(thread.thread_id);
                resolved_count += 1;
            }
        }
    } else {
        // Resolve one or more threads by ID
        for tid in thread_ids {
            let thread = match services.threads().get_optional(tid)? {
                None => return Err(thread_not_found_error(repo_root, tid)),
                Some(t) if t.status == "resolved" => {
                    bail!("Thread is already resolved: {tid}");
                }
                Some(t) => t,
            };
            drop(thread);

            services.threads().resolve(tid, reason.clone(), author)?;
            resolved_ids.push(tid.clone());
            resolved_count += 1;
        }
    }

    let result = serde_json::json!({
        "resolved_count": resolved_count,
        "thread_ids": resolved_ids,
        "reason": reason,
    });

    let formatter = Formatter::new(format);
    formatter.print(&result)?;

    Ok(())
}

/// Reopen a resolved thread.
pub fn run_threads_reopen(
    repo_root: &Path,
    thread_id: &str,
    reason: Option<String>,
    author: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(repo_root)?;

    let services = open_services(repo_root)?;

    let thread = match services.threads().get_optional(thread_id)? {
        None => return Err(thread_not_found_error(repo_root, thread_id)),
        Some(t) if t.status != "resolved" => {
            bail!(
                "Cannot reopen thread with status '{}': {}",
                t.status,
                thread_id
            );
        }
        Some(t) => t,
    };
    drop(thread);

    services
        .threads()
        .reopen(thread_id, reason.clone(), author)?;

    let result = serde_json::json!({
        "thread_id": thread_id,
        "status": "open",
        "reason": reason,
    });

    let formatter = Formatter::new(format);
    formatter.print(&result)?;

    Ok(())
}

// ============================================================================
// Helpers
// ============================================================================

/// Parse a line selection string like "42" or "10-20".
pub fn parse_line_selection(lines: &str) -> Result<CodeSelection> {
    if lines.contains('-') {
        let parts: Vec<&str> = lines.split('-').collect();
        if parts.len() != 2 {
            bail!("Invalid line range format: '{lines}'. Expected 'start-end'");
        }
        let start: u32 = parts[0]
            .trim()
            .parse()
            .with_context(|| format!("Invalid start line: '{}'", parts[0]))?;
        let end: u32 = parts[1]
            .trim()
            .parse()
            .with_context(|| format!("Invalid end line: '{}'", parts[1]))?;

        if start == 0 || end == 0 {
            bail!("Line numbers must be 1-based");
        }
        if start > end {
            bail!("Start line ({start}) must be <= end line ({end})");
        }

        Ok(CodeSelection::range(start, end))
    } else {
        let line: u32 = lines
            .trim()
            .parse()
            .with_context(|| format!("Invalid line number: '{lines}'"))?;

        if line == 0 {
            bail!("Line numbers must be 1-based");
        }

        Ok(CodeSelection::line(line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_line_selection_single() {
        let sel = parse_line_selection("42").unwrap();
        assert_eq!(sel.start_line(), 42);
        assert_eq!(sel.end_line(), 42);
    }

    #[test]
    fn test_parse_line_selection_range() {
        let sel = parse_line_selection("10-20").unwrap();
        assert_eq!(sel.start_line(), 10);
        assert_eq!(sel.end_line(), 20);
    }

    #[test]
    fn test_parse_line_selection_range_with_spaces() {
        let sel = parse_line_selection("10 - 20").unwrap();
        assert_eq!(sel.start_line(), 10);
        assert_eq!(sel.end_line(), 20);
    }

    #[test]
    fn test_parse_line_selection_invalid_zero() {
        assert!(parse_line_selection("0").is_err());
        assert!(parse_line_selection("0-10").is_err());
        assert!(parse_line_selection("10-0").is_err());
    }

    #[test]
    fn test_parse_line_selection_invalid_range() {
        assert!(parse_line_selection("20-10").is_err());
    }

    #[test]
    fn test_parse_line_selection_invalid_format() {
        assert!(parse_line_selection("abc").is_err());
        assert!(parse_line_selection("10-20-30").is_err());
        assert!(parse_line_selection("").is_err());
    }
}

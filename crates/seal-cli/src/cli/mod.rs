//! CLI command definitions and handlers.

use clap::{Parser, Subcommand};
use std::io::IsTerminal;

pub mod commands;

use crate::output::OutputFormat;
use seal_core::scm::ScmPreference;

/// Agent-centric distributed code review tool for Git and jj
#[derive(Parser, Debug)]
#[command(name = "seal")]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Output format (default: auto-detected based on TTY - 'pretty' for interactive, 'text' for pipes)
    #[arg(long, global = true, value_enum)]
    pub format: Option<OutputFormat>,

    /// Hidden alias for --format=json
    #[arg(long, global = true, hide = true)]
    pub json: bool,

    /// Override agent identity (falls back to env vars, then $USER if TTY)
    #[arg(long, global = true)]
    pub agent: Option<String>,

    /// Path to repository (can be repo root, .seal dir, or subdirectory)
    #[arg(long, global = true)]
    pub path: Option<std::path::PathBuf>,

    /// Select SCM backend (auto-detected by default)
    #[arg(long, global = true, value_enum)]
    pub scm: Option<ScmPreference>,

    #[command(subcommand)]
    pub command: Commands,
}

impl Cli {
    /// Get the effective output format with priority chain:
    /// 1. --format flag (highest priority)
    /// 2. --json alias
    /// 3. FORMAT environment variable
    /// 4. TTY auto-detection: Pretty for TTY, Text otherwise
    #[must_use]
    pub fn output_format(&self) -> OutputFormat {
        // Priority 1: --format flag
        if let Some(format) = self.format {
            return format;
        }

        // Priority 2: --json alias
        if self.json {
            return OutputFormat::Json;
        }

        // Priority 3: FORMAT environment variable
        if let Ok(format_str) = std::env::var("FORMAT") {
            match format_str.to_lowercase().as_str() {
                "pretty" => return OutputFormat::Pretty,
                "text" => return OutputFormat::Text,
                "json" => return OutputFormat::Json,
                "toon" => {
                    tracing::warn!("'toon' format has been removed, using 'text' instead");
                    return OutputFormat::Text;
                }
                _ => {
                    tracing::warn!("unknown FORMAT value '{format_str}', using auto-detection");
                }
            }
        }

        // Priority 4: TTY auto-detection
        if std::io::stdout().is_terminal() {
            OutputFormat::Pretty
        } else {
            OutputFormat::Text
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Initialize a new .seal directory in the current repository
    Init,

    /// Health check - verify SCM detection, .seal/, and sync status
    Doctor,

    /// Migrate from v1 (single events.jsonl) to v2 (per-review event logs)
    Migrate {
        /// Show what would be migrated without making changes
        #[arg(long)]
        dry_run: bool,

        /// Keep backup of old events.jsonl (default: true)
        #[arg(long, default_value = "true")]
        backup: bool,

        /// Re-migrate from v1 backup even if already on v2.
        /// Use this to recover data lost by a buggy earlier migration.
        #[arg(long)]
        from_backup: bool,
    },

    /// Manage AGENTS.md integration
    #[command(subcommand)]
    Agents(AgentsCommands),

    /// Manage code reviews
    #[command(subcommand)]
    Reviews(ReviewsCommands),

    /// Manage comment threads
    #[command(subcommand)]
    Threads(ThreadsCommands),

    /// Manage comments
    #[command(subcommand)]
    Comments(CommentsCommands),

    /// Show status of reviews
    Status {
        /// Review ID (optional - shows all if omitted)
        review_id: Option<String>,

        /// Show only unresolved threads
        #[arg(long)]
        unresolved_only: bool,
    },

    /// Show diff for a review
    Diff {
        /// Review ID
        review_id: String,
    },

    /// Interactive UI for browsing reviews
    Ui,

    /// Add a comment to a review (auto-creates thread if needed). Use `reply` to respond to an existing thread.
    Comment {
        /// Review ID
        review_id: String,

        /// File path
        #[arg(long)]
        file: String,

        /// Line number or range (e.g., "42" or "10-20")
        #[arg(long, visible_alias = "lines")]
        line: String,

        /// Comment message
        #[arg(value_name = "MESSAGE")]
        message: String,
    },

    /// Approve a review (LGTM - Looks Good To Me)
    Lgtm {
        /// Review ID
        review_id: String,

        /// Optional approval message
        #[arg(long = "message", short = 'm')]
        message: Option<String>,
    },

    /// Block a review (request changes before merge)
    Block {
        /// Review ID
        review_id: String,

        /// Reason for blocking (required)
        #[arg(long = "reason", short = 'r')]
        reason: String,
    },

    /// Show full review with all threads and comments
    Review {
        /// Review ID
        review_id: String,

        /// Number of context lines around each thread (default: 3)
        #[arg(long, default_value = "3")]
        context: u32,

        /// Hide code context
        #[arg(long)]
        no_context: bool,

        /// Only show activity since this timestamp (ISO 8601 or relative like "1h", "2d")
        #[arg(long)]
        since: Option<String>,

        /// Include per-file diffs and orphaned thread content in JSON output
        #[arg(long)]
        include_diffs: bool,
    },

    /// Reply to an existing thread (shortcut for `comments add`)
    Reply {
        /// Thread ID
        thread_id: String,

        /// Reply message
        #[arg(value_name = "MESSAGE")]
        message: String,
    },

    /// Show reviews and threads needing your attention
    Inbox,

    /// Sync projection database from event logs
    Sync {
        /// Full rebuild from scratch (destructive)
        #[arg(long)]
        rebuild: bool,

        /// Re-baseline a specific review file after regression
        #[arg(long, value_name = "REVIEW_ID")]
        accept_regression: Option<String>,
    },

    /// Import findings from a SARIF file into an existing review
    #[command(subcommand)]
    Sarif(SarifCommands),
}

// ============================================================================
// SARIF subcommands
// ============================================================================

#[derive(Subcommand, Debug)]
pub enum SarifCommands {
    /// Import a SARIF file into an existing review.
    ///
    /// Each finding above --min-level becomes a comment thread anchored to
    /// the finding's file and line. A fingerprint is embedded in each comment
    /// so re-importing the same scan is idempotent.
    Import {
        /// Path to a SARIF 2.x JSON file
        file: std::path::PathBuf,

        /// Review ID to attach findings to (must be open)
        #[arg(long)]
        review: String,

        /// Minimum severity to import: none, note, warning, error
        #[arg(long, default_value = "warning")]
        min_level: String,
    },
}

// ============================================================================
// Agents subcommands
// ============================================================================

#[derive(Subcommand, Debug)]
pub enum AgentsCommands {
    /// Insert seal instructions into AGENTS.md
    Init,
    /// Print seal instructions to stdout
    Show,
}

// ============================================================================
// Reviews subcommands
// ============================================================================

#[derive(Subcommand, Debug)]
pub enum ReviewsCommands {
    /// Create a new review for the current change
    Create {
        /// Review title
        #[arg(long)]
        title: String,

        /// Optional description
        #[arg(long = "description", visible_alias = "desc")]
        description: Option<String>,

        /// Comma-separated list of reviewers to request
        #[arg(long = "reviewers", visible_alias = "reviewer")]
        reviewers: Option<String>,
    },

    /// List reviews
    List {
        /// Filter by status
        #[arg(long)]
        status: Option<ReviewStatus>,

        /// Filter by author
        #[arg(long)]
        author: Option<String>,

        /// Show only reviews where I am a requested reviewer
        #[arg(long)]
        needs_review: bool,

        /// Show only reviews with unresolved threads
        #[arg(long)]
        has_unresolved: bool,
    },

    /// Show review details
    Show {
        /// Review ID
        review_id: String,
    },

    /// Request reviewers for a review
    Request {
        /// Review ID
        review_id: String,

        /// Comma-separated list of reviewers
        #[arg(long = "reviewers", visible_alias = "reviewer")]
        reviewers: String,
    },

    /// Approve a review
    Approve {
        /// Review ID
        review_id: String,
    },

    /// Abandon a review
    Abandon {
        /// Review ID
        review_id: String,

        /// Reason for abandoning
        #[arg(long)]
        reason: Option<String>,
    },

    /// Mark a review as merged (records that the code has landed)
    #[command(name = "mark-merged")]
    MarkMerged {
        /// Review ID
        review_id: String,

        /// Final commit hash (auto-detected from @ if not provided)
        #[arg(long)]
        commit: Option<String>,

        /// Auto-approve before merging (for solo/self-review workflows)
        #[arg(long)]
        self_approve: bool,
    },
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum ReviewStatus {
    Open,
    Approved,
    Merged,
    Abandoned,
}

// ============================================================================
// Threads subcommands
// ============================================================================

#[derive(Subcommand, Debug)]
pub enum ThreadsCommands {
    /// Create a new comment thread
    Create {
        /// Review ID
        review_id: String,

        /// File path
        #[arg(long)]
        file: String,

        /// Line or range (e.g., "42" or "10-20")
        #[arg(long)]
        lines: String,
    },

    /// List threads for a review
    List {
        /// Review ID
        review_id: String,

        /// Filter by status
        #[arg(long)]
        status: Option<ThreadStatus>,

        /// Filter by file path
        #[arg(long)]
        file: Option<String>,

        /// Show first comment body for each thread
        #[arg(long, short = 'v')]
        verbose: bool,

        /// Only show threads with activity since this timestamp
        #[arg(long)]
        since: Option<String>,
    },

    /// Show thread details with context
    Show {
        /// Thread ID
        thread_id: String,

        /// Number of context lines (default: 3)
        #[arg(long, default_value = "3")]
        context: u32,

        /// Hide code context (shorthand for --context 0)
        #[arg(long)]
        no_context: bool,

        /// Show context at current commit instead of original
        #[arg(long)]
        current: bool,

        /// Display as human-readable conversation with timestamps
        #[arg(long)]
        conversation: bool,

        /// Disable colored output
        #[arg(long)]
        no_color: bool,
    },

    /// Resolve a thread
    Resolve {
        /// Thread IDs (can specify multiple, or use --all)
        thread_ids: Vec<String>,

        /// Resolve all threads matching criteria
        #[arg(long)]
        all: bool,

        /// Filter by file (with --all)
        #[arg(long)]
        file: Option<String>,

        /// Reason for resolving
        #[arg(long)]
        reason: Option<String>,
    },

    /// Reopen a resolved thread
    Reopen {
        /// Thread ID
        thread_id: String,

        /// Reason for reopening
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum ThreadStatus {
    Open,
    Resolved,
}

// ============================================================================
// Comments subcommands
// ============================================================================

#[derive(Subcommand, Debug)]
pub enum CommentsCommands {
    /// Add a comment to a thread
    Add {
        /// Thread ID
        thread_id: String,

        /// Comment message (positional or use --message)
        #[arg(long = "message", visible_alias = "msg")]
        message: Option<String>,

        /// Comment message (positional argument)
        #[arg(value_name = "MESSAGE")]
        message_positional: Option<String>,
    },

    /// List comments in a thread
    List {
        /// Thread ID
        thread_id: String,
    },
}

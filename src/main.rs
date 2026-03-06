//! seal - Agent-centric distributed code review tool for Git and jj

use anyhow::Result;
use clap::Parser;
use std::env;

use seal::cli::commands::{
    run_agents_init, run_agents_show, run_block, run_comment, run_comments_add, run_comments_list,
    run_diff, run_doctor, run_inbox, run_init, run_lgtm, run_migrate, run_review,
    run_reviews_abandon, run_reviews_approve, run_reviews_create, run_reviews_list,
    run_reviews_merge, run_reviews_request, run_reviews_show, run_status, run_sync,
    run_threads_create, run_threads_list, run_threads_reopen, run_threads_resolve,
    run_threads_show,
};
use seal::cli::{
    AgentsCommands, Cli, Commands, CommentsCommands, ReviewsCommands, ThreadsCommands,
};
use seal::events::get_agent_identity;
use seal::jj::{resolve_seal_root_from_path, resolve_workspace_root};
use seal::scm::{resolve_backend, resolve_preference};

/// Resolve identity based on CLI flags.
/// Priority: --agent > BOTSEAL_AGENT/SEAL_AGENT/AGENT/BOTBUS_AGENT > $USER (TTY only)
fn resolve_identity(cli: &Cli) -> Result<Option<String>> {
    if let Some(ref agent) = cli.agent {
        return Ok(Some(agent.clone()));
    }
    // Will be resolved lazily by get_agent_identity when needed
    Ok(None)
}

fn main() -> Result<()> {
    let _telemetry = seal::telemetry::init();
    let cli = Cli::parse();

    // We need two paths:
    // 1. seal_root: where .seal/ lives (workspace-local, so changes are tracked in workspace)
    // 2. workspace_root: where jj commands run (current workspace, for @ resolution)
    //
    // IMPORTANT: seal_root uses workspace-local path (not following .jj/repo pointer to main).
    // This ensures that seal changes are tracked in the workspace's jj working copy,
    // so they're included when the workspace is merged back to main.
    let (seal_root, workspace_root) = if let Some(path) = &cli.path {
        // --path provided: prefer an existing .seal root, otherwise fall back to
        // detected repository root (jj/git), then the provided path itself.
        let seal_root = resolve_seal_root_from_path(path)
            .or_else(|_| resolve_workspace_root(path))
            .unwrap_or_else(|_| {
                seal::scm::git::detect_git_root(path).unwrap_or_else(|| path.clone())
            });
        // Use the seal root as workspace root too (user-specified path)
        (seal_root.clone(), seal_root)
    } else {
        // No --path: use current directory's workspace root
        let workspace_root = env::current_dir()?;
        let seal_root = resolve_seal_root_from_path(&workspace_root)
            .or_else(|_| resolve_workspace_root(&workspace_root))
            .unwrap_or_else(|_| {
                seal::scm::git::detect_git_root(&workspace_root)
                    .unwrap_or_else(|| workspace_root.clone())
            });
        (seal_root, workspace_root)
    };

    let scm_preference = resolve_preference(cli.scm)?;

    // Determine output format (--format flag, --json alias, FORMAT env, or TTY detection)
    let format = cli.output_format();

    // Resolve identity (--agent override, otherwise deferred to env vars / TTY fallback)
    let identity = resolve_identity(&cli)?;

    match cli.command {
        Commands::Init => {
            run_init(&seal_root)?;
        }

        Commands::Doctor => {
            run_doctor(&seal_root, &workspace_root, scm_preference, format)?;
        }

        Commands::Migrate {
            dry_run,
            backup,
            from_backup,
        } => {
            run_migrate(&seal_root, dry_run, backup, from_backup, format)?;
        }

        Commands::Agents(cmd) => match cmd {
            AgentsCommands::Init => {
                run_agents_init(&seal_root)?;
            }
            AgentsCommands::Show => {
                run_agents_show()?;
            }
        },

        Commands::Reviews(cmd) => match cmd {
            ReviewsCommands::Create {
                title,
                description,
                reviewers,
            } => {
                let scm = resolve_backend(&workspace_root, scm_preference)?;
                run_reviews_create(
                    &seal_root,
                    scm.as_ref(),
                    title,
                    description,
                    reviewers,
                    identity.as_deref(),
                    format,
                )?;
            }
            ReviewsCommands::List {
                status,
                author,
                needs_review,
                has_unresolved,
            } => {
                let status_str = status.map(|s| match s {
                    seal::cli::ReviewStatus::Open => "open",
                    seal::cli::ReviewStatus::Approved => "approved",
                    seal::cli::ReviewStatus::Merged => "merged",
                    seal::cli::ReviewStatus::Abandoned => "abandoned",
                });
                // For --needs-review, use the subcommand --author as identity (if provided),
                // falling back to resolved identity.
                // When --needs-review is used, --author should NOT also filter by review author.
                let (author_filter, needs_reviewer) = if needs_review {
                    // Use --author for identity, not filtering
                    let id = author.as_deref().or(identity.as_deref());
                    (None, Some(get_agent_identity(id)?))
                } else {
                    // Normal case: --author filters by review author
                    (author.as_deref().map(String::from), None)
                };
                run_reviews_list(
                    &seal_root,
                    status_str,
                    author_filter.as_deref(),
                    needs_reviewer.as_deref(),
                    has_unresolved,
                    format,
                )?;
            }
            ReviewsCommands::Show { review_id } => {
                run_reviews_show(&seal_root, &review_id, format)?;
            }
            ReviewsCommands::Request {
                review_id,
                reviewers,
            } => {
                run_reviews_request(
                    &seal_root,
                    &review_id,
                    &reviewers,
                    identity.as_deref(),
                    format,
                )?;
            }
            ReviewsCommands::Approve { review_id } => {
                run_reviews_approve(&seal_root, &review_id, identity.as_deref(), format)?;
            }
            ReviewsCommands::Abandon { review_id, reason } => {
                run_reviews_abandon(&seal_root, &review_id, reason, identity.as_deref(), format)?;
            }
            ReviewsCommands::MarkMerged {
                review_id,
                commit,
                self_approve,
            } => {
                let scm = resolve_backend(&workspace_root, scm_preference)?;
                run_reviews_merge(
                    &seal_root,
                    scm.as_ref(),
                    &review_id,
                    commit,
                    self_approve,
                    identity.as_deref(),
                    format,
                )?;
            }
        },

        Commands::Threads(cmd) => match cmd {
            ThreadsCommands::Create {
                review_id,
                file,
                lines,
            } => {
                let scm = resolve_backend(&workspace_root, scm_preference)?;
                run_threads_create(
                    &seal_root,
                    scm.as_ref(),
                    &review_id,
                    &file,
                    &lines,
                    identity.as_deref(),
                    format,
                )?;
            }
            ThreadsCommands::List {
                review_id,
                status,
                file,
                verbose,
                since,
            } => {
                let status_str = status.map(|s| match s {
                    seal::cli::ThreadStatus::Open => "open",
                    seal::cli::ThreadStatus::Resolved => "resolved",
                });
                let since_dt = since
                    .map(|s| seal::cli::commands::reviews::parse_since(&s))
                    .transpose()?;
                run_threads_list(
                    &seal_root,
                    &review_id,
                    status_str,
                    file.as_deref(),
                    verbose,
                    since_dt,
                    format,
                )?;
            }
            ThreadsCommands::Show {
                thread_id,
                context,
                no_context,
                current,
                conversation,
                no_color,
            } => {
                // --no-context overrides --context
                let context_lines = if no_context { 0 } else { context };
                let scm = resolve_backend(&workspace_root, scm_preference)?;
                run_threads_show(
                    &seal_root,
                    scm.as_ref(),
                    &thread_id,
                    context_lines,
                    current,
                    conversation,
                    !no_color, // use_color
                    format,
                )?;
            }
            ThreadsCommands::Resolve {
                thread_ids,
                all,
                file,
                reason,
            } => {
                run_threads_resolve(
                    &seal_root,
                    &thread_ids,
                    all,
                    file.as_deref(),
                    reason,
                    identity.as_deref(),
                    format,
                )?;
            }
            ThreadsCommands::Reopen { thread_id, reason } => {
                run_threads_reopen(&seal_root, &thread_id, reason, identity.as_deref(), format)?;
            }
        },

        Commands::Comments(cmd) => match cmd {
            CommentsCommands::Add {
                thread_id,
                message,
                message_positional,
            } => {
                // Support both --message and positional argument
                let msg = message.or(message_positional).ok_or_else(|| {
                    anyhow::anyhow!("Message is required (use --message or provide as argument)")
                })?;
                run_comments_add(&seal_root, &thread_id, &msg, identity.as_deref(), format)?;
            }
            CommentsCommands::List { thread_id } => {
                run_comments_list(&seal_root, &thread_id, format)?;
            }
        },

        Commands::Status {
            review_id,
            unresolved_only,
        } => {
            let scm = resolve_backend(&workspace_root, scm_preference)?;
            run_status(
                &seal_root,
                scm.as_ref(),
                review_id.as_deref(),
                unresolved_only,
                format,
            )?;
        }

        Commands::Diff { review_id } => {
            let scm = resolve_backend(&workspace_root, scm_preference)?;
            run_diff(&seal_root, scm.as_ref(), &review_id, format)?;
        }

        Commands::Ui => {
            eprintln!("WARNING: `seal ui` is deprecated and will be removed in a future release.");
            eprintln!("  Install botseal-ui for the canonical interactive review experience:");
            eprintln!("    cargo install --git https://github.com/bobisme/seal-ui");
            eprintln!("  Then run: seal-ui");
            eprintln!();
            seal::tui::run(&seal_root)?;
        }

        Commands::Comment {
            review_id,
            file,
            line,
            message,
        } => {
            let scm = resolve_backend(&workspace_root, scm_preference)?;
            run_comment(
                &seal_root,
                scm.as_ref(),
                &review_id,
                &file,
                &line,
                &message,
                identity.as_deref(),
                format,
            )?;
        }

        Commands::Lgtm { review_id, message } => {
            run_lgtm(&seal_root, &review_id, message, identity.as_deref(), format)?;
        }

        Commands::Block { review_id, reason } => {
            run_block(&seal_root, &review_id, reason, identity.as_deref(), format)?;
        }

        Commands::Review {
            review_id,
            context,
            no_context,
            since,
            include_diffs,
        } => {
            let context_lines = if no_context { 0 } else { context };
            let since_dt = since
                .map(|s| seal::cli::commands::reviews::parse_since(&s))
                .transpose()?;
            let scm = resolve_backend(&workspace_root, scm_preference)?;
            run_review(
                &seal_root,
                scm.as_ref(),
                &review_id,
                context_lines,
                since_dt,
                include_diffs,
                format,
            )?;
        }

        Commands::Reply { thread_id, message } => {
            run_comments_add(
                &seal_root,
                &thread_id,
                &message,
                identity.as_deref(),
                format,
            )?;
        }

        Commands::Inbox => {
            let agent = get_agent_identity(identity.as_deref())?;
            run_inbox(&seal_root, &agent, format)?;
        }

        Commands::Sync {
            rebuild,
            accept_regression,
        } => {
            run_sync(&seal_root, rebuild, accept_regression, format)?;
        }
    }

    Ok(())
}

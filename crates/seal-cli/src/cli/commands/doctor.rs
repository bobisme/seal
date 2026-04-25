//! Implementation of `seal doctor` health check command.

use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use std::process::Command;

use crate::cli::commands::init::{events_path, index_path, is_initialized, SEAL_DIR};
use crate::output::{Formatter, OutputFormat};
use seal_core::events::EventEnvelope;
use seal_core::log::open_or_create;
use seal_core::projection::{sync_from_log_with_backup, ProjectionDb};
use seal_core::scm::{resolve_backend, BackendDetection, ScmPreference};
use seal_core::version::{detect_version, DataVersion};

/// Result of a single health check.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub status: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

impl CheckResult {
    fn pass(name: &str, message: &str) -> Self {
        Self {
            name: name.to_string(),
            status: "pass".to_string(),
            message: message.to_string(),
            remediation: None,
        }
    }

    fn fail(name: &str, message: &str, remediation: &str) -> Self {
        Self {
            name: name.to_string(),
            status: "fail".to_string(),
            message: message.to_string(),
            remediation: Some(remediation.to_string()),
        }
    }

    fn warn(name: &str, message: &str, remediation: Option<&str>) -> Self {
        Self {
            name: name.to_string(),
            status: "warn".to_string(),
            message: message.to_string(),
            remediation: remediation.map(ToString::to_string),
        }
    }
}

/// Overall health status.
#[derive(Debug, Clone, Serialize)]
pub struct HealthReport {
    pub healthy: bool,
    pub checks: Vec<CheckResult>,
}

/// Run the doctor health check.
pub fn run_doctor(
    repo_root: &Path,
    workspace_root: &Path,
    scm_preference: ScmPreference,
    format: OutputFormat,
) -> Result<()> {
    let mut checks = Vec::new();

    // Check 1: SCM detection / selection
    checks.push(check_scm_backend(workspace_root, scm_preference));

    // Check 2: .seal directory
    checks.push(check_crit_initialized(repo_root));

    // Check 3: events.jsonl parseable (only if initialized)
    if is_initialized(repo_root) {
        checks.push(check_events_parseable(repo_root));

        // Check 4: index.db sync status
        checks.push(check_index_sync(repo_root));

        // Check 5: index.db gitignored
        checks.push(check_index_gitignored(repo_root));
    }

    let healthy = checks.iter().all(|c| c.status != "fail");

    let report = HealthReport { healthy, checks };

    let formatter = Formatter::new(format);
    formatter.print(&report)?;

    // Exit with error code if unhealthy
    if !healthy {
        std::process::exit(1);
    }

    Ok(())
}

/// Check SCM backend detection and active backend selection.
fn check_scm_backend(workspace_root: &Path, scm_preference: ScmPreference) -> CheckResult {
    let detection = BackendDetection::detect(workspace_root);
    let git_root = detection
        .git_root
        .as_ref()
        .map_or_else(|| "not detected".to_string(), |p| p.display().to_string());
    let jj_root = detection
        .jj_root
        .as_ref()
        .map_or_else(|| "not detected".to_string(), |p| p.display().to_string());

    if detection.git_root.is_none() && detection.jj_root.is_none() {
        return CheckResult::fail(
            "scm_backend",
            "No SCM backend detected (Git or jj)",
            "Run inside a repository, or initialize one with `git init` or `jj git init`",
        );
    }

    if detection.has_both() && !detection.roots_match() {
        return CheckResult::fail(
            "scm_backend",
            &format!(
                "Detected both backends with mismatched roots (git: {git_root}, jj: {jj_root})"
            ),
            "Rerun with explicit backend selection: `seal --scm git ...` or `seal --scm jj ...`",
        );
    }

    match resolve_backend(workspace_root, scm_preference) {
        Ok(repo) => {
            let message = format!(
                "Active backend: {} (git root: {git_root}, jj root: {jj_root})",
                repo.kind().as_str()
            );

            if detection.has_both() {
                CheckResult::warn(
                    "scm_backend",
                    &message,
                    Some("Both backends are available; use `--scm git` or `--scm jj` to force a backend"),
                )
            } else {
                CheckResult::pass("scm_backend", &message)
            }
        }
        Err(e) => CheckResult::fail(
            "scm_backend",
            &format!("Failed to resolve active backend: {e}"),
            "Set explicit backend with `--scm git` or `--scm jj`",
        ),
    }
}

/// Check if seal is initialized.
fn check_crit_initialized(repo_root: &Path) -> CheckResult {
    if is_initialized(repo_root) {
        let seal_dir = repo_root.join(".seal");
        let events = seal_dir.join("events.jsonl");
        let index = seal_dir.join("index.db");

        let mut details = vec![".seal/ exists"];
        if events.exists() {
            details.push("events.jsonl present");
        }
        if index.exists() {
            details.push("index.db present");
        }

        CheckResult::pass("crit_initialized", &details.join(", "))
    } else {
        CheckResult::fail(
            "crit_initialized",
            ".seal directory not found",
            "Run 'seal --agent <your-name> init' to initialize seal in this repository",
        )
    }
}

/// Check if events.jsonl is parseable.
fn check_events_parseable(repo_root: &Path) -> CheckResult {
    match detect_version(repo_root).ok().flatten() {
        Some(DataVersion::V2) => check_review_logs_parseable(repo_root),
        _ => check_legacy_events_parseable(repo_root),
    }
}

fn check_legacy_events_parseable(repo_root: &Path) -> CheckResult {
    let events_file = events_path(repo_root);

    match std::fs::read_to_string(&events_file) {
        Ok(contents) => {
            let mut valid_count = 0;
            let mut errors = Vec::new();

            for (i, line) in contents.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                match EventEnvelope::from_json_line(line) {
                    Ok(_) => valid_count += 1,
                    Err(e) => {
                        errors.push(format!("Line {}: {}", i + 1, e));
                        if errors.len() >= 3 {
                            break;
                        }
                    }
                }
            }

            if errors.is_empty() {
                CheckResult::pass(
                    "events_parseable",
                    &format!("events.jsonl is valid ({valid_count} events)"),
                )
            } else {
                CheckResult::fail(
                    "events_parseable",
                    &format!(
                        "events.jsonl has {} parse error(s): {}",
                        errors.len(),
                        errors.join("; ")
                    ),
                    "Fix the malformed JSON lines or restore from backup",
                )
            }
        }
        Err(e) => CheckResult::fail(
            "events_parseable",
            &format!("Cannot read events.jsonl: {e}"),
            "Check file permissions or run 'seal --agent <your-name> init' to recreate",
        ),
    }
}

fn check_review_logs_parseable(repo_root: &Path) -> CheckResult {
    let reviews_dir = repo_root.join(SEAL_DIR).join("reviews");
    if !reviews_dir.exists() {
        return CheckResult::warn(
            "events_parseable",
            "No .seal/reviews directory found for v2 repository",
            Some("Run `seal sync --rebuild` to regenerate projection after creating review logs"),
        );
    }

    let entries = match std::fs::read_dir(&reviews_dir) {
        Ok(entries) => entries,
        Err(e) => {
            return CheckResult::fail(
                "events_parseable",
                &format!("Cannot read review logs directory: {e}"),
                "Check file permissions on .seal/reviews",
            );
        }
    };

    let mut review_count = 0usize;
    let mut event_count = 0usize;
    let mut errors = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let Some(review_id) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };

        let events_file = path.join("events.jsonl");
        if !events_file.exists() {
            continue;
        }

        review_count += 1;
        let contents = match std::fs::read_to_string(&events_file) {
            Ok(contents) => contents,
            Err(e) => {
                errors.push(format!("{}: {}", events_file.display(), e));
                if errors.len() >= 3 {
                    break;
                }
                continue;
            }
        };

        for (line_no, line) in contents.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }

            match EventEnvelope::from_json_line(line) {
                Ok(_) => event_count += 1,
                Err(e) => {
                    errors.push(format!("{review_id} line {}: {}", line_no + 1, e));
                    if errors.len() >= 3 {
                        break;
                    }
                }
            }
        }

        if errors.len() >= 3 {
            break;
        }
    }

    if errors.is_empty() {
        CheckResult::pass(
            "events_parseable",
            &format!("review logs are valid ({review_count} reviews, {event_count} events)"),
        )
    } else {
        CheckResult::fail(
            "events_parseable",
            &format!(
                "review logs have {} parse/read error(s): {}",
                errors.len(),
                errors.join("; ")
            ),
            "Fix malformed log lines or restore affected .seal/reviews/<id>/events.jsonl files",
        )
    }
}

/// Check if index.db is in sync with events.jsonl.
fn check_index_sync(repo_root: &Path) -> CheckResult {
    let db_result = ProjectionDb::open(&index_path(repo_root));
    let log_result = open_or_create(&events_path(repo_root));

    match (db_result, log_result) {
        (Ok(db), Ok(log)) => {
            // Initialize schema if needed
            if let Err(e) = db.init_schema() {
                return CheckResult::fail(
                    "index_sync",
                    &format!("Failed to initialize schema: {e}"),
                    "Delete .seal/index.db and it will be recreated",
                );
            }

            // Try to sync and check for errors
            let seal_dir = repo_root.join(SEAL_DIR);
            match sync_from_log_with_backup(&db, &log, Some(&seal_dir)) {
                Ok(events_processed) => {
                    // Get some stats
                    let review_count = db.list_reviews(None, None).map(|r| r.len()).unwrap_or(0);
                    CheckResult::pass(
                        "index_sync",
                        &format!(
                            "index.db is in sync ({review_count} reviews, {events_processed} events)"
                        ),
                    )
                }
                Err(e) => CheckResult::warn(
                    "index_sync",
                    &format!("Sync completed with warning: {e}"),
                    Some("This may indicate corrupted events or schema mismatch"),
                ),
            }
        }
        (Err(e), _) => CheckResult::fail(
            "index_sync",
            &format!("Cannot open index.db: {e}"),
            "Delete .seal/index.db and it will be recreated on next command",
        ),
        (_, Err(e)) => CheckResult::fail(
            "index_sync",
            &format!("Cannot open events.jsonl: {e}"),
            "Check file permissions",
        ),
    }
}

/// Check if .seal/index.db is gitignored.
///
/// The index is an ephemeral cache rebuilt from events.jsonl and should not be committed.
fn check_index_gitignored(repo_root: &Path) -> CheckResult {
    let index_rel = ".seal/index.db";

    // Use git check-ignore (works in both git and jj-backed repos)
    let result = Command::new("git")
        .args(["check-ignore", "-q", index_rel])
        .current_dir(repo_root)
        .output();

    match result {
        Ok(output) if output.status.success() => {
            CheckResult::pass("index_gitignored", ".seal/index.db is gitignored")
        }
        Ok(_) => CheckResult::warn(
            "index_gitignored",
            ".seal/index.db is not gitignored",
            Some("Add '.seal/index.db' to .gitignore — it's a rebuildable cache"),
        ),
        Err(_) => {
            // git not available — skip silently
            CheckResult::pass(
                "index_gitignored",
                "Skipped gitignore check (git not available)",
            )
        }
    }
}

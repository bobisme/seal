//! Implementation of `seal status` and `seal diff` commands.

use anyhow::Result;
use serde::Serialize;
use std::path::Path;

use crate::cli::commands::helpers::{ensure_initialized, open_services, review_not_found_error};
use crate::output::{Formatter, OutputFormat};
use seal_core::anchor::{AnchorLocation, AnchorLocator, AnchorStatus};
use seal_core::core::SealServices;
use seal_core::projection::{ReviewDetail, ThreadSummary};
use seal_core::range::{
    approval_coverage, detect_stale_base, resolve_review_range, resolve_target_commit,
};
use seal_core::scm::{git_diff_changed_paths, ScmRepo};
use seal_core::sealignore::{AllFilesIgnoredError, SealIgnore};

/// Thread status with drift information.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadStatusEntry {
    pub thread_id: String,
    pub file_path: String,
    pub original_line: i64,
    pub current_line: Option<i64>,
    /// Last anchored line in the review target, for range threads.
    pub current_line_end: Option<i64>,
    /// `unchanged`, `shifted(+N)`, `modified`, `deleted` or `unknown`.
    pub drift_status: String,
    /// The anchored lines were edited, removed, or cannot be mapped. The
    /// thread is still listed.
    pub outdated: bool,
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
        statuses.push(review_status(&services, scm, &review, unresolved_only)?);
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

/// Status of one review, with each thread located in the review target.
fn review_status(
    services: &SealServices,
    scm: &dyn ScmRepo,
    review: &ReviewDetail,
    unresolved_only: bool,
) -> Result<ReviewStatus> {
    let status_filter = if unresolved_only { Some("open") } else { None };
    let threads = services
        .threads()
        .list(&review.review_id, status_filter, None)?;

    // Threads are located in the code the review shows now, which is the
    // review target, not whatever the workspace has checked out.
    let target_commit = resolve_target_commit(scm, review);
    let mut locator = AnchorLocator::new(scm);
    let mut thread_entries = Vec::new();
    let mut drift_count = 0;

    for thread in &threads {
        let location = locator.locate(
            &thread.file_path,
            thread.selection_start,
            thread.selection_end,
            &thread.commit_hash,
            &target_commit,
        );
        if location.status != AnchorStatus::Unchanged {
            drift_count += 1;
        }
        thread_entries.push(thread_status_entry(thread, &location));
    }

    let open_count = threads.iter().filter(|t| t.status == "open").count();

    Ok(ReviewStatus {
        review_id: review.review_id.clone(),
        title: review.title.clone(),
        status: review.status.clone(),
        total_threads: threads.len(),
        open_threads: open_count,
        threads_with_drift: drift_count,
        threads: thread_entries,
    })
}

fn thread_status_entry(thread: &ThreadSummary, location: &AnchorLocation) -> ThreadStatusEntry {
    ThreadStatusEntry {
        thread_id: thread.thread_id.clone(),
        file_path: thread.file_path.clone(),
        original_line: thread.selection_start,
        current_line: location.current_start.map(i64::from),
        current_line_end: location.current_end.map(i64::from),
        drift_status: location.drift_label(),
        outdated: location.is_outdated(),
        status: thread.status.clone(),
        comment_count: thread.comment_count,
    }
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

    // Resolve the reviewed range.
    // - The target is final_commit for merged reviews, otherwise the current
    //   commit of the review anchor, which handles rewrites correctly.
    // - The base is the review's persisted base, so all commits of a
    //   multi-commit feature stay in the diff. Reviews created before ranges
    //   were supported fall back to the parent of the target.
    let range = resolve_review_range(scm, &review);
    // Warn when a rebase left the persisted base behind the fork point, so
    // the range also holds trunk commits.
    let stale_base = detect_stale_base(scm, &review, &range);
    let target_commit = range.target;
    let base_commit = range.base;

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

    // File metadata and the rendered patch are separate output fields. Filter
    // both so ignored generated files cannot consume reviewer context through
    // the serialized diff payload.
    let diff = sealignore.filter_diff(&diff);

    // Get threads for context
    let threads: Vec<_> = services
        .threads()
        .list(review_id, None, None)?
        .into_iter()
        .filter(|thread| !sealignore.is_ignored(&thread.file_path))
        .collect();
    let mut locator = AnchorLocator::new(scm);
    let thread_locations: Vec<AnchorLocation> = threads
        .iter()
        .map(|thread| {
            locator.locate(
                &thread.file_path,
                thread.selection_start,
                thread.selection_end,
                &thread.commit_hash,
                &target_commit,
            )
        })
        .collect();

    // Build structured output
    // Say when the standing approval predates this code.
    let coverage = approval_coverage(scm, &review);

    let result = serde_json::json!({
        "review_id": review_id,
        "base_commit": base_commit,
        "base_is_persisted": range.base_is_persisted,
        "base_warning": stale_base.as_ref().map(|stale| stale.warning(review_id)),
        "suggested_base": stale_base.as_ref().map(|stale| stale.discovered.clone()),
        "approval_stale": coverage.as_ref().map(seal_core::range::ApprovalCoverage::is_stale),
        "approved_commit": coverage.as_ref().and_then(|c| c.approved_commit.clone()),
        // The commit the approval was checked against: the workspace commit
        // when it builds on target_commit, else target_commit.
        "approval_target_commit": coverage.as_ref().map(|c| c.current_target.clone()),
        "uncovered_commits": coverage.as_ref().and_then(|c| c.uncovered_commits),
        "exempt_commits": coverage.as_ref().and_then(|c| c.exempt_commits),
        "initial_commit": review.initial_commit,
        "target_commit": target_commit,
        "changed_files": changed_files,
        "thread_count": threads.len(),
        "threads_by_file": group_threads_by_file(&threads, &thread_locations),
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
///
/// `line` stays the stored anchor line. `current_line`, `drift_status` and
/// `outdated` say where that line is in the diffed target, so a thread whose
/// lines changed is still listed, marked outdated.
fn group_threads_by_file(
    threads: &[ThreadSummary],
    locations: &[AnchorLocation],
) -> serde_json::Value {
    let mut by_file: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();

    for (thread, location) in threads.iter().zip(locations) {
        by_file
            .entry(thread.file_path.clone())
            .or_default()
            .push(serde_json::json!({
                "thread_id": thread.thread_id,
                "line": thread.selection_start,
                "current_line": location.current_start,
                "drift_status": location.drift_label(),
                "outdated": location.is_outdated(),
                "status": thread.status,
            }));
    }

    serde_json::json!(by_file
        .into_iter()
        .map(|(file, threads)| serde_json::json!({ "file": file, "threads": threads }))
        .collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn summary(id: &str, line: i64) -> ThreadSummary {
        ThreadSummary {
            thread_id: id.to_string(),
            file_path: "f.txt".to_string(),
            selection_start: line,
            selection_end: None,
            status: "open".to_string(),
            comment_count: 1,
            commit_hash: "c0ffee".to_string(),
        }
    }

    fn location(status: AnchorStatus, original: u32, current: Option<u32>) -> AnchorLocation {
        AnchorLocation {
            status,
            original_start: original,
            original_end: original,
            current_start: current,
            current_end: current,
            reason: None,
        }
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn file_text(lines: &[&str]) -> String {
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    /// `seal status` measures drift against the review target. The workspace
    /// here sits on trunk, where the commented feature line does not exist
    /// at all; the review target (after rebase + retarget) has it two lines
    /// lower than where it was commented.
    #[test]
    fn status_locates_threads_in_the_review_target_not_the_workspace_commit() {
        use seal_core::core::CoreContext;
        use seal_core::events::CodeSelection;
        use seal_core::scm::git::GitRepo;

        let temp = TempDir::new().unwrap();
        let root = temp.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "Test User"]);
        fs::write(root.join(".git/info/exclude"), ".seal/\n").unwrap();
        fs::write(root.join("f.txt"), file_text(&["a", "b", "c"])).unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "base"]);
        git(root, &["branch", "-M", "main"]);
        git(root, &["checkout", "-q", "--detach"]);
        fs::write(root.join("f.txt"), file_text(&["a", "b", "c", "feature"])).unwrap();
        git(root, &["commit", "-qam", "feature"]);

        let seal_dir = root.join(".seal");
        fs::create_dir_all(seal_dir.join("reviews")).unwrap();
        fs::write(seal_dir.join("version"), "2\n").unwrap();
        let ctx = CoreContext::new(root, &seal_dir.join("index.db")).unwrap();
        let scm = GitRepo::new(root.to_path_buf());
        let target = scm.current_commit().unwrap();
        let base = seal_core::range::resolve_new_review_base(&scm, &target, None).unwrap();
        let review_id = ctx
            .services()
            .unwrap()
            .reviews()
            .create(&scm, "T".to_string(), None, None, Some("alice"), Some(base))
            .unwrap();
        ctx.services()
            .unwrap()
            .threads()
            .create(
                &review_id,
                "f.txt",
                CodeSelection::line(4),
                target.clone(),
                Some("alice"),
            )
            .unwrap();

        // Trunk inserts two lines at the top; rebase and retarget.
        git(root, &["checkout", "-q", "main"]);
        fs::write(root.join("f.txt"), file_text(&["t1", "t2", "a", "b", "c"])).unwrap();
        git(root, &["commit", "-qam", "trunk"]);
        git(root, &["checkout", "-q", "--detach", &target]);
        git(root, &["rebase", "-q", "main"]);
        ctx.services()
            .unwrap()
            .reviews()
            .retarget(&scm, &review_id, Some("alice"), None)
            .unwrap();

        // Move the workspace off the review target, onto trunk.
        git(root, &["checkout", "-q", "--detach", "main"]);
        assert_ne!(
            scm.current_commit().unwrap(),
            resolve_target_commit(
                &scm,
                &ctx.services().unwrap().reviews().get(&review_id).unwrap()
            )
        );

        let services = ctx.services().unwrap();
        let review = services.reviews().get(&review_id).unwrap();
        let status = review_status(&services, &scm, &review, false).unwrap();
        assert_eq!(status.threads.len(), 1);
        let thread = &status.threads[0];
        assert_eq!(thread.original_line, 4);
        assert_eq!(thread.current_line, Some(6));
        assert_eq!(thread.drift_status, "shifted(+2)");
        assert!(!thread.outdated);
    }

    #[test]
    fn outdated_threads_stay_listed_and_marked_in_status_and_diff() {
        let threads = [summary("th-shifted", 5), summary("th-edited", 9)];
        let locations = [
            location(AnchorStatus::Shifted, 5, Some(7)),
            location(AnchorStatus::Modified, 9, None),
        ];

        let shifted = thread_status_entry(&threads[0], &locations[0]);
        assert_eq!(shifted.current_line, Some(7));
        assert_eq!(shifted.drift_status, "shifted(+2)");
        assert!(!shifted.outdated);
        let edited = thread_status_entry(&threads[1], &locations[1]);
        assert_eq!(edited.current_line, None);
        assert_eq!(edited.original_line, 9);
        assert_eq!(edited.drift_status, "modified");
        assert!(edited.outdated);

        let grouped = group_threads_by_file(&threads, &locations);
        let listed = grouped[0]["threads"].as_array().expect("threads");
        assert_eq!(listed.len(), 2, "no thread is dropped");
        assert_eq!(listed[0]["line"], 5);
        assert_eq!(listed[0]["current_line"], 7);
        assert_eq!(listed[0]["outdated"], false);
        assert_eq!(listed[1]["thread_id"], "th-edited");
        assert_eq!(listed[1]["line"], 9);
        assert!(listed[1]["current_line"].is_null());
        assert_eq!(listed[1]["drift_status"], "modified");
        assert_eq!(listed[1]["outdated"], true);
    }

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

    #[test]
    fn ignored_large_diff_payload_is_absent_from_every_output_format() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join(".sealignore"), "web/engine.js\n").unwrap();
        let sealignore = SealIgnore::load(temp.path());
        let generated_payload = format!(
            "diff --git a/web/engine.js b/web/engine.js\n--- a/web/engine.js\n+++ b/web/engine.js\n@@ -1 +1 @@\n-old\n+{}IGNORED_SENTINEL\n",
            "x".repeat(2_100_000)
        );
        let reviewable_diff = "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+REVIEWABLE_SENTINEL\n";
        let full_diff = format!("{reviewable_diff}{generated_payload}");
        let filtered_diff = sealignore.filter_diff(&full_diff);
        let result = serde_json::json!({
            "changed_files": ["src/main.rs"],
            "diff": filtered_diff,
        });

        for format in [OutputFormat::Text, OutputFormat::Json, OutputFormat::Pretty] {
            let output = Formatter::new(format).format(&result).unwrap();
            assert!(output.contains("REVIEWABLE_SENTINEL"), "format {format:?}");
            assert!(!output.contains("web/engine.js"), "format {format:?}");
            assert!(!output.contains("IGNORED_SENTINEL"), "format {format:?}");
            assert!(
                output.len() < 10_000,
                "format {format:?} leaked the large payload"
            );
        }
    }
}

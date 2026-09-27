//! `SealClient` implementation backed by `SealServices` (direct library calls).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;

use seal_core::anchor::{AnchorLocation, AnchorLocator};
use seal_core::core::{CoreContext, SealServices};
use seal_core::events::CodeSelection;
use seal_core::scm::{git_diff_section_path, resolve_backend, ScmPreference, ScmRepo};
use seal_core::sealignore::SealIgnore;

use crate::db::{
    Comment, FileContentData, FileData, ReviewData, ReviewDetail, ReviewSummary, SealClient,
    ThreadSummary,
};

/// Client that calls seal-core services directly (no subprocess).
pub struct CoreClient {
    ctx: CoreContext,
    repo_root: PathBuf,
}

impl CoreClient {
    #[must_use]
    pub fn new(ctx: CoreContext, repo_root: &Path) -> Self {
        Self {
            ctx,
            repo_root: repo_root.to_path_buf(),
        }
    }

    /// Re-sync and get fresh services.
    fn services(&self) -> Result<SealServices> {
        self.ctx.services().map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn comment_agent() -> String {
        std::env::var("USER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    }
}

// -- Conversions from seal-core types to UI types --

fn convert_review_summary(r: &seal_core::projection::ReviewSummary) -> ReviewSummary {
    ReviewSummary {
        review_id: r.review_id.clone(),
        title: r.title.clone(),
        author: r.author.clone(),
        status: r.status.clone(),
        thread_count: r.thread_count,
        open_thread_count: r.open_thread_count,
        reviewers: r.reviewers.clone(),
    }
}

fn convert_review_detail(r: &seal_core::projection::ReviewDetail) -> ReviewDetail {
    ReviewDetail {
        review_id: r.review_id.clone(),
        jj_change_id: r.jj_change_id.clone(),
        scm_kind: r.scm_kind.clone(),
        scm_anchor: r.scm_anchor.clone(),
        initial_commit: r.initial_commit.clone(),
        final_commit: r.final_commit.clone(),
        title: r.title.clone(),
        description: r.description.clone(),
        author: r.author.clone(),
        created_at: r.created_at.clone(),
        status: r.status.clone(),
        status_changed_at: r.status_changed_at.clone(),
        status_changed_by: r.status_changed_by.clone(),
        abandon_reason: r.abandon_reason.clone(),
        thread_count: r.thread_count,
        open_thread_count: r.open_thread_count,
    }
}

/// Convert a core thread summary, positioning it with `location` (its anchor
/// mapped into the review's current target — see [`locate_threads`]).
fn convert_thread_summary(
    t: &seal_core::projection::ThreadSummary,
    location: &AnchorLocation,
) -> ThreadSummary {
    ThreadSummary {
        thread_id: t.thread_id.clone(),
        file_path: t.file_path.clone(),
        selection_start: t.selection_start,
        selection_end: t.selection_end,
        status: t.status.clone(),
        comment_count: t.comment_count,
        current_selection_start: location.current_start.map(i64::from),
        current_selection_end: location.current_end.map(i64::from),
        drift_status: location.drift_label(),
        outdated: location.is_outdated(),
    }
}

/// Map each thread's anchor into `target_commit`, parsing each diff at most
/// once. Called once per review load (not per render frame) so the TUI stays
/// responsive; when `scm` cannot be resolved, every thread is treated as
/// unchanged at its stored line rather than blocking the load.
fn locate_threads(
    scm: &dyn ScmRepo,
    threads: &[seal_core::projection::ThreadSummary],
    target_commit: &str,
) -> HashMap<String, AnchorLocation> {
    let mut locator = AnchorLocator::new(scm);
    threads
        .iter()
        .map(|t| {
            let location = locator.locate(
                &t.file_path,
                t.selection_start,
                t.selection_end,
                &t.commit_hash,
                target_commit,
            );
            (t.thread_id.clone(), location)
        })
        .collect()
}

/// The best-effort location for a thread when the anchor could not be
/// computed at all (no SCM backend available): assume unchanged so the
/// thread still renders at its stored line rather than being hidden.
fn assume_unchanged(t: &seal_core::projection::ThreadSummary) -> AnchorLocation {
    AnchorLocation {
        status: seal_core::anchor::AnchorStatus::Unchanged,
        original_start: u32::try_from(t.selection_start.max(0)).unwrap_or(u32::MAX),
        original_end: u32::try_from(t.selection_end.unwrap_or(t.selection_start).max(0))
            .unwrap_or(u32::MAX),
        current_start: u32::try_from(t.selection_start.max(0)).ok(),
        current_end: u32::try_from(t.selection_end.unwrap_or(t.selection_start).max(0)).ok(),
        reason: None,
    }
}

fn convert_comment(c: &seal_core::projection::Comment) -> Comment {
    Comment {
        comment_id: c.comment_id.clone(),
        author: c.author.clone(),
        body: c.body.clone(),
        created_at: c.created_at.clone(),
    }
}

impl SealClient for CoreClient {
    fn list_reviews(&self, status: Option<&str>) -> Result<Vec<ReviewSummary>> {
        let services = self.services()?;
        let reviews = services
            .reviews()
            .list(status, None)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(reviews.iter().map(convert_review_summary).collect())
    }

    fn load_review_data(&self, review_id: &str) -> Result<Option<ReviewData>> {
        let services = self.services()?;

        let Some(detail) = services
            .reviews()
            .get_optional(review_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?
        else {
            return Ok(None);
        };

        let core_threads = services
            .threads()
            .list(review_id, None, None)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let sealignore = SealIgnore::load(&self.repo_root);
        let visible_threads: Vec<_> = core_threads
            .into_iter()
            .filter(|thread| !sealignore.is_ignored(&thread.file_path))
            .collect();

        // Map each thread's anchor into the review's current target once,
        // here at load time, so rendering never re-parses diffs per frame.
        let scm = resolve_backend(&self.repo_root, ScmPreference::Auto).ok();
        let target_commit = scm
            .as_ref()
            .map(|scm| resolve_review_target_commit(scm.as_ref(), &detail));
        let locations: HashMap<String, AnchorLocation> = match (&scm, &target_commit) {
            (Some(scm), Some(target)) => locate_threads(scm.as_ref(), &visible_threads, target),
            _ => HashMap::new(),
        };

        let mut threads = Vec::with_capacity(visible_threads.len());
        let mut comments: HashMap<String, Vec<Comment>> = HashMap::new();

        for t in &visible_threads {
            let location = locations
                .get(&t.thread_id)
                .cloned()
                .unwrap_or_else(|| assume_unchanged(t));
            threads.push(convert_thread_summary(t, &location));

            let core_comments = services
                .comments()
                .list(&t.thread_id)
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            if !core_comments.is_empty() {
                comments.insert(
                    t.thread_id.clone(),
                    core_comments.iter().map(convert_comment).collect(),
                );
            }
        }

        let review_detail = convert_review_detail(&detail);

        // Build file diffs using SCM. Uses the already-positioned threads so
        // the orphan/context-window heuristic agrees with where threads are
        // actually rendered (mapped line for intact anchors, no preview for
        // outdated ones).
        let files = self.build_file_diffs(&detail, &threads);

        Ok(Some(ReviewData {
            detail: review_detail,
            threads,
            comments,
            files,
        }))
    }

    fn comment(
        &self,
        review_id: &str,
        file_path: &str,
        start_line: i64,
        end_line: Option<i64>,
        body: &str,
    ) -> Result<()> {
        let services = self.services()?;
        let agent = Self::comment_agent();

        let review = services
            .reviews()
            .get(review_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let thread_commit = resolve_backend(&self.repo_root, ScmPreference::Auto).map_or_else(
            |_| review.initial_commit.clone(),
            |scm| resolve_review_target_commit(scm.as_ref(), &review),
        );

        #[allow(clippy::cast_sign_loss)]
        let selection = match end_line {
            Some(end) if end != start_line => CodeSelection::range(start_line as u32, end as u32),
            _ => CodeSelection::line(start_line as u32),
        };

        services
            .comments()
            .add_to_review(
                review_id,
                file_path,
                selection,
                body,
                thread_commit,
                Some(&agent),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok(())
    }

    fn reply(&self, thread_id: &str, body: &str) -> Result<()> {
        let services = self.services()?;
        let agent = Self::comment_agent();

        services
            .comments()
            .add_to_thread(thread_id, body, Some(&agent))
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok(())
    }

    fn resolve_thread(&self, thread_id: &str) -> Result<()> {
        let services = self.services()?;
        let agent = Self::comment_agent();

        services
            .threads()
            .resolve(thread_id, None, Some(&agent))
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok(())
    }

    fn reopen_thread(&self, thread_id: &str) -> Result<()> {
        let services = self.services()?;
        let agent = Self::comment_agent();

        services
            .threads()
            .reopen(thread_id, None, Some(&agent))
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok(())
    }
}

// -- Diff assembly (mirrors CLI `build_file_diffs` logic) --

impl CoreClient {
    fn build_file_diffs(
        &self,
        review: &seal_core::projection::ReviewDetail,
        threads: &[ThreadSummary],
    ) -> Vec<FileData> {
        let Ok(scm) = resolve_backend(&self.repo_root, ScmPreference::Auto) else {
            return Vec::new();
        };

        // The review's persisted base keeps every commit of a multi-commit
        // feature in the diff, not only the tip commit.
        let range = seal_core::range::resolve_review_range(scm.as_ref(), review);
        let target_commit = range.target;
        let base_commit = range.base;

        // Get full diff and split by file
        let full_diff = scm
            .diff_git(&base_commit, &target_commit)
            .unwrap_or_default();
        let diffs_by_file = split_diff_by_file(&full_diff);

        // Collect files: union of files with threads + files with diffs
        let sealignore = SealIgnore::load(&self.repo_root);
        let files_with_threads: BTreeSet<String> =
            threads.iter().map(|t| t.file_path.clone()).collect();
        let mut all_files: BTreeSet<String> = files_with_threads;
        for key in diffs_by_file.keys() {
            all_files.insert((*key).to_string());
        }

        // Pre-fetch file contents for thread files
        let mut file_cache: HashMap<String, String> = HashMap::new();
        for thread in threads {
            if !file_cache.contains_key(&thread.file_path) {
                if let Ok(contents) = scm.show_file(&target_commit, &thread.file_path) {
                    file_cache.insert(thread.file_path.clone(), contents);
                }
            }
        }

        let mut result = Vec::new();
        for file_path in &all_files {
            if sealignore.is_ignored(file_path) {
                continue;
            }

            let diff = diffs_by_file
                .get(file_path.as_str())
                .map(std::string::ToString::to_string);

            // Check for orphaned threads (not covered by diff hunks). Outdated
            // threads are excluded: there is no reliable target-commit line
            // to window around for them, so they render as plain comment
            // blocks instead (see `view::diff::mod::render_file_with_diff`).
            let file_threads: Vec<&ThreadSummary> = threads
                .iter()
                .filter(|t| &t.file_path == file_path && !t.outdated)
                .collect();

            let content = if file_threads.is_empty() {
                None
            } else if let Some(ref diff_text) = diff {
                let hunks = parse_hunk_ranges(diff_text);
                let has_orphan = file_threads.iter().any(|t| {
                    let line = u32::try_from(t.effective_start().max(0)).unwrap_or(u32::MAX);
                    !hunks.iter().any(|h| line >= h.0 && line <= h.1)
                });
                if has_orphan {
                    build_content_window(&file_cache, file_path, &file_threads)
                } else {
                    None
                }
            } else {
                build_content_window(&file_cache, file_path, &file_threads)
            };

            result.push(FileData {
                path: file_path.clone(),
                diff,
                content,
            });
        }

        result
    }
}

/// Split a full git-format diff into per-file sections.
fn split_diff_by_file(full_diff: &str) -> HashMap<&str, &str> {
    let mut result = HashMap::new();
    let mut current_start: Option<usize> = None;
    let mut offset = 0;

    for line in full_diff.lines() {
        let byte_offset = offset;
        offset += line.len() + 1; // +1 for newline

        if line.starts_with("diff --git") {
            if let Some(start) = current_start {
                let section = &full_diff[start..byte_offset];
                if !section.trim().is_empty() {
                    if let Some(file) = git_diff_section_path(section) {
                        result.insert(file, section);
                    }
                }
            }
            current_start = Some(byte_offset);
        }
    }

    if let Some(start) = current_start {
        let section = &full_diff[start..];
        if !section.trim().is_empty() {
            if let Some(file) = git_diff_section_path(section) {
                result.insert(file, section);
            }
        }
    }

    result
}

fn resolve_review_target_commit(
    scm: &dyn ScmRepo,
    review: &seal_core::projection::ReviewDetail,
) -> String {
    seal_core::range::resolve_target_commit(scm, review)
}

/// Parse unified diff hunk headers to extract new-side line ranges.
fn parse_hunk_ranges(diff: &str) -> Vec<(u32, u32)> {
    let mut ranges = Vec::new();
    for line in diff.lines() {
        if !line.starts_with("@@") {
            continue;
        }
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
                ranges.push((start, start));
            }
        }
    }
    ranges
}

/// Build a content window covering all thread locations in a file.
fn build_content_window(
    file_cache: &HashMap<String, String>,
    file_path: &str,
    threads: &[&ThreadSummary],
) -> Option<FileContentData> {
    let contents = file_cache.get(file_path)?;
    let lines: Vec<&str> = contents.lines().collect();
    if lines.is_empty() {
        return None;
    }

    // Find the range covering all threads with context
    let context = 5u32;
    let mut min_line = u32::MAX;
    let mut max_line = 0u32;

    for t in threads {
        let start = u32::try_from(t.effective_start().max(0)).unwrap_or(u32::MAX);
        let end = u32::try_from(t.effective_end().max(0)).unwrap_or(u32::MAX);
        min_line = min_line.min(start.saturating_sub(context));
        max_line = max_line.max(end.saturating_add(context));
    }

    // Clamp to file bounds (1-based)
    let start_line = min_line.max(1);
    let end_line = max_line.min(lines.len() as u32);

    if start_line > end_line {
        return None;
    }

    let window_lines: Vec<String> = lines[(start_line as usize - 1)..(end_line as usize)]
        .iter()
        .map(|l| (*l).to_string())
        .collect();

    Some(FileContentData {
        start_line: i64::from(start_line),
        lines: window_lines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_diff_by_file_path_with_spaces() {
        let diff = "\
diff --git a/src/has space.rs b/src/has space.rs
--- a/src/has space.rs
+++ b/src/has space.rs
@@ -1 +1 @@
-old
+new
";

        let result = split_diff_by_file(diff);
        assert_eq!(result.len(), 1);
        assert!(result.contains_key("src/has space.rs"));
    }
}

//! `SealClient` implementation backed by `SealServices` (direct library calls).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;

use seal_core::core::{CoreContext, SealServices};
use seal_core::events::CodeSelection;
use seal_core::scm::{resolve_backend, ScmPreference};
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

fn convert_thread_summary(t: &seal_core::projection::ThreadSummary) -> ThreadSummary {
    ThreadSummary {
        thread_id: t.thread_id.clone(),
        file_path: t.file_path.clone(),
        selection_start: t.selection_start,
        selection_end: t.selection_end,
        status: t.status.clone(),
        comment_count: t.comment_count,
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

        let mut threads = Vec::with_capacity(visible_threads.len());
        let mut comments: HashMap<String, Vec<Comment>> = HashMap::new();

        for t in &visible_threads {
            threads.push(convert_thread_summary(t));

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

        // Build file diffs using SCM
        let files = self.build_file_diffs(&detail, &visible_threads);

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

        // Get the review's initial commit for thread creation
        let review = services
            .reviews()
            .get(review_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

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
                review.initial_commit,
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
}

// -- Diff assembly (mirrors CLI `build_file_diffs` logic) --

impl CoreClient {
    fn build_file_diffs(
        &self,
        review: &seal_core::projection::ReviewDetail,
        threads: &[seal_core::projection::ThreadSummary],
    ) -> Vec<FileData> {
        let Ok(scm) = resolve_backend(&self.repo_root, ScmPreference::Auto) else {
            return Vec::new();
        };

        // Resolve target commit
        let target_commit = review
            .final_commit
            .clone()
            .or_else(|| scm.commit_for_anchor(&review.scm_anchor).ok())
            .or_else(|| scm.commit_for_anchor(&review.jj_change_id).ok())
            .unwrap_or_else(|| review.initial_commit.clone());

        let base_commit = scm
            .parent_commit(&target_commit)
            .unwrap_or_else(|_| review.initial_commit.clone());

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

            // Check for orphaned threads (not covered by diff hunks)
            let file_threads: Vec<&seal_core::projection::ThreadSummary> = threads
                .iter()
                .filter(|t| &t.file_path == file_path)
                .collect();

            let content = if file_threads.is_empty() {
                None
            } else if let Some(ref diff_text) = diff {
                let hunks = parse_hunk_ranges(diff_text);
                let has_orphan = file_threads.iter().any(|t| {
                    let line = t.selection_start as u32;
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
    let mut current_file: Option<&str> = None;
    let mut current_start: usize = 0;
    let mut offset = 0;

    for line in full_diff.lines() {
        let byte_offset = offset;
        offset += line.len() + 1; // +1 for newline

        if line.starts_with("diff --git") {
            if let Some(file) = current_file {
                let section = &full_diff[current_start..byte_offset];
                if !section.trim().is_empty() {
                    result.insert(file, section);
                }
            }
            current_file = line
                .split_whitespace()
                .nth(3)
                .map(|s| s.trim_start_matches("b/"));
            current_start = byte_offset;
        }
    }

    if let Some(file) = current_file {
        let section = &full_diff[current_start..];
        if !section.trim().is_empty() {
            result.insert(file, section);
        }
    }

    result
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
    threads: &[&seal_core::projection::ThreadSummary],
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
        let start = t.selection_start as u32;
        let end = t.selection_end.unwrap_or(t.selection_start) as u32;
        min_line = min_line.min(start.saturating_sub(context));
        max_line = max_line.max(end + context);
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

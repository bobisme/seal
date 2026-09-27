//! `SealClient` implementation backed by `SealServices` (direct library calls).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;

use seal_core::anchor::{AnchorLocation, AnchorLocator, AnchorStatus};
use seal_core::core::{CoreContext, SealServices};
use seal_core::events::CodeSelection;
use seal_core::scm::{git_diff_section_path, resolve_backend, ScmPreference, ScmRepo};
use seal_core::sealignore::SealIgnore;

use crate::db::{
    Comment, FileContentData, FileData, OutdatedPreview, ReviewData, ReviewDetail, ReviewSummary,
    SealClient, ThreadSummary,
};

/// Cap on the anchored range shown in an outdated thread's code preview.
/// The stored anchor range comes from a thread event; a forged or corrupted
/// one could claim an enormous range, so the preview is always clamped to
/// this many anchor lines before it is ever read into memory, regardless of
/// what the location reports.
const MAX_PREVIEW_ANCHOR_LINES: u32 = 20;

/// Context lines around the anchor in an outdated-thread preview. Kept as
/// its own `u32` constant (rather than reusing `crate::layout::CONTEXT_LINES`,
/// which is `i64` for stream-layout arithmetic) but intentionally the same
/// value, so the preview reads like any other code preview in the pane.
const PREVIEW_CONTEXT_LINES: u32 = 5;

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
/// mapped into the review's current target — see [`locate_threads`]) and
/// attaching `preview` when one was read for it (see
/// [`build_outdated_previews`]).
fn convert_thread_summary(
    t: &seal_core::projection::ThreadSummary,
    location: &AnchorLocation,
    preview: Option<OutdatedPreview>,
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
        outdated_preview: preview,
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

/// Read the code an outdated (modified/deleted) thread was commented on,
/// from its own anchor commit — mirrors the CLI's `context_source`
/// (`crates/seal-cli/src/cli/commands/reviews.rs`). Returns `None` for any
/// other anchor status (in particular `Unknown`, which has no commit that
/// can be trusted to read) or on any read failure: the caller then falls
/// back to the plain comment block, exactly as before this preview existed.
///
/// `file_cache` is keyed by `(commit, file_path)` so a file read once for
/// one thread is reused for every other thread anchored to the same
/// commit and file.
fn build_outdated_preview(
    file_cache: &mut HashMap<(String, String), Option<String>>,
    scm: &dyn ScmRepo,
    file_path: &str,
    commit: &str,
    location: &AnchorLocation,
) -> Option<OutdatedPreview> {
    if !matches!(
        location.status,
        AnchorStatus::Modified | AnchorStatus::Deleted
    ) {
        return None;
    }

    let key = (commit.to_string(), file_path.to_string());
    let contents = file_cache
        .entry(key)
        .or_insert_with(|| scm.show_file(commit, file_path).ok())
        .clone()?;

    let anchor_start = location.original_start;
    let anchor_end = location.original_end;
    if anchor_start == 0 || anchor_end == 0 || anchor_start > anchor_end {
        return None;
    }

    let file_lines: Vec<&str> = contents.lines().collect();
    let total_lines = u32::try_from(file_lines.len()).unwrap_or(u32::MAX);
    if total_lines == 0 || anchor_start > total_lines {
        return None;
    }

    // Clamp the anchor span itself before touching the window below, so a
    // huge stored range (forged or corrupted) can never inflate the
    // preview, regardless of file size.
    let anchor_end = anchor_end
        .min(anchor_start.saturating_add(MAX_PREVIEW_ANCHOR_LINES - 1))
        .min(total_lines);

    let start_line = anchor_start.saturating_sub(PREVIEW_CONTEXT_LINES).max(1);
    let end_line = anchor_end
        .saturating_add(PREVIEW_CONTEXT_LINES)
        .min(total_lines);

    let lines: Vec<String> = file_lines[(start_line as usize - 1)..(end_line as usize)]
        .iter()
        .map(|l| (*l).to_string())
        .collect();

    Some(OutdatedPreview {
        commit: commit.to_string(),
        start_line: i64::from(start_line),
        lines,
        anchor_start: i64::from(anchor_start),
        anchor_end: i64::from(anchor_end),
    })
}

/// Build code previews for every modified/deleted thread, one `show_file`
/// per unique (commit, file) pair. Called once at review load, alongside
/// `locate_threads` — never per frame.
fn build_outdated_previews(
    scm: &dyn ScmRepo,
    threads: &[seal_core::projection::ThreadSummary],
    locations: &HashMap<String, AnchorLocation>,
) -> HashMap<String, OutdatedPreview> {
    let mut file_cache: HashMap<(String, String), Option<String>> = HashMap::new();
    let mut previews = HashMap::new();
    for t in threads {
        let Some(location) = locations.get(&t.thread_id) else {
            continue;
        };
        if let Some(preview) =
            build_outdated_preview(&mut file_cache, scm, &t.file_path, &t.commit_hash, location)
        {
            previews.insert(t.thread_id.clone(), preview);
        }
    }
    previews
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

        // Read the code that each modified/deleted thread was commented on
        // (its own anchor commit, not the target), here at load time so
        // rendering never re-reads a file per frame. Unknown threads and
        // any read failure yield no preview, so the plain comment block is
        // used, same as before this existed.
        let previews: HashMap<String, OutdatedPreview> = match &scm {
            Some(scm) => build_outdated_previews(scm.as_ref(), &visible_threads, &locations),
            None => HashMap::new(),
        };

        let mut threads = Vec::with_capacity(visible_threads.len());
        let mut comments: HashMap<String, Vec<Comment>> = HashMap::new();

        for t in &visible_threads {
            let location = locations
                .get(&t.thread_id)
                .cloned()
                .unwrap_or_else(|| assume_unchanged(t));
            let preview = previews.get(&t.thread_id).cloned();
            threads.push(convert_thread_summary(t, &location, preview));

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
    use seal_core::scm::ScmKind;
    use std::path::Path;

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

    /// A fake backend whose `show_file` always succeeds with fixed content,
    /// so these tests isolate `build_outdated_preview`'s own logic (the
    /// `Modified`/`Deleted`-only guard, the range validation, and the
    /// clamp) from whatever `AnchorLocator`/git would actually decide.
    struct FakeScm {
        content: String,
    }

    impl ScmRepo for FakeScm {
        fn kind(&self) -> ScmKind {
            ScmKind::Git
        }
        fn root(&self) -> &Path {
            Path::new("/repo")
        }
        fn current_anchor(&self) -> Result<String> {
            unimplemented!("not used by build_outdated_preview")
        }
        fn current_commit(&self) -> Result<String> {
            unimplemented!("not used by build_outdated_preview")
        }
        fn commit_for_anchor(&self, _anchor: &str) -> Result<String> {
            unimplemented!("not used by build_outdated_preview")
        }
        fn parent_commit(&self, _commit: &str) -> Result<String> {
            unimplemented!("not used by build_outdated_preview")
        }
        fn diff_git(&self, _from: &str, _to: &str) -> Result<String> {
            unimplemented!("not used by build_outdated_preview")
        }
        fn diff_git_file(&self, _from: &str, _to: &str, _file: &str) -> Result<String> {
            unimplemented!("not used by build_outdated_preview")
        }
        fn changed_files_between(&self, _from: &str, _to: &str) -> Result<Vec<String>> {
            unimplemented!("not used by build_outdated_preview")
        }
        fn file_exists(&self, _rev: &str, _path: &str) -> Result<bool> {
            unimplemented!("not used by build_outdated_preview")
        }
        fn show_file(&self, _rev: &str, _path: &str) -> Result<String> {
            Ok(self.content.clone())
        }
    }

    fn location(status: AnchorStatus, original_start: u32, original_end: u32) -> AnchorLocation {
        AnchorLocation {
            status,
            original_start,
            original_end,
            current_start: None,
            current_end: None,
            reason: None,
        }
    }

    /// `Modified`/`Deleted` are the only statuses with a trustworthy anchor
    /// commit to read from; `Unknown` (and, by construction, `Unchanged`/
    /// `Shifted`, which never even reach this function in `load_review_data`)
    /// must never get a preview, even when the file happens to be readable.
    #[test]
    fn build_outdated_preview_never_reads_unknown_anchors() {
        let scm = FakeScm {
            content: (1..=20)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let mut file_cache = HashMap::new();
        let unknown = location(AnchorStatus::Unknown, 5, 5);

        let preview = build_outdated_preview(&mut file_cache, &scm, "f.txt", "deadbeef", &unknown);

        assert!(
            preview.is_none(),
            "an unknown anchor must never read a preview, even though the \
             file was readable"
        );
    }

    #[test]
    fn build_outdated_preview_clamps_a_huge_anchor_range() {
        let scm = FakeScm {
            content: (1..=500)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let mut file_cache = HashMap::new();
        let huge = location(AnchorStatus::Modified, 1, 500);

        let preview = build_outdated_preview(&mut file_cache, &scm, "f.txt", "abc123", &huge)
            .expect("modified anchor in a readable file must get a preview");

        let span = preview.anchor_end - preview.anchor_start + 1;
        assert_eq!(
            span,
            i64::from(MAX_PREVIEW_ANCHOR_LINES),
            "a 500-line stored range must clamp to MAX_PREVIEW_ANCHOR_LINES"
        );
        assert_eq!(
            preview.lines.len(),
            (MAX_PREVIEW_ANCHOR_LINES + PREVIEW_CONTEXT_LINES) as usize,
            "window = clamped anchor + trailing context only (anchor starts \
             at line 1, so there is no leading context to add)"
        );
    }

    #[test]
    fn build_outdated_preview_reads_the_given_commit_not_some_other_one() {
        let scm = FakeScm {
            content: "a\nb\nc\nd\ne\n".to_string(),
        };
        let mut file_cache = HashMap::new();
        let loc = location(AnchorStatus::Deleted, 3, 3);

        let preview =
            build_outdated_preview(&mut file_cache, &scm, "f.txt", "the-anchor-commit", &loc)
                .expect("deleted anchor in a readable file must get a preview");

        assert_eq!(preview.commit, "the-anchor-commit");
        // The fake ignores `rev`, so this mainly documents/locks the field;
        // the real-git tests in `real_git_tests` prove the content actually
        // comes from the anchor commit and not the target.
    }
}

/// Real-git tests for the load-time preview layer (`build_outdated_preview`,
/// `build_outdated_previews`, and their wiring into `load_review_data`).
/// Mirrors the fixture pattern in `seal_core::anchor` tests and
/// `seal_cli::cli::commands::threads::real_git_tests`.
#[cfg(test)]
mod real_git_tests {
    use super::*;
    use seal_core::core::CoreContext;
    use seal_core::events::CodeSelection;
    use seal_core::scm::git::GitRepo;
    use std::path::Path;
    use std::process::Command;
    use tempfile::{tempdir, TempDir};

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("utf8")
            .trim()
            .to_string()
    }

    fn write_lines(repo: &Path, path: &str, lines: &[String]) {
        let mut body = lines.join("\n");
        body.push('\n');
        std::fs::write(repo.join(path), body).expect("write file");
    }

    fn base_lines(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("base {i}")).collect()
    }

    fn init_repo(dir: &TempDir) {
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "Test User"]);
        std::fs::write(root.join(".git/info/exclude"), ".seal/\n").expect("exclude");
    }

    fn init_seal(root: &Path) -> CoreContext {
        let seal_dir = root.join(".seal");
        std::fs::create_dir(&seal_dir).expect("create .seal");
        std::fs::write(seal_dir.join("version"), "2\n").expect("write version");
        std::fs::create_dir(seal_dir.join("reviews")).expect("create reviews dir");
        CoreContext::new(root, &seal_dir.join("index.db")).expect("context")
    }

    fn open_client(root: &Path) -> CoreClient {
        let ctx = CoreContext::new(root, &root.join(".seal/index.db")).expect("context");
        CoreClient::new(ctx, root)
    }

    fn find<'a>(threads: &'a [ThreadSummary], thread_id: &str) -> &'a ThreadSummary {
        threads
            .iter()
            .find(|t| t.thread_id == thread_id)
            .unwrap_or_else(|| panic!("thread {thread_id} not loaded"))
    }

    /// A repo with `f.txt` holding 200 `base N` lines and a review whose
    /// threads all anchor to the initial commit, followed by one trunk edit
    /// that edits line 50 in place, deletes line 60, and edits line 75
    /// (inside a 1-150 "huge" selection) — then a real rebase + retarget,
    /// the way a maw workspace does it.
    struct Fixture {
        dir: TempDir,
        review_id: String,
        unchanged_thread: String,
        modified_thread: String,
        deleted_thread: String,
        huge_thread: String,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempdir().expect("tempdir");
            init_repo(&dir);
            let root = dir.path();
            write_lines(root, "f.txt", &base_lines(200));
            git(root, &["add", "f.txt"]);
            git(root, &["commit", "-qm", "base"]);
            git(root, &["branch", "-M", "main"]);
            git(root, &["checkout", "-q", "--detach"]);

            // A distinct commit ahead of "base" so the review's base can
            // resolve to a real parent commit (the root commit has none,
            // which the review base-resolution machinery doesn't accept).
            std::fs::write(root.join("marker.txt"), "marker\n").expect("write marker");
            git(root, &["add", "marker.txt"]);
            git(root, &["commit", "-qm", "feature"]);

            let ctx = init_seal(root);
            let scm = GitRepo::new(root.to_path_buf());

            let target = scm.current_commit().expect("target");
            let base =
                seal_core::range::resolve_new_review_base(&scm, &target, None).expect("base");
            let review_id = ctx
                .services()
                .expect("services")
                .reviews()
                .create(
                    &scm,
                    "Review".to_string(),
                    None,
                    None,
                    Some("alice"),
                    Some(base),
                )
                .expect("create review");

            let review = ctx
                .services()
                .expect("services")
                .reviews()
                .get(&review_id)
                .expect("review");
            let commit = seal_core::range::resolve_target_commit(&scm, &review);

            let unchanged_thread = ctx
                .services()
                .expect("services")
                .threads()
                .create(
                    &review_id,
                    "f.txt",
                    CodeSelection::line(3),
                    commit.clone(),
                    Some("alice"),
                )
                .expect("create unchanged thread");
            let modified_thread = ctx
                .services()
                .expect("services")
                .threads()
                .create(
                    &review_id,
                    "f.txt",
                    CodeSelection::line(50),
                    commit.clone(),
                    Some("alice"),
                )
                .expect("create modified thread");
            let deleted_thread = ctx
                .services()
                .expect("services")
                .threads()
                .create(
                    &review_id,
                    "f.txt",
                    CodeSelection::line(60),
                    commit.clone(),
                    Some("alice"),
                )
                .expect("create deleted thread");
            let huge_thread = ctx
                .services()
                .expect("services")
                .threads()
                .create(
                    &review_id,
                    "f.txt",
                    CodeSelection::range(1, 150),
                    commit,
                    Some("alice"),
                )
                .expect("create huge thread");

            // Trunk edit: rewrite line 50 in place, delete line 60, and edit
            // line 75 (inside the huge thread's 1-150 range).
            let head = git(root, &["rev-parse", "HEAD"]);
            git(root, &["checkout", "-q", "main"]);
            let mut lines = base_lines(200);
            lines[49] = "base 50 CHANGED".to_string();
            lines.remove(59); // "base 60"
            let idx75 = lines
                .iter()
                .position(|l| l == "base 75")
                .expect("line 75 present after the removal above");
            lines[idx75] = "base 75 CHANGED".to_string();
            write_lines(root, "f.txt", &lines);
            git(root, &["commit", "-qam", "trunk edit"]);
            git(root, &["checkout", "-q", "--detach", &head]);
            git(root, &["rebase", "-q", "main"]);
            assert_ne!(git(root, &["rev-parse", "HEAD"]), head, "rebase moved HEAD");

            ctx.services()
                .expect("services")
                .reviews()
                .retarget(&scm, &review_id, Some("alice"), None)
                .expect("retarget");

            Self {
                dir,
                review_id,
                unchanged_thread,
                modified_thread,
                deleted_thread,
                huge_thread,
            }
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        fn load(&self) -> ReviewData {
            open_client(self.root())
                .load_review_data(&self.review_id)
                .expect("load review data")
                .expect("review exists")
        }
    }

    #[test]
    fn load_review_data_reads_anchor_commit_preview_for_modified_and_deleted_threads() {
        let fx = Fixture::new();
        let data = fx.load();

        let unchanged = find(&data.threads, &fx.unchanged_thread);
        assert!(!unchanged.outdated, "an untouched line stays intact");
        assert!(
            unchanged.outdated_preview.is_none(),
            "an intact anchor must never get a preview read"
        );

        let modified = find(&data.threads, &fx.modified_thread);
        assert!(modified.outdated);
        assert_eq!(modified.drift_status, "modified");
        let preview = modified
            .outdated_preview
            .as_ref()
            .expect("modified thread must have a preview");
        assert_eq!(preview.lines.iter().filter(|l| *l == "base 50").count(), 1);
        assert!(
            !preview.lines.iter().any(|l| l == "base 50 CHANGED"),
            "preview must show the anchor commit's original line, not the \
             target's edit: {:?}",
            preview.lines
        );

        let deleted = find(&data.threads, &fx.deleted_thread);
        assert!(deleted.outdated);
        assert_eq!(deleted.drift_status, "deleted");
        let preview = deleted
            .outdated_preview
            .as_ref()
            .expect("deleted thread must have a preview");
        assert_eq!(preview.lines.iter().filter(|l| *l == "base 60").count(), 1);
    }

    #[test]
    fn load_review_data_clamps_a_huge_anchor_range_to_a_small_preview() {
        let fx = Fixture::new();
        let data = fx.load();

        let huge = find(&data.threads, &fx.huge_thread);
        assert!(huge.outdated);
        assert_eq!(huge.drift_status, "modified");
        let preview = huge
            .outdated_preview
            .as_ref()
            .expect("huge (1-150) thread must have a preview");

        // The stored anchor claims a 150-line range; the preview must clamp
        // to a small window regardless of what the event claims.
        let span = preview.anchor_end - preview.anchor_start + 1;
        assert!(
            span <= i64::from(MAX_PREVIEW_ANCHOR_LINES),
            "anchor span must be clamped to MAX_PREVIEW_ANCHOR_LINES, got {span} \
             ({}-{})",
            preview.anchor_start,
            preview.anchor_end
        );
        assert!(
            preview.lines.len() <= (MAX_PREVIEW_ANCHOR_LINES + 2 * PREVIEW_CONTEXT_LINES) as usize,
            "preview window (anchor + context) must stay small: {} lines",
            preview.lines.len()
        );
    }

    /// A thread whose anchor commit is later pruned entirely (not merely
    /// rewritten by a rebase) must fall back to the plain comment block:
    /// `unknown` never reads a preview, because there is no commit that can
    /// be trusted to read from.
    #[test]
    fn load_review_data_shows_no_preview_for_an_unknown_thread() {
        let dir = tempdir().expect("tempdir");
        init_repo(&dir);
        let root = dir.path();
        write_lines(root, "f.txt", &base_lines(20));
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "base"]);
        git(root, &["branch", "-M", "main"]);
        git(root, &["checkout", "-q", "--detach"]);

        // A commit that only this thread anchors to, so it can be pruned in
        // isolation.
        let mut lines = base_lines(20);
        lines.push("feature line".to_string());
        write_lines(root, "f.txt", &lines);
        git(root, &["commit", "-qam", "feature"]);

        let ctx = init_seal(root);
        let scm = GitRepo::new(root.to_path_buf());

        let target = scm.current_commit().expect("target");
        let base = seal_core::range::resolve_new_review_base(&scm, &target, None).expect("base");
        let review_id = ctx
            .services()
            .expect("services")
            .reviews()
            .create(
                &scm,
                "Review".to_string(),
                None,
                None,
                Some("alice"),
                Some(base),
            )
            .expect("create review");

        let review = ctx
            .services()
            .expect("services")
            .reviews()
            .get(&review_id)
            .expect("review");
        let commit = seal_core::range::resolve_target_commit(&scm, &review);
        let orphaned_commit = commit.clone();
        let orphaned_thread = ctx
            .services()
            .expect("services")
            .threads()
            .create(
                &review_id,
                "f.txt",
                CodeSelection::line(19),
                commit,
                Some("alice"),
            )
            .expect("create orphaned thread");

        // Rebase onto a trunk edit: the commit the thread anchors to is
        // rewritten (new hash), so the old one becomes unreachable.
        let head = git(root, &["rev-parse", "HEAD"]);
        git(root, &["checkout", "-q", "main"]);
        let mut trunk_lines = base_lines(20);
        trunk_lines.insert(0, "top".to_string());
        write_lines(root, "f.txt", &trunk_lines);
        git(root, &["commit", "-qam", "trunk edit"]);
        git(root, &["checkout", "-q", "--detach", &head]);
        git(root, &["rebase", "-q", "main"]);
        assert_ne!(git(root, &["rev-parse", "HEAD"]), head, "rebase moved HEAD");

        ctx.services()
            .expect("services")
            .reviews()
            .retarget(&scm, &review_id, Some("alice"), None)
            .expect("retarget");

        // Prune the old commit so it is genuinely unresolvable, not merely
        // rewritten (rebase alone keeps it around via loose objects).
        git(root, &["reflog", "expire", "--expire=now", "--all"]);
        git(root, &["gc", "-q", "--prune=now"]);
        let probe = Command::new("git")
            .current_dir(root)
            .args(["cat-file", "-e", &format!("{orphaned_commit}^{{commit}}")])
            .output()
            .expect("run git");
        assert!(!probe.status.success(), "anchor commit was pruned");

        let data = open_client(root)
            .load_review_data(&review_id)
            .expect("load review data")
            .expect("review exists");

        let orphaned = find(&data.threads, &orphaned_thread);
        assert!(orphaned.outdated);
        assert_eq!(orphaned.drift_status, "unknown");
        assert!(
            orphaned.outdated_preview.is_none(),
            "an unknown anchor must never get a preview: {:?}",
            orphaned.outdated_preview.as_ref().map(|p| &p.lines)
        );
    }
}

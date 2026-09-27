//! Property tests over `sync_from_review_logs` (the v2 per-review-log sync
//! path), exercised end-to-end against real files under a temp `.seal` dir.
//!
//! History this guards against:
//! - bd-1s1: blank lines in the log shifted the sync cursor and caused vote
//!   replay (an incremental sync landed in a different state than a full
//!   sync of the same final content).
//! - bd-oum: a same-length content replacement (same line count, same byte
//!   count, different bytes) went undetected.
//! - bd-2ji: `prefix_hash` used a non-deterministic hasher.
//! - 1ee4902: the sync cursor advanced by parsed-event count instead of raw
//!   line count.
//!
//! Properties:
//! 2. Tamper detection — any non-append edit to the already-synced prefix of
//!    a review's log is reported as an anomaly by `sync_from_review_logs`.
//! 3. Incremental == full — splitting a review's log into chunks and syncing
//!    after each chunk produces the same projected state as one sync of the
//!    complete log.

// Integration test binary: clippy's unwrap_used test-exemption only
// recognizes `#[test]`-annotated functions and `#[cfg(test)] mod tests`, not
// plain helper functions in a `tests/*.rs` file, so allow it file-wide here
// (consistent with `clippy.toml`'s `allow-unwrap-in-tests`).
#![allow(clippy::unwrap_used)]

use std::path::Path;

use proptest::prelude::*;
use rusqlite::OptionalExtension;
use tempfile::tempdir;

use seal_core::events::{
    CodeSelection, CommentAdded, Event, EventEnvelope, ReviewCreated, ReviewerVoted, ThreadCreated,
    VoteType,
};
use seal_core::log::{open_or_create_review, AppendLog, ReviewLog};
use seal_core::projection::{sync_from_review_logs, AnomalyKind, ProjectionDb};

// ============================================================================
// Shared helpers
// ============================================================================

fn review_created(review_id: &str, author: &str) -> EventEnvelope {
    EventEnvelope::new(
        author,
        Event::ReviewCreated(ReviewCreated {
            review_id: review_id.to_string(),
            jj_change_id: "change123".to_string(),
            scm_kind: Some("jj".to_string()),
            scm_anchor: Some("change123".to_string()),
            initial_commit: "commit456".to_string(),
            base_commit: None,
            title: format!("Review {review_id}"),
            description: None,
        }),
    )
}

fn reviewer_voted(review_id: &str, reviewer: &str, vote: VoteType, tag: usize) -> EventEnvelope {
    EventEnvelope::new(
        reviewer,
        Event::ReviewerVoted(ReviewerVoted {
            review_id: review_id.to_string(),
            vote,
            // `tag` makes every generated line's JSON distinct, so a
            // "swap two lines" tamper is guaranteed to actually change
            // content instead of silently swapping identical lines.
            reason: Some(format!("reason-{tag}")),
            target_commit: None,
        }),
    )
}

fn thread_created(review_id: &str, thread_id: &str) -> EventEnvelope {
    EventEnvelope::new(
        "author",
        Event::ThreadCreated(ThreadCreated {
            thread_id: thread_id.to_string(),
            review_id: review_id.to_string(),
            file_path: "src/main.rs".to_string(),
            selection: CodeSelection::range(1, 3),
            commit_hash: "abc123".to_string(),
        }),
    )
}

fn comment_added(thread_id: &str, comment_id: &str, body: &str) -> EventEnvelope {
    EventEnvelope::new(
        "author",
        Event::CommentAdded(CommentAdded {
            comment_id: comment_id.to_string(),
            thread_id: thread_id.to_string(),
            body: body.to_string(),
        }),
    )
}

/// Write raw lines (each terminated with `\n`, none containing an embedded
/// `\n`) as the complete contents of a review's log file.
fn write_review_log(seal_root: &Path, review_id: &str, lines: &[String]) -> ReviewLog {
    let log = open_or_create_review(seal_root, review_id).unwrap();
    let mut content = String::new();
    for line in lines {
        content.push_str(line);
        content.push('\n');
    }
    std::fs::write(log.path(), content).unwrap();
    log
}

fn find_alnum_char_index(s: &str) -> Option<usize> {
    s.char_indices()
        .find(|(_, c)| c.is_ascii_alphanumeric())
        .map(|(i, _)| i)
}

/// Flip one ASCII alphanumeric character in `line` to a different ASCII
/// alphanumeric character, keeping the byte length identical and staying in
/// valid UTF-8 (so the file remains readable as text either way).
fn flip_one_char(line: &str) -> Option<String> {
    let idx = find_alnum_char_index(line)?;
    let mut bytes = line.as_bytes().to_vec();
    let c = bytes[idx];
    bytes[idx] = match c {
        b'9' => b'0',
        b'0'..=b'8' | b'a'..=b'y' | b'A'..=b'Y' => c + 1,
        b'z' => b'a',
        b'Z' => b'A',
        _ => return None,
    };
    Some(String::from_utf8(bytes).unwrap())
}

/// Snapshot everything `sync_from_review_logs` projects for one review:
/// review detail (incl. votes), and every thread's full detail (incl.
/// comments). Used to compare incremental vs. full sync outcomes.
fn snapshot_review(db: &ProjectionDb, review_id: &str) -> serde_json::Value {
    let review = db.get_review(review_id).unwrap();
    let threads = db.list_threads(review_id, None, None).unwrap();
    // Threads are already ordered deterministically by list_threads (file
    // path, then line), so no extra sort is needed here.
    let thread_details: Vec<serde_json::Value> = threads
        .iter()
        .map(|t| serde_json::to_value(db.get_thread(&t.thread_id).unwrap()).unwrap())
        .collect();
    serde_json::json!({
        "review": review,
        "threads": thread_details,
    })
}

// ============================================================================
// Property 2: tamper detection
// ============================================================================

#[derive(Debug, Clone, Copy)]
enum TamperKind {
    ByteFlip,
    SwapLines,
    DeleteLine,
    SameLengthReplace,
}

fn arb_tamper_kind() -> impl Strategy<Value = TamperKind> {
    prop_oneof![
        Just(TamperKind::ByteFlip),
        Just(TamperKind::SwapLines),
        Just(TamperKind::DeleteLine),
        Just(TamperKind::SameLengthReplace),
    ]
}

/// A single review's raw event lines: `ReviewCreated` followed by 1-5
/// distinguishable `ReviewerVoted` lines (real serialized events, no blanks
/// — tamper detection is about content/line integrity, not blank-line
/// handling, which property 1 and 3 already cover).
fn arb_review_lines(review_id: String) -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(prop::bool::ANY, 1..=5).prop_map(move |votes| {
        let mut lines = vec![review_created(&review_id, "author").to_json_line().unwrap()];
        for (i, is_block) in votes.iter().enumerate() {
            let vote = if *is_block {
                VoteType::Block
            } else {
                VoteType::Lgtm
            };
            lines.push(
                reviewer_voted(&review_id, "reviewer-a", vote, i)
                    .to_json_line()
                    .unwrap(),
            );
        }
        lines
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Any non-append edit to the already-synced prefix of a review log is
    /// caught by `sync_from_review_logs` as an anomaly (Shrunk, HashMismatch,
    /// or ParseError). This is the property bd-oum's fast byte-count path
    /// violated (see the regression test in projection::tests for the
    /// minimized counterexample and the fix).
    #[test]
    fn prop_tamper_is_detected(
        lines in arb_review_lines("cr-tamper".to_string()),
        tamper in arb_tamper_kind(),
        idx_seed in any::<usize>(),
    ) {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();
        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        write_review_log(seal_root, "cr-tamper", &lines);

        // Baseline sync: establishes review_file_state for the untampered file.
        let report1 = sync_from_review_logs(&db, seal_root).unwrap();
        prop_assert!(report1.anomalies.is_empty());

        let total = lines.len();
        prop_assume!(total >= 1);
        let idx = idx_seed % total;

        let tampered: Option<Vec<String>> = match tamper {
            TamperKind::ByteFlip | TamperKind::SameLengthReplace => {
                flip_one_char(&lines[idx]).map(|flipped| {
                    let mut new_lines = lines.clone();
                    new_lines[idx] = flipped;
                    new_lines
                })
            }
            TamperKind::SwapLines => {
                if total < 2 {
                    None
                } else {
                    let jdx = (idx + 1) % total;
                    let mut new_lines = lines.clone();
                    new_lines.swap(idx, jdx);
                    if new_lines == lines {
                        None
                    } else {
                        Some(new_lines)
                    }
                }
            }
            TamperKind::DeleteLine => {
                let mut new_lines = lines;
                new_lines.remove(idx);
                Some(new_lines)
            }
        };

        let Some(tampered_lines) = tampered else {
            // Could not construct a valid tamper for this input (e.g. no
            // alnum char to flip, or a single-line file for swap) — skip.
            return Ok(());
        };

        write_review_log(seal_root, "cr-tamper", &tampered_lines);

        let report2 = sync_from_review_logs(&db, seal_root).unwrap();
        prop_assert!(
            report2.anomalies.iter().any(|a| {
                a.review_id == "cr-tamper"
                    && matches!(
                        a.kind,
                        AnomalyKind::HashMismatch | AnomalyKind::Shrunk | AnomalyKind::ParseError
                    )
            }),
            "tamper ({tamper:?} at idx {idx} of {total}) went undetected: {:?}",
            report2.anomalies
        );
    }
}

// ============================================================================
// Property 3: incremental == full
// ============================================================================

/// One review's full script: `ReviewCreated`, a run of votes, a thread, two
/// comments on it, and a run of blank/whitespace filler lines scattered
/// through — real serialized events interleaved with the kind of blank
/// lines bd-1s1 was about.
fn arb_full_review_script(review_id: String) -> impl Strategy<Value = Vec<String>> {
    let thread_id = format!("th-{review_id}");
    (
        prop::collection::vec(prop::bool::ANY, 0..=6),
        prop::collection::vec(0usize..3, 0..=4), // filler line positions/kinds
    )
        .prop_map(move |(votes, filler_kinds)| {
            let mut events = vec![review_created(&review_id, "author")];
            for (i, is_block) in votes.iter().enumerate() {
                let vote = if *is_block {
                    VoteType::Block
                } else {
                    VoteType::Lgtm
                };
                events.push(reviewer_voted(&review_id, "reviewer-a", vote, i));
            }
            events.push(thread_created(&review_id, &thread_id));
            events.push(comment_added(
                &thread_id,
                &format!("{thread_id}.1"),
                "first",
            ));
            events.push(comment_added(
                &thread_id,
                &format!("{thread_id}.2"),
                "second",
            ));

            let mut lines: Vec<String> = events.iter().map(|e| e.to_json_line().unwrap()).collect();

            // Scatter blank/whitespace filler lines through the sequence,
            // inserting from the back so earlier indices stay valid.
            let mut positions: Vec<(usize, usize)> = filler_kinds
                .into_iter()
                .enumerate()
                .map(|(n, kind)| (n % (lines.len() + 1), kind))
                .collect();
            positions.sort_by_key(|a| std::cmp::Reverse(a.0));
            for (pos, kind) in positions {
                let filler = match kind {
                    0 => String::new(),
                    1 => "   ".to_string(),
                    _ => "\t".to_string(),
                };
                lines.insert(pos.min(lines.len()), filler);
            }
            lines
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Splitting each review's log into `k` append-only chunks, syncing
    /// after every chunk, produces the same final projection as one sync of
    /// the complete log — for reviews, votes, threads, and comments. This is
    /// exactly the property bd-1s1 violated (incremental vote replay drifted
    /// from the true final state whenever blank lines were present).
    #[test]
    fn prop_incremental_sync_equals_full_sync(
        review_count in 1usize..=3,
        chunk_count in 1usize..=4,
        script0 in arb_full_review_script("cr-inc-0".to_string()),
        script1 in arb_full_review_script("cr-inc-1".to_string()),
        script2 in arb_full_review_script("cr-inc-2".to_string()),
    ) {
        // Always generate 3 independent review scripts through proptest's
        // normal parameter binding (so each is properly shrinkable and
        // varies per case), then use only the first `review_count` of them.
        let all_scripts = vec![
            ("cr-inc-0".to_string(), script0),
            ("cr-inc-1".to_string(), script1),
            ("cr-inc-2".to_string(), script2),
        ];
        let review_scripts: Vec<(String, Vec<String>)> =
            all_scripts.into_iter().take(review_count).collect();

        // --- Full sync: write everything, one sync_from_review_logs call ---
        let full_dir = tempdir().unwrap();
        let full_root = full_dir.path();
        let full_db = ProjectionDb::open_in_memory().unwrap();
        full_db.init_schema().unwrap();
        for (review_id, lines) in &review_scripts {
            write_review_log(full_root, review_id, lines);
        }
        let full_report = sync_from_review_logs(&full_db, full_root).unwrap();
        prop_assert!(full_report.anomalies.is_empty(), "full sync anomalies: {:?}", full_report.anomalies);

        // --- Incremental sync: same content, split into chunks, synced after each ---
        let inc_dir = tempdir().unwrap();
        let inc_root = inc_dir.path();
        let inc_db = ProjectionDb::open_in_memory().unwrap();
        inc_db.init_schema().unwrap();

        for review_id in review_scripts.iter().map(|(id, _)| id) {
            open_or_create_review(inc_root, review_id).unwrap();
        }

        for chunk_idx in 0..chunk_count {
            for (review_id, lines) in &review_scripts {
                let len = lines.len();
                let start = len * chunk_idx / chunk_count;
                let end = len * (chunk_idx + 1) / chunk_count;
                if start >= end {
                    continue;
                }
                let log = ReviewLog::new(inc_root, review_id).unwrap();
                let mut content = String::new();
                for line in &lines[start..end] {
                    content.push_str(line);
                    content.push('\n');
                }
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(log.path())
                    .unwrap();
                file.write_all(content.as_bytes()).unwrap();
                file.flush().unwrap();
            }
            let report = sync_from_review_logs(&inc_db, inc_root).unwrap();
            prop_assert!(report.anomalies.is_empty(), "incremental sync anomalies at chunk {chunk_idx}: {:?}", report.anomalies);
        }

        // Every raw line must have been written across the chunk loop.
        for (review_id, lines) in &review_scripts {
            let log = ReviewLog::new(inc_root, review_id).unwrap();
            prop_assert_eq!(log.total_lines().unwrap(), lines.len());
        }

        for (review_id, _) in &review_scripts {
            let full_snapshot = snapshot_review(&full_db, review_id);
            let inc_snapshot = snapshot_review(&inc_db, review_id);
            prop_assert_eq!(
                full_snapshot, inc_snapshot,
                "incremental projection for {} diverged from full-sync projection", review_id
            );
        }
    }
}

// ============================================================================
// Direct cursor invariant (blank lines must not shift the cursor)
// ============================================================================

fn stored_line_count(db: &ProjectionDb, review_id: &str) -> Option<i64> {
    db.conn()
        .query_row(
            "SELECT line_count FROM review_file_state WHERE review_id = ?",
            [review_id],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Direct statement of the bone's third property: "blank and trailing
    /// lines do not shift the cursor". After every sync that records state
    /// for a review, `review_file_state.line_count` (the sync cursor) must
    /// equal the file's true raw line count (`total_lines()`) — never the
    /// parsed-event count. This is the exact invariant 1ee4902 restored for
    /// the v1 log; `sync_new_file`/`sync_grew_file` (v2) have their own
    /// independent bookkeeping and must uphold it too. A cursor that
    /// undercounts (event count instead of line count) doesn't necessarily
    /// corrupt idempotent vote replay (see `prop_incremental_sync_equals_full_sync`,
    /// which can stay green under that mutation), but it is still a drift
    /// bug in its own right — this test targets it directly.
    #[test]
    fn prop_cursor_matches_total_lines(
        chunk_count in 1usize..=4,
        script in arb_full_review_script("cr-cursor".to_string()),
    ) {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();
        let db = ProjectionDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let log = open_or_create_review(seal_root, "cr-cursor").unwrap();

        let len = script.len();
        prop_assume!(len > 0);

        for chunk_idx in 0..chunk_count {
            let start = len * chunk_idx / chunk_count;
            let end = len * (chunk_idx + 1) / chunk_count;
            if start >= end {
                continue;
            }
            let mut content = String::new();
            for line in &script[start..end] {
                content.push_str(line);
                content.push('\n');
            }
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(log.path())
                .unwrap();
            file.write_all(content.as_bytes()).unwrap();
            file.flush().unwrap();

            let report = sync_from_review_logs(&db, seal_root).unwrap();
            prop_assert!(report.anomalies.is_empty(), "unexpected anomalies: {:?}", report.anomalies);

            if let Some(stored) = stored_line_count(&db, "cr-cursor") {
                let actual_total = log.total_lines().unwrap();
                prop_assert_eq!(
                    stored, actual_total as i64,
                    "cursor drifted from total_lines at chunk {}", chunk_idx
                );
            }
        }
    }
}

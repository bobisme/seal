//! Property tests for the append-only log primitives (`FileLog` and
//! `ReviewLog`): [`AppendLog::total_lines`] and [`AppendLog::prefix_hash`].
//!
//! History this guards against:
//! - bd-1s1: blank lines shifted the sync cursor and caused vote replay.
//! - bd-2ji: `prefix_hash` used an unstable (randomized) hasher.
//! - 1ee4902: cursor advancement used the parsed-event count instead of the
//!   raw line count, drifting whenever blank lines were present.
//!
//! ## Line-counting definition under test
//!
//! A "line" is exactly one `\n`-terminated write to the log file, whether it
//! is a real serialized event, an empty string, or a whitespace-only string.
//! `total_lines()` counts every one of these, including blank ones.
//! `prefix_hash(n)` hashes the first `n` such raw lines (blank ones
//! included), independent of whether they parse as events. This module
//! writes each generated line explicitly terminated by `\n` (never embedding
//! a `\n` inside a single generated line), so "number of lines written" is
//! unambiguous and directly comparable to `total_lines()`.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

use proptest::prelude::*;
use tempfile::tempdir;

use crate::events::{
    CodeSelection, Event, EventEnvelope, ReviewCreated, ReviewerVoted, ThreadCreated, VoteType,
};
use crate::log::{open_or_create, open_or_create_review, AppendLog};

/// Append a single already-terminated-by-us raw line (no embedded `\n`) to a
/// file, adding the trailing newline ourselves. Bypasses `AppendLog::append`
/// so we can inject blank / whitespace-only lines, matching how a stray
/// editor save, `jj` restore, or partial write can produce them in practice.
fn append_raw_line(path: &Path, line: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{line}").unwrap();
    file.flush().unwrap();
}

/// One raw line to write into a log file for these tests.
#[derive(Debug, Clone)]
enum LineKind {
    Blank,
    Whitespace(String),
    Event(String),
}

impl LineKind {
    fn as_str(&self) -> &str {
        match self {
            Self::Blank => "",
            Self::Whitespace(s) | Self::Event(s) => s,
        }
    }
}

fn arb_event_json() -> impl Strategy<Value = String> {
    ("[a-z]{1,6}", "cr-[a-z0-9]{1,6}", 0u8..3).prop_map(|(author, review_id, kind)| {
        let event = match kind {
            0 => Event::ReviewCreated(ReviewCreated {
                review_id: review_id.clone(),
                jj_change_id: "change123".to_string(),
                scm_kind: Some("jj".to_string()),
                scm_anchor: Some("change123".to_string()),
                initial_commit: "commit456".to_string(),
                base_commit: None,
                title: format!("Review {review_id}"),
                description: None,
            }),
            1 => Event::ReviewerVoted(ReviewerVoted {
                review_id,
                vote: VoteType::Lgtm,
                reason: None,
                target_commit: None,
            }),
            _ => Event::ThreadCreated(ThreadCreated {
                thread_id: format!("th-{review_id}"),
                review_id,
                file_path: "src/main.rs".to_string(),
                selection: CodeSelection::line(1),
                commit_hash: "abc123".to_string(),
            }),
        };
        EventEnvelope::new(author, event).to_json_line().unwrap()
    })
}

fn arb_line_kind() -> impl Strategy<Value = LineKind> {
    prop_oneof![
        2 => Just(LineKind::Blank),
        2 => "[ \t]{1,6}".prop_map(LineKind::Whitespace),
        5 => arb_event_json().prop_map(LineKind::Event),
    ]
}

fn arb_lines(max_len: usize) -> impl Strategy<Value = Vec<LineKind>> {
    prop::collection::vec(arb_line_kind(), 0..=max_len)
}

fn write_lines(path: &Path, lines: &[LineKind]) {
    for line in lines {
        append_raw_line(path, line.as_str());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Property 1 (FileLog / v1 legacy log): appending events never changes
    /// the hash of the previously-hashed prefix, and `total_lines()` grows by
    /// exactly the number of raw lines appended — regardless of how many of
    /// those lines are blank or whitespace-only.
    #[test]
    fn prop_append_stability_filelog(
        initial in arb_lines(12),
        appended in arb_lines(12),
    ) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = open_or_create(&path).unwrap();

        write_lines(&path, &initial);
        let n = log.total_lines().unwrap();
        prop_assert_eq!(n, initial.len(), "total_lines must count every raw line, blank or not");

        let hash_before = log.prefix_hash(n).unwrap();

        write_lines(&path, &appended);

        let hash_after = log.prefix_hash(n).unwrap();
        prop_assert_eq!(
            hash_before, hash_after,
            "prefix_hash(n) for the pre-append prefix length must be unchanged by an append"
        );

        let new_total = log.total_lines().unwrap();
        prop_assert_eq!(
            new_total, initial.len() + appended.len(),
            "total_lines must grow by exactly the number of appended raw lines"
        );
    }

    /// Same property, for the v2 per-review log (`ReviewLog`), which
    /// duplicates `FileLog`'s line/hash logic independently.
    #[test]
    fn prop_append_stability_reviewlog(
        initial in arb_lines(12),
        appended in arb_lines(12),
    ) {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();
        let log = open_or_create_review(seal_root, "cr-append-stability").unwrap();
        let path = log.path();

        write_lines(&path, &initial);
        let n = log.total_lines().unwrap();
        prop_assert_eq!(n, initial.len());

        let hash_before = log.prefix_hash(n).unwrap();

        write_lines(&path, &appended);

        let hash_after = log.prefix_hash(n).unwrap();
        prop_assert_eq!(hash_before, hash_after);

        let new_total = log.total_lines().unwrap();
        prop_assert_eq!(new_total, initial.len() + appended.len());
    }

    /// `len()` (parsed, non-blank event count) never exceeds `total_lines()`
    /// (raw line count) — the invariant the cursor-drift bugs violated by
    /// conflating the two.
    #[test]
    fn prop_len_never_exceeds_total_lines(lines in arb_lines(16)) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = open_or_create(&path).unwrap();
        write_lines(&path, &lines);

        let total = log.total_lines().unwrap();
        // len() skips lines whose *trimmed* content is empty (blank or
        // whitespace-only), matching `AppendLog::read_from`'s skip rule.
        let expected_len = lines.iter().filter(|l| !l.as_str().trim().is_empty()).count();
        prop_assert!(expected_len <= total);
        prop_assert_eq!(log.len().unwrap(), expected_len);
    }
}

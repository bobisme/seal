//! Which changes made after an approval leave that approval standing.
//!
//! Recording a vote writes `.seal/reviews/<id>/events.jsonl`. Committing that
//! log after the LGTM is itself a commit on top of the approved code, and
//! before this module every such commit made the approval stale. That made
//! "track review logs in the repository" and "merge on a fresh approval"
//! mutually exclusive.
//!
//! # The rule
//!
//! The change from the approved commit `A` to the current target `T` is
//! EXEMPT, and the approval stays fresh, iff every path-level change in the
//! tree diff `A -> T` is exempt:
//!
//! - The path is inside `.seal/`, compared component by component: the first
//!   component is exactly `.seal`, something follows it, and no later
//!   component is empty, `.` or `..` (see [`is_exempt_path_bytes`]).
//!   `.sealignore`, `.seal-foo`, `x/.seal/y` and `.seal` itself are NOT exempt.
//! - An add is exempt. A delete is exempt unless it deletes a review log.
//! - A modify is exempt, but a modify of a review log (any `events.jsonl`
//!   under `.seal/`) must be append-only: the old content is a byte prefix of
//!   the new content. Review history may grow; it may not be rewritten.
//! - A rename or copy needs BOTH sides exempt and neither side a review log
//!   (renaming a log away deletes it).
//! - Anything else (entry type change, conflict, unknown status) is never
//!   exempt.
//!
//! # Why the net tree diff, not per-commit diffs
//!
//! The approval vouches for the tree at `A`. What merges is the tree at `T`.
//! If every path outside `.seal/` has identical content in both trees, the
//! code that merges is byte-for-byte the code the reviewer approved, however
//! the history in between is shaped. Comparing the two trees directly:
//!
//! - needs no rule for merge commits. A per-commit walk would have to pick a
//!   parent to diff against, and `A..T` also lists commits from a merged side
//!   branch whose own diffs say nothing about what the merge brought in;
//! - means the same thing on Git (`git diff A T`) and jj
//!   (`jj diff --from A --to T`), since both compare trees;
//! - can only err towards stale: any failure to list or read changes counts
//!   as non-exempt.
//!
//! One consequence: a source change that a later commit reverts leaves the
//! trees equal outside `.seal/`, so the approval stays fresh. The merged code
//! is then exactly the approved code, so this is not a false "fresh".

use crate::scm::{PathChange, ScmRepo};

/// First path component of the seal data directory, plus its separator.
const SEAL_DIR_PREFIX: &[u8] = b".seal/";

/// File name of an append-only review event log.
const REVIEW_LOG_NAME: &[u8] = b"events.jsonl";

/// True when a repository-relative path lies inside `.seal/`.
///
/// Exact spec (Kani-checked in `kani_proofs::exempt_path_matches_spec`):
/// the path is `.seal/` followed by one or more `/`-separated components,
/// each non-empty and neither `.` nor `..`.
#[must_use]
pub fn is_exempt_path_bytes(path: &[u8]) -> bool {
    let Some(rest) = path.strip_prefix(SEAL_DIR_PREFIX) else {
        return false;
    };
    !rest.is_empty()
        && rest
            .split(|byte| *byte == b'/')
            .all(|component| !component.is_empty() && component != b"." && component != b"..")
}

/// [`is_exempt_path_bytes`] for a `&str` path.
#[must_use]
pub fn is_exempt_path(path: &str) -> bool {
    is_exempt_path_bytes(path.as_bytes())
}

/// True when an exempt path names a review event log (`events.jsonl`).
///
/// This covers `.seal/reviews/<id>/events.jsonl` and any other
/// `events.jsonl` under `.seal/`, such as the v1 log. Treating more files
/// as logs only adds checks, so it can only turn "fresh" into "stale".
#[must_use]
pub fn is_review_log_path(path: &str) -> bool {
    is_exempt_path(path)
        && path
            .as_bytes()
            .rsplit(|byte| *byte == b'/')
            .next()
            .is_some_and(|name| name == REVIEW_LOG_NAME)
}

/// True when every byte is ASCII alphanumeric or one of `. _ - /`.
///
/// The append check reads a log back through the backend. jj parses a path
/// argument as a fileset, so a path with pattern or quoting characters could
/// select other files. Logs whose path is not plain are never exempt.
fn is_plain_path(path: &str) -> bool {
    path.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
}

/// True when `new` only appends to `old`: `old` is a byte prefix of `new`.
#[must_use]
pub fn is_append_only(old: &[u8], new: &[u8]) -> bool {
    new.starts_with(old)
}

/// How one path change bears on an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeVerdict<'a> {
    /// The change cannot alter reviewed code.
    Exempt,
    /// The change modifies this review log. It is exempt only if the new
    /// content appends to the old.
    NeedsAppendCheck(&'a str),
    /// The change can alter reviewed code or rewrite review history.
    NonExempt,
}

/// What the rule needs to know about one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathClass {
    /// Not inside `.seal/` (see [`is_exempt_path`]).
    Outside,
    /// Inside `.seal/`, not a review log.
    SealData,
    /// A review log with a plain path, so its content can be read back.
    ReviewLog,
    /// A review log whose path is not plain; its content is never trusted.
    OpaqueReviewLog,
}

/// Classify one repository-relative path.
#[must_use]
pub fn path_class(path: &str) -> PathClass {
    if !is_exempt_path(path) {
        PathClass::Outside
    } else if !is_review_log_path(path) {
        PathClass::SealData
    } else if is_plain_path(path) {
        PathClass::ReviewLog
    } else {
        PathClass::OpaqueReviewLog
    }
}

/// The kind of a [`PathChange`], without its paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Other,
}

/// The decision for one change, without its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Exempt,
    NeedsAppendCheck,
    NonExempt,
}

/// The whole per-change rule as a finite table.
///
/// `from` and `to` are the classes of the old and new path. A change with
/// one path passes the same class twice. Kani checks this table over its
/// entire domain in `kani_proofs::decide_contract`.
#[must_use]
pub const fn decide(kind: ChangeKind, from: PathClass, to: PathClass) -> Decision {
    match (kind, from, to) {
        // New files anywhere under .seal/, including a new review log.
        (ChangeKind::Added, PathClass::SealData | PathClass::ReviewLog | PathClass::OpaqueReviewLog, _)
        // Deleting or editing seal data that is not review history.
        | (ChangeKind::Deleted | ChangeKind::Modified, PathClass::SealData, _)
        // Moving or copying seal data within .seal/.
        | (ChangeKind::Renamed | ChangeKind::Copied, PathClass::SealData, PathClass::SealData) => {
            Decision::Exempt
        }
        // Editing a review log: only an append is allowed.
        (ChangeKind::Modified, PathClass::ReviewLog, _) => Decision::NeedsAppendCheck,
        _ => Decision::NonExempt,
    }
}

/// Classify one path change without reading any file content.
#[must_use]
pub fn classify_change(change: &PathChange) -> ChangeVerdict<'_> {
    let (kind, from, to) = match change {
        PathChange::Added(path) => (ChangeKind::Added, path, path),
        PathChange::Modified(path) => (ChangeKind::Modified, path, path),
        PathChange::Deleted(path) => (ChangeKind::Deleted, path, path),
        PathChange::Renamed { from, to } => (ChangeKind::Renamed, from, to),
        PathChange::Copied { from, to } => (ChangeKind::Copied, from, to),
        PathChange::Other(path) => (ChangeKind::Other, path, path),
    };

    match decide(kind, path_class(from), path_class(to)) {
        Decision::Exempt => ChangeVerdict::Exempt,
        Decision::NeedsAppendCheck => ChangeVerdict::NeedsAppendCheck(to),
        Decision::NonExempt => ChangeVerdict::NonExempt,
    }
}

/// True when every change is exempt.
///
/// `append_only` decides each [`ChangeVerdict::NeedsAppendCheck`] path.
///
/// Composition argument (single step, per the verification notes): this is
/// `all` over the per-change verdicts, so "exempt" holds iff no change is
/// non-exempt. Staleness is its negation, so one non-exempt change anywhere
/// in the range, including one path of a mixed commit, makes the approval
/// stale. No invariant beyond the per-change classification is needed, so
/// the proof obligation reduces to [`classify_change`] and the path
/// predicates, which the Kani harnesses below cover.
pub fn changes_are_exempt(changes: &[PathChange], append_only: impl FnMut(&str) -> bool) -> bool {
    verdicts_pass(changes.iter().map(classify_change), append_only)
}

/// True when every verdict passes; the aggregation step of the rule.
fn verdicts_pass<'a>(
    mut verdicts: impl Iterator<Item = ChangeVerdict<'a>>,
    mut append_only: impl FnMut(&str) -> bool,
) -> bool {
    verdicts.all(|verdict| match verdict {
        ChangeVerdict::Exempt => true,
        ChangeVerdict::NeedsAppendCheck(path) => append_only(path),
        ChangeVerdict::NonExempt => false,
    })
}

/// True when the tree change from `from` to `to` touches only exempt paths.
///
/// Any backend failure, or a backend that cannot list changes, returns
/// false, so the caller reports the approval as stale.
pub fn range_is_exempt(scm: &dyn ScmRepo, from: &str, to: &str) -> bool {
    let Ok(Some(changes)) = scm.path_changes_between(from, to) else {
        return false;
    };

    changes_are_exempt(&changes, |path| {
        match (scm.show_file(from, path), scm.show_file(to, path)) {
            (Ok(old), Ok(new)) => is_append_only(old.as_bytes(), new.as_bytes()),
            _ => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exempt_paths() {
        for path in [
            ".seal/reviews/cr-1/events.jsonl",
            ".seal/version",
            ".seal/.gitignore",
            ".seal/x",
        ] {
            assert!(is_exempt_path(path), "{path} should be exempt");
        }
    }

    #[test]
    fn non_exempt_paths() {
        for path in [
            ".seal",
            ".seal/",
            ".sealignore",
            ".seal-foo/x",
            ".seal.d/x",
            "x/.seal/y",
            "src/main.rs",
            ".seal//x",
            ".seal/x/",
            ".seal/./x",
            ".seal/../src/main.rs",
            ".seal/reviews/..",
            "",
            "/.seal/x",
        ] {
            assert!(!is_exempt_path(path), "{path} should not be exempt");
        }
    }

    #[test]
    fn review_log_paths() {
        assert!(is_review_log_path(".seal/reviews/cr-1/events.jsonl"));
        assert!(is_review_log_path(".seal/events.jsonl"));
        assert!(!is_review_log_path("events.jsonl"));
        assert!(!is_review_log_path("src/events.jsonl"));
        assert!(!is_review_log_path(".seal/reviews/cr-1/events.jsonl.bak"));
        assert!(!is_review_log_path(".seal/reviews/cr-1/xevents.jsonl"));
    }

    #[test]
    fn classify_changes() {
        let log = ".seal/reviews/cr-1/events.jsonl".to_string();
        let src = "src/lib.rs".to_string();
        let data = ".seal/version".to_string();

        assert_eq!(
            classify_change(&PathChange::Modified(log.clone())),
            ChangeVerdict::NeedsAppendCheck(&log)
        );
        assert_eq!(
            classify_change(&PathChange::Added(log.clone())),
            ChangeVerdict::Exempt
        );
        assert_eq!(
            classify_change(&PathChange::Deleted(log.clone())),
            ChangeVerdict::NonExempt
        );
        assert_eq!(
            classify_change(&PathChange::Deleted(data.clone())),
            ChangeVerdict::Exempt
        );
        assert_eq!(
            classify_change(&PathChange::Renamed {
                from: src.clone(),
                to: data.clone()
            }),
            ChangeVerdict::NonExempt
        );
        assert_eq!(
            classify_change(&PathChange::Renamed {
                from: data.clone(),
                to: src
            }),
            ChangeVerdict::NonExempt
        );
        assert_eq!(
            classify_change(&PathChange::Renamed {
                from: log,
                to: ".seal/reviews/cr-2/events.jsonl".to_string()
            }),
            ChangeVerdict::NonExempt
        );
        assert_eq!(
            classify_change(&PathChange::Other(data)),
            ChangeVerdict::NonExempt
        );
        assert_eq!(
            classify_change(&PathChange::Modified(
                ".seal/reviews/cr-*/events.jsonl".to_string()
            )),
            ChangeVerdict::NonExempt
        );
    }

    #[test]
    fn append_only() {
        assert!(is_append_only(b"", b""));
        assert!(is_append_only(b"a\n", b"a\nb\n"));
        assert!(is_append_only(b"a\n", b"a\n"));
        assert!(!is_append_only(b"a\nb\n", b"a\n"));
        assert!(!is_append_only(b"a\n", b"x\nb\n"));
    }

    #[test]
    fn mixed_changes_are_not_exempt() {
        let changes = vec![
            PathChange::Modified(".seal/reviews/cr-1/events.jsonl".to_string()),
            PathChange::Modified("src/lib.rs".to_string()),
        ];
        assert!(!changes_are_exempt(&changes, |_| true));
        assert!(changes_are_exempt(&changes[..1], |_| true));
        assert!(!changes_are_exempt(&changes[..1], |_| false));
        assert!(changes_are_exempt(&[], |_| false));
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Longest path the byte-level harnesses explore. Ten bytes fit
    /// `.seal/` plus a four-byte tail, enough for `..`, `.`, empty and
    /// multi-component tails such as `a/..` and `./a`.
    const MAX_LEN: usize = 10;

    /// Independent, index-based statement of the exempt-path spec.
    fn spec_exempt(p: &[u8]) -> bool {
        let prefix = b".seal/";
        if p.len() <= prefix.len() {
            return false;
        }
        let mut i = 0;
        while i < prefix.len() {
            if p[i] != prefix[i] {
                return false;
            }
            i += 1;
        }
        // Walk the tail, checking each component when its end is reached.
        let mut start = prefix.len();
        let mut j = prefix.len();
        while j <= p.len() {
            if j == p.len() || p[j] == b'/' {
                let len = j - start;
                let is_dot = len == 1 && p[start] == b'.';
                let is_dotdot = len == 2 && p[start] == b'.' && p[start + 1] == b'.';
                if len == 0 || is_dot || is_dotdot {
                    return false;
                }
                start = j + 1;
            }
            j += 1;
        }
        true
    }

    /// Complete for paths up to `MAX_LEN` bytes over the full byte domain:
    /// every byte is `kani::any()`, so `.`, `/`, `s`, `e`, `a`, `l` and all
    /// other values are covered at every position.
    #[kani::proof]
    #[kani::unwind(12)]
    fn exempt_path_matches_spec() {
        let bytes: [u8; MAX_LEN] = kani::any();
        let len: usize = kani::any();
        kani::assume(len <= MAX_LEN);
        let path = &bytes[..len];

        let exempt = is_exempt_path_bytes(path);
        assert_eq!(exempt, spec_exempt(path));

        // Direct consequences, stated separately so a spec slip shows up.
        if exempt {
            assert!(path.len() > 6);
            assert!(path[..5] == *b".seal");
            // A string prefix is not enough: `.sealignore`, `.seal-x`, ...
            assert!(path[5] == b'/');
        }
    }

    /// `.sealignore` and friends never match, for any continuation.
    #[kani::proof]
    #[kani::unwind(12)]
    fn sealignore_is_never_exempt() {
        let tail: [u8; 4] = kani::any();
        let mut bytes = [0u8; 15];
        bytes[..11].copy_from_slice(b".sealignore");
        bytes[11..].copy_from_slice(&tail);
        let len: usize = kani::any();
        kani::assume((11..=15).contains(&len));
        assert!(!is_exempt_path_bytes(&bytes[..len]));
    }

    /// `is_append_only` is exactly "old is a byte prefix of new".
    #[kani::proof]
    #[kani::unwind(6)]
    fn append_only_matches_spec() {
        let old_bytes: [u8; 4] = kani::any();
        let new_bytes: [u8; 4] = kani::any();
        let old_len: usize = kani::any();
        let new_len: usize = kani::any();
        kani::assume(old_len <= 4 && new_len <= 4);
        let old = &old_bytes[..old_len];
        let new = &new_bytes[..new_len];

        let mut spec = old.len() <= new.len();
        let mut i = 0;
        while spec && i < old.len() {
            spec = old[i] == new[i];
            i += 1;
        }
        assert_eq!(is_append_only(old, new), spec);
    }

    fn any_class() -> PathClass {
        match kani::any::<u8>() % 4 {
            0 => PathClass::Outside,
            1 => PathClass::SealData,
            2 => PathClass::ReviewLog,
            _ => PathClass::OpaqueReviewLog,
        }
    }

    fn any_kind() -> ChangeKind {
        match kani::any::<u8>() % 6 {
            0 => ChangeKind::Added,
            1 => ChangeKind::Modified,
            2 => ChangeKind::Deleted,
            3 => ChangeKind::Renamed,
            4 => ChangeKind::Copied,
            _ => ChangeKind::Other,
        }
    }

    /// The per-change rule, over its ENTIRE domain (6 kinds x 4 x 4 classes):
    /// - nothing inside-out: any path `Outside` .seal/ is never exempt;
    /// - a rename or copy needs both sides to be plain seal data;
    /// - deleting a review log is never exempt;
    /// - only modifying a plain review log defers to the append check;
    /// - `Other` (type change, conflict, unknown) is never exempt.
    #[kani::proof]
    fn decide_contract() {
        let kind = any_kind();
        let from = any_class();
        let to = any_class();
        let single = !matches!(kind, ChangeKind::Renamed | ChangeKind::Copied);
        // A single-path change reports the same class on both sides.
        kani::assume(!single || from == to);

        let decision = decide(kind, from, to);
        let is_log = |c: PathClass| matches!(c, PathClass::ReviewLog | PathClass::OpaqueReviewLog);

        if from == PathClass::Outside || to == PathClass::Outside {
            assert_eq!(decision, Decision::NonExempt);
        }
        match kind {
            ChangeKind::Added => {
                assert_eq!(decision == Decision::Exempt, from != PathClass::Outside);
            }
            ChangeKind::Deleted => {
                assert_eq!(decision == Decision::Exempt, from == PathClass::SealData);
                assert!(!is_log(from) || decision == Decision::NonExempt);
            }
            ChangeKind::Modified => match decision {
                Decision::Exempt => assert_eq!(from, PathClass::SealData),
                Decision::NeedsAppendCheck => assert_eq!(from, PathClass::ReviewLog),
                Decision::NonExempt => {
                    assert!(matches!(
                        from,
                        PathClass::Outside | PathClass::OpaqueReviewLog
                    ));
                }
            },
            ChangeKind::Renamed | ChangeKind::Copied => {
                assert_eq!(
                    decision == Decision::Exempt,
                    from == PathClass::SealData && to == PathClass::SealData
                );
                assert!(decision != Decision::NeedsAppendCheck);
            }
            ChangeKind::Other => assert_eq!(decision, Decision::NonExempt),
        }
    }

    fn any_verdict() -> ChangeVerdict<'static> {
        match kani::any::<u8>() % 3 {
            0 => ChangeVerdict::Exempt,
            1 => ChangeVerdict::NeedsAppendCheck("log"),
            _ => ChangeVerdict::NonExempt,
        }
    }

    /// Aggregation over three verdicts with an arbitrary append oracle:
    /// the range passes iff each verdict passes on its own. With the doc
    /// comment on `changes_are_exempt` (it is `all`), this is the
    /// single-step argument for "stale iff some change is non-exempt"; the
    /// three-element bound is illustration, not the argument.
    #[kani::proof]
    #[kani::unwind(5)]
    fn verdicts_pass_iff_each_passes() {
        let verdicts = [any_verdict(), any_verdict(), any_verdict()];
        let answers: [bool; 3] = kani::any();
        let passes = |i: usize| match verdicts[i] {
            ChangeVerdict::Exempt => true,
            ChangeVerdict::NeedsAppendCheck(_) => answers[i],
            ChangeVerdict::NonExempt => false,
        };
        // The oracle answers the k-th query with the answer of the k-th
        // verdict that needs one, in order.
        let mut queue = [0usize; 3];
        let mut queued = 0;
        for (i, verdict) in verdicts.iter().enumerate() {
            if matches!(verdict, ChangeVerdict::NeedsAppendCheck(_)) {
                queue[queued] = i;
                queued += 1;
            }
        }
        let mut asked = 0;
        let result = verdicts_pass(verdicts.into_iter(), |_| {
            let answer = answers[queue[asked]];
            asked += 1;
            answer
        });
        assert_eq!(result, passes(0) && passes(1) && passes(2));
    }
}

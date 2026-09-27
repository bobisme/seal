//! Pure in-memory oracle for the SQL projection reducer (bn-2a4t).
//!
//! The real reducer is `apply_event` in `projection/mod.rs`: one SQL
//! statement (or a few) per event. Nothing can reason about SQL text, so this
//! module restates every `apply_*` handler as plain Rust over `BTreeMap`s.
//! The tests below check the SQL against the model step by step.
//!
//! Every rule below names the SQL handler it mirrors. If a handler changes,
//! the differential test fails until the matching rule here changes too.
//!
//! SQLite facts the model depends on (the connection runs with
//! `PRAGMA foreign_keys = ON`):
//! - `INSERT OR IGNORE` skips a row on a PRIMARY KEY *or CHECK* violation,
//!   but never on a FOREIGN KEY violation, which is an error.
//! - A row skipped by `OR IGNORE` is never inserted, so its foreign key is
//!   never checked.
//! - `UPDATE ... WHERE <guard>` that matches no row is a silent no-op.
//!
//! Sync paths (`filter_orphaned_events_for_repo`) drop events whose review has
//! no `ReviewCreated` before calling `apply_event`. The model mirrors raw
//! `apply_event`, so it keeps the FK errors the filter exists to avoid.

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::events::{CodeSelection, Event, EventEnvelope};

// ============================================================================
// Pure lifecycle step (small enough for Kani)
// ============================================================================

/// `reviews.status`, restricted by the schema CHECK constraint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum ReviewStatus {
    Open,
    Approved,
    Merged,
    Abandoned,
}

impl ReviewStatus {
    pub fn from_sql(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "approved" => Some(Self::Approved),
            "merged" => Some(Self::Merged),
            "abandoned" => Some(Self::Abandoned),
            _ => None,
        }
    }

    /// The guard `status IN ('open', 'approved')` shared by
    /// `apply_review_approved`, `apply_review_merged`,
    /// `apply_review_abandoned` and `apply_review_retargeted`.
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Open | Self::Approved)
    }
}

/// The status-changing review events, stripped of everything except what
/// decides `status` and `approved_commit`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleEvent<C> {
    Approve(Option<C>),
    Merge,
    Abandon,
    /// Only the status half of the retarget guard. The anchor and author
    /// halves are checked by the caller.
    Retarget,
}

/// The part of a review row that the lifecycle step reads and writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lifecycle<C> {
    pub status: ReviewStatus,
    pub approved_commit: Option<C>,
}

/// Outcome of one lifecycle step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepOutcome<C> {
    /// The `UPDATE` matched the row: this is the new lifecycle.
    Updated(Lifecycle<C>),
    /// The status guard matched zero rows: a silent no-op.
    NoOp,
    /// The retarget guard matched zero rows: `apply_review_retargeted` bails.
    RetargetRejected,
}

/// One status transition, exactly as the four guarded `UPDATE reviews`
/// statements in `projection/mod.rs` perform it.
pub fn lifecycle_step<C: Clone>(s: &Lifecycle<C>, ev: &LifecycleEvent<C>) -> StepOutcome<C> {
    if !s.status.is_active() {
        return match ev {
            LifecycleEvent::Retarget => StepOutcome::RetargetRejected,
            _ => StepOutcome::NoOp,
        };
    }
    match ev {
        // apply_review_approved: SET status='approved', approved_commit=?
        // (NULL overwrites an earlier commit).
        LifecycleEvent::Approve(c) => StepOutcome::Updated(Lifecycle {
            status: ReviewStatus::Approved,
            approved_commit: c.clone(),
        }),
        // apply_review_merged / apply_review_abandoned leave approved_commit.
        LifecycleEvent::Merge => StepOutcome::Updated(Lifecycle {
            status: ReviewStatus::Merged,
            approved_commit: s.approved_commit.clone(),
        }),
        LifecycleEvent::Abandon => StepOutcome::Updated(Lifecycle {
            status: ReviewStatus::Abandoned,
            approved_commit: s.approved_commit.clone(),
        }),
        // apply_review_retargeted: SET status='open', approved_commit=NULL.
        LifecycleEvent::Retarget => StepOutcome::Updated(Lifecycle {
            status: ReviewStatus::Open,
            approved_commit: None,
        }),
    }
}

// ============================================================================
// Rows
// ============================================================================

/// One `reviews` row, keyed by `review_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewRow {
    pub jj_change_id: String,
    pub scm_kind: String,
    pub scm_anchor: Option<String>,
    pub initial_commit: String,
    pub base_commit: Option<String>,
    pub approved_commit: Option<String>,
    pub final_commit: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub author: String,
    pub created_at: String,
    pub status: ReviewStatus,
    pub status_changed_at: Option<String>,
    pub status_changed_by: Option<String>,
    pub abandon_reason: Option<String>,
}

/// One `review_reviewers` row, keyed by `(review_id, reviewer)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewerRow {
    pub requested_at: String,
    pub requested_by: String,
}

/// One `reviewer_votes` row, keyed by `(review_id, reviewer)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoteRow {
    pub vote: String,
    pub reason: Option<String>,
    pub voted_at: String,
    pub voted_commit: Option<String>,
}

/// One `threads` row, keyed by `thread_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadRow {
    pub review_id: String,
    pub file_path: String,
    pub selection_type: String,
    pub selection_start: i64,
    pub selection_end: Option<i64>,
    pub commit_hash: String,
    pub author: String,
    pub created_at: String,
    pub open: bool,
    pub status_changed_at: Option<String>,
    pub status_changed_by: Option<String>,
    pub resolve_reason: Option<String>,
    pub reopen_reason: Option<String>,
    pub next_comment_number: i64,
}

/// One `comments` row, keyed by `comment_id`. Display order is derived from
/// `created_at`, so the map carries all ordering information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentRow {
    pub thread_id: String,
    pub body: String,
    pub author: String,
    pub created_at: String,
}

/// Why the SQL reducer returns `Err` for an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelError {
    /// `apply_review_retargeted`: old and new target commit are equal.
    RetargetNoChange,
    /// `apply_review_retargeted`: `updated != 1`.
    RetargetMismatch,
    /// A child row references a missing parent (`foreign_keys = ON`).
    ForeignKey,
}

/// The whole projection, one map per table. Equality is snapshot equality.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectionModel {
    pub reviews: BTreeMap<String, ReviewRow>,
    pub reviewers: BTreeMap<(String, String), ReviewerRow>,
    pub votes: BTreeMap<(String, String), VoteRow>,
    pub threads: BTreeMap<String, ThreadRow>,
    pub comments: BTreeMap<String, CommentRow>,
}

/// Every row that belongs to one review. Frame-condition unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewSlice {
    pub review: Option<ReviewRow>,
    pub reviewers: Vec<((String, String), ReviewerRow)>,
    pub votes: Vec<((String, String), VoteRow)>,
    pub threads: Vec<(String, ThreadRow)>,
    pub comments: Vec<(String, CommentRow)>,
}

impl ProjectionModel {
    /// Mirror of `apply_event_inner`.
    pub fn apply(&mut self, env: &EventEnvelope) -> Result<(), ModelError> {
        let ts = env.ts.to_rfc3339();
        let author = env.author.as_str();
        match &env.event {
            // apply_review_created: INSERT OR IGNORE. A duplicate review_id is
            // ignored; so is a scm_kind outside CHECK (scm_kind IN ('jj','git')),
            // because OR IGNORE also covers CHECK violations.
            Event::ReviewCreated(e) => {
                let kind = e.scm_kind.as_deref().unwrap_or("jj");
                if self.reviews.contains_key(&e.review_id) || !matches!(kind, "jj" | "git") {
                    return Ok(());
                }
                self.reviews.insert(
                    e.review_id.clone(),
                    ReviewRow {
                        jj_change_id: e.jj_change_id.clone(),
                        scm_kind: kind.to_string(),
                        scm_anchor: Some(
                            e.scm_anchor
                                .clone()
                                .unwrap_or_else(|| e.jj_change_id.clone()),
                        ),
                        initial_commit: e.initial_commit.clone(),
                        base_commit: e.base_commit.clone(),
                        approved_commit: None,
                        final_commit: None,
                        title: e.title.clone(),
                        description: e.description.clone(),
                        author: author.to_string(),
                        created_at: ts,
                        status: ReviewStatus::Open,
                        status_changed_at: None,
                        status_changed_by: None,
                        abandon_reason: None,
                    },
                );
                Ok(())
            }
            // apply_review_retargeted: bail if the commits are equal; otherwise a
            // guarded UPDATE (review_id, scm_anchor = old, author = envelope
            // author, status IN ('open','approved')) that must match exactly one
            // row, then DELETE every vote of the review.
            Event::ReviewRetargeted(e) => {
                if e.old_target_commit == e.new_target_commit {
                    return Err(ModelError::RetargetNoChange);
                }
                let row = self
                    .reviews
                    .get_mut(&e.review_id)
                    .ok_or(ModelError::RetargetMismatch)?;
                if row.scm_anchor.as_deref() != Some(e.old_scm_anchor.as_str())
                    || row.author != author
                {
                    return Err(ModelError::RetargetMismatch);
                }
                match lifecycle_step(&row.lifecycle(), &LifecycleEvent::Retarget) {
                    StepOutcome::Updated(next) => {
                        row.set_lifecycle(next);
                        row.scm_anchor = Some(e.new_scm_anchor.clone());
                        row.status_changed_at = Some(ts);
                        row.status_changed_by = Some(author.to_string());
                    }
                    StepOutcome::NoOp | StepOutcome::RetargetRejected => {
                        return Err(ModelError::RetargetMismatch);
                    }
                }
                self.votes.retain(|(rid, _), _| rid != &e.review_id);
                Ok(())
            }
            // apply_reviewers_requested: one upsert per reviewer, no status
            // guard. The FK to reviews fails on the first reviewer when the review
            // is missing; an empty list therefore succeeds even then.
            Event::ReviewersRequested(e) => {
                for reviewer in &e.reviewers {
                    if !self.reviews.contains_key(&e.review_id) {
                        return Err(ModelError::ForeignKey);
                    }
                    self.reviewers.insert(
                        (e.review_id.clone(), reviewer.clone()),
                        ReviewerRow {
                            requested_at: ts.clone(),
                            requested_by: author.to_string(),
                        },
                    );
                }
                Ok(())
            }
            // apply_reviewer_voted: upsert keyed by (review, envelope author),
            // no status guard (votes on merged reviews are recorded). FK error
            // for a missing review.
            Event::ReviewerVoted(e) => {
                if !self.reviews.contains_key(&e.review_id) {
                    return Err(ModelError::ForeignKey);
                }
                self.votes.insert(
                    (e.review_id.clone(), author.to_string()),
                    VoteRow {
                        vote: e.vote.to_string(),
                        reason: e.reason.clone(),
                        voted_at: ts,
                        voted_commit: e.target_commit.clone(),
                    },
                );
                Ok(())
            }
            // apply_review_approved: guarded UPDATE; missing review or inactive
            // status is a silent no-op.
            Event::ReviewApproved(e) => {
                self.lifecycle_update(
                    &e.review_id,
                    &LifecycleEvent::Approve(e.target_commit.clone()),
                    &ts,
                    author,
                    |_| {},
                );
                Ok(())
            }
            // apply_review_merged: guarded UPDATE, also sets final_commit.
            Event::ReviewMerged(e) => {
                self.lifecycle_update(&e.review_id, &LifecycleEvent::Merge, &ts, author, |row| {
                    row.final_commit = Some(e.final_commit.clone());
                });
                Ok(())
            }
            // apply_review_abandoned: guarded UPDATE, also sets abandon_reason
            // (NULL overwrites).
            Event::ReviewAbandoned(e) => {
                self.lifecycle_update(&e.review_id, &LifecycleEvent::Abandon, &ts, author, |row| {
                    row.abandon_reason.clone_from(&e.reason);
                });
                Ok(())
            }
            // apply_thread_created: INSERT OR IGNORE. An existing thread_id wins
            // (even if the event names another, or a missing, review). A new
            // thread on a missing review is an FK error.
            Event::ThreadCreated(e) => {
                if self.threads.contains_key(&e.thread_id) {
                    return Ok(());
                }
                if !self.reviews.contains_key(&e.review_id) {
                    return Err(ModelError::ForeignKey);
                }
                let (selection_type, start, end) = match &e.selection {
                    CodeSelection::Line { line } => ("line", *line, None),
                    CodeSelection::Range { start, end } => ("range", *start, Some(*end)),
                };
                self.threads.insert(
                    e.thread_id.clone(),
                    ThreadRow {
                        review_id: e.review_id.clone(),
                        file_path: e.file_path.clone(),
                        selection_type: selection_type.to_string(),
                        selection_start: i64::from(start),
                        selection_end: end.map(i64::from),
                        commit_hash: e.commit_hash.clone(),
                        author: author.to_string(),
                        created_at: ts,
                        open: true,
                        status_changed_at: None,
                        status_changed_by: None,
                        resolve_reason: None,
                        reopen_reason: None,
                        next_comment_number: 1,
                    },
                );
                Ok(())
            }
            // apply_thread_resolved: UPDATE ... WHERE status = 'open'. No review
            // status guard; missing thread is a no-op.
            Event::ThreadResolved(e) => {
                if let Some(t) = self.threads.get_mut(&e.thread_id).filter(|t| t.open) {
                    t.open = false;
                    t.status_changed_at = Some(ts);
                    t.status_changed_by = Some(author.to_string());
                    t.resolve_reason.clone_from(&e.reason);
                }
                Ok(())
            }
            // apply_thread_reopened: UPDATE ... WHERE status = 'resolved'.
            Event::ThreadReopened(e) => {
                if let Some(t) = self.threads.get_mut(&e.thread_id).filter(|t| !t.open) {
                    t.open = true;
                    t.status_changed_at = Some(ts);
                    t.status_changed_by = Some(author.to_string());
                    t.reopen_reason.clone_from(&e.reason);
                }
                Ok(())
            }
            // apply_comment_added: INSERT OR IGNORE (existing comment_id wins,
            // whatever thread it names); FK error for a new comment on a missing
            // thread; next_comment_number += 1 only when a row was inserted.
            Event::CommentAdded(e) => {
                if self.comments.contains_key(&e.comment_id) {
                    return Ok(());
                }
                let thread = self
                    .threads
                    .get_mut(&e.thread_id)
                    .ok_or(ModelError::ForeignKey)?;
                thread.next_comment_number += 1;
                self.comments.insert(
                    e.comment_id.clone(),
                    CommentRow {
                        thread_id: e.thread_id.clone(),
                        body: e.body.clone(),
                        author: author.to_string(),
                        created_at: ts,
                    },
                );
                Ok(())
            }
        }
    }

    /// Shared shape of the three guarded, non-failing status UPDATEs.
    fn lifecycle_update(
        &mut self,
        review_id: &str,
        ev: &LifecycleEvent<String>,
        ts: &str,
        author: &str,
        extra: impl FnOnce(&mut ReviewRow),
    ) {
        let Some(row) = self.reviews.get_mut(review_id) else {
            return;
        };
        if let StepOutcome::Updated(next) = lifecycle_step(&row.lifecycle(), ev) {
            row.set_lifecycle(next);
            row.status_changed_at = Some(ts.to_string());
            row.status_changed_by = Some(author.to_string());
            extra(row);
        }
    }

    /// The review an event can touch, given the state *before* it applies.
    ///
    /// Events that carry a `review_id` name it directly. Thread-only events
    /// resolve through the projected thread (its `ThreadCreated`); an unknown
    /// thread maps to `None`, meaning the event can touch no review.
    pub fn event_review(&self, event: &Event) -> Option<String> {
        match event {
            Event::ReviewCreated(e) => Some(e.review_id.clone()),
            Event::ReviewRetargeted(e) => Some(e.review_id.clone()),
            Event::ReviewersRequested(e) => Some(e.review_id.clone()),
            Event::ReviewerVoted(e) => Some(e.review_id.clone()),
            Event::ReviewApproved(e) => Some(e.review_id.clone()),
            Event::ReviewMerged(e) => Some(e.review_id.clone()),
            Event::ReviewAbandoned(e) => Some(e.review_id.clone()),
            Event::ThreadCreated(e) => Some(e.review_id.clone()),
            Event::ThreadResolved(e) => self.thread_review(&e.thread_id),
            Event::ThreadReopened(e) => self.thread_review(&e.thread_id),
            Event::CommentAdded(e) => self.thread_review(&e.thread_id),
        }
    }

    fn thread_review(&self, thread_id: &str) -> Option<String> {
        self.threads.get(thread_id).map(|t| t.review_id.clone())
    }

    /// All rows owned by `review_id`.
    pub fn slice(&self, review_id: &str) -> ReviewSlice {
        let threads: Vec<(String, ThreadRow)> = self
            .threads
            .iter()
            .filter(|(_, t)| t.review_id == review_id)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let comments = self
            .comments
            .iter()
            .filter(|(_, c)| threads.iter().any(|(tid, _)| tid == &c.thread_id))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        ReviewSlice {
            review: self.reviews.get(review_id).cloned(),
            reviewers: self
                .reviewers
                .iter()
                .filter(|((rid, _), _)| rid == review_id)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            votes: self
                .votes
                .iter()
                .filter(|((rid, _), _)| rid == review_id)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            threads,
            comments,
        }
    }

    /// Drop every row owned by `review_id`: the targeted reset that bn-2h0a
    /// proposes for `accept_regression`.
    pub fn reset_review(&mut self, review_id: &str) {
        let threads: Vec<String> = self
            .threads
            .iter()
            .filter(|(_, t)| t.review_id == review_id)
            .map(|(k, _)| k.clone())
            .collect();
        self.comments.retain(|_, c| !threads.contains(&c.thread_id));
        self.threads.retain(|_, t| t.review_id != review_id);
        self.votes.retain(|(rid, _), _| rid != review_id);
        self.reviewers.retain(|(rid, _), _| rid != review_id);
        self.reviews.remove(review_id);
    }

    /// Read the SQL projection back into model form.
    pub fn from_sql(conn: &Connection) -> rusqlite::Result<Self> {
        let mut m = Self::default();

        let mut stmt = conn.prepare(
            "SELECT review_id, jj_change_id, scm_kind, scm_anchor, initial_commit, base_commit,
                    approved_commit, final_commit, title, description, author, created_at,
                    status, status_changed_at, status_changed_by, abandon_reason
             FROM reviews",
        )?;
        let rows = stmt.query_map([], |r| {
            let status: String = r.get(12)?;
            Ok((
                r.get::<_, String>(0)?,
                ReviewRow {
                    jj_change_id: r.get(1)?,
                    scm_kind: r.get(2)?,
                    scm_anchor: r.get(3)?,
                    initial_commit: r.get(4)?,
                    base_commit: r.get(5)?,
                    approved_commit: r.get(6)?,
                    final_commit: r.get(7)?,
                    title: r.get(8)?,
                    description: r.get(9)?,
                    author: r.get(10)?,
                    created_at: r.get(11)?,
                    status: ReviewStatus::from_sql(&status).ok_or_else(|| {
                        rusqlite::Error::InvalidColumnType(
                            12,
                            "status".to_string(),
                            rusqlite::types::Type::Text,
                        )
                    })?,
                    status_changed_at: r.get(13)?,
                    status_changed_by: r.get(14)?,
                    abandon_reason: r.get(15)?,
                },
            ))
        })?;
        for row in rows {
            let (k, v) = row?;
            m.reviews.insert(k, v);
        }

        let mut stmt = conn.prepare(
            "SELECT review_id, reviewer, requested_at, requested_by FROM review_reviewers",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                ReviewerRow {
                    requested_at: r.get(2)?,
                    requested_by: r.get(3)?,
                },
            ))
        })?;
        for row in rows {
            let (k, v) = row?;
            m.reviewers.insert(k, v);
        }

        let mut stmt = conn.prepare(
            "SELECT review_id, reviewer, vote, reason, voted_at, voted_commit FROM reviewer_votes",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                VoteRow {
                    vote: r.get(2)?,
                    reason: r.get(3)?,
                    voted_at: r.get(4)?,
                    voted_commit: r.get(5)?,
                },
            ))
        })?;
        for row in rows {
            let (k, v) = row?;
            m.votes.insert(k, v);
        }

        let mut stmt = conn.prepare(
            "SELECT thread_id, review_id, file_path, selection_type, selection_start,
                    selection_end, commit_hash, author, created_at, status,
                    status_changed_at, status_changed_by, resolve_reason, reopen_reason,
                    next_comment_number
             FROM threads",
        )?;
        let rows = stmt.query_map([], |r| {
            let status: String = r.get(9)?;
            Ok((
                r.get::<_, String>(0)?,
                ThreadRow {
                    review_id: r.get(1)?,
                    file_path: r.get(2)?,
                    selection_type: r.get(3)?,
                    selection_start: r.get(4)?,
                    selection_end: r.get(5)?,
                    commit_hash: r.get(6)?,
                    author: r.get(7)?,
                    created_at: r.get(8)?,
                    open: status == "open",
                    status_changed_at: r.get(10)?,
                    status_changed_by: r.get(11)?,
                    resolve_reason: r.get(12)?,
                    reopen_reason: r.get(13)?,
                    next_comment_number: r.get(14)?,
                },
            ))
        })?;
        for row in rows {
            let (k, v) = row?;
            m.threads.insert(k, v);
        }

        let mut stmt =
            conn.prepare("SELECT comment_id, thread_id, body, author, created_at FROM comments")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                CommentRow {
                    thread_id: r.get(1)?,
                    body: r.get(2)?,
                    author: r.get(3)?,
                    created_at: r.get(4)?,
                },
            ))
        })?;
        for row in rows {
            let (k, v) = row?;
            m.comments.insert(k, v);
        }

        Ok(m)
    }
}

impl ReviewRow {
    fn lifecycle(&self) -> Lifecycle<String> {
        Lifecycle {
            status: self.status,
            approved_commit: self.approved_commit.clone(),
        }
    }

    fn set_lifecycle(&mut self, l: Lifecycle<String>) {
        self.status = l.status;
        self.approved_commit = l.approved_commit;
    }
}

// ============================================================================
// Kani: single-step lifecycle facts
// ============================================================================
//
// Both properties are per-step (skill Q2): the claim over any event sequence
// follows by induction with no invariant beyond the step itself.
// - Absorbing: if every step maps a terminal status to itself, a terminal
//   status is never left, whatever follows.
// - Approved commit: the step proves "result is Approved => this event was
//   Approve(c) and approved_commit == c". No other event yields Approved, so
//   over a sequence, `status == approved` implies approved_commit is the commit
//   of the latest effective ReviewApproved.
#[cfg(kani)]
mod proofs {
    use super::{lifecycle_step, Lifecycle, LifecycleEvent, ReviewStatus, StepOutcome};

    fn any_event() -> LifecycleEvent<u8> {
        match kani::any::<u8>() % 4 {
            0 => LifecycleEvent::Approve(kani::any()),
            1 => LifecycleEvent::Merge,
            2 => LifecycleEvent::Abandon,
            _ => LifecycleEvent::Retarget,
        }
    }

    #[kani::proof]
    fn merged_and_abandoned_are_absorbing() {
        let s = Lifecycle::<u8> {
            status: kani::any(),
            approved_commit: kani::any(),
        };
        kani::assume(matches!(
            s.status,
            ReviewStatus::Merged | ReviewStatus::Abandoned
        ));
        let ev = any_event();
        match lifecycle_step(&s, &ev) {
            StepOutcome::Updated(_) => panic!("terminal review was updated"),
            StepOutcome::NoOp => assert!(!matches!(ev, LifecycleEvent::Retarget)),
            StepOutcome::RetargetRejected => assert!(matches!(ev, LifecycleEvent::Retarget)),
        }
    }

    #[kani::proof]
    fn approved_implies_commit_of_this_approve() {
        let s = Lifecycle::<u8> {
            status: kani::any(),
            approved_commit: kani::any(),
        };
        let ev = any_event();
        if let StepOutcome::Updated(next) = lifecycle_step(&s, &ev) {
            if next.status == ReviewStatus::Approved {
                match ev {
                    LifecycleEvent::Approve(c) => assert_eq!(next.approved_commit, c),
                    _ => panic!("non-approve event produced Approved"),
                }
            }
        }
    }

    #[kani::proof]
    fn active_approve_with_commit_sets_it() {
        let s = Lifecycle::<u8> {
            status: kani::any(),
            approved_commit: kani::any(),
        };
        kani::assume(s.status.is_active());
        let c: u8 = kani::any();
        let out = lifecycle_step(&s, &LifecycleEvent::Approve(Some(c)));
        assert_eq!(
            out,
            StepOutcome::Updated(Lifecycle {
                status: ReviewStatus::Approved,
                approved_commit: Some(c),
            })
        );
    }
}

// ============================================================================
// Differential and property tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{
        CommentAdded, ReviewAbandoned, ReviewApproved, ReviewCreated, ReviewMerged,
        ReviewRetargeted, ReviewerVoted, ReviewersRequested, ThreadCreated, ThreadReopened,
        ThreadResolved, VoteType,
    };
    use crate::log::{open_or_create_review, AppendLog};
    use crate::projection::{apply_event, rebuild_review_from_log, ProjectionDb};
    use chrono::{DateTime, TimeZone, Utc};
    use proptest::prelude::*;

    const REVIEWS: [&str; 3] = ["r0", "r1", "r2"];
    const THREADS: [&str; 3] = ["t0", "t1", "t2"];
    const AUTHORS: [&str; 3] = ["alice", "bob", "carol"];
    const ANCHORS: [&str; 2] = ["a0", "a1"];
    const COMMITS: [&str; 3] = ["c0", "c1", "c2"];

    /// Small-index description of one event. Indices shrink toward 0, so a
    /// minimized case uses `r0`, `alice`, `a0`, `c0` wherever it can.
    #[derive(Clone, Debug)]
    enum Op {
        Create {
            review: usize,
            author: usize,
            anchor: Option<usize>,
            change: usize,
            /// 0: None (defaults to jj), 1: "git", 2: "jj", 3: "hg" (violates CHECK).
            kind: usize,
            base: Option<usize>,
        },
        Retarget {
            review: usize,
            author: usize,
            old_anchor: usize,
            new_anchor: usize,
            old_commit: usize,
            new_commit: usize,
        },
        Request {
            review: usize,
            author: usize,
            reviewers: Vec<usize>,
        },
        Vote {
            review: usize,
            voter: usize,
            lgtm: bool,
            commit: Option<usize>,
            reason: Option<usize>,
        },
        Approve {
            review: usize,
            author: usize,
            commit: Option<usize>,
        },
        Merge {
            review: usize,
            author: usize,
            commit: usize,
        },
        Abandon {
            review: usize,
            author: usize,
            reason: Option<usize>,
        },
        ThreadCreate {
            thread: usize,
            review: usize,
            author: usize,
            range: bool,
            line: u32,
        },
        Resolve {
            thread: usize,
            author: usize,
            reason: Option<usize>,
        },
        Reopen {
            thread: usize,
            author: usize,
            reason: Option<usize>,
        },
        Comment {
            comment: usize,
            thread: usize,
            author: usize,
        },
    }

    fn ts(step: usize) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + step as i64, 0)
            .single()
            .expect("valid timestamp")
    }

    fn s(v: &str) -> String {
        v.to_string()
    }

    impl Op {
        fn envelope(&self, step: usize) -> EventEnvelope {
            let (author, event) = match self {
                Self::Create {
                    review,
                    author,
                    anchor,
                    change,
                    kind,
                    base,
                } => (
                    *author,
                    Event::ReviewCreated(ReviewCreated {
                        review_id: s(REVIEWS[*review]),
                        jj_change_id: s(ANCHORS[*change]),
                        scm_kind: [None, Some("git"), Some("jj"), Some("hg")][*kind].map(s),
                        scm_anchor: anchor.map(|i| s(ANCHORS[i])),
                        initial_commit: s("c0"),
                        base_commit: base.map(|i| s(COMMITS[i])),
                        title: format!("title {review}"),
                        description: None,
                    }),
                ),
                Self::Retarget {
                    review,
                    author,
                    old_anchor,
                    new_anchor,
                    old_commit,
                    new_commit,
                } => (
                    *author,
                    Event::ReviewRetargeted(ReviewRetargeted {
                        review_id: s(REVIEWS[*review]),
                        old_scm_anchor: s(ANCHORS[*old_anchor]),
                        new_scm_anchor: s(ANCHORS[*new_anchor]),
                        old_target_commit: s(COMMITS[*old_commit]),
                        new_target_commit: s(COMMITS[*new_commit]),
                    }),
                ),
                Self::Request {
                    review,
                    author,
                    reviewers,
                } => (
                    *author,
                    Event::ReviewersRequested(ReviewersRequested {
                        review_id: s(REVIEWS[*review]),
                        reviewers: reviewers.iter().map(|i| s(AUTHORS[*i])).collect(),
                    }),
                ),
                Self::Vote {
                    review,
                    voter,
                    lgtm,
                    commit,
                    reason,
                } => (
                    *voter,
                    Event::ReviewerVoted(ReviewerVoted {
                        review_id: s(REVIEWS[*review]),
                        vote: if *lgtm {
                            VoteType::Lgtm
                        } else {
                            VoteType::Block
                        },
                        reason: reason.map(|i| format!("why{i}")),
                        target_commit: commit.map(|i| s(COMMITS[i])),
                    }),
                ),
                Self::Approve {
                    review,
                    author,
                    commit,
                } => (
                    *author,
                    Event::ReviewApproved(ReviewApproved {
                        review_id: s(REVIEWS[*review]),
                        target_commit: commit.map(|i| s(COMMITS[i])),
                    }),
                ),
                Self::Merge {
                    review,
                    author,
                    commit,
                } => (
                    *author,
                    Event::ReviewMerged(ReviewMerged {
                        review_id: s(REVIEWS[*review]),
                        final_commit: s(COMMITS[*commit]),
                    }),
                ),
                Self::Abandon {
                    review,
                    author,
                    reason,
                } => (
                    *author,
                    Event::ReviewAbandoned(ReviewAbandoned {
                        review_id: s(REVIEWS[*review]),
                        reason: reason.map(|i| format!("why{i}")),
                    }),
                ),
                Self::ThreadCreate {
                    thread,
                    review,
                    author,
                    range,
                    line,
                } => (
                    *author,
                    Event::ThreadCreated(ThreadCreated {
                        thread_id: s(THREADS[*thread]),
                        review_id: s(REVIEWS[*review]),
                        file_path: s("src/lib.rs"),
                        selection: if *range {
                            CodeSelection::range(*line, line + 2)
                        } else {
                            CodeSelection::line(*line)
                        },
                        commit_hash: s("c0"),
                    }),
                ),
                Self::Resolve {
                    thread,
                    author,
                    reason,
                } => (
                    *author,
                    Event::ThreadResolved(ThreadResolved {
                        thread_id: s(THREADS[*thread]),
                        reason: reason.map(|i| format!("why{i}")),
                    }),
                ),
                Self::Reopen {
                    thread,
                    author,
                    reason,
                } => (
                    *author,
                    Event::ThreadReopened(ThreadReopened {
                        thread_id: s(THREADS[*thread]),
                        reason: reason.map(|i| format!("why{i}")),
                    }),
                ),
                Self::Comment {
                    comment,
                    thread,
                    author,
                } => (
                    *author,
                    Event::CommentAdded(CommentAdded {
                        comment_id: format!("m{comment}"),
                        thread_id: s(THREADS[*thread]),
                        body: format!("body {comment}"),
                    }),
                ),
            };
            EventEnvelope {
                ts: ts(step),
                author: s(AUTHORS[author]),
                event,
            }
        }
    }

    fn op_strategy(with_retarget: bool) -> BoxedStrategy<Op> {
        let r = 0..REVIEWS.len();
        let a = 0..AUTHORS.len();
        let an = 0..ANCHORS.len();
        let c = 0..COMMITS.len();
        let t = 0..THREADS.len();
        // Review authors come from the first two agents only, so a retarget
        // (which must be issued by the review author) is valid more often.
        let owner = 0..2usize;
        let reason = proptest::option::of(0..2usize);
        let create = (
            r.clone(),
            owner.clone(),
            proptest::option::of(an.clone()),
            an.clone(),
            0..4usize,
            proptest::option::of(c.clone()),
        )
            .prop_map(|(review, author, anchor, change, kind, base)| Op::Create {
                review,
                author,
                anchor,
                change,
                kind,
                base,
            });
        let retarget = (r.clone(), owner, an.clone(), an, c.clone(), c.clone()).prop_map(
            |(review, author, old_anchor, new_anchor, old_commit, new_commit)| Op::Retarget {
                review,
                author,
                old_anchor,
                new_anchor,
                old_commit,
                new_commit,
            },
        );
        let request = (
            r.clone(),
            a.clone(),
            proptest::collection::vec(a.clone(), 0..3),
        )
            .prop_map(|(review, author, reviewers)| Op::Request {
                review,
                author,
                reviewers,
            });
        let vote = (
            r.clone(),
            a.clone(),
            any::<bool>(),
            proptest::option::of(c.clone()),
            reason.clone(),
        )
            .prop_map(|(review, voter, lgtm, commit, reason)| Op::Vote {
                review,
                voter,
                lgtm,
                commit,
                reason,
            });
        let approve = (r.clone(), a.clone(), proptest::option::of(c.clone())).prop_map(
            |(review, author, commit)| Op::Approve {
                review,
                author,
                commit,
            },
        );
        let merge = (r.clone(), a.clone(), c).prop_map(|(review, author, commit)| Op::Merge {
            review,
            author,
            commit,
        });
        let abandon =
            (r.clone(), a.clone(), reason.clone()).prop_map(|(review, author, reason)| {
                Op::Abandon {
                    review,
                    author,
                    reason,
                }
            });
        let thread = (t.clone(), r, a.clone(), any::<bool>(), 1..5u32).prop_map(
            |(thread, review, author, range, line)| Op::ThreadCreate {
                thread,
                review,
                author,
                range,
                line,
            },
        );
        let resolve =
            (t.clone(), a.clone(), reason.clone()).prop_map(|(thread, author, reason)| {
                Op::Resolve {
                    thread,
                    author,
                    reason,
                }
            });
        let reopen =
            (t.clone(), a.clone(), reason).prop_map(|(thread, author, reason)| Op::Reopen {
                thread,
                author,
                reason,
            });
        let comment = (0..4usize, t, a).prop_map(|(comment, thread, author)| Op::Comment {
            comment,
            thread,
            author,
        });
        let mut arms: Vec<(u32, BoxedStrategy<Op>)> = vec![
            (4, create.boxed()),
            (2, request.boxed()),
            (4, vote.boxed()),
            (3, approve.boxed()),
            (1, merge.boxed()),
            (1, abandon.boxed()),
            (3, thread.boxed()),
            (2, resolve.boxed()),
            (2, reopen.boxed()),
            (3, comment.boxed()),
        ];
        if with_retarget {
            arms.push((3, retarget.boxed()));
        }
        proptest::strategy::Union::new_weighted(arms).boxed()
    }

    fn ops(with_retarget: bool) -> impl Strategy<Value = Vec<Op>> {
        proptest::collection::vec(op_strategy(with_retarget), 0..=30)
    }

    fn fresh_db() -> ProjectionDb {
        let db = ProjectionDb::open_in_memory().expect("open db");
        db.init_schema().expect("schema");
        db
    }

    fn snapshot(db: &ProjectionDb) -> ProjectionModel {
        ProjectionModel::from_sql(db.conn()).expect("read snapshot")
    }

    /// Apply `env` to SQL and model, and check they agree. Returns whether the
    /// event applied successfully.
    fn step_both(
        db: &ProjectionDb,
        model: &mut ProjectionModel,
        env: &EventEnvelope,
    ) -> Result<bool, TestCaseError> {
        let sql = apply_event(db, env);
        let m = model.apply(env);
        prop_assert_eq!(
            sql.is_ok(),
            m.is_ok(),
            "outcome differs for {:?}: sql={:?} model={:?}",
            env.event,
            sql.as_ref().err().map(ToString::to_string),
            m
        );
        if let (Err(sql_err), Err(model_err)) = (&sql, &m) {
            let msg = format!("{sql_err:#}");
            let is_retarget = matches!(
                model_err,
                ModelError::RetargetNoChange | ModelError::RetargetMismatch
            );
            prop_assert_eq!(
                is_retarget,
                msg.contains("ReviewRetargeted"),
                "error class differs: sql={} model={:?}",
                msg,
                model_err
            );
        }
        prop_assert_eq!(
            &snapshot(db),
            &*model,
            "state differs after {:?}",
            env.event
        );
        Ok(sql.is_ok())
    }

    fn run_differential(ops: &[Op]) -> Result<(), TestCaseError> {
        let db = fresh_db();
        let mut model = ProjectionModel::default();
        for (i, op) in ops.iter().enumerate() {
            step_both(&db, &mut model, &op.envelope(i))?;
        }
        Ok(())
    }

    /// All review ids that can appear, so the frame check also covers reviews
    /// that do not exist yet.
    fn all_reviews() -> impl Iterator<Item = &'static str> {
        REVIEWS.iter().copied()
    }

    /// Frame condition for one step, on one state representation.
    fn check_frame(
        before: &ProjectionModel,
        after: &ProjectionModel,
        touched: Option<&str>,
        what: &str,
    ) -> Result<(), TestCaseError> {
        for r in all_reviews().filter(|r| Some(*r) != touched) {
            prop_assert_eq!(
                before.slice(r),
                after.slice(r),
                "{}: event for {:?} changed review {}",
                what,
                touched,
                r
            );
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// SQL projection == pure model after every step, and both agree on
        /// success/failure.
        #[test]
        fn differential_sql_matches_model(ops in ops(true)) {
            run_differential(&ops)?;
        }

        /// Frame condition: an event whose review is R leaves every other
        /// review's rows unchanged, on the model and on SQL.
        ///
        /// This is a per-step property (skill Q2): the claim "a sequence of
        /// events for R never changes review S" follows by induction from the
        /// single step, with no extra invariant. The generator only supplies
        /// reachable pre-states for that step.
        #[test]
        fn frame_condition_other_reviews_unchanged(ops in ops(true)) {
            let db = fresh_db();
            let mut model = ProjectionModel::default();
            for (i, op) in ops.iter().enumerate() {
                let env = op.envelope(i);
                let touched = model.event_review(&env.event);
                let model_before = model.clone();
                let sql_before = snapshot(&db);
                let _ = model.apply(&env);
                let _ = apply_event(&db, &env);
                check_frame(&model_before, &model, touched.as_deref(), "model")?;
                check_frame(&sql_before, &snapshot(&db), touched.as_deref(), "sql")?;
            }
        }

        /// Replay idempotence for every event kind except ReviewRetargeted:
        /// re-applying a review's (effective-only) log on top of its projected
        /// state changes nothing and raises no error.
        #[test]
        fn replay_idempotent_without_retarget(ops in ops(false), pick in 0..REVIEWS.len()) {
            check_replay_on_top(&ops, REVIEWS[pick])?;
        }

        /// Oracle for the bn-2h0a fix, on the model: reset one review's rows,
        /// replay its log, and the whole state (every review) is unchanged.
        #[test]
        fn model_reset_then_replay_restores_state(ops in ops(true), pick in 0..REVIEWS.len()) {
            let (model, log) = project_model(&ops);
            let review = REVIEWS[pick];
            let mut rebuilt = model.clone();
            rebuilt.reset_review(review);
            for env in log.iter().filter(|(r, _)| r.as_deref() == Some(review)).map(|(_, e)| e) {
                prop_assert!(rebuilt.apply(env).is_ok(), "replay failed at {:?}", env.event);
            }
            prop_assert_eq!(rebuilt, model);
        }

        /// SQL `rebuild_review_from_log` (bn-2h0a) on a temp seal root gives
        /// the same snapshot as the model's reset + replay of that review's log.
        #[test]
        fn sql_rebuild_review_matches_model_reset(ops in ops(true), pick in 0..REVIEWS.len()) {
            let review = REVIEWS[pick];
            let (model, log) = project_model(&ops);
            prop_assume!(model.reviews.contains_key(review));
            let db = fresh_db();
            let dir = tempfile::tempdir().expect("tempdir");
            let review_log = open_or_create_review(dir.path(), review).expect("review log");
            for (owner, env) in &log {
                apply_event(&db, env).expect("logged event applies");
                if owner.as_deref() == Some(review) {
                    review_log.append(env).expect("append");
                }
            }
            rebuild_review_from_log(&db, dir.path(), review).expect("rebuild");
            let mut expected = model;
            expected.reset_review(review);
            for env in log.iter().filter(|(r, _)| r.as_deref() == Some(review)).map(|(_, e)| e) {
                prop_assert!(expected.apply(env).is_ok(), "model replay failed at {:?}", env.event);
            }
            prop_assert_eq!(snapshot(&db), expected);
        }
    }

    /// The per-review log an event would be written to. A client writes to a
    /// review's log only once the review exists, so events generated for a
    /// review before its `ReviewCreated` are not part of any log.
    fn log_owner(model: &ProjectionModel, event: &Event) -> Option<String> {
        model
            .event_review(event)
            .filter(|r| matches!(event, Event::ReviewCreated(_)) || model.reviews.contains_key(r))
    }

    /// Project `ops` on the model; return the state and the log of events
    /// that applied, each tagged with the review it belongs to (per-review log
    /// file). Failed events never reach a log, so they are left out.
    fn project_model(ops: &[Op]) -> (ProjectionModel, Vec<(Option<String>, EventEnvelope)>) {
        let mut model = ProjectionModel::default();
        let mut log = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            let env = op.envelope(i);
            let review = log_owner(&model, &env.event);
            if model.apply(&env).is_ok() {
                log.push((review, env));
            }
        }
        (model, log)
    }

    /// Replay `review`'s log on top of the projection, without deleting its
    /// rows first. Checks SQL and model: no error, state unchanged. Only
    /// holds without ReviewRetargeted; see `replay_on_top_rejects_retarget`.
    ///
    /// The log keeps only events that changed the state. The CLI refuses to
    /// write a guarded no-op (e.g. `ThreadsService::reopen` on an open thread),
    /// so a single-writer log has none. With a no-op in the log, replay on top
    /// is not idempotent: see `replay_on_top_noop_reopen_changes_only_reasons`.
    fn check_replay_on_top(ops: &[Op], review: &str) -> Result<(), TestCaseError> {
        let db = fresh_db();
        let mut model = ProjectionModel::default();
        let mut log = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            let env = op.envelope(i);
            let r = log_owner(&model, &env.event);
            let before = model.clone();
            if step_both(&db, &mut model, &env)? && model != before {
                log.push((r, env));
            }
        }
        let before = model.clone();
        for env in log
            .iter()
            .filter(|(r, _)| r.as_deref() == Some(review))
            .map(|(_, e)| e)
        {
            let ok = step_both(&db, &mut model, env)?;
            prop_assert!(ok, "replay of {:?} failed", env.event);
        }
        prop_assert_eq!(&model, &before, "replay changed state");
        Ok(())
    }

    // ------------------------------------------------------------------------
    // Named regressions
    // ------------------------------------------------------------------------

    fn create(review: usize, author: usize, anchor: usize) -> Op {
        Op::Create {
            review,
            author,
            anchor: Some(anchor),
            change: 0,
            kind: 0,
            base: None,
        }
    }

    /// Project `ops` on a fresh db; return the db and the logged envelopes.
    fn project_sql(ops: &[Op]) -> (ProjectionDb, Vec<EventEnvelope>) {
        let db = fresh_db();
        let log: Vec<EventEnvelope> = ops
            .iter()
            .enumerate()
            .map(|(i, op)| op.envelope(i))
            .collect();
        for env in &log {
            apply_event(&db, env).expect("first projection applies");
        }
        (db, log)
    }

    /// Replaying a log on top of its own projection is not idempotent:
    /// a replayed ReviewRetargeted fails because its guard
    /// `scm_anchor = <old anchor>` no longer matches (the anchor already
    /// moved). This is why `rebuild_review_from_log` deletes the review's rows
    /// before replaying (bn-2h0a). Minimized from the replay proptest.
    #[test]
    fn replay_on_top_rejects_retarget() {
        let (db, log) = project_sql(&[
            create(0, 0, 0),
            Op::Retarget {
                review: 0,
                author: 0,
                old_anchor: 0,
                new_anchor: 1,
                old_commit: 0,
                new_commit: 1,
            },
        ]);
        let before = snapshot(&db);
        apply_event(&db, &log[0]).expect("duplicate ReviewCreated is ignored");
        let err = apply_event(&db, &log[1]).expect_err("replayed retarget must fail");
        assert!(format!("{err:#}").contains("ReviewRetargeted"), "{err:#}");
        assert_eq!(snapshot(&db), before);
    }

    /// Replay on top is also not idempotent when the log holds a guarded no-op
    /// thread event. Reopen of an open thread is a no-op the first time; on
    /// replay the thread is resolved, so the reopen fires and sets
    /// `reopen_reason`, which the following resolve does not clear. Only the
    /// reason columns differ. The CLI refuses such writes, but concurrent
    /// writers can still produce them. Another reason `rebuild_review_from_log`
    /// deletes before replaying (bn-2h0a).
    #[test]
    fn replay_on_top_noop_reopen_changes_only_reasons() {
        let (db, log) = project_sql(&[
            create(0, 0, 0),
            Op::ThreadCreate {
                thread: 0,
                review: 0,
                author: 0,
                range: false,
                line: 1,
            },
            Op::Reopen {
                thread: 0,
                author: 0,
                reason: Some(0),
            },
            Op::Resolve {
                thread: 0,
                author: 0,
                reason: None,
            },
        ]);
        let before = snapshot(&db);
        for env in &log {
            apply_event(&db, env).expect("replay applies");
        }
        let after = snapshot(&db);
        assert_ne!(after, before);
        let mut expected = before;
        let thread = expected.threads.get_mut("t0").expect("thread t0");
        thread.reopen_reason = Some(s("why0"));
        assert_eq!(after, expected);
    }

    /// Pins SQLite behavior the model relies on: `INSERT OR IGNORE` swallows a
    /// CHECK violation on `scm_kind`, so an unknown kind silently drops the
    /// review instead of failing.
    #[test]
    fn review_created_with_bad_scm_kind_is_ignored() {
        run_differential(&[Op::Create {
            review: 0,
            author: 0,
            anchor: None,
            change: 0,
            kind: 3,
            base: None,
        }])
        .expect("model and SQL agree");
    }

    /// Pins: a duplicate thread_id pointing at a missing review is ignored,
    /// not an FK error (the ignored row is never FK-checked).
    #[test]
    fn duplicate_thread_on_missing_review_is_ignored() {
        run_differential(&[
            create(0, 0, 0),
            Op::ThreadCreate {
                thread: 0,
                review: 0,
                author: 0,
                range: false,
                line: 1,
            },
            Op::ThreadCreate {
                thread: 0,
                review: 1,
                author: 0,
                range: false,
                line: 1,
            },
        ])
        .expect("model and SQL agree");
    }

    /// Pins: an empty ReviewersRequested for a missing review succeeds (the
    /// FK is only checked per inserted reviewer).
    #[test]
    fn empty_reviewers_request_on_missing_review_succeeds() {
        run_differential(&[Op::Request {
            review: 0,
            author: 0,
            reviewers: vec![],
        }])
        .expect("model and SQL agree");
    }
}

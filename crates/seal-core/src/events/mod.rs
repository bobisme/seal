//! Event types for the botseal event log.
//!
//! All events share a common envelope structure and are serialized as JSON Lines.

pub mod identity;
pub mod ids;

pub use identity::get_agent_identity;
pub use ids::{is_review_id, make_comment_id, new_review_id, new_thread_id};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Common envelope for all events in the event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Timestamp when the event was created
    pub ts: DateTime<Utc>,
    /// Agent/user who created this event
    pub author: String,
    /// The event payload
    #[serde(flatten)]
    pub event: Event,
}

/// All possible events in the botseal system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum Event {
    /// A new review was created
    ReviewCreated(ReviewCreated),
    /// A review target was moved to new code
    ReviewRetargeted(ReviewRetargeted),
    /// Reviewers were requested for a review
    ReviewersRequested(ReviewersRequested),
    /// A reviewer voted on a review (LGTM or block)
    ReviewerVoted(ReviewerVoted),
    /// A review was approved
    ReviewApproved(ReviewApproved),
    /// A review was merged
    ReviewMerged(ReviewMerged),
    /// A review was abandoned
    ReviewAbandoned(ReviewAbandoned),
    /// A new comment thread was created
    ThreadCreated(ThreadCreated),
    /// A comment was added to a thread
    CommentAdded(CommentAdded),
    /// A thread was resolved
    ThreadResolved(ThreadResolved),
    /// A thread was reopened
    ThreadReopened(ThreadReopened),
}

// ============================================================================
// Review Events
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewCreated {
    /// Unique review identifier (e.g., "cr-1d3")
    pub review_id: String,
    /// jj change ID (stable across rebases)
    pub jj_change_id: String,
    /// SCM kind for backend-neutral review anchors ("jj" | "git")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scm_kind: Option<String>,
    /// Backend-neutral anchor (jj change id or git ref-like anchor)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scm_anchor: Option<String>,
    /// Commit hash at review creation
    pub initial_commit: String,
    /// Base commit of the review range (`base_commit..target`).
    ///
    /// Absent on reviews created before ranges were supported. Readers then
    /// fall back to the parent of the target commit, which covers one commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// Review title
    pub title: String,
    /// Optional description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Audit record for moving an existing review to a new SCM target.
///
/// The old and new anchors explain how each target was selected. The resolved
/// commits make the transition stable even when a named SCM anchor later moves.
///
/// When the branch was rebased onto a newer trunk, the retarget also moves the
/// exclusive range base, so the review keeps covering only the branch's own
/// commits. `new_base_commit` is then the base the projection records;
/// `old_base_commit` is the persisted base it replaced (absent for legacy
/// reviews that had none). Both are absent when the base did not change, so
/// events written before base moves existed parse and serialize unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRetargeted {
    pub review_id: String,
    pub old_scm_anchor: String,
    pub new_scm_anchor: String,
    pub old_target_commit: String,
    pub new_target_commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_base_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_base_commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewersRequested {
    pub review_id: String,
    pub reviewers: Vec<String>,
}

/// Vote type for reviewer decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoteType {
    /// Looks Good To Me - approval
    Lgtm,
    /// Request changes - blocks merge
    Block,
}

impl std::fmt::Display for VoteType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lgtm => write!(f, "lgtm"),
            Self::Block => write!(f, "block"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewerVoted {
    pub review_id: String,
    pub vote: VoteType,
    /// Optional reason (typically used for blocks)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Target commit the vote applied to.
    ///
    /// Absent on votes cast before this was recorded. Readers then cannot
    /// tell whether the vote still covers the current code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewApproved {
    pub review_id: String,
    /// Target commit the approval applied to.
    ///
    /// Absent on approvals recorded before this was tracked. An approval
    /// that names no commit cannot be checked against later commits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewMerged {
    pub review_id: String,
    /// Final commit hash after merge
    pub final_commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewAbandoned {
    pub review_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ============================================================================
// Thread Events
// ============================================================================

/// Represents a code selection (line or range).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CodeSelection {
    /// Single line
    Line { line: u32 },
    /// Inclusive line range
    Range { start: u32, end: u32 },
}

impl CodeSelection {
    #[must_use]
    pub const fn line(n: u32) -> Self {
        Self::Line { line: n }
    }

    #[must_use]
    pub const fn range(start: u32, end: u32) -> Self {
        Self::Range { start, end }
    }

    /// Get the start line of the selection.
    #[must_use]
    pub const fn start_line(&self) -> u32 {
        match self {
            Self::Line { line } => *line,
            Self::Range { start, .. } => *start,
        }
    }

    /// Get the end line of the selection (same as start for single line).
    #[must_use]
    pub const fn end_line(&self) -> u32 {
        match self {
            Self::Line { line } => *line,
            Self::Range { end, .. } => *end,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadCreated {
    /// Unique thread identifier (e.g., "th-99a")
    pub thread_id: String,
    /// Parent review
    pub review_id: String,
    /// File path the thread is anchored to
    pub file_path: String,
    /// Line selection
    pub selection: CodeSelection,
    /// Commit hash where the selection was made
    pub commit_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadResolved {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadReopened {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ============================================================================
// Comment Events
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommentAdded {
    /// Comment identifier as thread child (e.g., "th-abc.1")
    pub comment_id: String,
    /// Parent thread
    pub thread_id: String,
    /// Comment body
    pub body: String,
}

// ============================================================================
// Constructors and helpers
// ============================================================================

impl EventEnvelope {
    /// Create a new event envelope with the current timestamp.
    pub fn new(author: impl Into<String>, event: Event) -> Self {
        Self {
            ts: Utc::now(),
            author: author.into(),
            event,
        }
    }

    /// Serialize the envelope to a JSON line (no trailing newline).
    pub fn to_json_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Parse an envelope from a JSON line.
    pub fn from_json_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_roundtrip() {
        let event = EventEnvelope::new(
            "test_agent",
            Event::ReviewCreated(ReviewCreated {
                review_id: "cr-abc".to_string(),
                jj_change_id: "abc123".to_string(),
                scm_kind: Some("jj".to_string()),
                scm_anchor: Some("abc123".to_string()),
                initial_commit: "def456".to_string(),
                base_commit: None,
                title: "Test Review".to_string(),
                description: Some("A test".to_string()),
            }),
        );

        let json = event.to_json_line().unwrap();
        let parsed = EventEnvelope::from_json_line(&json).unwrap();

        assert_eq!(parsed.author, "test_agent");
        match parsed.event {
            Event::ReviewCreated(r) => {
                assert_eq!(r.review_id, "cr-abc");
                assert_eq!(r.title, "Test Review");
            }
            _ => panic!("Expected ReviewCreated"),
        }
    }

    #[test]
    fn test_review_retargeted_roundtrip() {
        let event = EventEnvelope::new(
            "review-author",
            Event::ReviewRetargeted(ReviewRetargeted {
                review_id: "cr-abc1".to_string(),
                old_scm_anchor: "detached:abc123".to_string(),
                new_scm_anchor: "detached:def456".to_string(),
                old_target_commit: "abc123".to_string(),
                new_target_commit: "def456".to_string(),
                old_base_commit: None,
                new_base_commit: None,
            }),
        );

        let json = event.to_json_line().unwrap();
        // A retarget without a base move serializes exactly as before the
        // base fields existed.
        assert!(!json.contains("base_commit"), "{json}");
        let parsed = EventEnvelope::from_json_line(&json).unwrap();

        match parsed.event {
            Event::ReviewRetargeted(retargeted) => {
                assert_eq!(retargeted.review_id, "cr-abc1");
                assert_eq!(retargeted.old_scm_anchor, "detached:abc123");
                assert_eq!(retargeted.new_scm_anchor, "detached:def456");
                assert_eq!(retargeted.old_target_commit, "abc123");
                assert_eq!(retargeted.new_target_commit, "def456");
                assert_eq!(retargeted.old_base_commit, None);
                assert_eq!(retargeted.new_base_commit, None);
            }
            _ => panic!("Expected ReviewRetargeted"),
        }
    }

    #[test]
    fn test_review_retargeted_base_move_roundtrip() {
        let event = EventEnvelope::new(
            "review-author",
            Event::ReviewRetargeted(ReviewRetargeted {
                review_id: "cr-abc1".to_string(),
                old_scm_anchor: "detached:abc123".to_string(),
                new_scm_anchor: "detached:def456".to_string(),
                old_target_commit: "abc123".to_string(),
                new_target_commit: "def456".to_string(),
                old_base_commit: Some("b0".to_string()),
                new_base_commit: Some("b1".to_string()),
            }),
        );
        let json = event.to_json_line().unwrap();
        let parsed = EventEnvelope::from_json_line(&json).unwrap();
        match parsed.event {
            Event::ReviewRetargeted(retargeted) => {
                assert_eq!(retargeted.old_base_commit.as_deref(), Some("b0"));
                assert_eq!(retargeted.new_base_commit.as_deref(), Some("b1"));
            }
            _ => panic!("Expected ReviewRetargeted"),
        }
    }

    #[test]
    fn test_code_selection_line() {
        let sel = CodeSelection::line(42);
        assert_eq!(sel.start_line(), 42);
        assert_eq!(sel.end_line(), 42);
    }

    #[test]
    fn test_code_selection_range() {
        let sel = CodeSelection::range(10, 20);
        assert_eq!(sel.start_line(), 10);
        assert_eq!(sel.end_line(), 20);
    }

    #[test]
    fn test_thread_created_serialization() {
        let event = Event::ThreadCreated(ThreadCreated {
            thread_id: "th-123".to_string(),
            review_id: "cr-abc".to_string(),
            file_path: "src/main.rs".to_string(),
            selection: CodeSelection::range(10, 15),
            commit_hash: "abc123".to_string(),
        });

        let envelope = EventEnvelope::new("agent", event);
        let json = envelope.to_json_line().unwrap();

        // Ensure it contains expected fields
        assert!(json.contains("ThreadCreated"));
        assert!(json.contains("th-123"));
        assert!(json.contains("Range"));
    }
}

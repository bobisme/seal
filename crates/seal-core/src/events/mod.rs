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
    /// Review title
    pub title: String,
    /// Optional description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewApproved {
    pub review_id: String,
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

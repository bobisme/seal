//! Inbox service — get inbox summary for an agent.

use crate::projection::{
    InboxSummary, OpenThreadOnMyReview, ProjectionDb, ReviewAwaitingVote, ThreadWithNewResponses,
};

use super::{CoreError, CoreResult};

/// Service for inbox operations.
pub struct InboxService<'a> {
    db: &'a ProjectionDb,
}

impl<'a> InboxService<'a> {
    pub(crate) const fn new(db: &'a ProjectionDb) -> Self {
        Self { db }
    }

    /// Get complete inbox summary for an agent.
    pub fn get(&self, agent: &str) -> CoreResult<InboxSummary> {
        self.db.get_inbox(agent).map_err(CoreError::Internal)
    }

    /// Get reviews awaiting the agent's vote.
    pub fn reviews_awaiting_vote(&self, agent: &str) -> CoreResult<Vec<ReviewAwaitingVote>> {
        self.db
            .get_reviews_awaiting_vote(agent)
            .map_err(CoreError::Internal)
    }

    /// Get threads with new responses since the agent's last comment.
    pub fn threads_with_responses(&self, agent: &str) -> CoreResult<Vec<ThreadWithNewResponses>> {
        self.db
            .get_threads_with_new_responses(agent)
            .map_err(CoreError::Internal)
    }

    /// Get open threads on reviews the agent authored.
    pub fn open_threads_on_my_reviews(&self, agent: &str) -> CoreResult<Vec<OpenThreadOnMyReview>> {
        self.db
            .get_open_threads_on_my_reviews(agent)
            .map_err(CoreError::Internal)
    }
}

//! Sync service — sync projection, rebuild, accept regression.

use crate::projection::{
    rebuild_from_review_logs, sync_from_review_logs, ProjectionDb, SyncReport,
};

use super::{CoreContext, CoreError, CoreResult};

/// Service for sync operations.
pub struct SyncService<'a> {
    ctx: &'a CoreContext,
    db: &'a ProjectionDb,
}

impl<'a> SyncService<'a> {
    pub(crate) const fn new(ctx: &'a CoreContext, db: &'a ProjectionDb) -> Self {
        Self { ctx, db }
    }

    /// Sync the projection from event logs (incremental).
    ///
    /// Only processes new/changed review log files since the last sync.
    pub fn sync(&self) -> CoreResult<SyncReport> {
        sync_from_review_logs(self.db, self.ctx.seal_root()).map_err(CoreError::Internal)
    }

    /// Full destructive rebuild of the projection database.
    ///
    /// Drops all projected data and replays all events from scratch.
    /// Returns the number of events that were replayed during the rebuild phase.
    /// After rebuild, a sync is performed to re-populate file state tracking.
    pub fn rebuild(&self) -> CoreResult<RebuildResult> {
        let events_rebuilt =
            rebuild_from_review_logs(self.db, self.ctx.seal_root()).map_err(CoreError::Internal)?;
        let sync_report =
            sync_from_review_logs(self.db, self.ctx.seal_root()).map_err(CoreError::Internal)?;

        Ok(RebuildResult {
            events_rebuilt,
            sync_report,
        })
    }

    /// Accept a regression for a single review by re-baselining its file state.
    ///
    /// Deletes the stored file state for the review, then re-syncs so the file
    /// is treated as "new" and its current content becomes the baseline.
    pub fn accept_regression(&self, review_id: &str) -> CoreResult<SyncReport> {
        self.db
            .delete_review_file_state(review_id)
            .map_err(CoreError::Internal)?;
        sync_from_review_logs(self.db, self.ctx.seal_root()).map_err(CoreError::Internal)
    }
}

/// Result of a full rebuild operation.
#[derive(Debug)]
pub struct RebuildResult {
    /// Number of events replayed during the rebuild phase.
    pub events_rebuilt: usize,
    /// Sync report from the post-rebuild incremental sync.
    pub sync_report: SyncReport,
}

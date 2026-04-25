//! Service layer for seal-core.
//!
//! Provides typed, high-level APIs for review, thread, comment, inbox, and sync
//! operations. The service layer encapsulates projection database management and
//! event log appends behind a clean interface.
//!
//! # Usage
//!
//! ```no_run
//! use std::path::Path;
//! use seal_core::core::{CoreContext, SealServices};
//!
//! let ctx = CoreContext::new(
//!     Path::new("/repo"),
//!     Path::new("/repo/.seal/index.db"),
//! ).unwrap();
//!
//! let services = ctx.services().unwrap();
//! let reviews = services.reviews().list(None, None).unwrap();
//! ```

pub mod comments;
pub mod errors;
pub mod inbox;
pub mod reviews;
pub mod sync;
pub mod threads;

pub use errors::{CoreError, CoreResult};

use std::path::{Path, PathBuf};

use crate::projection::{sync_from_review_logs, ProjectionDb};
use crate::version::require_v2;

/// Context for seal-core services.
///
/// Holds the paths needed to locate event logs and the projection database.
/// Create one per operation or hold for the duration of a session.
#[derive(Debug, Clone)]
pub struct CoreContext {
    /// Path to the repository root (parent of `.seal/`).
    seal_root: PathBuf,
    /// Path to the projection database file.
    db_path: PathBuf,
}

impl CoreContext {
    /// Create a new core context.
    ///
    /// Validates that the seal repository is initialized and uses v2 format.
    ///
    /// # Arguments
    /// * `seal_root` - Path to the repository root (parent of `.seal/`)
    /// * `db_path` - Path to the `SQLite` projection database
    pub fn new(seal_root: &Path, db_path: &Path) -> CoreResult<Self> {
        let seal_dir = seal_root.join(".seal");
        if !seal_dir.exists() {
            return Err(CoreError::NotInitialized {
                path: seal_root.display().to_string(),
            });
        }

        require_v2(seal_root).map_err(|_| CoreError::V1NeedsMigration)?;

        Ok(Self {
            seal_root: seal_root.to_path_buf(),
            db_path: db_path.to_path_buf(),
        })
    }

    /// Path to the repository root.
    #[must_use]
    pub fn seal_root(&self) -> &Path {
        &self.seal_root
    }

    /// Path to the projection database.
    #[must_use]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Open the projection database, initialize its schema, and sync from event logs.
    ///
    /// This is the standard way to get a ready-to-query projection.
    pub fn open_and_sync(&self) -> CoreResult<ProjectionDb> {
        let db = ProjectionDb::open(&self.db_path).map_err(CoreError::Internal)?;
        db.init_schema().map_err(CoreError::Internal)?;
        sync_from_review_logs(&db, &self.seal_root).map_err(CoreError::Internal)?;
        Ok(db)
    }

    /// Create a `SealServices` instance backed by this context.
    ///
    /// Opens and syncs the projection database.
    pub fn services(&self) -> CoreResult<SealServices> {
        let db = self.open_and_sync()?;
        Ok(SealServices {
            ctx: self.clone(),
            db,
        })
    }
}

/// Facade providing all seal service APIs.
///
/// Owns a synced projection database and provides access to domain-specific
/// service objects for reviews, threads, comments, inbox, and sync.
pub struct SealServices {
    ctx: CoreContext,
    db: ProjectionDb,
}

impl SealServices {
    /// Access review operations.
    #[must_use]
    pub const fn reviews(&self) -> reviews::ReviewService<'_> {
        reviews::ReviewService::new(&self.ctx, &self.db)
    }

    /// Access thread operations.
    #[must_use]
    pub const fn threads(&self) -> threads::ThreadService<'_> {
        threads::ThreadService::new(&self.ctx, &self.db)
    }

    /// Access comment operations.
    #[must_use]
    pub const fn comments(&self) -> comments::CommentService<'_> {
        comments::CommentService::new(&self.ctx, &self.db)
    }

    /// Access inbox operations.
    #[must_use]
    pub const fn inbox(&self) -> inbox::InboxService<'_> {
        inbox::InboxService::new(&self.db)
    }

    /// Access sync operations.
    #[must_use]
    pub const fn sync(&self) -> sync::SyncService<'_> {
        sync::SyncService::new(&self.ctx, &self.db)
    }

    /// Get a reference to the underlying projection database.
    ///
    /// Useful for advanced queries not covered by the service layer.
    #[must_use]
    pub const fn db(&self) -> &ProjectionDb {
        &self.db
    }

    /// Get a reference to the core context.
    #[must_use]
    pub const fn context(&self) -> &CoreContext {
        &self.ctx
    }
}

//! Hybrid session storage: append-only JSONL rollouts, SQLite (`WAL` + `FTS5`) indexing,
//! single-writer session leases, retained output paging, and atomic workspace file snapshots/revert.

pub mod index;
pub mod rollout;
pub mod snapshots;

use std::path::PathBuf;

use thiserror::Error;

pub use index::{
    HistorySearchHit, RetainedOutputPage, SessionSummary, StateIndex, StoredPermissionGrant,
    UsageGroupBy, UsageRecordRow, UsageRow,
};
pub use rollout::{RolloutHeader, RolloutLog, RolloutRecord, SessionLease, rollout_path};
pub use snapshots::{FilePreImage, RevertOutcome, SnapshotManifest, WorkspaceSnapshotStore};

/// Errors produced by `tekvra-storage`.
#[derive(Debug, Error)]
pub enum StorageError {
    /// Filesystem I/O failure.
    #[error("storage I/O failed on `{path}`: {source}")]
    Io {
        /// Affected path.
        path: PathBuf,
        /// OS error.
        #[source]
        source: std::io::Error,
    },
    /// Another writer holds the session lease.
    #[error("session `{session_id}` is currently locked by another active writer")]
    SessionLocked {
        /// Locked session id.
        session_id: String,
    },
    /// Session rollout does not exist.
    #[error("session `{session_id}` not found")]
    SessionNotFound {
        /// Missing session id.
        session_id: String,
    },
    /// Referenced turn id does not exist in the rollout.
    #[error("turn `{turn_id}` not found in session rollout")]
    TurnNotFound {
        /// Missing turn id.
        turn_id: String,
    },
    /// Retained raw tool output id was not found.
    #[error("retained tool output `{output_id}` not found")]
    OutputNotFound {
        /// Missing output id.
        output_id: String,
    },
    /// Corrupt mid-file rollout record.
    #[error("corrupt rollout log `{path}`: {message}")]
    CorruptRollout {
        /// Rollout path.
        path: PathBuf,
        /// Parse error message.
        message: String,
    },
    /// Snapshot blob failed SHA-256 verification.
    #[error("snapshot blob failed SHA-256 check (expected {expected}, got {actual})")]
    SnapshotIntegrity {
        /// Expected digest.
        expected: String,
        /// Computed digest.
        actual: String,
    },
    /// Target path escapes the workspace root.
    #[error("snapshot path `{path}` escapes workspace root")]
    PathOutsideWorkspace {
        /// Rejected path.
        path: PathBuf,
    },
    /// Invalid regex or search query.
    #[error("invalid storage query: {message}")]
    InvalidQuery {
        /// Error details.
        message: String,
    },
    /// SQLite error.
    #[error("sqlite index error: {message}")]
    Sqlite {
        /// Error details.
        message: String,
    },
    /// JSON serialization/deserialization error.
    #[error("json serialization error: {message}")]
    Json {
        /// Error details.
        message: String,
    },
}

//! Append-only `Frame` engine, `SessionHandle` actor loop (`Next` / `Guide` / `Replace`),
//! Caudra-style automatic steering nudges, durable steerable `task` subagents,
//! tool-less `/goal` transcript evaluator, and read-only `/review` workflow.

pub mod context;
pub mod frame;
pub mod goal_and_review;
pub mod session;
pub mod steering;
pub mod subagents;

use thiserror::Error;

pub use frame::{CompactionConfig, Frame, FrameSnapshot};
pub use goal_and_review::{
    GoalVerdict, ReviewOutcome, ReviewTarget, collect_review_diff, evaluate_goal, run_code_review,
};
pub use session::{
    PluginToolExecutor, SessionBootstrap, SessionHandle, SessionRuntimeState, spawn_session,
};
pub use steering::{
    SteeringAction, SteeringDetector, ToolFailureTracker, detect_abandoned_turn_intent,
};
pub use subagents::{
    BackgroundSubagentNotice, SubagentEntry, SubagentManager, SubagentManagerDeps,
    SubagentSpawnRequest,
};

/// Errors returned by `tekvra-core`.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Append-only conversation prefix invariant was violated.
    #[error("append-only frame invariant violated at index {index}: {detail}")]
    FramePrefixMutated {
        /// Message index or 0 for developer header.
        index: usize,
        /// Explanation of mutation.
        detail: String,
    },
    /// Provider or model routing failure.
    #[error("provider error: {0}")]
    Provider(#[from] tekvra_providers::ProviderError),
    /// Rollout, snapshot, or SQLite index failure.
    #[error("storage error: {0}")]
    Storage(#[from] tekvra_storage::StorageError),
    /// Native tool or permission error.
    #[error("tool error: {0}")]
    Tools(#[from] tekvra_tools::ToolsError),
    /// Session actor channel closed.
    #[error("session `{session_id}` actor has stopped")]
    SessionClosed {
        /// Stopped session id.
        session_id: String,
    },
    /// Git or diff collection error during `/review`.
    #[error("review error: {message}")]
    Review {
        /// Explanation.
        message: String,
    },
    /// Subagent management error.
    #[error("subagent error: {message}")]
    Subagent {
        /// Explanation.
        message: String,
    },
}

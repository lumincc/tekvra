//! Full-screen Alternate-Screen `ratatui` TUI (`tekvra-tui`) with permissive-only `syntect`
//! syntax highlighting, streaming `pulldown-cmark` Markdown & diff rendering, collapsible
//! native `ToolOutput` cards with Raw/Filtered toggling, scope-ladder approval modal,
//! `/tasks` (`Ctrl-X`) subagent view switcher & live steerer, `/search` picker, and revert preview.

pub mod app;
pub mod cards;
pub mod highlight;
pub mod markdown;
pub mod modals;

use thiserror::Error;

pub use app::{
    ActiveOverlay, BoxReviewFuture, PluginCommandHandler, SwitchSessionHandler,
    SwitchSessionOutcome, TranscriptEntry, TuiApp, TuiCommand, TuiRuntimeHooks, run_fullscreen_tui,
};
pub use cards::{ToolCardState, render_tool_card};
pub use highlight::{CodeHighlighter, highlight_unified_diff};
pub use markdown::render_markdown_lines;
pub use modals::{
    ApprovalModalState, CompletionCandidate, CompletionKind, CompletionPopupState,
    RevertOverlayState, RevertPreviewItem, SearchOverlayState, SessionsOverlayState,
    TasksOverlayState, build_completion_popup, is_excluded_mention_path,
};

/// Errors produced by `tekvra-tui`.
#[derive(Debug, Error)]
pub enum TuiError {
    /// Terminal I/O error.
    #[error("terminal I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Core session error.
    #[error("session error: {0}")]
    Core(#[from] tekvra_core::CoreError),
    /// Storage query error.
    #[error("storage error: {0}")]
    Storage(#[from] tekvra_storage::StorageError),
}

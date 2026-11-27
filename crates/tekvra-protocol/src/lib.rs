//! Shared protocol contracts for Tekvra's agent core, native tools, TUI, and Codex `app-server` v2.

pub mod codex_v2;
pub mod permissions;
pub mod session;
pub mod tools;

pub use permissions::{
    ApprovalPrompt, ApprovalResolution, ApprovalScope, GrantLifetime, PermissionMode, RuleAction,
    ScopeLadderOption,
};
pub use session::{
    InputDeliveryMode, InputImage, RevertScope, SessionCommand, SessionEvent, TokenUsage,
};
pub use tools::{
    FileChangeKind, FileDiffEntry, ReviewFinding, SearchMatch, SymbolOutline, ToolOutput,
};

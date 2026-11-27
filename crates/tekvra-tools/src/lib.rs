//! Native zero-Lua tool suite, `apply_patch` engine, interactive Tokio PTY sessions,
//! RTK-inspired output filtering, tree-sitter Bash AST permission decomposition,
//! SSRF-safe `webfetch`, and isolated Monty Python `code` + MCP gateway.

pub mod fs_tools;
pub mod mcp_client;
pub mod output_filter;
pub mod patch;
pub mod permissions;
pub mod pty;
pub mod registry;
pub mod web_and_code;

use std::path::PathBuf;

use thiserror::Error;

pub use fs_tools::{
    FileEditSpec, WorkspacePathLocks, extract_file_outline, file_edit, file_glob, file_grep,
    file_read, file_write, is_sensitive_credential_path, view_image,
};
pub use mcp_client::{ConfiguredMcpGateway, McpStartupReport};
pub use output_filter::{
    Corpus, FilteredOutput, RowRenderer, collapse_progress_lines, filter_command_output,
    render_terminal, strip_escape_sequences,
};
pub use patch::{PatchHunk, apply_patch_to_workspace, parse_patch};
pub use permissions::{
    BashSubcommand, DecomposedBashCommand, PermissionDecision, PermissionEvaluator, ToolAction,
    build_command_scope_ladder, decompose_bash_command,
};
pub use pty::{
    PtyCommandRequest, PtyCompletionNotice, PtySessionManager, build_sanitized_shell_env,
};
pub use registry::{
    ApprovalCallback, NativeToolRegistry, QuestionCallback, ToolExecutionContext,
    build_tool_definitions_for_model,
};
pub use web_and_code::{McpGateway, McpToolDescriptor, MontyCodeSandbox, SafeWebFetcher};

/// Errors emitted by `tekvra-tools`.
#[derive(Debug, Error)]
pub enum ToolsError {
    /// Invalid tool arguments or malformed `apply_patch` text.
    #[error("invalid tool input: {message}")]
    InvalidInput {
        /// Diagnostic message.
        message: String,
    },
    /// Rejected by permission mode, explicit `deny` rule, or user denial.
    #[error("permission denied: {message}")]
    PermissionDenied {
        /// Diagnostic message.
        message: String,
    },
    /// Path escaped workspace root without authorization or targeted sensitive files.
    #[error("path violation for `{}`: {message}", path.display())]
    PathViolation {
        /// Rejected path.
        path: PathBuf,
        /// Explanation.
        message: String,
    },
    /// Filesystem I/O failure.
    #[error("I/O error on `{}`: {source}", path.display())]
    Io {
        /// Target path.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// PTY or subprocess execution error.
    #[error("PTY error: {message}")]
    Pty {
        /// Explanation.
        message: String,
    },
    /// Blocked SSRF address or outbound HTTP transport failure.
    #[error("webfetch blocked or failed: {message}")]
    WebFetch {
        /// Explanation.
        message: String,
    },
    /// Isolated Monty Python interpreter or MCP gateway error.
    #[error("code sandbox error: {message}")]
    CodeSandbox {
        /// Explanation.
        message: String,
    },
    /// Storage/snapshot layer failure.
    #[error("storage error: {0}")]
    Storage(#[from] tekvra_storage::StorageError),
    /// Requested native tool does not exist.
    #[error("unknown tool `{name}`")]
    UnknownTool {
        /// Tool name.
        name: String,
    },
}

impl ToolsError {
    /// Helper for constructing [`ToolsError::InvalidInput`].
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput {
            message: message.into(),
        }
    }
}

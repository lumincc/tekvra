//! Permission modes, scoped approval ladders, and policy mapping contracts.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Session-wide consent and execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Read-only exploration and updating the session plan document only.
    Plan,
    /// Dedicated read-only mode used by code reviews and read-only subagents.
    ReadOnly,
    /// Workspace file edits allowed; unapproved shell, external paths, web, and MCP side effects prompt.
    #[default]
    Build,
    /// Skips routine unmatched prompts in `Build` mode while enforcing `deny`/`ask` rules and path boundaries.
    Auto,
    /// Skips interactive prompts and `ask` rules while enforcing explicit `deny` rules.
    Yolo,
}

impl PermissionMode {
    /// Returns whether workspace file mutations (`apply_patch`, `file_edit`, `file_write`) are permitted.
    #[must_use]
    pub const fn allows_workspace_writes(self) -> bool {
        matches!(self, Self::Build | Self::Auto | Self::Yolo)
    }

    /// Returns whether routine unmatched tool calls prompt the user.
    #[must_use]
    pub const fn prompts_on_unmatched(self) -> bool {
        matches!(self, Self::Plan | Self::ReadOnly | Self::Build)
    }
}

/// Rule effect configured in `permissions.toml` or persisted grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    /// Allow without prompting.
    Allow,
    /// Always prompt the user (unless in `Yolo` mode).
    Ask,
    /// Reject unconditionally in every mode, including `Auto` and `Yolo`.
    Deny,
}

/// Lifetime for an interactive approval decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantLifetime {
    /// Valid only for the pending tool invocation.
    #[default]
    Once,
    /// Valid for the remainder of the current conversation.
    Conversation,
    /// Persisted to project-level permissions store.
    Project,
    /// Persisted to global user permissions store.
    Global,
}

/// Typed scope pattern offered in an interactive approval ladder.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalScope {
    /// Exact command line string.
    ExactCommand {
        /// Exact command text.
        command: String,
    },
    /// Prefix of parsed command tokens (e.g., `["cargo", "test"]`).
    CommandPrefix {
        /// Leading tokens matched before trailing arguments.
        tokens: Vec<String>,
    },
    /// Canonical filesystem directory or file prefix.
    PathPrefix {
        /// Covered filesystem path.
        path: PathBuf,
    },
    /// Scheme + host (+ optional path prefix) for outbound network fetches.
    UrlPrefix {
        /// Covered URL prefix.
        prefix: String,
    },
    /// Specific MCP server and tool name.
    McpTool {
        /// MCP server identifier.
        server: String,
        /// Tool name, or `*` for all tools on that server.
        tool: String,
    },
    /// Session-wide tool permission (no narrower scope meaningful).
    SessionTool {
        /// Tool name.
        tool: String,
    },
}

impl ApprovalScope {
    /// Returns `true` when persisting this scope requires explicit secondary broad-grant confirmation.
    #[must_use]
    pub fn requires_broad_confirmation(&self, home_dir: Option<&std::path::Path>) -> bool {
        match self {
            Self::ExactCommand { .. } => false,
            Self::CommandPrefix { tokens } => tokens.len() <= 1,
            Self::PathPrefix { path } => {
                if path.parent().is_none() {
                    return true;
                }
                match home_dir {
                    Some(home) => !path.starts_with(home),
                    None => true,
                }
            }
            Self::UrlPrefix { prefix } => {
                prefix == "*" || prefix == "https://*" || prefix == "http://*"
            }
            Self::McpTool { tool, .. } => tool == "*",
            // Session-wide scopes are the session itself; persisting is inherently broad.
            Self::SessionTool { .. } => false,
        }
    }
}

/// Candidate rung in a permission approval ladder presented to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeLadderOption {
    /// Human-readable label shown in the TUI or client dialog.
    pub label: String,
    /// Underlying typed scope.
    pub scope: ApprovalScope,
    /// Whether selecting a persistent lifetime requires broad-grant confirmation.
    pub broad_grant: bool,
}

/// Interactive approval request emitted by the permission evaluator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPrompt {
    /// Unique identifier for correlating responses.
    pub approval_id: String,
    /// Tool call item identifier.
    pub item_id: String,
    /// Tool name requesting authorization.
    pub tool_name: String,
    /// Concise summary of the requested action.
    pub summary: String,
    /// Working directory when applicable.
    pub cwd: Option<PathBuf>,
    /// Whether reusable pattern grants are disallowed (e.g., `eval` or unparseable scripts).
    pub once_only: bool,
    /// Ordered narrow-to-broad scope options.
    pub ladder: Vec<ScopeLadderOption>,
}

/// User resolution for an [`ApprovalPrompt`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalResolution {
    /// Approve with the chosen scope and lifetime.
    Approve {
        /// Selected scope (defaults to exact invocation if `None`).
        scope: Option<ApprovalScope>,
        /// Selected persistence lifetime.
        lifetime: GrantLifetime,
        /// Explicit confirmation flag required when `scope` is broad and `lifetime != Once`.
        confirmed_broad: bool,
    },
    /// Deny this tool call.
    Deny {
        /// Optional feedback message returned to the model.
        reason: Option<String>,
    },
}

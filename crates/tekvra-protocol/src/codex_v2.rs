//! Codex `app-server` v2 JSON-RPC 2.0 wire data transfer objects and policy mappers.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::permissions::PermissionMode;

/// JSON-RPC 2.0 request identifier (integer or string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    /// Numeric JSON-RPC id.
    Number(i64),
    /// String JSON-RPC id.
    String(String),
}

/// Inbound or outbound JSON-RPC 2.0 request frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Optional `"2.0"` marker (Codex v2 allows omitting or sending `"2.0"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<String>,
    /// Request correlation id.
    pub id: JsonRpcId,
    /// Method name (e.g., `"initialize"`, `"thread/start"`, `"turn/start"`).
    pub method: String,
    /// Optional method parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 notification frame (no `id`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    /// Optional `"2.0"` marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<String>,
    /// Notification method name.
    pub method: String,
    /// Optional notification payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 success response frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Optional `"2.0"` marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<String>,
    /// Correlating request id.
    pub id: JsonRpcId,
    /// Successful result payload.
    pub result: Value,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcErrorBody {
    /// Standard or server-defined error code.
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
    /// Optional structured error details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC 2.0 error response frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcErrorResponse {
    /// Optional `"2.0"` marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<String>,
    /// Correlating request id.
    pub id: JsonRpcId,
    /// Error details.
    pub error: JsonRpcErrorBody,
}

/// Client metadata supplied in `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    /// Client application name.
    pub name: String,
    /// Optional client display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Client semantic version.
    pub version: String,
}

/// Parameters for the `initialize` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// Connected client metadata.
    pub client_info: ClientInfo,
    /// Optional capability hints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Value>,
}

/// Response returned for `initialize`, matching Codex `InitializeResponse`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    /// State/config home path.
    pub codex_home: String,
    /// OS family (`"unix"` or `"windows"`).
    pub platform_family: String,
    /// Operating system name (`"linux"`, `"macos"`, `"windows"`).
    pub platform_os: String,
    /// Server user-agent string (`"tekvra/<version>"`).
    pub user_agent: String,
}

/// Codex v2 `SandboxMode` enum (`read-only`, `workspace-write`, `danger-full-access`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodexSandboxMode {
    /// Read-only access.
    ReadOnly,
    /// Workspace writes permitted.
    #[default]
    WorkspaceWrite,
    /// Unrestricted host access.
    DangerFullAccess,
}

/// Codex v2 turn-level `SandboxPolicy` tagged union.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CodexSandboxPolicy {
    /// Unrestricted execution.
    DangerFullAccess,
    /// Read-only execution.
    #[serde(rename_all = "camelCase")]
    ReadOnly {
        /// Whether outbound network access is allowed.
        #[serde(default)]
        network_access: bool,
    },
    /// Workspace-scoped write execution.
    #[serde(rename_all = "camelCase")]
    WorkspaceWrite {
        /// Additional writable roots.
        #[serde(default)]
        writable_roots: Vec<String>,
        /// Whether outbound network access is allowed.
        #[serde(default)]
        network_access: bool,
    },
    /// Caller manages sandboxing externally.
    ExternalSandbox,
}

/// Codex v2 `AskForApproval` policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CodexApprovalPolicy {
    /// Standard string policy (`"untrusted"`, `"on-request"`, `"never"`).
    Simple(CodexSimpleApprovalPolicy),
    /// Granular object policy (`{"granular": ...}`).
    Granular(BTreeMap<String, Value>),
}

impl Default for CodexApprovalPolicy {
    fn default() -> Self {
        Self::Simple(CodexSimpleApprovalPolicy::OnRequest)
    }
}

/// String variants of Codex `AskForApproval`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodexSimpleApprovalPolicy {
    /// Prompt for untrusted commands/mutations.
    Untrusted,
    /// Prompt on request.
    OnRequest,
    /// Never prompt interactively.
    Never,
}

/// Reviewer assignment in Codex v2 (`user`, `auto_review`, `guardian_subagent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexApprovalsReviewer {
    /// Human client user reviews approvals.
    #[default]
    User,
    /// Automatic policy review (`Auto` mode).
    AutoReview,
    /// Subagent guardian review.
    GuardianSubagent,
}

/// Outcome of mapping incoming Codex sandbox + approval parameters onto Tekvra's consent model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedPolicy {
    /// Resulting Tekvra [`PermissionMode`].
    pub mode: PermissionMode,
    /// Optional `configWarning` emitted when the client requested restrictive OS kernel sandboxing.
    pub warning: Option<ConfigWarningNotification>,
}

/// Maps Codex `sandbox`/`sandboxPolicy`, `approvalPolicy`, and `approvalsReviewer` onto Tekvra's [`PermissionMode`].
#[must_use]
pub fn map_codex_policy(
    sandbox_mode: Option<CodexSandboxMode>,
    sandbox_policy: Option<&CodexSandboxPolicy>,
    approval_policy: Option<&CodexApprovalPolicy>,
    reviewer: Option<CodexApprovalsReviewer>,
) -> MappedPolicy {
    let effective_sandbox = match sandbox_policy {
        Some(CodexSandboxPolicy::ReadOnly { .. }) => CodexSandboxMode::ReadOnly,
        Some(CodexSandboxPolicy::DangerFullAccess | CodexSandboxPolicy::ExternalSandbox) => {
            CodexSandboxMode::DangerFullAccess
        }
        Some(CodexSandboxPolicy::WorkspaceWrite { .. }) => CodexSandboxMode::WorkspaceWrite,
        None => sandbox_mode.unwrap_or(CodexSandboxMode::WorkspaceWrite),
    };

    let never_ask = matches!(
        approval_policy,
        Some(CodexApprovalPolicy::Simple(
            CodexSimpleApprovalPolicy::Never
        ))
    );
    let auto_reviewer = matches!(
        reviewer,
        Some(CodexApprovalsReviewer::AutoReview | CodexApprovalsReviewer::GuardianSubagent)
    );

    let mode = match effective_sandbox {
        CodexSandboxMode::ReadOnly => PermissionMode::Plan,
        CodexSandboxMode::DangerFullAccess if never_ask => PermissionMode::Yolo,
        CodexSandboxMode::DangerFullAccess | CodexSandboxMode::WorkspaceWrite if auto_reviewer => {
            PermissionMode::Auto
        }
        CodexSandboxMode::DangerFullAccess | CodexSandboxMode::WorkspaceWrite if never_ask => {
            PermissionMode::Auto
        }
        CodexSandboxMode::WorkspaceWrite | CodexSandboxMode::DangerFullAccess => {
            PermissionMode::Build
        }
    };

    let needs_warning = !matches!(effective_sandbox, CodexSandboxMode::DangerFullAccess)
        && !matches!(sandbox_policy, Some(CodexSandboxPolicy::ExternalSandbox));

    let warning = needs_warning.then(|| ConfigWarningNotification {
        summary: "Tekvra enforces Bash AST decomposition and tool consent rather than an OS kernel process sandbox.".to_string(),
        details: Some(format!(
            "Mapped requested sandbox policy ({effective_sandbox:?}) onto Tekvra permission mode ({mode:?})."
        )),
        path: None,
        range: None,
    });

    MappedPolicy { mode, warning }
}

/// Codex v2 `ConfigWarningNotification`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigWarningNotification {
    /// Concise warning headline.
    pub summary: String,
    /// Optional detailed explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    /// Optional config file path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Optional source range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Value>,
}

/// Codex v2 `ThreadStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CodexThreadStatus {
    /// Thread metadata loaded without active session actor.
    NotLoaded,
    /// Loaded and waiting for turns.
    Idle,
    /// Actively executing a turn.
    #[serde(rename_all = "camelCase")]
    Active {
        /// Active flags (`waitingOnApproval`, `waitingOnUserInput`, etc.).
        #[serde(default)]
        active_flags: Vec<String>,
    },
    /// Encountered system error.
    SystemError,
}

/// Codex v2 `TurnStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodexTurnStatus {
    /// Currently running.
    InProgress,
    /// Completed normally.
    Completed,
    /// Interrupted by client/user.
    Interrupted,
    /// Terminated with an error.
    Failed,
}

/// Codex v2 `UserInput` item inside `turn/start`, `turn/steer`, and `userMessage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CodexUserInput {
    /// Plain UTF-8 text input.
    Text {
        /// Prompt string.
        text: String,
        /// Optional UI span annotations.
        #[serde(default)]
        text_elements: Vec<Value>,
    },
    /// Local image path input.
    LocalImage {
        /// Local filesystem path.
        path: String,
    },
}

/// Status for `commandExecution` and `fileChange` items in Codex v2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodexItemExecutionStatus {
    /// Currently executing.
    InProgress,
    /// Completed successfully or with exit code.
    Completed,
    /// Failed execution.
    Failed,
    /// Declined by user approval.
    Declined,
}

/// File update entry inside a `fileChange` `ThreadItem`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexFileUpdateChange {
    /// Path modified.
    pub path: String,
    /// Unified diff text.
    pub diff: String,
    /// Kind object (`{"type": "add" | "delete" | "update"}`).
    pub kind: Value,
}

/// Codex v2 `ThreadItem` flat wire representation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CodexThreadItem {
    /// User prompt item.
    #[serde(rename_all = "camelCase")]
    UserMessage {
        /// Item id.
        id: String,
        /// Input content blocks.
        content: Vec<CodexUserInput>,
    },
    /// Assistant message item.
    #[serde(rename_all = "camelCase")]
    AgentMessage {
        /// Item id.
        id: String,
        /// Message text.
        text: String,
    },
    /// Model reasoning item.
    #[serde(rename_all = "camelCase")]
    Reasoning {
        /// Item id.
        id: String,
        /// Summary paragraphs.
        #[serde(default)]
        summary: Vec<String>,
        /// Optional reasoning text blocks.
        #[serde(default)]
        content: Vec<String>,
    },
    /// Plan document item.
    #[serde(rename_all = "camelCase")]
    Plan {
        /// Item id.
        id: String,
        /// Markdown plan body.
        text: String,
    },
    /// Shell / PTY command execution item.
    #[serde(rename_all = "camelCase")]
    CommandExecution {
        /// Item id.
        id: String,
        /// Command string.
        command: String,
        /// Working directory.
        cwd: String,
        /// Parsed command action descriptors.
        command_actions: Vec<Value>,
        /// Execution status.
        status: CodexItemExecutionStatus,
        /// Optional aggregated output.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aggregated_output: Option<String>,
        /// Optional exit code.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    /// Workspace file patch item.
    #[serde(rename_all = "camelCase")]
    FileChange {
        /// Item id.
        id: String,
        /// Per-file changes.
        changes: Vec<CodexFileUpdateChange>,
        /// Apply status.
        status: CodexItemExecutionStatus,
    },
}

/// Codex v2 `Turn` wire representation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexTurn {
    /// Turn UUIDv7 identifier.
    pub id: String,
    /// Ordered items in this turn.
    pub items: Vec<CodexThreadItem>,
    /// Turn status.
    pub status: CodexTurnStatus,
    /// Optional error payload when `status == Failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

/// Codex v2 `Thread` wire representation matching all required schema fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexThread {
    /// Thread identifier.
    pub id: String,
    /// Session identifier.
    pub session_id: String,
    /// Project identifier derived from canonical workspace path.
    pub project_id: String,
    /// CLI version string.
    pub cli_version: String,
    /// Unix timestamp (seconds) when created.
    pub created_at: i64,
    /// Unix timestamp (seconds) when last updated.
    pub updated_at: i64,
    /// Absolute working directory path.
    pub cwd: String,
    /// Whether the thread is ephemeral.
    pub ephemeral: bool,
    /// Provider name (e.g., `"openai"`).
    pub model_provider: String,
    /// Short human-readable preview of the conversation.
    pub preview: String,
    /// Session source (`"appServer"`, `"cli"`, `"exec"`).
    pub source: String,
    /// Current thread status.
    pub status: CodexThreadStatus,
    /// Recorded turns.
    pub turns: Vec<CodexTurn>,
    /// Optional parent thread id when forked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from_id: Option<String>,
    /// Optional human-assigned thread title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Parameters for `review/start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CodexReviewTarget {
    /// Review staged, unstaged, and untracked working tree changes.
    UncommittedChanges,
    /// Review diff against `branch`.
    BaseBranch {
        /// Base branch name (e.g., `"main"`).
        branch: String,
    },
    /// Review a specific commit SHA.
    Commit {
        /// Commit hash.
        sha: String,
        /// Optional commit title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
}

/// Server-to-client `item/commandExecution/requestApproval` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionRequestApprovalParams {
    /// Thread id.
    pub thread_id: String,
    /// Turn id.
    pub turn_id: String,
    /// Item id.
    pub item_id: String,
    /// Start timestamp in epoch milliseconds.
    pub started_at_ms: i64,
    /// Optional approval correlation id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    /// Command line string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Optional human-readable reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Client decision returned for `item/commandExecution/requestApproval` or `item/fileChange/requestApproval`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDecisionResponse {
    /// `"accept"`, `"acceptForSession"`, `"decline"`, or `"cancel"`.
    pub decision: String,
}

/// Server-to-client `item/fileChange/requestApproval` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeRequestApprovalParams {
    /// Thread id.
    pub thread_id: String,
    /// Turn id.
    pub turn_id: String,
    /// Item id.
    pub item_id: String,
    /// Start timestamp in epoch milliseconds.
    pub started_at_ms: i64,
    /// Optional reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional requested grant root path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_root: Option<String>,
}

/// Question item inside `item/tool/requestUserInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUserInputQuestion {
    /// Question identifier.
    pub id: String,
    /// Short header label.
    pub header: String,
    /// Prompt text.
    pub question: String,
}

/// Server-to-client `item/tool/requestUserInput` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputParams {
    /// Thread id.
    pub thread_id: String,
    /// Turn id.
    pub turn_id: String,
    /// Item id.
    pub item_id: String,
    /// Whether the turn blocks on user input.
    pub is_blocking: bool,
    /// Questions presented to the user.
    pub questions: Vec<ToolUserInputQuestion>,
}

//! Internal event-driven session actor commands, events, and input delivery modes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::permissions::{ApprovalPrompt, ApprovalResolution, PermissionMode};
use crate::tools::{FileDiffEntry, ReviewFinding, ToolOutput};

/// Delivery intent when a user submits input while a turn is already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDeliveryMode {
    /// Queue until the active run completes.
    #[default]
    Next,
    /// Inject at the next safe turn boundary without cancelling running tools (`turn/steer`).
    Guide,
    /// Cancel the in-flight turn, preserve partial tool output, and start a new turn immediately.
    Replace,
}

/// Reversible scope when rolling a session back to an earlier message/turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevertScope {
    /// Revert conversation transcript only, leaving workspace files untouched.
    ConversationOnly,
    /// Restore workspace files only, keeping the conversation transcript.
    FilesOnly,
    /// Atomically restore both conversation history and workspace files.
    #[default]
    ConversationAndFiles,
}

/// Multimodal image attachment supplied on the wire by CLI/TUI frontends and converted into
/// provider `MessagePart::Image` blocks by the session runner (never stored in rollouts directly).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputImage {
    /// IANA media type (`image/png`, `image/jpeg`, `image/webp`, `image/gif`).
    pub media_type: String,
    /// Base64-encoded image bytes.
    pub data_base64: String,
    /// Optional display path (workspace-relative or absolute) for diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

/// Token and prompt-cache usage metrics for a turn or session.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Uncached + cached input tokens.
    pub input_tokens: u64,
    /// Tokens served from provider prompt cache.
    pub cached_input_tokens: u64,
    /// Generated output tokens.
    pub output_tokens: u64,
    /// Reasoning tokens included in output.
    pub reasoning_tokens: u64,
    /// Estimated cost in USD.
    pub cost_usd: f64,
}

impl TokenUsage {
    /// Computes the prompt-cache hit ratio in `[0.0, 1.0]`.
    #[must_use]
    pub fn cache_hit_ratio(&self) -> f64 {
        if self.input_tokens == 0 {
            0.0
        } else {
            (self.cached_input_tokens as f64 / self.input_tokens as f64).clamp(0.0, 1.0)
        }
    }
}

/// Commands accepted by a live `SessionHandle` actor in `tekvra-core`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum SessionCommand {
    /// Start or queue user input according to `delivery`.
    SubmitInput {
        /// User prompt text.
        prompt: String,
        /// Delivery behavior if a turn is already active.
        delivery: InputDeliveryMode,
        /// Multimodal image attachments appended after the prompt text part.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<InputImage>,
    },
    /// Interrupt the currently running turn.
    InterruptTurn,
    /// Resolve a pending permission prompt.
    ResolveApproval {
        /// Correlating approval request id.
        approval_id: String,
        /// User decision.
        resolution: ApprovalResolution,
    },
    /// Supply answers to an interactive `question` / `requestUserInput` prompt.
    ResolveUserInput {
        /// Correlating item id.
        item_id: String,
        /// Question id -> answer strings.
        answers: BTreeMap<String, Vec<String>>,
    },
    /// Revert session state to a target turn or message id.
    RevertTo {
        /// Target turn/message id.
        target_id: String,
        /// Revert mode (`ConversationOnly`, `FilesOnly`, `ConversationAndFiles`).
        scope: RevertScope,
    },
    /// Manually compact the context into a fresh frame generation, reporting token deltas
    /// via [`SessionEvent::CompactionCompleted`].
    Compact,
    /// Atomically revert the most recent user turn (conversation and/or workspace file
    /// snapshots) by delegating to the latest `TurnStarted` rollout record.
    UndoLastTurn {
        /// Revert mode (`ConversationOnly`, `FilesOnly`, `ConversationAndFiles`).
        scope: RevertScope,
    },
    /// Set or clear an evidence-gated `/goal` condition.
    SetGoal {
        /// Goal condition (`None` clears the active goal).
        condition: Option<String>,
    },
    /// Change the active session permission mode (`Plan`, `Build`, `Auto`, `Yolo`).
    SetMode {
        /// New permission mode.
        mode: PermissionMode,
    },
    /// Switch the active session model spec (e.g., `"openai/gpt-5.4-codex"`).
    SwitchModel {
        /// New `provider/model` identifier.
        model: String,
    },
}

/// Events broadcast by a `SessionHandle` to `tekvra-tui`, `tekvra-app-server`, and `tekvra-acp`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SessionEvent {
    /// A new turn has started.
    TurnStarted {
        /// Session/thread id.
        thread_id: String,
        /// Turn id.
        turn_id: String,
    },
    /// An item (message, reasoning, tool call, plan, review) started.
    ItemStarted {
        /// Session/thread id.
        thread_id: String,
        /// Turn id.
        turn_id: String,
        /// Item id.
        item_id: String,
        /// Item kind (`user_message`, `agent_message`, `reasoning`, `tool_call`, `plan`, `review`).
        kind: String,
    },
    /// Incremental streaming delta for an active item.
    ItemDelta {
        /// Session/thread id.
        thread_id: String,
        /// Turn id.
        turn_id: String,
        /// Item id.
        item_id: String,
        /// Channel (`text`, `reasoning_summary`, `reasoning_text`, `terminal_output`).
        channel: String,
        /// Appended text fragment.
        delta: String,
    },
    /// An item completed with optional structured [`ToolOutput`].
    ItemCompleted {
        /// Session/thread id.
        thread_id: String,
        /// Turn id.
        turn_id: String,
        /// Item id.
        item_id: String,
        /// Summary text or final message text.
        text: String,
        /// Structured native tool output when the item is a tool call.
        tool_output: Option<ToolOutput>,
    },
    /// Visible automatic steering nudge injected into the transcript.
    SteeringInjected {
        /// Session/thread id.
        thread_id: String,
        /// Turn id.
        turn_id: String,
        /// Reason category (`abandoned_turn`, `empty_after_tools`, `repeated_failing_tool`, `goal_unmet`).
        reason: String,
        /// Injected guidance message.
        message: String,
    },
    /// Interactive permission approval required before executing a tool.
    ApprovalRequested {
        /// Session/thread id.
        thread_id: String,
        /// Turn id.
        turn_id: String,
        /// Prompt payload.
        prompt: ApprovalPrompt,
    },
    /// A turn was refused before it started (e.g., spend cap reached). No frame was written.
    TurnRefused {
        /// Session/thread id.
        thread_id: String,
        /// Actionable refusal reason (limit, spent total, fix hint).
        reason: String,
    },
    /// Interactive user question input requested.
    UserInputRequested {
        /// Session/thread id.
        thread_id: String,
        /// Turn id.
        turn_id: String,
        /// Item id.
        item_id: String,
        /// Questions `(id, prompt)`.
        questions: Vec<(String, String)>,
    },
    /// Workspace diff updated during a turn.
    DiffUpdated {
        /// Session/thread id.
        thread_id: String,
        /// Turn id.
        turn_id: String,
        /// Updated file diffs.
        files: Vec<FileDiffEntry>,
    },
    /// Session plan updated.
    PlanUpdated {
        /// Session/thread id.
        thread_id: String,
        /// Turn id.
        turn_id: String,
        /// Plan markdown.
        markdown: String,
    },
    /// Structured code review completed.
    ReviewCompleted {
        /// Session/thread id.
        thread_id: String,
        /// Turn id.
        turn_id: String,
        /// Extracted review findings.
        findings: Vec<ReviewFinding>,
    },
    /// Cumulative token usage updated.
    TokenUsageUpdated {
        /// Session/thread id.
        thread_id: String,
        /// Turn id.
        turn_id: String,
        /// Usage snapshot.
        usage: TokenUsage,
    },
    /// Manual (or automatic) context compaction finished under a new frame generation.
    CompactionCompleted {
        /// Session/thread id.
        thread_id: String,
        /// New frame generation after compaction.
        generation: u64,
        /// Estimated token count before compaction.
        before_tokens: u64,
        /// Estimated token count after compaction.
        after_tokens: u64,
    },
    /// Configuration or policy warning (e.g., non-kernel-sandbox notice).
    ConfigWarning {
        /// Short warning summary.
        summary: String,
        /// Optional details.
        details: Option<String>,
    },
    /// Live progress of an in-flight MCP `tools/call` (never persisted, never enters the prompt).
    McpProgress {
        /// Session/thread id.
        thread_id: String,
        /// Turn id.
        turn_id: String,
        /// MCP server name.
        server: String,
        /// Tool name.
        tool: String,
        /// Progress value.
        progress: f64,
        /// Total when known.
        total: Option<f64>,
        /// Optional human-readable status.
        message: Option<String>,
    },
    /// Turn finished (`completed`, `interrupted`, `failed`).
    TurnCompleted {
        /// Session/thread id.
        thread_id: String,
        /// Turn id.
        turn_id: String,
        /// Final status string.
        status: String,
        /// Optional error message if failed.
        error: Option<String>,
    },
}

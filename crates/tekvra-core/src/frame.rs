//! Append-only `Frame` prompt assembly, cryptographic prefix verification (`assert_append_only`),
//! trailing `<system-reminder>` context injection, and context compaction.

use std::path::Path;

use sha2::{Digest, Sha256};
use tekvra_protocol::PermissionMode;
use tekvra_providers::{
    CompletionRequest, MessagePart, MessageRole, ModelCapabilities, ProviderDefinition,
    ProviderDriver, ProviderMessage, ToolDefinition,
};

use crate::CoreError;

/// Lightweight immutable snapshot of a [`Frame`] used by [`Frame::assert_append_only`]
/// to guarantee that every prior message and cached developer instruction stays byte-identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSnapshot {
    /// Frame compaction/revert generation counter.
    pub generation: u64,
    /// SHA-256 hex digest of serialized `developer_prefix`.
    pub developer_hash: String,
    /// SHA-256 hex digests of every message in `messages` at snapshot time.
    pub message_hashes: Vec<String>,
    /// SHA-256 hex digest of serialized `tools` roster.
    pub tools_hash: String,
    /// Whether the snapshot was bound to an `apply_patch` model family.
    pub uses_apply_patch: bool,
}

/// Threshold configuration controlling automatic context compaction.
#[derive(Debug, Clone, Copy)]
pub struct CompactionConfig {
    /// Fraction of `model.context_window` (`0.0..=1.0`) at which compaction triggers.
    pub window_fraction: f64,
    /// Optional explicit token ceiling override (useful in tests and constrained profiles).
    pub max_estimated_tokens_override: Option<u64>,
    /// Number of most recent conversation messages preserved verbatim after the compacted summary.
    pub preserve_recent_messages: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            window_fraction: 0.80,
            max_estimated_tokens_override: None,
            preserve_recent_messages: 4,
        }
    }
}

/// Append-only prompt frame preserving provider prompt-cache prefixes across multi-step turns
/// and mid-session mode/model transitions.
#[derive(Debug, Clone)]
pub struct Frame {
    generation: u64,
    developer_prefix: ProviderMessage,
    developer_hash: String,
    messages: Vec<ProviderMessage>,
    message_hashes: Vec<String>,
    tools: Vec<ToolDefinition>,
    tools_hash: String,
    uses_apply_patch: bool,
}

impl Frame {
    /// Creates a new generation-`0` [`Frame`] with a cache-breakpointed `DeveloperMessage`
    /// and model-aligned `tools`.
    #[must_use]
    pub fn new(
        developer_instructions: impl Into<String>,
        tools: Vec<ToolDefinition>,
        uses_apply_patch: bool,
    ) -> Self {
        let developer_prefix = ProviderMessage::developer_cached(developer_instructions);
        let developer_hash = hash_json(&developer_prefix);
        let tools_hash = hash_json(&tools);
        Self {
            generation: 0,
            developer_prefix,
            developer_hash,
            messages: Vec::new(),
            message_hashes: Vec::new(),
            tools,
            tools_hash,
            uses_apply_patch,
        }
    }

    /// Reconstructs a [`Frame`] from persisted rollout messages while bumping `generation` when rehydrated/reverted.
    #[must_use]
    pub fn from_messages(
        generation: u64,
        developer_instructions: impl Into<String>,
        existing_messages: Vec<ProviderMessage>,
        tools: Vec<ToolDefinition>,
        uses_apply_patch: bool,
    ) -> Self {
        let mut frame = Self::new(developer_instructions, tools, uses_apply_patch);
        frame.generation = generation;
        for msg in existing_messages {
            if msg.role != MessageRole::Developer {
                frame.push_message(msg);
            }
        }
        frame
    }

    /// Current compaction/revert generation counter.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Immutable cache-breakpointed developer prefix message.
    #[must_use]
    pub const fn developer_prefix(&self) -> &ProviderMessage {
        &self.developer_prefix
    }

    /// Appended non-developer conversation messages (`User`, `Assistant`, `Tool`).
    #[must_use]
    pub fn messages(&self) -> &[ProviderMessage] {
        &self.messages
    }

    /// Current tool definition roster.
    #[must_use]
    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    /// Captures a [`FrameSnapshot`] for subsequent [`Frame::assert_append_only`] verification.
    #[must_use]
    pub fn snapshot(&self) -> FrameSnapshot {
        FrameSnapshot {
            generation: self.generation,
            developer_hash: self.developer_hash.clone(),
            message_hashes: self.message_hashes.clone(),
            tools_hash: self.tools_hash.clone(),
            uses_apply_patch: self.uses_apply_patch,
        }
    }

    /// Verifies that `self` strictly extends `previous` within the same `generation`:
    /// - `developer_prefix` is byte-identical
    /// - `messages.len() >= previous.message_hashes.len()`
    /// - every message `0..previous.len()` has the exact same SHA-256 JSON digest
    /// - if `uses_apply_patch` did not change, `tools` is also byte-identical.
    ///
    /// # Errors
    /// Returns [`CoreError::FramePrefixMutated`] if any historical message or header was altered.
    pub fn assert_append_only(&self, previous: &FrameSnapshot) -> Result<(), CoreError> {
        if self.generation != previous.generation {
            // A fresh compacted or reverted generation explicitly replaces the frame.
            return Ok(());
        }
        if self.developer_hash != previous.developer_hash {
            return Err(CoreError::FramePrefixMutated {
                index: 0,
                detail: "cached developer instruction prefix was mutated mid-generation"
                    .to_string(),
            });
        }
        if self.messages.len() < previous.message_hashes.len() {
            return Err(CoreError::FramePrefixMutated {
                index: self.messages.len(),
                detail: format!(
                    "frame shrank from {} to {} messages without generation bump",
                    previous.message_hashes.len(),
                    self.messages.len()
                ),
            });
        }
        for (idx, prev_hash) in previous.message_hashes.iter().enumerate() {
            let actual_hash = hash_json(&self.messages[idx]);
            if &actual_hash != prev_hash {
                return Err(CoreError::FramePrefixMutated {
                    index: idx + 1,
                    detail: format!(
                        "historical message at position {idx} changed digest ({prev_hash} -> {actual_hash})"
                    ),
                });
            }
        }
        if self.uses_apply_patch == previous.uses_apply_patch
            && self.tools_hash != previous.tools_hash
        {
            return Err(CoreError::FramePrefixMutated {
                index: 0,
                detail: "top-level tool schema list mutated within the same editor family"
                    .to_string(),
            });
        }
        Ok(())
    }

    /// Appends a new conversation message to the tail of the frame.
    pub fn push_message(&mut self, message: ProviderMessage) {
        let digest = hash_json(&message);
        self.message_hashes.push(digest);
        self.messages.push(message);
    }

    /// Appends a trailing `<system-reminder>` user message when switching permission modes
    /// (e.g., `Plan` -> `Build`) WITHOUT modifying `developer_prefix` or any prior message!
    pub fn append_mode_transition(
        &mut self,
        previous_mode: PermissionMode,
        new_mode: PermissionMode,
        plan_markdown: &str,
    ) -> ProviderMessage {
        let mut body = format!(
            "<system-reminder>\nSession mode changed from `{previous_mode:?}` to `{new_mode:?}`."
        );
        match new_mode {
            PermissionMode::Plan => {
                body.push_str(
                    "\nWorkspace file mutations are disabled in `Plan` mode; explore with read-only tools and record changes via `plan`.",
                );
            }
            PermissionMode::ReadOnly => {
                body.push_str(
                    "\nSession is now strictly `ReadOnly`; all file writes, plan edits, and mutating commands are blocked.",
                );
            }
            PermissionMode::Build | PermissionMode::Auto | PermissionMode::Yolo => {
                body.push_str("\nWorkspace file edits and implementation tools are now active.");
                if !plan_markdown.trim().is_empty() {
                    body.push_str("\nActive session plan:\n");
                    body.push_str(plan_markdown.trim());
                }
            }
        }
        body.push_str("\n</system-reminder>");
        let msg = ProviderMessage::user_text(body);
        self.push_message(msg.clone());
        msg
    }

    /// Appends a trailing `<system-reminder>` noting a working-directory update.
    pub fn append_cwd_update(&mut self, new_cwd: &Path) -> ProviderMessage {
        let msg = ProviderMessage::user_text(format!(
            "<system-reminder>\nWorking directory updated to `{}`.\n</system-reminder>",
            new_cwd.display()
        ));
        self.push_message(msg.clone());
        msg
    }

    /// Appends a trailing `<system-reminder>` carrying an automatic steering nudge or background notice.
    pub fn append_system_reminder(&mut self, reminder_body: &str) -> ProviderMessage {
        let wrapped = if reminder_body.trim_start().starts_with("<system-reminder>") {
            reminder_body.to_string()
        } else {
            format!("<system-reminder>\n{reminder_body}\n</system-reminder>")
        };
        let msg = ProviderMessage::user_text(wrapped);
        self.push_message(msg.clone());
        msg
    }

    /// Updates `tools` ONLY when switching across model editor families (`apply_patch` <-> `file_edit`+`file_write`),
    /// leaving `tools` untouched when `uses_apply_patch` is unchanged.
    pub fn sync_tools_for_model(
        &mut self,
        uses_apply_patch: bool,
        build_fn: impl FnOnce() -> Vec<ToolDefinition>,
    ) {
        if self.uses_apply_patch != uses_apply_patch {
            let new_tools = build_fn();
            self.tools_hash = hash_json(&new_tools);
            self.tools = new_tools;
            self.uses_apply_patch = uses_apply_patch;
        }
    }

    /// Builds a [`CompletionRequest`] containing `developer_prefix` followed by all appended `messages`.
    #[must_use]
    pub fn build_completion_request(
        &self,
        conversation_id: &str,
        reasoning_effort: Option<String>,
        output_schema: Option<serde_json::Value>,
    ) -> CompletionRequest {
        let mut all_messages = Vec::with_capacity(1 + self.messages.len());
        all_messages.push(self.developer_prefix.clone());
        all_messages.extend(self.messages.iter().cloned());
        CompletionRequest {
            conversation_id: conversation_id.to_string(),
            messages: all_messages,
            tools: self.tools.clone(),
            reasoning_effort,
            output_schema,
        }
    }

    /// Estimates approximate prompt token count (`~4` UTF-8 bytes per token).
    #[must_use]
    pub fn estimated_tokens(&self) -> u64 {
        let mut bytes = message_byte_estimate(&self.developer_prefix);
        for m in &self.messages {
            bytes = bytes.saturating_add(message_byte_estimate(m));
        }
        (bytes as u64 / 4).max(1)
    }

    /// Compacts the frame into a new generation (`generation + 1`) if estimated tokens exceed
    /// `config` limits for `target_model`, using `compact_provider` / `compact_model`.
    ///
    /// Returns `Ok(Some(summary_text))` when compaction ran, or `Ok(None)` when below threshold.
    ///
    /// # Errors
    /// Returns [`CoreError`] if the compaction summarization call fails.
    pub async fn compact_if_needed(
        &mut self,
        conversation_id: &str,
        target_model: &ModelCapabilities,
        compact_provider: &ProviderDefinition,
        compact_model: &ModelCapabilities,
        driver: &dyn ProviderDriver,
        config: CompactionConfig,
    ) -> Result<Option<String>, CoreError> {
        let ceiling = config.max_estimated_tokens_override.unwrap_or_else(|| {
            ((target_model.context_window as f64) * config.window_fraction.clamp(0.1, 0.98)) as u64
        });
        if self.estimated_tokens() <= ceiling
            || self.messages.len() <= config.preserve_recent_messages
        {
            return Ok(None);
        }
        let summary = self
            .force_compact(
                conversation_id,
                compact_provider,
                compact_model,
                driver,
                config.preserve_recent_messages,
            )
            .await?;
        Ok(Some(summary))
    }

    /// Unconditionally summarizes older transcript entries into a fresh `generation + 1` [`Frame`].
    ///
    /// # Errors
    /// Returns [`CoreError`] if the compaction provider call fails.
    pub async fn force_compact(
        &mut self,
        conversation_id: &str,
        compact_provider: &ProviderDefinition,
        compact_model: &ModelCapabilities,
        driver: &dyn ProviderDriver,
        preserve_recent_messages: usize,
    ) -> Result<String, CoreError> {
        let split_at = self.messages.len().saturating_sub(preserve_recent_messages);
        let to_summarize = &self.messages[..split_at];
        let mut kept_tail: Vec<ProviderMessage> = self.messages[split_at..].to_vec();
        // Ensure we don't start `kept_tail` with an orphan `Tool` message whose `ToolCall` was summarized!
        while matches!(kept_tail.first().map(|m| m.role), Some(MessageRole::Tool)) {
            kept_tail.remove(0);
        }

        let transcript_digest = render_messages_for_summary(to_summarize);
        let compact_req = CompletionRequest {
            conversation_id: format!("{conversation_id}:compact"),
            messages: vec![
                ProviderMessage::developer_cached(
                    "Summarize the earlier coding session concisely and accurately. Preserve all file paths modified, key architectural decisions, test results, unresolved bugs, and user constraints.",
                ),
                ProviderMessage::user_text(transcript_digest),
            ],
            tools: Vec::new(),
            reasoning_effort: None,
            output_schema: None,
        };

        let resp = driver
            .complete(compact_provider, compact_model, &compact_req)
            .await?;
        let summary_text = extract_assistant_text(&resp.assistant_message);

        self.generation = self.generation.saturating_add(1);
        self.messages.clear();
        self.message_hashes.clear();
        self.push_message(ProviderMessage::user_text(format!(
            "<compacted-context generation=\"{}\">\n{}\n</compacted-context>",
            self.generation,
            summary_text.trim()
        )));
        for tail_msg in kept_tail {
            self.push_message(tail_msg);
        }

        Ok(summary_text)
    }
}

pub(crate) fn extract_assistant_text(msg: &ProviderMessage) -> String {
    let mut out = String::new();
    for part in &msg.parts {
        if let MessagePart::Text { text, .. } = part {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

pub(crate) fn render_messages_for_summary(messages: &[ProviderMessage]) -> String {
    let mut buf = String::new();
    for msg in messages {
        for part in &msg.parts {
            match part {
                MessagePart::Text { text, .. } => {
                    buf.push_str(&format!("[{:?}]: {}\n", msg.role, text));
                }
                MessagePart::ToolCall {
                    name,
                    arguments_json,
                    ..
                } => {
                    buf.push_str(&format!("[ToolCall {name}]: {arguments_json}\n"));
                }
                MessagePart::ToolResult {
                    call_id,
                    output,
                    is_error,
                } => {
                    buf.push_str(&format!(
                        "[ToolResult {call_id} error={is_error}]: {}\n",
                        truncate_chars(output, 1_200)
                    ));
                }
                MessagePart::Reasoning { summary, .. } if !summary.is_empty() => {
                    buf.push_str(&format!("[Reasoning]: {}\n", summary.join(" ")));
                }
                MessagePart::Reasoning { .. } => {}
            }
        }
    }
    buf
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let prefix: String = s.chars().take(max_chars).collect();
        format!("{prefix}... [truncated]")
    }
}

fn message_byte_estimate(msg: &ProviderMessage) -> usize {
    msg.parts
        .iter()
        .map(|p| match p {
            MessagePart::Text { text, .. } => text.len() + 16,
            MessagePart::Reasoning {
                summary,
                encrypted_content,
                ..
            } => {
                summary.iter().map(String::len).sum::<usize>()
                    + encrypted_content.as_ref().map_or(0, String::len)
                    + 24
            }
            MessagePart::ToolCall {
                name,
                arguments_json,
                ..
            } => name.len() + arguments_json.len() + 24,
            MessagePart::ToolResult { output, .. } => output.len() + 24,
        })
        .sum()
}

fn hash_json<T: serde::Serialize>(val: &T) -> String {
    let bytes = serde_json::to_vec(val).unwrap_or_default();
    format!("{:x}", Sha256::digest(&bytes))
}

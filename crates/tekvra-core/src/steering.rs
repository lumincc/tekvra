//! Caudra-style automatic steering nudges:
//! 1. Empty post-tool assistant response (`empty_after_tools`)
//! 2. Prose-only announcement of imminent tool action (`abandoned_turn`)
//! 3. 3-strike identical failing `(tool_name, arguments)` refusal (`repeated_failing_tool`)

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use tekvra_providers::{MessagePart, ProviderMessage};

static ABANDONED_INTENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(i\s+will|i'll|let\s+me|let's|i\s+am\s+going\s+to|i'm\s+going\s+to|next,?\s+i\s+will|now\s+i\s+will|now\s+let\s+me)\s+(now\s+|first\s+|go\s+ahead\s+and\s+)?(run|execute|read|inspect|open|check|search|grep|glob|edit|modify|update|patch|write|create|fix|apply|test|build|verify|compile)\b",
    )
    .expect("valid abandoned-turn intent regex")
});

/// Steering intervention decided after inspecting an assistant turn or repeated tool failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteeringAction {
    /// Machine-readable reason (`empty_after_tools`, `abandoned_turn`, `repeated_failing_tool`).
    pub reason: String,
    /// Guidance message injected as a `<system-reminder>` into the append-only frame.
    pub message: String,
}

/// Returns `true` when an assistant prose-only reply announces an imminent tool action without calling a tool.
#[must_use]
pub fn detect_abandoned_turn_intent(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.ends_with('?') {
        return false;
    }
    // Check the last 2 non-empty lines/paragraphs where models typically announce their next step.
    let tail_lines: Vec<&str> = trimmed
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .rev()
        .take(2)
        .collect();
    if let Some(last_line) = tail_lines.first()
        && last_line.ends_with(':')
        && !last_line.starts_with('#')
        && !last_line.starts_with('-')
        && !last_line.starts_with('*')
    {
        return true;
    }
    tail_lines
        .iter()
        .any(|line| ABANDONED_INTENT_RE.is_match(line))
}

/// Bounded per-turn detector for `empty_after_tools` and `abandoned_turn` steering nudges.
#[derive(Debug, Clone)]
pub struct SteeringDetector {
    max_nudges: u32,
    nudges_emitted: u32,
}

impl SteeringDetector {
    /// Creates a [`SteeringDetector`] bounded by `max_nudges` per user turn.
    #[must_use]
    pub const fn new(max_nudges: u32) -> Self {
        Self {
            max_nudges,
            nudges_emitted: 0,
        }
    }

    /// Number of steering nudges emitted during the current user turn.
    #[must_use]
    pub const fn nudges_emitted(&self) -> u32 {
        self.nudges_emitted
    }

    /// Evaluates a completed assistant message that had **zero** tool calls.
    /// Returns `Some(SteeringAction)` if a nudge is warranted and within `max_nudges`.
    pub fn evaluate_no_tool_response(
        &mut self,
        assistant_msg: &ProviderMessage,
        had_tool_execution_in_turn: bool,
    ) -> Option<SteeringAction> {
        if self.nudges_emitted >= self.max_nudges {
            return None;
        }

        let mut prose = String::new();
        for part in &assistant_msg.parts {
            if let MessagePart::Text { text, .. } = part {
                if !prose.is_empty() {
                    prose.push('\n');
                }
                prose.push_str(text);
            }
        }

        if prose.trim().is_empty() && had_tool_execution_in_turn {
            self.nudges_emitted += 1;
            return Some(SteeringAction {
                reason: "empty_after_tools".to_string(),
                message: "Your previous response after tool execution was empty. Summarize the result or invoke the next tool needed to finish the task.".to_string(),
            });
        }

        if detect_abandoned_turn_intent(&prose) {
            self.nudges_emitted += 1;
            return Some(SteeringAction {
                reason: "abandoned_turn".to_string(),
                message: "You announced an imminent action in prose without invoking any tool. Invoke the appropriate tool call now, or provide your final answer if no further tool calls are needed.".to_string(),
            });
        }

        None
    }
}

/// Tracks consecutive identical failing `(tool_name, canonical_arguments_json)` calls so the
/// 3rd identical call in a row is refused before execution!
#[derive(Debug, Clone, Default)]
pub struct ToolFailureTracker {
    last_failed_signature: Option<(String, String)>,
    consecutive_failures: u32,
}

impl ToolFailureTracker {
    /// Creates a fresh [`ToolFailureTracker`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks BEFORE executing `(tool_name, raw_arguments_json)` whether the exact same call
    /// already failed twice consecutively (making this the 3rd identical attempt in a row).
    #[must_use]
    pub fn check_third_strike(
        &self,
        tool_name: &str,
        raw_arguments_json: &str,
    ) -> Option<SteeringAction> {
        let canonical = canonicalize_json(raw_arguments_json);
        if let Some((prev_tool, prev_args)) = &self.last_failed_signature
            && prev_tool == tool_name
            && prev_args == &canonical
            && self.consecutive_failures >= 2
        {
            return Some(SteeringAction {
                reason: "repeated_failing_tool".to_string(),
                message: format!(
                    "Refused 3rd consecutive identical call to `{tool_name}` with the same arguments after 2 consecutive failures. Diagnose why `{tool_name}` failed and change your arguments or approach instead of retrying verbatim."
                ),
            });
        }
        None
    }

    /// Records the outcome of executing `(tool_name, raw_arguments_json)`.
    pub fn record_outcome(&mut self, tool_name: &str, raw_arguments_json: &str, failed: bool) {
        if !failed {
            self.last_failed_signature = None;
            self.consecutive_failures = 0;
            return;
        }
        let canonical = canonicalize_json(raw_arguments_json);
        if let Some((prev_tool, prev_args)) = &self.last_failed_signature
            && prev_tool == tool_name
            && prev_args == &canonical
        {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        } else {
            self.last_failed_signature = Some((tool_name.to_string(), canonical));
            self.consecutive_failures = 1;
        }
    }
}

fn canonicalize_json(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(val) => serde_json::to_string(&val).unwrap_or_else(|_| raw.trim().to_string()),
        Err(_) => raw.trim().to_string(),
    }
}

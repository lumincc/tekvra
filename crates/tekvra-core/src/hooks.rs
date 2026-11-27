//! Luau lifecycle hook contract shared by the session loop and the plugin host
//! (`tekvra.api.register_hook`), keeping `tekvra-core` free of any `tekvra-lua`
//! dependency direction.

/// Lifecycle events a plugin hook can observe (`tekvra.api.register_hook(event, handler)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    /// Fired once after the session's developer instructions are built.
    SessionStart,
    /// Fired at every user/wakeup turn start.
    TurnStarted,
    /// Fired at turn completion with the terminal status and model.
    TurnCompleted,
    /// Fired before a native tool call executes; may deny it.
    PreToolUse,
    /// Fired after a tool call completed (its result is already recorded).
    PostToolUse,
}

impl HookEvent {
    /// Canonical event name surfaced to plugins.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::TurnStarted => "turn_started",
            Self::TurnCompleted => "turn_completed",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
        }
    }

    /// Parses an event name; `None` for unknown events (rejected at registration).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "session_start" => Some(Self::SessionStart),
            "turn_started" => Some(Self::TurnStarted),
            "turn_completed" => Some(Self::TurnCompleted),
            "pre_tool_use" => Some(Self::PreToolUse),
            "post_tool_use" => Some(Self::PostToolUse),
            _ => None,
        }
    }
}

/// Payload table passed to a hook handler (only the event-relevant fields are set).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookEventPayload {
    /// Turn identifier (`turn_*` events).
    pub turn_id: Option<String>,
    /// Terminal status (`turn_completed`).
    pub status: Option<String>,
    /// Active model spec (`turn_*` events).
    pub model: Option<String>,
    /// Tool name (`pre_tool_use` / `post_tool_use`).
    pub tool: Option<String>,
    /// Raw JSON arguments string (`pre_tool_use` / `post_tool_use`).
    pub arguments: Option<String>,
    /// Tool call id (`pre_tool_use` / `post_tool_use`).
    pub call_id: Option<String>,
}

/// Result of dispatching one hook event across all registered handlers
/// (registration order): string notes, an optional deny decision, and
/// failure descriptions for handlers that errored or exceeded their budget.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookOutcome {
    /// String notes returned by handlers, in registration order.
    pub notes: Vec<String>,
    /// Denial reason from the first `pre_tool_use` handler returning
    /// `{ decision = "deny", reason = ... }`.
    pub denied: Option<String>,
    /// Per-handler failure descriptions (error or instruction-budget overrun).
    pub failures: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_event_names_round_trip() {
        for name in [
            "session_start",
            "turn_started",
            "turn_completed",
            "pre_tool_use",
            "post_tool_use",
        ] {
            let ev = HookEvent::parse(name).unwrap_or_else(|| panic!("parse {name}"));
            assert_eq!(ev.as_str(), name);
        }
        assert!(HookEvent::parse("session_end").is_none());
        assert!(HookEvent::parse("").is_none());
    }
}

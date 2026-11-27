//! Modal overlays for scope-ladder approvals, `/tasks` (`Ctrl-X`) subagent navigation & live steering,
//! `/search` FTS5 history picker, and workspace file-revert preview.

use tekvra_core::SubagentEntry;
use tekvra_protocol::{
    ApprovalPrompt, ApprovalResolution, GrantLifetime, InputDeliveryMode, RevertScope,
};
use tekvra_storage::{FilePreImage, HistorySearchHit, SessionSummary, SnapshotManifest};

/// Interactive state for the narrow-to-broad scope-ladder permission approval dialog.
#[derive(Debug, Clone)]
pub struct ApprovalModalState {
    /// Pending approval prompt from `tekvra-core`.
    pub prompt: ApprovalPrompt,
    /// Selected rung index in `prompt.ladder`.
    pub selected_rung: usize,
    /// Selected persistence lifetime (`Once`, `Conversation`, `Project`, `Global`).
    pub lifetime: GrantLifetime,
    /// True when the user selected a persistent broad grant and must confirm a second time.
    pub awaiting_broad_confirmation: bool,
}

impl ApprovalModalState {
    /// Creates an [`ApprovalModalState`] defaulting to rung `0` (narrowest exact scope) and `GrantLifetime::Once`.
    #[must_use]
    pub fn new(prompt: ApprovalPrompt) -> Self {
        Self {
            prompt,
            selected_rung: 0,
            lifetime: GrantLifetime::Once,
            awaiting_broad_confirmation: false,
        }
    }

    /// Moves selection to the next broader scope rung (if allowed).
    pub fn next_rung(&mut self) {
        if !self.prompt.ladder.is_empty() {
            self.selected_rung = (self.selected_rung + 1) % self.prompt.ladder.len();
            self.awaiting_broad_confirmation = false;
        }
    }

    /// Moves selection to the previous narrower scope rung.
    pub fn prev_rung(&mut self) {
        if !self.prompt.ladder.is_empty() {
            self.selected_rung = if self.selected_rung == 0 {
                self.prompt.ladder.len() - 1
            } else {
                self.selected_rung - 1
            };
            self.awaiting_broad_confirmation = false;
        }
    }

    /// Cycles persistence lifetime (`Once` -> `Conversation` -> `Project` -> `Global`),
    /// staying locked to `Once` when `prompt.once_only == true`!
    pub fn cycle_lifetime(&mut self) {
        if self.prompt.once_only {
            self.lifetime = GrantLifetime::Once;
            return;
        }
        self.lifetime = match self.lifetime {
            GrantLifetime::Once => GrantLifetime::Conversation,
            GrantLifetime::Conversation => GrantLifetime::Project,
            GrantLifetime::Project => GrantLifetime::Global,
            GrantLifetime::Global => GrantLifetime::Once,
        };
        self.awaiting_broad_confirmation = false;
    }

    /// Returns whether the currently selected `(rung, lifetime)` requires secondary broad-grant confirmation.
    #[must_use]
    pub fn requires_secondary_confirm(&self) -> bool {
        self.lifetime != GrantLifetime::Once
            && self
                .prompt
                .ladder
                .get(self.selected_rung)
                .is_some_and(|r| r.broad_grant)
    }

    /// Attempts to approve the current selection:
    /// - If secondary broad confirmation is required and not yet shown, enters `awaiting_broad_confirmation = true` and returns `None`.
    /// - Otherwise returns `Some(ApprovalResolution::Approve { .. })`.
    pub fn confirm_approve(&mut self) -> Option<ApprovalResolution> {
        if self.requires_secondary_confirm() && !self.awaiting_broad_confirmation {
            self.awaiting_broad_confirmation = true;
            return None;
        }
        let scope = self
            .prompt
            .ladder
            .get(self.selected_rung)
            .map(|r| r.scope.clone());
        Some(ApprovalResolution::Approve {
            scope,
            lifetime: self.lifetime,
            confirmed_broad: self.awaiting_broad_confirmation,
        })
    }

    /// Denies the pending prompt.
    #[must_use]
    pub fn deny(&self) -> ApprovalResolution {
        ApprovalResolution::Deny {
            reason: Some("Denied by user in TUI approval dialog.".to_string()),
        }
    }
}

/// Interactive state for the `/tasks` (`Ctrl-X`) subagent view switcher and live steerer.
#[derive(Debug, Clone)]
pub struct TasksOverlayState {
    /// Entry `0` is `[Main Session]`; `1..=entries.len()` correspond to `entries[idx - 1]`.
    pub entries: Vec<SubagentEntry>,
    /// Currently highlighted row (`0` = Main Session).
    pub selected_index: usize,
    /// Optional inline steering message being typed for the highlighted subagent.
    pub steer_input: String,
    /// Selected steering delivery mode (`Guide` or `Replace`).
    pub steer_delivery: InputDeliveryMode,
}

impl TasksOverlayState {
    /// Creates a [`TasksOverlayState`] from live subagent entries.
    #[must_use]
    pub fn new(entries: Vec<SubagentEntry>) -> Self {
        Self {
            entries,
            selected_index: 0,
            steer_input: String::new(),
            steer_delivery: InputDeliveryMode::Guide,
        }
    }

    /// Total rows including row `0` (`[Main Session]`).
    #[must_use]
    pub fn total_rows(&self) -> usize {
        1 + self.entries.len()
    }

    /// Moves cursor down.
    pub fn next(&mut self) {
        self.selected_index = (self.selected_index + 1) % self.total_rows();
    }

    /// Moves cursor up.
    pub fn prev(&mut self) {
        self.selected_index = if self.selected_index == 0 {
            self.total_rows() - 1
        } else {
            self.selected_index - 1
        };
    }

    /// Returns the selected `task_id` (`None` when `[Main Session]` row `0` is selected).
    #[must_use]
    pub fn selected_task_id(&self) -> Option<&str> {
        if self.selected_index == 0 {
            None
        } else {
            self.entries
                .get(self.selected_index - 1)
                .map(|e| e.task_id.as_str())
        }
    }

    /// Toggles steering mode between `Guide` and `Replace`.
    pub fn toggle_delivery_mode(&mut self) {
        self.steer_delivery = match self.steer_delivery {
            InputDeliveryMode::Guide | InputDeliveryMode::Next => InputDeliveryMode::Replace,
            InputDeliveryMode::Replace => InputDeliveryMode::Guide,
        };
    }
}

/// Interactive state for the `/search` FTS5 history session picker.
#[derive(Debug, Clone)]
pub struct SearchOverlayState {
    /// Search query string.
    pub query: String,
    /// Ranked FTS5 matches.
    pub hits: Vec<HistorySearchHit>,
    /// Highlighted match index.
    pub selected: usize,
}

/// Single file entry shown in the `/revert` workspace preview dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertPreviewItem {
    /// Workspace-relative path.
    pub path: String,
    /// Human-readable restoration action (`"delete newly created file"` or `"restore pre-turn content"`).
    pub action: String,
}

/// Interactive state for the `/revert` confirmation & file-restoration preview modal.
#[derive(Debug, Clone)]
pub struct RevertOverlayState {
    /// Target turn id to revert from.
    pub target_turn_id: String,
    /// Selected revert scope (`ConversationAndFiles`, `FilesOnly`, `ConversationOnly`).
    pub scope: RevertScope,
    /// Preview of affected workspace files derived from `SnapshotManifest`s.
    pub affected_files: Vec<RevertPreviewItem>,
}

impl RevertOverlayState {
    /// Builds a [`RevertOverlayState`] from the session's recorded [`SnapshotManifest`]s for `reverted_turn_ids`.
    #[must_use]
    pub fn from_manifests(
        target_turn_id: String,
        manifests: &[SnapshotManifest],
        reverted_turn_ids: &[String],
    ) -> Self {
        let mut seen = std::collections::BTreeMap::new();
        for m in manifests
            .iter()
            .rev()
            .filter(|m| reverted_turn_ids.iter().any(|t| t == &m.turn_id))
        {
            for (path, pre) in &m.entries {
                let desc = match pre {
                    FilePreImage::Absent => "delete newly created file".to_string(),
                    FilePreImage::Existed { sha256, .. } => {
                        format!(
                            "restore pre-turn snapshot ({})",
                            &sha256[..8.min(sha256.len())]
                        )
                    }
                };
                seen.insert(path.clone(), desc);
            }
        }
        let affected_files = seen
            .into_iter()
            .map(|(path, action)| RevertPreviewItem { path, action })
            .collect();
        Self {
            target_turn_id,
            scope: RevertScope::ConversationAndFiles,
            affected_files,
        }
    }

    /// Cycles between `ConversationAndFiles` -> `FilesOnly` -> `ConversationOnly`.
    pub fn cycle_scope(&mut self) {
        self.scope = match self.scope {
            RevertScope::ConversationAndFiles => RevertScope::FilesOnly,
            RevertScope::FilesOnly => RevertScope::ConversationOnly,
            RevertScope::ConversationOnly => RevertScope::ConversationAndFiles,
        };
    }
}

/// Interactive state for the `/sessions` (`/resume`) recent-session browser modal.
#[derive(Debug, Clone)]
pub struct SessionsOverlayState {
    /// Indexed sessions ordered by `updated_at DESC`.
    pub sessions: Vec<SessionSummary>,
    /// Highlighted session row.
    pub selected: usize,
    /// When `true`, `Enter` forks a new child session instead of resuming in place.
    pub fork_mode: bool,
}

impl SessionsOverlayState {
    /// Creates a [`SessionsOverlayState`] from indexed sessions.
    #[must_use]
    pub fn new(sessions: Vec<SessionSummary>) -> Self {
        Self {
            sessions,
            selected: 0,
            fork_mode: false,
        }
    }

    /// Moves selection down.
    pub fn next(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1) % self.sessions.len();
        }
    }

    /// Moves selection up.
    pub fn prev(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = if self.selected == 0 {
                self.sessions.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    /// Toggles between `Resume` (`fork_mode = false`) and `Fork` (`fork_mode = true`).
    pub fn toggle_fork_mode(&mut self) {
        self.fork_mode = !self.fork_mode;
    }

    /// Returns the highlighted session identifier, if any.
    #[must_use]
    pub fn selected_session_id(&self) -> Option<&str> {
        self.sessions.get(self.selected).map(|s| s.id.as_str())
    }
}

/// Kind of active inline composer completion popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    /// Leading `/command` completion.
    SlashCommand,
    /// `@path/to/file` workspace file mention completion.
    WorkspaceFile,
}

/// Single ranked candidate in [`CompletionPopupState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionCandidate {
    /// Text inserted in place of `input_buffer[token_start..token_end]`.
    pub replacement: String,
    /// Display label (`"/model"` or `"@crates/tekvra-core/src/lib.rs"`).
    pub label: String,
    /// Short description shown next to `label`.
    pub description: String,
    /// Relevance score (higher is better).
    pub score: u32,
}

/// Inline composer fuzzy-completion popup state (`/` commands and `@file` mentions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionPopupState {
    /// Trigger kind (`SlashCommand` or `WorkspaceFile`).
    pub kind: CompletionKind,
    /// UTF-8 byte start of the active token (`/` or `@`).
    pub token_start: usize,
    /// UTF-8 byte end of the active token (`cursor_byte_pos`).
    pub token_end: usize,
    /// Raw query after `/` or `@`.
    pub query: String,
    /// Ranked completion candidates.
    pub candidates: Vec<CompletionCandidate>,
    /// Currently highlighted candidate index.
    pub selected: usize,
    /// True once the user explicitly navigated candidates via `Up`/`Down`.
    pub navigated: bool,
}

impl CompletionPopupState {
    /// Moves highlight to the next candidate.
    pub fn next(&mut self) {
        if !self.candidates.is_empty() {
            self.selected = (self.selected + 1) % self.candidates.len();
            self.navigated = true;
        }
    }

    /// Moves highlight to the previous candidate.
    pub fn prev(&mut self) {
        if !self.candidates.is_empty() {
            self.selected = if self.selected == 0 {
                self.candidates.len() - 1
            } else {
                self.selected - 1
            };
            self.navigated = true;
        }
    }

    /// Returns the currently highlighted candidate.
    #[must_use]
    pub fn selected_candidate(&self) -> Option<&CompletionCandidate> {
        self.candidates.get(self.selected)
    }
}

const BUILTIN_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("help", "Show available slash commands and shortcuts"),
    ("model", "Switch active provider/model"),
    ("mode", "Switch permission mode (plan|build|auto|yolo)"),
    ("mcp", "Show connected MCP servers and tools"),
    ("review", "Run read-only code review over Git diffs"),
    ("sessions", "Browse recent sessions to resume or fork"),
    ("resume", "Open interactive session picker or resume ID"),
    ("fork", "Fork a new branch from a session ID"),
    ("tasks", "Switch or steer active subagents (Ctrl-X)"),
    ("search", "Full-text search across session history"),
    ("revert", "Preview and revert conversation/files to turn"),
    ("goal", "Set or clear autonomous completion condition"),
    ("reload", "Hot-reload opt-in Luau plugins"),
    ("quit", "Exit Tekvra TUI"),
];

/// Returns `true` if `rel_path` points into `.git` or a sensitive credential file that MUST
/// never surface in `@file` TUI completions.
#[must_use]
pub fn is_excluded_mention_path(rel_path: &str) -> bool {
    let normalized = rel_path.replace('\\', "/");
    for seg in normalized.split('/') {
        if seg == ".git" {
            return true;
        }
    }
    let file_name = normalized
        .rsplit('/')
        .next()
        .unwrap_or(&normalized)
        .to_ascii_lowercase();
    file_name == ".env"
        || (file_name.starts_with(".env.")
            && !matches!(
                file_name.as_str(),
                ".env.example" | ".env.sample" | ".env.template"
            ))
        || PathHasSecretExt::matches(&file_name)
        || matches!(
            file_name.as_str(),
            "id_rsa" | "id_ed25519" | "id_ecdsa" | ".npmrc" | ".pypirc" | ".netrc"
        )
}

struct PathHasSecretExt;

impl PathHasSecretExt {
    fn matches(lower_file_name: &str) -> bool {
        lower_file_name.ends_with(".pem")
            || lower_file_name.ends_with(".key")
            || lower_file_name.ends_with(".p12")
            || lower_file_name.ends_with(".pfx")
    }
}

/// Computes the inline fuzzy-completion popup for `input[..cursor_byte_pos]`, if the cursor is
/// currently positioned inside a leading `/command` token or an `@file` mention token.
#[must_use]
pub fn build_completion_popup(
    input: &str,
    cursor_byte_pos: usize,
    workspace_files: &[String],
    extra_slash_commands: &[(String, String)],
    max_items: usize,
) -> Option<CompletionPopupState> {
    let pos = cursor_byte_pos.min(input.len());
    if !input.is_char_boundary(pos) {
        return None;
    }
    let before = &input[..pos];
    let token_start = before.rfind(char::is_whitespace).map_or(0, |idx| {
        idx + before[idx..].chars().next().map_or(1, char::len_utf8)
    });
    let token = &before[token_start..pos];

    // 1. Leading `/command` completion (only when `/` starts at byte 0 with no whitespace yet)
    if token_start == 0
        && let Some(query) = token.strip_prefix('/')
        && !query.contains('/')
    {
        let mut candidates = Vec::new();
        for &(name, desc) in BUILTIN_SLASH_COMMANDS {
            if let Some(score) = fuzzy_score_token(name, query) {
                candidates.push(CompletionCandidate {
                    replacement: format!("/{name} "),
                    label: format!("/{name}"),
                    description: desc.to_string(),
                    score,
                });
            }
        }
        for (name, desc) in extra_slash_commands {
            if BUILTIN_SLASH_COMMANDS.iter().any(|(b, _)| b == name) {
                continue;
            }
            if let Some(score) = fuzzy_score_token(name, query) {
                candidates.push(CompletionCandidate {
                    replacement: format!("/{name} "),
                    label: format!("/{name}"),
                    description: if desc.trim().is_empty() {
                        "Luau plugin command".to_string()
                    } else {
                        desc.clone()
                    },
                    score,
                });
            }
        }
        candidates.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
        candidates.truncate(max_items.max(1));
        if candidates.is_empty() {
            return None;
        }
        return Some(CompletionPopupState {
            kind: CompletionKind::SlashCommand,
            token_start,
            token_end: pos,
            query: query.to_string(),
            candidates,
            selected: 0,
            navigated: false,
        });
    }

    // 2. `@file` workspace path completion
    if let Some(query) = token.strip_prefix('@') {
        let mut candidates = Vec::new();
        for raw_path in workspace_files {
            if is_excluded_mention_path(raw_path) {
                continue;
            }
            let file_name = raw_path.rsplit('/').next().unwrap_or(raw_path.as_str());
            let path_score = fuzzy_score_token(raw_path, query);
            let name_bonus = fuzzy_score_token(file_name, query).map_or(0, |s| s / 2);
            if let Some(base) = path_score {
                candidates.push(CompletionCandidate {
                    replacement: format!("@{raw_path} "),
                    label: format!("@{raw_path}"),
                    description: "workspace file".to_string(),
                    score: base.saturating_add(name_bonus),
                });
            }
        }
        candidates.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.label.len().cmp(&b.label.len()))
                .then_with(|| a.label.cmp(&b.label))
        });
        candidates.truncate(max_items.max(1));
        if candidates.is_empty() {
            return None;
        }
        return Some(CompletionPopupState {
            kind: CompletionKind::WorkspaceFile,
            token_start,
            token_end: pos,
            query: query.to_string(),
            candidates,
            selected: 0,
            navigated: false,
        });
    }

    None
}

fn fuzzy_score_token(candidate: &str, query: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(10);
    }
    let cand_lower = candidate.to_ascii_lowercase();
    let q_lower = query.to_ascii_lowercase();

    if cand_lower == q_lower {
        return Some(2_000);
    }
    if cand_lower.starts_with(&q_lower) {
        let penalty = u32::try_from(cand_lower.len().min(200)).unwrap_or(200);
        return Some(1_500_u32.saturating_sub(penalty));
    }
    if let Some(pos) = cand_lower.find(&q_lower) {
        let penalty = u32::try_from(pos.min(300)).unwrap_or(300);
        return Some(1_000_u32.saturating_sub(penalty));
    }

    // Subsequence match with boundary bonus (`/`, `_`, `-`, `.`)
    let mut q_chars = q_lower.chars().peekable();
    let mut prev_boundary = true;
    let mut boundary_hits = 0_u32;
    for ch in cand_lower.chars() {
        if q_chars.peek() == Some(&ch) {
            let _ = q_chars.next();
            if prev_boundary {
                boundary_hits = boundary_hits.saturating_add(25);
            }
        }
        prev_boundary = matches!(ch, '/' | '_' | '-' | '.');
    }
    if q_chars.peek().is_none() {
        let len_penalty = u32::try_from(cand_lower.len().min(150)).unwrap_or(150);
        Some(
            300_u32
                .saturating_add(boundary_hits)
                .saturating_sub(len_penalty),
        )
    } else {
        None
    }
}

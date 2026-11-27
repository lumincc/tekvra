//! Modal overlays for scope-ladder approvals, `/tasks` (`Ctrl-X`) subagent navigation & live steering,
//! `/search` FTS5 history picker, and workspace file-revert preview.

use tekvra_core::SubagentEntry;
use tekvra_protocol::{
    ApprovalPrompt, ApprovalResolution, GrantLifetime, InputDeliveryMode, RevertScope,
};
use tekvra_storage::{FilePreImage, HistorySearchHit, SnapshotManifest};

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

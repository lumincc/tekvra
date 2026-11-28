//! Full-screen Alternate-Screen `ratatui` application state, key dispatcher, layout renderer,
//! and 60 FPS `crossterm` event loop.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use futures::StreamExt;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tekvra_core::{ReviewTarget, SessionHandle, SubagentEntry};
use tekvra_protocol::{
    ApprovalResolution, InputDeliveryMode, KeyChord, KeySpec, KeymapSpec, PermissionMode,
    RevertScope, ReviewFinding, SessionCommand, SessionEvent, TokenUsage,
};
use tekvra_storage::{RolloutRecord, StateIndex, WorkspaceSnapshotStore};

use crate::TuiError;
use crate::cards::{ToolCardState, render_tool_card};
use crate::markdown::render_markdown_lines;
use crate::modals::{
    ApprovalModalState, CompletionKind, CompletionPopupState, ModelPickerState, QuestionModalState,
    RevertOverlayState, SearchOverlayState, SessionsOverlayState, TasksOverlayState,
    TrustUiCommand, build_completion_popup, is_excluded_mention_path,
};

/// Single entry in the TUI transcript view (main session or subagent).
#[derive(Debug, Clone)]
pub enum TranscriptEntry {
    /// User prompt message.
    UserMessage(String),
    /// Streaming or completed assistant Markdown message.
    AssistantMessage {
        /// Item id.
        item_id: String,
        /// Accumulated reasoning summary.
        reasoning: String,
        /// Accumulated Markdown body.
        markdown: String,
    },
    /// Native Rust structured tool output card.
    ToolCard(ToolCardState),
    /// Visible steering / `/goal` / policy banner.
    SystemBanner {
        /// Category tag.
        tag: String,
        /// Message text.
        text: String,
    },
}

/// Currently active modal overlay above the full-screen transcript.
#[derive(Debug, Clone)]
pub enum ActiveOverlay {
    /// No modal open.
    None,
    /// Interactive scope-ladder permission approval modal.
    Approval(ApprovalModalState),
    /// `/tasks` (`Ctrl-X`) subagent switcher and live steerer.
    Tasks(TasksOverlayState),
    /// `/search` FTS5 history session picker.
    Search(SearchOverlayState),
    /// `/sessions` (`/resume`) recent session browser & fork picker.
    Sessions(SessionsOverlayState),
    /// `/revert` workspace file-restoration preview.
    Revert(RevertOverlayState),
    /// `question` tool prompt awaiting user answers (`UserInputRequested`).
    Question(QuestionModalState),
    /// `/model` interactive fuzzy model picker.
    ModelPicker(ModelPickerState),
}

/// High-level command emitted by [`TuiApp::handle_key`] to be executed by the async host loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiCommand {
    /// No external side effect required.
    Noop,
    /// Quit the TUI cleanly.
    Quit,
    /// Dispatch a [`SessionCommand`] to the main `SessionHandle`.
    Session(SessionCommand),
    /// Steer a specific subagent `task_id` via `SubagentManager`.
    SteerSubagent {
        /// Target subagent id.
        task_id: String,
        /// Steering prompt.
        prompt: String,
        /// `Guide` or `Replace`.
        delivery: InputDeliveryMode,
    },
    /// Open `/tasks` overlay refreshed from `SubagentManager`.
    OpenTasksOverlay,
    /// Open `/sessions` (`/resume`) interactive session browser refreshed from `StateIndex`.
    OpenSessionsOverlay,
    /// Switch active TUI session by resuming `session_id` (`fork = false`) or forking a child session (`fork = true`).
    SwitchSession {
        /// Target session identifier.
        session_id: String,
        /// `true` to fork a new session branch from `session_id`; `false` to resume `session_id`.
        fork: bool,
    },
    /// Execute FTS5 `/search <query>` against `StateIndex`.
    RunSearch {
        /// FTS5 query string.
        query: String,
    },
    /// Open `/revert <turn_id>` preview modal.
    OpenRevertPreview {
        /// Turn id to revert from.
        turn_id: String,
    },
    /// Trigger `/reload` on opt-in Luau plugin host.
    ReloadPlugins,
    /// Display live MCP server & tool inventory (`/mcp`).
    ShowMcpStatus,
    /// Run a read-only `/review` workflow against the selected Git diff target.
    RunReview {
        /// Selected review diff target.
        target: ReviewTarget,
    },
    /// Dispatch a custom slash command (`/<name> <args>`) to the opt-in Luau plugin host.
    InvokePluginCommand {
        /// Slash command name without leading `/`.
        name: String,
        /// Remaining argument string.
        args: String,
    },
    /// Export `text` to the terminal clipboard via ANSI OSC-52 (with native pipe fallback).
    CopyToClipboard {
        /// Text payload (latest fenced code block or full assistant reply).
        text: String,
        /// Short banner label (`"code block"` or `"reply"`).
        label: String,
    },
    /// Inspect or toggle workspace folder trust through the host trust store.
    RunTrust {
        /// Concrete trust action.
        action: TrustUiCommand,
    },
}

/// Boxed future returned by the host `/review` callback in [`TuiRuntimeHooks`].
pub type BoxReviewFuture =
    Pin<Box<dyn Future<Output = Result<(String, Vec<ReviewFinding>), String>> + Send>>;

/// Boxed callback invoked on `/<name> <args>` for opt-in Luau plugin commands (`Ok(None)` = unknown command).
pub type PluginCommandHandler = Box<dyn FnMut(&str, &str) -> Result<Option<String>, String> + Send>;

/// Boxed callback for `/trust <status|allow|revoke>`: maps the action to a status banner message.
pub type TrustCommandHandler = Box<dyn FnMut(TrustUiCommand) -> Result<String, String> + Send>;

/// Result returned when switching or forking a session in [`TuiRuntimeHooks::on_switch_session`].
pub type SwitchSessionOutcome = (SessionHandle, Vec<RolloutRecord>);

/// Provider of the `(name, description)` slash commands currently registered by Luau plugins.
pub type PluginSlashCommandsProvider = Box<dyn Fn() -> Vec<(String, String)> + Send>;

/// Boxed callback invoked when resuming or forking a session from `/sessions`.
pub type SwitchSessionHandler =
    Box<dyn FnMut(&str, bool) -> Result<SwitchSessionOutcome, String> + Send>;

/// Optional host callbacks wired into [`run_fullscreen_tui`].
#[derive(Default)]
pub struct TuiRuntimeHooks {
    /// Initial persisted rollout records when launching `tekvra resume` or `tekvra fork`.
    pub initial_records: Vec<RolloutRecord>,
    /// Pre-scanned non-sensitive workspace relative file paths for `@file` completion.
    pub workspace_files: Vec<String>,
    /// Provider of `(name, description)` slash commands registered by active Luau plugins
    /// (re-queried at startup and after every successful `/reload`).
    pub plugin_slash_commands: Option<PluginSlashCommandsProvider>,
    /// Provider of keybindings registered by active Luau plugins (same lifecycle).
    pub plugin_keymaps: Option<Box<dyn Fn() -> Vec<KeymapSpec> + Send>>,
    /// Discovered `(name, description)` SKILL.md packages for `/skill` completion.
    pub discovered_skills: Vec<(String, String)>,
    /// Selectable `(provider/model, description)` entries for the interactive `/model` picker.
    pub available_models: Vec<(String, String)>,
    /// Invoked on `/trust <status|allow|revoke>` against the host trust store.
    pub on_trust_command: Option<TrustCommandHandler>,
    /// Invoked on `/reload` to rebuild opt-in Luau plugins.
    pub on_reload_plugins: Option<Box<dyn FnMut() -> Result<String, String> + Send>>,
    /// Invoked on `/<name> <args>` to execute a plugin-registered slash command (`Ok(None)` = unknown command).
    pub on_plugin_command: Option<PluginCommandHandler>,
    /// Invoked on `/mcp` to format live MCP server and tool status.
    pub on_mcp_status: Option<Box<dyn FnMut() -> String + Send>>,

    /// `/memory` support: returns the project memory content plus its file path.
    pub on_memory_view: Option<Box<dyn FnMut() -> String + Send>>,
    /// Invoked on `/review` to execute a read-only diff review pass.
    pub on_run_review: Option<Box<dyn FnMut(ReviewTarget) -> BoxReviewFuture + Send>>,
    /// Invoked when the user selects a session to resume or fork in `/sessions`.
    pub on_switch_session: Option<SwitchSessionHandler>,
}

impl TuiRuntimeHooks {
    /// Pulls the current plugin slash commands and keymaps from their providers into `app`.
    /// Called once at startup and after every successful `/reload`.
    pub fn sync_plugin_registrations(&mut self, app: &mut TuiApp) {
        if let Some(provider) = &self.plugin_slash_commands {
            app.set_extra_slash_commands(provider());
        }
        if let Some(provider) = &self.plugin_keymaps {
            app.set_plugin_keymaps(provider());
        }
    }
}

/// Pure, deterministic full-screen TUI state machine (testable headlessly with `ratatui::backend::TestBackend`).
pub struct TuiApp {
    /// Main session id.
    pub session_id: String,
    /// Active model spec displayed in status bar.
    pub active_model: String,
    /// Active permission mode (`Plan`, `ReadOnly`, `Build`, `Auto`, `Yolo`).
    pub mode: PermissionMode,
    /// Active `/goal` condition badge.
    pub active_goal: Option<String>,
    /// Cumulative token/cache/cost metrics.
    pub usage: TokenUsage,
    /// Whether a turn is actively running.
    pub turn_running: bool,
    /// Latest MCP progress of the running turn (`mcp server/tool: progress[/total][ message]`);
    /// each `McpProgress` event replaces it and `TurnCompleted` clears it.
    pub mcp_progress: Option<String>,
    /// Currently viewed transcript (`None` = Main Session, `Some(task_id)` = Subagent transcript).
    pub viewing_subagent: Option<String>,
    /// Main session transcript entries.
    pub main_transcript: Vec<TranscriptEntry>,
    /// Cached subagent transcripts (`task_id -> entries`).
    pub subagent_transcripts: BTreeMap<String, Vec<TranscriptEntry>>,
    /// Index of the focused `ToolCard` in the active transcript (for `r` Raw/Filtered toggle and `c` collapse).
    pub selected_tool_card: Option<usize>,
    /// Vertical scroll offset in lines.
    pub scroll_offset: u16,
    /// Prompt composer buffer.
    pub input_buffer: String,
    /// UTF-8 char-aligned byte cursor position within `input_buffer`.
    pub cursor_byte_pos: usize,
    /// Cached non-sensitive workspace relative file paths for `@file` completion.
    pub workspace_files: Vec<String>,
    /// Extra `(name, description)` slash commands from active Luau plugins.
    pub extra_slash_commands: Vec<(String, String)>,
    /// Active plugin keybindings (`tekvra.keymap.set`), latest registration last.
    pub keymaps: Vec<KeymapSpec>,
    /// Discovered `(name, description)` SKILL.md packages for `/skill` completion.
    pub skills: Vec<(String, String)>,
    /// Selectable models for the interactive `/model` picker.
    pub available_models: Vec<(String, String)>,
    /// `/memory` support: returns the project memory content plus its file path.
    pub memory_view: Option<Box<dyn FnMut() -> String + Send>>,
    /// Context window (tokens) of the active model for utilization telemetry.
    pub context_window: u64,
    /// Active inline `/` or `@file` fuzzy completion popup.
    pub completion: Option<CompletionPopupState>,
    /// Active modal overlay.
    pub overlay: ActiveOverlay,
}

impl TuiApp {
    /// Creates a new [`TuiApp`] initialized with `session_id`, `active_model`, and `mode`.
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        active_model: impl Into<String>,
        mode: PermissionMode,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            active_model: active_model.into(),
            mode,
            active_goal: None,
            usage: TokenUsage::default(),
            turn_running: false,
            mcp_progress: None,
            viewing_subagent: None,
            main_transcript: Vec::new(),
            subagent_transcripts: BTreeMap::new(),
            selected_tool_card: None,
            scroll_offset: 0,
            input_buffer: String::new(),
            cursor_byte_pos: 0,
            workspace_files: Vec::new(),
            extra_slash_commands: Vec::new(),
            keymaps: Vec::new(),
            skills: Vec::new(),
            available_models: Vec::new(),
            memory_view: None,
            context_window: 0,
            completion: None,
            overlay: ActiveOverlay::None,
        }
    }

    /// Sets the cached workspace relative paths for `@file` completion (automatically filtering `.git` and `.env*` secrets).
    pub fn set_workspace_files(&mut self, files: Vec<String>) {
        let mut clean: Vec<String> = files
            .into_iter()
            .map(|p| p.replace('\\', "/"))
            .filter(|p| !p.is_empty() && !is_excluded_mention_path(p))
            .collect();
        clean.sort();
        clean.dedup();
        self.workspace_files = clean;
        self.refresh_completions();
    }

    /// Sets custom Luau plugin slash commands `(name, description)` for `/` completion.
    pub fn set_extra_slash_commands(&mut self, cmds: Vec<(String, String)>) {
        self.extra_slash_commands = cmds;
        self.refresh_completions();
    }

    /// Sets the active plugin keybindings; later registrations win on equal chords.
    pub fn set_plugin_keymaps(&mut self, keymaps: Vec<KeymapSpec>) {
        self.keymaps = keymaps;
    }

    /// Attaches the `/memory` viewer (project memory content + file path).
    pub fn set_memory_view(&mut self, cb: Box<dyn FnMut() -> String + Send>) {
        self.memory_view = Some(cb);
    }

    /// Sets discovered `(name, description)` SKILL.md packages for `/skill` completion.
    pub fn set_discovered_skills(&mut self, skills: Vec<(String, String)>) {
        self.skills = skills;
        self.refresh_completions();
    }

    /// Sets selectable models and the active model context window for telemetry.
    pub fn set_model_catalog(&mut self, models: Vec<(String, String)>, context_window: u64) {
        self.available_models = models;
        self.context_window = context_window;
    }

    /// Recomputes `self.completion` from the current `input_buffer` and `cursor_byte_pos`.
    pub fn refresh_completions(&mut self) {
        self.completion = build_completion_popup(
            &self.input_buffer,
            self.cursor_byte_pos,
            &self.workspace_files,
            &self.extra_slash_commands,
            &self.skills,
            8,
        );
    }

    /// Accepts the currently highlighted candidate in `self.completion`, inserting it at the UTF-8 token span.
    pub fn accept_completion(&mut self) -> bool {
        let Some(popup) = self.completion.take() else {
            return false;
        };
        let Some(cand) = popup.selected_candidate() else {
            return false;
        };
        if popup.token_start <= popup.token_end
            && popup.token_end <= self.input_buffer.len()
            && self.input_buffer.is_char_boundary(popup.token_start)
            && self.input_buffer.is_char_boundary(popup.token_end)
        {
            self.input_buffer
                .replace_range(popup.token_start..popup.token_end, &cand.replacement);
            self.cursor_byte_pos = popup.token_start + cand.replacement.len();
            true
        } else {
            false
        }
    }

    /// Replaces the active main session view with persisted [`RolloutRecord`]s (`resume` or `fork`).
    pub fn load_main_rollout(
        &mut self,
        session_id: impl Into<String>,
        active_model: impl Into<String>,
        records: &[RolloutRecord],
    ) {
        self.session_id = session_id.into();
        self.active_model = active_model.into();
        self.viewing_subagent = None;
        self.turn_running = false;
        self.scroll_offset = 0;
        self.selected_tool_card = None;
        self.main_transcript = rollout_records_to_entries(records);
    }

    fn clamped_cursor(&self) -> usize {
        let max = self.input_buffer.len();
        let pos = self.cursor_byte_pos.min(max);
        if self.input_buffer.is_char_boundary(pos) {
            pos
        } else {
            self.input_buffer
                .char_indices()
                .map(|(i, _)| i)
                .take_while(|&i| i < pos)
                .last()
                .unwrap_or(0)
        }
    }

    /// Moves the composer cursor one Unicode scalar value to the left.
    pub fn move_cursor_left(&mut self) {
        let pos = self.clamped_cursor();
        self.cursor_byte_pos = self.input_buffer[..pos]
            .char_indices()
            .next_back()
            .map_or(0, |(idx, _)| idx);
        self.refresh_completions();
    }

    /// Moves the composer cursor one Unicode scalar value to the right.
    pub fn move_cursor_right(&mut self) {
        let pos = self.clamped_cursor();
        if let Some(ch) = self.input_buffer[pos..].chars().next() {
            self.cursor_byte_pos = pos + ch.len_utf8();
        }
        self.refresh_completions();
    }

    /// Moves the composer cursor to the start of `input_buffer`.
    pub fn move_cursor_home(&mut self) {
        self.cursor_byte_pos = 0;
        self.refresh_completions();
    }

    /// Moves the composer cursor to the end of `input_buffer`.
    pub fn move_cursor_end(&mut self) {
        self.cursor_byte_pos = self.input_buffer.len();
        self.refresh_completions();
    }

    /// Inserts `ch` at the current UTF-8 cursor boundary.
    pub fn insert_char_at_cursor(&mut self, ch: char) {
        let pos = self.clamped_cursor();
        self.input_buffer.insert(pos, ch);
        self.cursor_byte_pos = pos + ch.len_utf8();
        self.refresh_completions();
    }

    /// Deletes the Unicode scalar immediately preceding the cursor.
    pub fn backspace_at_cursor(&mut self) {
        let pos = self.clamped_cursor();
        if let Some((prev_idx, _)) = self.input_buffer[..pos].char_indices().next_back() {
            self.input_buffer.remove(prev_idx);
            self.cursor_byte_pos = prev_idx;
            self.refresh_completions();
        }
    }

    /// Deletes the Unicode scalar under the cursor.
    pub fn delete_at_cursor(&mut self) {
        let pos = self.clamped_cursor();
        if pos < self.input_buffer.len() {
            self.input_buffer.remove(pos);
            self.cursor_byte_pos = pos;
            self.refresh_completions();
        }
    }

    /// Records the outcome of a custom Luau slash command into the main transcript.
    pub fn apply_plugin_command_result(
        &mut self,
        name: &str,
        result: Result<Option<String>, String>,
    ) {
        match result {
            Ok(Some(output)) => {
                let text = if output.trim().is_empty() {
                    format!("Plugin command `/{name}` completed.")
                } else {
                    output
                };
                self.main_transcript.push(TranscriptEntry::SystemBanner {
                    tag: "plugin".to_string(),
                    text,
                });
            }
            Ok(None) => {
                self.main_transcript.push(TranscriptEntry::SystemBanner {
                    tag: "error".to_string(),
                    text: format!(
                        "Unknown slash command `/{name}` (run `/help` to list available commands)"
                    ),
                });
            }
            Err(err) => {
                self.main_transcript.push(TranscriptEntry::SystemBanner {
                    tag: "error".to_string(),
                    text: format!("Plugin command `/{name}` failed: {err}"),
                });
            }
        }
    }

    /// Replaces the MCP progress status line from one `McpProgress` event (latest wins).
    pub fn apply_mcp_progress(
        &mut self,
        server: &str,
        tool: &str,
        progress: f64,
        total: Option<f64>,
        message: Option<&str>,
    ) {
        self.mcp_progress = Some(format_mcp_progress(server, tool, progress, total, message));
    }

    /// Appends a formatted MCP server/tool status banner to the main transcript.
    pub fn apply_mcp_status(&mut self, status_text: impl Into<String>) {
        self.main_transcript.push(TranscriptEntry::SystemBanner {
            tag: "mcp".to_string(),
            text: status_text.into(),
        });
    }

    /// Appends structured `/review` findings into the main transcript.
    pub fn apply_review_findings(&mut self, summary: &str, findings: &[ReviewFinding]) {
        let mut lines = vec![summary.to_string()];
        for f in findings {
            lines.push(format!(
                "[{}] {}:{}-{} — {}: {}",
                f.severity,
                f.path.display(),
                f.start_line,
                f.end_line,
                f.title,
                f.body
            ));
        }
        self.main_transcript.push(TranscriptEntry::SystemBanner {
            tag: "review".to_string(),
            text: lines.join(" | "),
        });
    }

    /// Borrows the currently displayed transcript (`main_transcript` or the selected subagent's transcript).
    #[must_use]
    pub fn active_transcript(&self) -> &[TranscriptEntry] {
        if let Some(tid) = &self.viewing_subagent {
            self.subagent_transcripts
                .get(tid)
                .map_or(&[], Vec::as_slice)
        } else {
            &self.main_transcript
        }
    }

    fn active_transcript_mut(&mut self) -> &mut Vec<TranscriptEntry> {
        if let Some(tid) = self.viewing_subagent.clone() {
            self.subagent_transcripts.entry(tid).or_default()
        } else {
            &mut self.main_transcript
        }
    }

    /// Populates a subagent's transcript from persisted `.jsonl` [`RolloutRecord`]s.
    pub fn load_subagent_rollout(&mut self, task_id: &str, records: &[RolloutRecord]) {
        self.subagent_transcripts
            .insert(task_id.to_string(), rollout_records_to_entries(records));
    }

    /// Applies an incoming [`SessionEvent`] from the main session (or subagent stream) to the UI state.
    pub fn apply_session_event(&mut self, event: SessionEvent) {
        let is_subagent = event_thread_id(&event)
            .is_some_and(|tid| tid != self.session_id && tid.starts_with("task_"));
        let target_tid = event_thread_id(&event).map(str::to_string);

        match event {
            SessionEvent::TurnStarted { .. } if !is_subagent => {
                self.turn_running = true;
            }
            SessionEvent::ItemStarted { item_id, kind, .. } if kind == "agent_message" => {
                let list = self.transcript_for_thread_mut(target_tid.as_deref());
                list.push(TranscriptEntry::AssistantMessage {
                    item_id,
                    reasoning: String::new(),
                    markdown: String::new(),
                });
            }
            SessionEvent::ItemDelta {
                item_id,
                channel,
                delta,
                ..
            } => {
                let list = self.transcript_for_thread_mut(target_tid.as_deref());
                if let Some(TranscriptEntry::AssistantMessage {
                    reasoning,
                    markdown,
                    ..
                }) = list.iter_mut().rev().find(
                    |e| matches!(e, TranscriptEntry::AssistantMessage { item_id: id, .. } if id == &item_id),
                ) {
                    if channel == "reasoning_summary" {
                        reasoning.push_str(&delta);
                    } else {
                        markdown.push_str(&delta);
                    }
                }
            }
            SessionEvent::ItemCompleted {
                item_id,
                text,
                tool_output: Some(output),
                ..
            } => {
                let tool_name = infer_tool_label(&output);
                let list = self.transcript_for_thread_mut(target_tid.as_deref());
                list.push(TranscriptEntry::ToolCard(ToolCardState::new(
                    item_id, tool_name, output,
                )));
                let idx = list.len() - 1;
                if self.viewing_subagent.as_deref()
                    == target_tid.as_deref().filter(|t| *t != self.session_id)
                {
                    self.selected_tool_card = Some(idx);
                }
                let _ = text;
            }
            SessionEvent::ItemCompleted {
                item_id,
                text,
                tool_output: None,
                ..
            } => {
                let list = self.transcript_for_thread_mut(target_tid.as_deref());
                if let Some(TranscriptEntry::AssistantMessage { markdown, .. }) = list
                    .iter_mut()
                    .rev()
                    .find(|e| matches!(e, TranscriptEntry::AssistantMessage { item_id: id, .. } if id == &item_id))
                    && markdown.is_empty()
                {
                    *markdown = text;
                }
            }
            SessionEvent::SteeringInjected {
                reason, message, ..
            } => {
                let list = self.transcript_for_thread_mut(target_tid.as_deref());
                list.push(TranscriptEntry::SystemBanner {
                    tag: reason,
                    text: message,
                });
            }
            SessionEvent::ApprovalRequested { prompt, .. } => {
                self.overlay = ActiveOverlay::Approval(ApprovalModalState::new(prompt));
            }
            SessionEvent::UserInputRequested {
                turn_id,
                item_id,
                questions,
                ..
            } if !is_subagent => {
                self.overlay =
                    ActiveOverlay::Question(QuestionModalState::new(item_id, turn_id, questions));
            }
            SessionEvent::TokenUsageUpdated { usage, .. } if !is_subagent => {
                self.usage = usage;
            }
            SessionEvent::CompactionCompleted {
                generation,
                before_tokens,
                after_tokens,
                ..
            } if !is_subagent => {
                self.apply_compaction_banner(generation, before_tokens, after_tokens);
            }
            SessionEvent::ConfigWarning { summary, details } => {
                let msg = match details {
                    Some(d) => format!("{summary}: {d}"),
                    None => summary,
                };
                self.main_transcript.push(TranscriptEntry::SystemBanner {
                    tag: "warning".to_string(),
                    text: msg,
                });
            }
            SessionEvent::TurnCompleted { .. } if !is_subagent => {
                self.turn_running = false;
                self.mcp_progress = None;
            }
            SessionEvent::McpProgress {
                server,
                tool,
                progress,
                total,
                message,
                ..
            } if !is_subagent => {
                self.apply_mcp_progress(&server, &tool, progress, total, message.as_deref());
            }
            _ => {}
        }
    }

    fn transcript_for_thread_mut(&mut self, thread_id: Option<&str>) -> &mut Vec<TranscriptEntry> {
        match thread_id {
            Some(tid) if tid != self.session_id => self
                .subagent_transcripts
                .entry(tid.to_string())
                .or_default(),
            _ => &mut self.main_transcript,
        }
    }

    /// Opens the `/tasks` (`Ctrl-X`) overlay populated with `entries`.
    pub fn open_tasks_overlay(&mut self, entries: Vec<SubagentEntry>) {
        self.overlay = ActiveOverlay::Tasks(TasksOverlayState::new(entries));
    }

    /// Opens the `/sessions` (`/resume`) overlay populated with `sessions`.
    pub fn open_sessions_overlay(&mut self, sessions: Vec<tekvra_storage::SessionSummary>) {
        self.overlay = ActiveOverlay::Sessions(SessionsOverlayState::new(sessions));
    }

    /// Handles a mouse event: wheel scrolling adjusts the transcript viewport by 3 lines
    /// without touching the composer buffer or cursor position.
    pub fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> TuiCommand {
        use crossterm::event::MouseEventKind;
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_offset = self.scroll_offset.saturating_sub(3);
            }
            MouseEventKind::ScrollDown => {
                self.scroll_offset = self.scroll_offset.saturating_add(3);
            }
            _ => {}
        }
        TuiCommand::Noop
    }

    /// Appends a token-delta banner for a finished [`SessionEvent::CompactionCompleted`].
    pub fn apply_compaction_banner(&mut self, generation: u64, before: u64, after: u64) {
        let saved = before.saturating_sub(after);
        let pct = if before > 0 {
            (saved as f64 / before as f64) * 100.0
        } else {
            0.0
        };
        self.main_transcript.push(TranscriptEntry::SystemBanner {
            tag: "compact".to_string(),
            text: format!(
                "Context compacted to generation {generation}: {before} -> {after} estimated tokens (saved {saved}, {pct:.0}%)"
            ),
        });
    }

    /// Appends a clipboard-export confirmation banner.
    pub fn apply_copy_result(&mut self, label: &str, byte_len: usize) {
        self.main_transcript.push(TranscriptEntry::SystemBanner {
            tag: "copy".to_string(),
            text: format!(
                "Copied {label} ({byte_len} bytes) to the terminal clipboard via OSC-52."
            ),
        });
    }

    /// Returns the Markdown body of the most recent assistant message, if any.
    #[must_use]
    pub fn latest_assistant_markdown(&self) -> Option<String> {
        self.main_transcript
            .iter()
            .rev()
            .find_map(|entry| match entry {
                TranscriptEntry::AssistantMessage { markdown, .. } => {
                    (!markdown.trim().is_empty()).then(|| markdown.clone())
                }
                _ => None,
            })
    }

    /// Handles a single keyboard event and returns the resulting [`TuiCommand`].
    pub fn handle_key(&mut self, key: KeyEvent) -> TuiCommand {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.turn_running {
                return TuiCommand::Session(SessionCommand::InterruptTurn);
            }
            return TuiCommand::Quit;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('x') {
            return TuiCommand::OpenTasksOverlay;
        }

        match &mut self.overlay {
            ActiveOverlay::Approval(modal) => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    modal.next_rung();
                    TuiCommand::Noop
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    modal.prev_rung();
                    TuiCommand::Noop
                }
                KeyCode::Tab => {
                    modal.cycle_lifetime();
                    TuiCommand::Noop
                }
                KeyCode::Enter | KeyCode::Char('y') => {
                    let approval_id = modal.prompt.approval_id.clone();
                    if let Some(resolution) = modal.confirm_approve() {
                        self.overlay = ActiveOverlay::None;
                        TuiCommand::Session(SessionCommand::ResolveApproval {
                            approval_id,
                            resolution,
                        })
                    } else {
                        TuiCommand::Noop
                    }
                }
                KeyCode::Esc | KeyCode::Char('n') => {
                    let approval_id = modal.prompt.approval_id.clone();
                    let resolution: ApprovalResolution = modal.deny();
                    self.overlay = ActiveOverlay::None;
                    TuiCommand::Session(SessionCommand::ResolveApproval {
                        approval_id,
                        resolution,
                    })
                }
                _ => TuiCommand::Noop,
            },
            ActiveOverlay::Tasks(tasks) => match key.code {
                KeyCode::Esc => {
                    self.overlay = ActiveOverlay::None;
                    TuiCommand::Noop
                }
                KeyCode::Down => {
                    tasks.next();
                    TuiCommand::Noop
                }
                KeyCode::Up => {
                    tasks.prev();
                    TuiCommand::Noop
                }
                KeyCode::Tab => {
                    tasks.toggle_delivery_mode();
                    TuiCommand::Noop
                }
                KeyCode::Backspace => {
                    tasks.steer_input.pop();
                    TuiCommand::Noop
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    tasks.steer_input.push(ch);
                    TuiCommand::Noop
                }
                KeyCode::Enter => {
                    let selected_tid = tasks.selected_task_id().map(str::to_string);
                    let steer_prompt = tasks.steer_input.trim().to_string();
                    let delivery = tasks.steer_delivery;
                    self.viewing_subagent.clone_from(&selected_tid);
                    self.overlay = ActiveOverlay::None;
                    if let (Some(task_id), false) = (selected_tid, steer_prompt.is_empty()) {
                        TuiCommand::SteerSubagent {
                            task_id,
                            prompt: steer_prompt,
                            delivery,
                        }
                    } else {
                        TuiCommand::Noop
                    }
                }
                _ => TuiCommand::Noop,
            },
            ActiveOverlay::Search(search) => match key.code {
                KeyCode::Esc => {
                    self.overlay = ActiveOverlay::None;
                    TuiCommand::Noop
                }
                KeyCode::Down if !search.hits.is_empty() => {
                    search.selected = (search.selected + 1) % search.hits.len();
                    TuiCommand::Noop
                }
                KeyCode::Up if !search.hits.is_empty() => {
                    search.selected = if search.selected == 0 {
                        search.hits.len() - 1
                    } else {
                        search.selected - 1
                    };
                    TuiCommand::Noop
                }
                KeyCode::Enter => {
                    let target = search
                        .hits
                        .get(search.selected)
                        .map(|hit| hit.session_id.clone());
                    self.overlay = ActiveOverlay::None;
                    match target {
                        Some(session_id) => TuiCommand::SwitchSession {
                            session_id,
                            fork: false,
                        },
                        None => TuiCommand::Noop,
                    }
                }
                _ => TuiCommand::Noop,
            },
            ActiveOverlay::Sessions(sessions) => match key.code {
                KeyCode::Esc => {
                    self.overlay = ActiveOverlay::None;
                    TuiCommand::Noop
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    sessions.next();
                    TuiCommand::Noop
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    sessions.prev();
                    TuiCommand::Noop
                }
                KeyCode::Tab | KeyCode::Char('f') => {
                    sessions.toggle_fork_mode();
                    TuiCommand::Noop
                }
                KeyCode::Enter => {
                    let target = sessions.selected_session_id().map(str::to_string);
                    let fork = sessions.fork_mode;
                    self.overlay = ActiveOverlay::None;
                    match target {
                        Some(session_id) => TuiCommand::SwitchSession { session_id, fork },
                        None => TuiCommand::Noop,
                    }
                }
                _ => TuiCommand::Noop,
            },
            ActiveOverlay::Question(modal) => match key.code {
                KeyCode::Esc => {
                    // Dismissing still resolves the pending prompt (with whatever
                    // drafts exist) so the turn never hangs on a closed modal.
                    let item_id = modal.item_id.clone();
                    let answers = modal.answers();
                    self.overlay = ActiveOverlay::None;
                    TuiCommand::Session(SessionCommand::ResolveUserInput { item_id, answers })
                }
                KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                    modal.next();
                    TuiCommand::Noop
                }
                KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                    modal.prev();
                    TuiCommand::Noop
                }
                KeyCode::Backspace => {
                    modal.backspace();
                    TuiCommand::Noop
                }
                KeyCode::Enter => {
                    let item_id = modal.item_id.clone();
                    let answers = modal.answers();
                    self.overlay = ActiveOverlay::None;
                    TuiCommand::Session(SessionCommand::ResolveUserInput { item_id, answers })
                }
                KeyCode::Char(ch) => {
                    modal.type_char(ch);
                    TuiCommand::Noop
                }
                _ => TuiCommand::Noop,
            },
            ActiveOverlay::Revert(rev) => match key.code {
                KeyCode::Esc => {
                    self.overlay = ActiveOverlay::None;
                    TuiCommand::Noop
                }
                KeyCode::Tab => {
                    rev.cycle_scope();
                    TuiCommand::Noop
                }
                KeyCode::Enter => {
                    let target_id = rev.target_turn_id.clone();
                    let scope: RevertScope = rev.scope;
                    self.overlay = ActiveOverlay::None;
                    TuiCommand::Session(SessionCommand::RevertTo { target_id, scope })
                }
                _ => TuiCommand::Noop,
            },
            ActiveOverlay::ModelPicker(picker) => match key.code {
                KeyCode::Esc => {
                    self.overlay = ActiveOverlay::None;
                    TuiCommand::Noop
                }
                KeyCode::Down => {
                    picker.next();
                    TuiCommand::Noop
                }
                KeyCode::Up => {
                    picker.prev();
                    TuiCommand::Noop
                }
                KeyCode::Backspace => {
                    picker.pop_filter_char();
                    TuiCommand::Noop
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    picker.push_filter_char(ch);
                    TuiCommand::Noop
                }
                KeyCode::Enter => {
                    let Some(model) = picker.selected_model() else {
                        return TuiCommand::Noop;
                    };
                    self.overlay = ActiveOverlay::None;
                    self.active_model = model.clone();
                    self.main_transcript.push(TranscriptEntry::SystemBanner {
                        tag: "model".to_string(),
                        text: format!("Switched active model to `{model}`"),
                    });
                    TuiCommand::Session(SessionCommand::SwitchModel { model })
                }
                _ => TuiCommand::Noop,
            },
            ActiveOverlay::None => self.handle_normal_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> TuiCommand {
        // Quick card toggles when input buffer is empty!
        if self.input_buffer.is_empty() {
            match key.code {
                KeyCode::Char('r') => {
                    if let Some(idx) = self.selected_tool_card
                        && let Some(TranscriptEntry::ToolCard(card)) =
                            self.active_transcript_mut().get_mut(idx)
                    {
                        card.toggle_raw_output();
                        return TuiCommand::Noop;
                    }
                }
                KeyCode::Char('z') => {
                    if let Some(idx) = self.selected_tool_card
                        && let Some(TranscriptEntry::ToolCard(card)) =
                            self.active_transcript_mut().get_mut(idx)
                    {
                        card.toggle_collapsed();
                        return TuiCommand::Noop;
                    }
                }
                KeyCode::BackTab => {
                    self.mode = match self.mode {
                        PermissionMode::Plan => PermissionMode::Build,
                        PermissionMode::Build => PermissionMode::Auto,
                        PermissionMode::Auto => PermissionMode::Yolo,
                        PermissionMode::Yolo | PermissionMode::ReadOnly => PermissionMode::Plan,
                    };
                    return TuiCommand::Session(SessionCommand::SetMode { mode: self.mode });
                }
                _ => {}
            }
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('j') {
            self.insert_char_at_cursor('\n');
            return TuiCommand::Noop;
        }
        if key.code == KeyCode::Enter
            && !self.turn_running
            && (key.modifiers.contains(KeyModifiers::ALT)
                || key.modifiers.contains(KeyModifiers::SHIFT))
        {
            self.insert_char_at_cursor('\n');
            return TuiCommand::Noop;
        }

        // Plugin keymaps: a registered chord dispatches its slash command directly, without
        // touching the composer draft (open overlays never reach this handler).
        if let Some(spec) = self.match_keymap(key) {
            self.completion = None;
            return self.dispatch_slash_input(&spec.command, false);
        }

        // Inline `/` and `@file` fuzzy completion popup keyboard navigation
        if let Some(popup) = &mut self.completion {
            match key.code {
                KeyCode::Down => {
                    popup.next();
                    return TuiCommand::Noop;
                }
                KeyCode::Up => {
                    popup.prev();
                    return TuiCommand::Noop;
                }
                KeyCode::Esc => {
                    self.completion = None;
                    return TuiCommand::Noop;
                }
                KeyCode::Tab => {
                    let _ = self.accept_completion();
                    return TuiCommand::Noop;
                }
                KeyCode::Enter
                    if popup.kind == CompletionKind::WorkspaceFile
                        || popup.kind == CompletionKind::SkillName
                        || popup.navigated =>
                {
                    let _ = self.accept_completion();
                    return TuiCommand::Noop;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
                TuiCommand::Noop
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
                TuiCommand::Noop
            }
            KeyCode::Left => {
                self.move_cursor_left();
                TuiCommand::Noop
            }
            KeyCode::Right => {
                self.move_cursor_right();
                TuiCommand::Noop
            }
            KeyCode::Home => {
                self.move_cursor_home();
                TuiCommand::Noop
            }
            KeyCode::End => {
                self.move_cursor_end();
                TuiCommand::Noop
            }
            KeyCode::Backspace => {
                self.backspace_at_cursor();
                TuiCommand::Noop
            }
            KeyCode::Delete => {
                self.delete_at_cursor();
                TuiCommand::Noop
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_char_at_cursor(ch);
                TuiCommand::Noop
            }
            KeyCode::Enter => {
                self.completion = None;
                let submitted = std::mem::take(&mut self.input_buffer);
                self.cursor_byte_pos = 0;
                let trimmed = submitted.trim();
                if trimmed.is_empty() {
                    return TuiCommand::Noop;
                }
                self.dispatch_slash_input(trimmed, key.modifiers.contains(KeyModifiers::ALT))
            }
            _ => TuiCommand::Noop,
        }
    }

    /// Dispatches one trimmed composer line exactly as pressing <Enter> would: built-in slash
    /// commands, plugin commands, steering, or a new prompt (`alt` selects Alt+Enter delivery).
    /// Shared with plugin keymaps, which dispatch a bound command without touching the draft.
    fn dispatch_slash_input(&mut self, trimmed: &str, alt: bool) -> TuiCommand {
        if trimmed.is_empty() {
            return TuiCommand::Noop;
        }
        if trimmed == "/quit" || trimmed == "/exit" {
            return TuiCommand::Quit;
        }
        if trimmed == "/help" {
            self.main_transcript.push(TranscriptEntry::SystemBanner {
                tag: "help".to_string(),
                text: self.help_text(),
            });
            return TuiCommand::Noop;
        }
        if trimmed == "/sessions" || trimmed == "/resume" {
            return TuiCommand::OpenSessionsOverlay;
        }
        if trimmed == "/undo" {
            self.main_transcript.push(TranscriptEntry::SystemBanner {
                tag: "undo".to_string(),
                text: "Undo requested: reverting the most recent turn (conversation + workspace files).".to_string(),
            });
            return TuiCommand::Session(SessionCommand::UndoLastTurn {
                scope: RevertScope::ConversationAndFiles,
            });
        }
        if trimmed == "/compact" {
            self.main_transcript.push(TranscriptEntry::SystemBanner {
                tag: "compact".to_string(),
                text: "Compaction requested: summarizing older context into a new generation."
                    .to_string(),
            });
            return TuiCommand::Session(SessionCommand::Compact);
        }
        if trimmed == "/trust" || trimmed.starts_with("/trust ") {
            let arg = trimmed
                .strip_prefix("/trust")
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            let action = match arg.as_str() {
                "" | "status" => TrustUiCommand::Status,
                "allow" | "grant" => TrustUiCommand::Allow,
                "revoke" | "deny" => TrustUiCommand::Revoke,
                other => {
                    self.main_transcript.push(TranscriptEntry::SystemBanner {
                        tag: "error".to_string(),
                        text: format!("Usage: `/trust <status|allow|revoke>` (got `{other}`)"),
                    });
                    return TuiCommand::Noop;
                }
            };
            return TuiCommand::RunTrust { action };
        }
        if trimmed == "/copy" || trimmed.starts_with("/copy ") {
            let arg = trimmed.strip_prefix("/copy").unwrap_or_default().trim();
            let Some(markdown) = self.latest_assistant_markdown() else {
                self.main_transcript.push(TranscriptEntry::SystemBanner {
                    tag: "error".to_string(),
                    text: "Nothing to copy yet: no completed assistant reply.".to_string(),
                });
                return TuiCommand::Noop;
            };
            let (text, label) = if arg == "code" {
                match extract_last_fenced_code_block(&markdown) {
                    Some(code) => (code, "code block".to_string()),
                    None => {
                        self.main_transcript.push(TranscriptEntry::SystemBanner {
                            tag: "error".to_string(),
                            text: "No fenced code block found in the latest reply.".to_string(),
                        });
                        return TuiCommand::Noop;
                    }
                }
            } else {
                (markdown, "reply".to_string())
            };
            return TuiCommand::CopyToClipboard { text, label };
        }
        if let Some(rest) = trimmed.strip_prefix("/resume ") {
            let sid = rest.trim();
            if !sid.is_empty() {
                return TuiCommand::SwitchSession {
                    session_id: sid.to_string(),
                    fork: false,
                };
            }
        }
        if let Some(rest) = trimmed.strip_prefix("/fork ") {
            let sid = rest.trim();
            if !sid.is_empty() {
                return TuiCommand::SwitchSession {
                    session_id: sid.to_string(),
                    fork: true,
                };
            }
        }
        if trimmed == "/tasks" {
            return TuiCommand::OpenTasksOverlay;
        }
        if trimmed == "/reload" {
            return TuiCommand::ReloadPlugins;
        }
        if trimmed == "/mcp" {
            return TuiCommand::ShowMcpStatus;
        }
        if trimmed == "/memory" || trimmed == "/memory edit" {
            let edit = trimmed == "/memory edit";
            let text = match self.memory_view.as_mut() {
                Some(cb) => cb(),
                None => "Project memory unavailable (no memory hook attached).".to_string(),
            };
            if edit {
                // Purely informational: MEMORY.md is plain Markdown, edit it in any editor.
                let path = text.lines().next().unwrap_or("<unknown path>").to_string();
                self.main_transcript.push(TranscriptEntry::SystemBanner {
                    tag: "memory".to_string(),
                    text: format!("Edit {path} in your editor; it is re-read at session start."),
                });
            } else {
                self.main_transcript.push(TranscriptEntry::SystemBanner {
                    tag: "memory".to_string(),
                    text,
                });
            }
            return TuiCommand::Noop;
        }
        if trimmed == "/model" || trimmed.starts_with("/model ") {
            let arg = trimmed.strip_prefix("/model").unwrap_or_default().trim();
            if arg.is_empty() {
                if self.available_models.is_empty() {
                    self.main_transcript.push(TranscriptEntry::SystemBanner {
                        tag: "model".to_string(),
                        text: format!("Active model: `{}` (no model catalog available for the interactive picker)", self.active_model),
                    });
                } else {
                    self.overlay = ActiveOverlay::ModelPicker(ModelPickerState::new(
                        self.available_models.clone(),
                    ));
                }
                return TuiCommand::Noop;
            }
            self.active_model = arg.to_string();
            self.main_transcript.push(TranscriptEntry::SystemBanner {
                tag: "model".to_string(),
                text: format!("Switched active model to `{arg}`"),
            });
            return TuiCommand::Session(SessionCommand::SwitchModel {
                model: arg.to_string(),
            });
        }
        if trimmed == "/mode" || trimmed.starts_with("/mode ") {
            let arg = trimmed
                .strip_prefix("/mode")
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            let parsed = match arg.as_str() {
                "plan" => Some(PermissionMode::Plan),
                "readonly" | "read-only" => Some(PermissionMode::ReadOnly),
                "build" => Some(PermissionMode::Build),
                "auto" => Some(PermissionMode::Auto),
                "yolo" => Some(PermissionMode::Yolo),
                _ => None,
            };
            let Some(new_mode) = parsed else {
                self.main_transcript.push(TranscriptEntry::SystemBanner {
                    tag: "error".to_string(),
                    text: "Usage: `/mode <plan|build|auto|yolo>`".to_string(),
                });
                return TuiCommand::Noop;
            };
            self.mode = new_mode;
            self.main_transcript.push(TranscriptEntry::SystemBanner {
                tag: "mode".to_string(),
                text: format!("Switched permission mode to `{new_mode:?}`"),
            });
            return TuiCommand::Session(SessionCommand::SetMode { mode: new_mode });
        }
        if trimmed == "/review" || trimmed.starts_with("/review ") {
            let rest = trimmed.strip_prefix("/review").unwrap_or_default().trim();
            let target = parse_tui_review_target(rest);
            return TuiCommand::RunReview { target };
        }
        if trimmed == "/goal" || trimmed.starts_with("/goal ") {
            let cond = trimmed.strip_prefix("/goal").unwrap_or_default().trim();
            let opt = (!cond.is_empty() && cond != "clear").then(|| cond.to_string());
            self.active_goal.clone_from(&opt);
            return TuiCommand::Session(SessionCommand::SetGoal { condition: opt });
        }
        if trimmed == "/search" || trimmed.starts_with("/search ") {
            let q = trimmed.strip_prefix("/search").unwrap_or_default().trim();
            return TuiCommand::RunSearch {
                query: q.to_string(),
            };
        }
        if trimmed == "/revert" || trimmed.starts_with("/revert ") {
            let tid = trimmed.strip_prefix("/revert").unwrap_or_default().trim();
            return TuiCommand::OpenRevertPreview {
                turn_id: tid.to_string(),
            };
        }
        // `/skill <name> [prompt]` is expanded by the session runner, not the plugin host.
        if trimmed == "/skill" || trimmed.starts_with("/skill ") {
            let arg = trimmed.strip_prefix("/skill").unwrap_or_default().trim();
            if arg.is_empty() {
                self.main_transcript.push(TranscriptEntry::SystemBanner {
                    tag: "error".to_string(),
                    text: "Usage: `/skill <name> [prompt]`".to_string(),
                });
                return TuiCommand::Noop;
            }
            self.main_transcript
                .push(TranscriptEntry::UserMessage(trimmed.to_string()));
            let delivery = if self.turn_running {
                InputDeliveryMode::Guide
            } else {
                InputDeliveryMode::Next
            };
            return TuiCommand::Session(SessionCommand::SubmitInput {
                prompt: trimmed.to_string(),
                delivery,
                images: Vec::new(),
            });
        }
        if let Some(after_slash) = trimmed.strip_prefix('/') {
            let mut parts = after_slash.splitn(2, char::is_whitespace);
            let cmd_name = parts.next().unwrap_or_default();
            let cmd_args = parts.next().unwrap_or_default().trim();
            if !cmd_name.is_empty()
                && !cmd_name.contains('/')
                && cmd_name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            {
                return TuiCommand::InvokePluginCommand {
                    name: cmd_name.to_string(),
                    args: cmd_args.to_string(),
                };
            }
        }

        if let Some(sub_tid) = self.viewing_subagent.clone() {
            return TuiCommand::SteerSubagent {
                task_id: sub_tid,
                prompt: trimmed.to_string(),
                delivery: InputDeliveryMode::Guide,
            };
        }

        self.main_transcript
            .push(TranscriptEntry::UserMessage(trimmed.to_string()));
        let delivery = if self.turn_running {
            if alt {
                InputDeliveryMode::Replace
            } else {
                InputDeliveryMode::Guide
            }
        } else {
            InputDeliveryMode::Next
        };
        TuiCommand::Session(SessionCommand::SubmitInput {
            prompt: trimmed.to_string(),
            delivery,
            images: Vec::new(),
        })
    }

    /// Plugin binding for `key`, if any: the last registration whose mode matches the composer
    /// state wins; matching ignores `Shift` and letter case.
    fn match_keymap(&self, key: KeyEvent) -> Option<KeymapSpec> {
        let pressed = Self::chord_of(key)?;
        let draft_empty = self.input_buffer.is_empty();
        self.keymaps
            .iter()
            .rev()
            .find(|spec| spec.chord == pressed && spec.mode.matches_draft(draft_empty))
            .cloned()
    }

    /// Normalizes a key event to the chord it represents: `Ctrl`/`Alt` plus a letter or
    /// function key, ignoring `Shift` and letter case. `None` for anything unbindable.
    fn chord_of(key: KeyEvent) -> Option<KeyChord> {
        let mut modifiers = key.modifiers;
        modifiers.set(KeyModifiers::SHIFT, false);
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        let alt = modifiers.contains(KeyModifiers::ALT);
        modifiers.set(KeyModifiers::CONTROL, false);
        modifiers.set(KeyModifiers::ALT, false);
        if !modifiers.is_empty() {
            return None;
        }
        let key = match key.code {
            KeyCode::Char(ch) if ch.is_ascii() => KeySpec::Char(ch.to_ascii_lowercase()),
            KeyCode::F(n) => KeySpec::Function(n),
            _ => return None,
        };
        KeyChord::new(ctrl, alt, key)
    }

    /// `/help` banner: the built-in command reference plus active plugin keys, if any.
    fn help_text(&self) -> String {
        let mut text = "Commands: /help, /model <provider/model>, /mode <plan|build|auto|yolo>, /sessions (/resume [id]), /fork <id>, /goal <cond|clear>, /review [uncommitted|base <branch>|commit <sha>], /mcp, /tasks (Ctrl-X), /search <query>, /revert <turn_id>, /reload, /quit | Keys: @file & / completion (Tab/Up/Down), Left/Right/Home/End/Delete, Ctrl-J newline, Shift-Tab cycle mode, r Raw/Filtered".to_string();
        if !self.keymaps.is_empty() {
            let keys: Vec<String> = self
                .keymaps
                .iter()
                .map(|spec| format!("{} → {} ({})", spec.chord, spec.command, spec.mode.as_str()))
                .collect();
            text.push_str(" | Plugin keys: ");
            text.push_str(&keys.join(", "));
        }
        text
    }

    /// Renders the full-screen Alternate-Screen interface onto `frame`.
    pub fn render(&self, frame: &mut Frame<'_>) {
        let composer_lines = (self.input_buffer.lines().count().max(1) as u16)
            .saturating_add(2)
            .clamp(3, 6);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1 + u16::from(self.mcp_progress.is_some())),
                Constraint::Min(5),
                Constraint::Length(composer_lines),
            ])
            .split(frame.area());

        self.render_status_bar(frame, chunks[0]);
        self.render_transcript(frame, chunks[1]);
        self.render_composer(frame, chunks[2]);

        match &self.overlay {
            ActiveOverlay::None => {
                if let Some(popup) = &self.completion {
                    self.render_completion_popup(frame, chunks[2], popup);
                }
            }
            ActiveOverlay::Approval(modal) => self.render_approval_modal(frame, modal),
            ActiveOverlay::Tasks(tasks) => self.render_tasks_modal(frame, tasks),
            ActiveOverlay::Search(search) => self.render_search_modal(frame, search),
            ActiveOverlay::Sessions(sessions) => self.render_sessions_modal(frame, sessions),
            ActiveOverlay::Revert(rev) => self.render_revert_modal(frame, rev),
            ActiveOverlay::Question(modal) => self.render_question_modal(frame, modal),
            ActiveOverlay::ModelPicker(picker) => self.render_model_picker(frame, picker),
        }
    }

    fn render_status_bar(&self, frame: &mut Frame<'_>, area: Rect) {
        let view_label = match &self.viewing_subagent {
            Some(tid) => format!("SUBAGENT:{tid}"),
            None => format!("SESSION:{}", self.session_id),
        };
        let goal_badge = self
            .active_goal
            .as_deref()
            .map(|g| format!(" | GOAL:{g}"))
            .unwrap_or_default();
        let ctx_pct = if self.context_window > 0 {
            format!(
                " | ctx {:.0}%",
                (self.usage.input_tokens.min(u32::MAX as u64) as f64 / self.context_window as f64
                    * 100.0)
                    .clamp(0.0, 100.0)
            )
        } else {
            String::new()
        };
        let status_text = format!(
            " TEKVRA | {} | mode={:?} | model={} | in={} (cache {:.0}%) out={} ${:.4}{ctx_pct}{}",
            view_label,
            self.mode,
            self.active_model,
            self.usage.input_tokens,
            self.usage.cache_hit_ratio() * 100.0,
            self.usage.output_tokens,
            self.usage.cost_usd,
            goal_badge,
        );
        let bar = Paragraph::new(Line::from(Span::styled(
            status_text,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(area);
        frame.render_widget(bar, rows[0]);
        if let Some(text) = &self.mcp_progress
            && let Some(progress_area) = rows.get(1)
        {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(" {text}"),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ))),
                *progress_area,
            );
        }
    }

    fn render_transcript(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut all_lines: Vec<Line<'static>> = Vec::new();
        for (idx, entry) in self.active_transcript().iter().enumerate() {
            match entry {
                TranscriptEntry::UserMessage(text) => {
                    all_lines.push(Line::from(vec![
                        Span::styled(
                            "› ".to_string(),
                            Style::default()
                                .fg(Color::Green)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(text.clone(), Style::default().add_modifier(Modifier::BOLD)),
                    ]));
                }
                TranscriptEntry::AssistantMessage {
                    reasoning,
                    markdown,
                    ..
                } => {
                    if !reasoning.trim().is_empty() {
                        all_lines.push(Line::from(Span::styled(
                            format!("  ⋯ thinking: {}", reasoning.trim()),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        )));
                    }
                    all_lines.extend(render_markdown_lines(markdown));
                }
                TranscriptEntry::ToolCard(card) => {
                    let is_sel = self.selected_tool_card == Some(idx);
                    all_lines.extend(render_tool_card(card, is_sel));
                }
                TranscriptEntry::SystemBanner { tag, text } => {
                    // Multi-line text (e.g. `/mcp` catalogs): continuation lines align under
                    // the first one instead of embedding raw newlines in a single line.
                    let first = format!("  ⚡ [{tag}] ");
                    let indent = " ".repeat(tag.chars().count() + 8);
                    for (index, line) in text.split('\n').enumerate() {
                        let prefix = if index == 0 {
                            first.as_str()
                        } else {
                            indent.as_str()
                        };
                        all_lines.push(Line::from(Span::styled(
                            format!("{prefix}{line}"),
                            Style::default().fg(Color::Yellow),
                        )));
                    }
                }
            }
            all_lines.push(Line::from(Span::raw(String::new())));
        }

        let block = Block::default().borders(Borders::ALL).title(
            if let Some(tid) = &self.viewing_subagent {
                format!(" Subagent Transcript ({tid}) — Ctrl-X to switch ")
            } else {
                " Conversation — Shift-Tab: Mode | Ctrl-X: /tasks | r: Raw/Filtered ".to_string()
            },
        );
        let para = Paragraph::new(all_lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll_offset, 0));
        frame.render_widget(para, area);
    }

    fn render_composer(&self, frame: &mut Frame<'_>, area: Rect) {
        let title = if self.turn_running {
            " Prompt (running: Enter sends Guide, Alt-Enter sends Replace, Ctrl-C interrupts) "
        } else {
            " Prompt (/help for commands, Ctrl-J newline) "
        };
        let pos = self.clamped_cursor();
        let (before, after) = self.input_buffer.split_at(pos);
        let composer = Paragraph::new(format!("> {before}▏{after}"))
            .block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(composer, area);
    }

    fn render_question_modal(&self, frame: &mut Frame<'_>, modal: &QuestionModalState) {
        let area = centered_rect(70, 45, frame.area());
        frame.render_widget(Clear, area);

        let mut lines = vec![Line::from(Span::styled(
            "The agent asks for your input (Enter: submit, Esc: dismiss, Tab: next):",
            Style::default().add_modifier(Modifier::BOLD),
        ))];

        for (idx, (qid, prompt)) in modal.questions.iter().enumerate() {
            let focused = idx == modal.selected;
            let marker = if focused { "●" } else { "○" };
            lines.push(Line::from(Span::styled(
                format!("{marker} [{qid}] {prompt}"),
                if focused {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            )));
            let draft = modal.drafts.get(idx).map(String::as_str).unwrap_or("");
            lines.push(Line::from(Span::raw(format!(
                "   > {draft}{}",
                if focused { "_" } else { "" }
            ))));
        }

        let widget = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Question ")
                .border_style(Style::default().fg(Color::Cyan)),
        );
        frame.render_widget(widget, area);
    }

    fn render_approval_modal(&self, frame: &mut Frame<'_>, modal: &ApprovalModalState) {
        let area = centered_rect(76, 65, frame.area());
        frame.render_widget(Clear, area);

        let mut lines = vec![
            Line::from(Span::styled(
                format!("Tool `{}` requests permission:", modal.prompt.tool_name),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("  {}", modal.prompt.summary),
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::raw(format!(
                "Persistence Lifetime (Tab): {:?}{}",
                modal.lifetime,
                if modal.prompt.once_only {
                    " [LOCKED TO ONCE FOR DYNAMIC/EVAL SCRIPT]"
                } else {
                    ""
                }
            ))),
            Line::from(Span::raw("Scope Ladder (Up/Down):")),
        ];

        for (idx, opt) in modal.prompt.ladder.iter().enumerate() {
            let marker = if idx == modal.selected_rung {
                "●"
            } else {
                "○"
            };
            let broad_tag = if opt.broad_grant { " [BROAD]" } else { "" };
            lines.push(Line::from(Span::styled(
                format!("  {marker} {}{broad_tag}", opt.label),
                if idx == modal.selected_rung {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            )));
        }

        if modal.awaiting_broad_confirmation {
            lines.push(Line::from(Span::raw(String::new())));
            lines.push(Line::from(Span::styled(
                "⚠ CONFIRM BROAD GRANT: Press Enter/y again to persist this broad rule, or Esc/n to cancel!",
                Style::default()
                    .fg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(Span::raw(
                "Enter/y: Approve | Tab: Lifetime | Esc/n: Deny",
            )));
        }

        let widget = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Permission Approval Required ")
                .border_style(Style::default().fg(Color::Yellow)),
        );
        frame.render_widget(widget, area);
    }

    fn render_tasks_modal(&self, frame: &mut Frame<'_>, tasks: &TasksOverlayState) {
        let area = centered_rect(76, 60, frame.area());
        frame.render_widget(Clear, area);

        let mut lines = Vec::new();
        let main_sel = tasks.selected_index == 0;
        lines.push(Line::from(Span::styled(
            format!("{} [Main Session]", if main_sel { "▸" } else { " " }),
            if main_sel {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        )));

        for (i, entry) in tasks.entries.iter().enumerate() {
            let sel = tasks.selected_index == i + 1;
            let mut line = format!(
                "{} {} [role={} model={} status={} depth={}] {}",
                if sel { "▸" } else { " " },
                entry.task_id,
                entry.role,
                entry.model,
                entry.status,
                entry.depth,
                entry.summary
            );
            if let Some(branch) = &entry.worktree_branch {
                line.push_str(&format!(" [worktree {branch}]"));
            }
            lines.push(Line::from(Span::styled(
                line,
                if sel {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            )));
        }

        lines.push(Line::from(Span::raw(String::new())));
        lines.push(Line::from(Span::styled(
            format!(
                "Steer [{:?} | Tab toggles Guide/Replace]: {}",
                tasks.steer_delivery, tasks.steer_input
            ),
            Style::default().fg(Color::Green),
        )));

        let widget = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Subagent Tasks (/tasks | Ctrl-X) "),
        );
        frame.render_widget(widget, area);
    }

    fn render_search_modal(&self, frame: &mut Frame<'_>, search: &SearchOverlayState) {
        let area = centered_rect(78, 60, frame.area());
        frame.render_widget(Clear, area);
        let mut lines = vec![Line::from(Span::styled(
            format!(
                "FTS5 Query: `{}` ({} hit(s))",
                search.query,
                search.hits.len()
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        for (i, h) in search.hits.iter().enumerate() {
            let sel = i == search.selected;
            lines.push(Line::from(Span::styled(
                format!(
                    "{} [{} / {}] ({}) {}",
                    if sel { "▸" } else { " " },
                    h.session_id,
                    h.turn_id,
                    h.role,
                    h.snippet
                ),
                if sel {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                },
            )));
        }
        let widget = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Session History Search "),
        );
        frame.render_widget(widget, area);
    }

    fn render_revert_modal(&self, frame: &mut Frame<'_>, rev: &RevertOverlayState) {
        let area = centered_rect(76, 60, frame.area());
        frame.render_widget(Clear, area);
        let mut lines = vec![
            Line::from(Span::styled(
                format!(
                    "Revert from turn `{}` | Scope (Tab): {:?}",
                    rev.target_turn_id, rev.scope
                ),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::raw("Workspace files to restore:")),
        ];
        if rev.affected_files.is_empty() {
            lines.push(Line::from(Span::raw(
                "  (no workspace file mutations recorded)",
            )));
        } else {
            for item in &rev.affected_files {
                lines.push(Line::from(Span::styled(
                    format!("  • {} -> {}", item.path, item.action),
                    Style::default().fg(Color::Yellow),
                )));
            }
        }
        lines.push(Line::from(Span::raw(
            "Enter: Confirm Atomic Revert | Tab: Cycle Scope | Esc: Cancel",
        )));
        let widget = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Revert Session & Files "),
        );
        frame.render_widget(widget, area);
    }

    fn render_completion_popup(
        &self,
        frame: &mut Frame<'_>,
        composer_area: Rect,
        popup: &CompletionPopupState,
    ) {
        let rows = u16::try_from(popup.candidates.len()).unwrap_or(8).min(8);
        let height = rows.saturating_add(2);
        if composer_area.y < height {
            return;
        }
        let width = composer_area.width.min(84);
        let popup_area = Rect {
            x: composer_area.x,
            y: composer_area.y.saturating_sub(height),
            width,
            height,
        };
        frame.render_widget(Clear, popup_area);

        let lines: Vec<Line<'static>> = popup
            .candidates
            .iter()
            .enumerate()
            .map(|(idx, cand)| {
                let sel = idx == popup.selected;
                Line::from(vec![
                    Span::styled(
                        format!(" {} {:<28} ", if sel { "▸" } else { " " }, cand.label),
                        if sel {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::White)
                        },
                    ),
                    Span::styled(
                        cand.description.clone(),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])
            })
            .collect();

        let title = match popup.kind {
            CompletionKind::SlashCommand => " Slash Commands (Tab/Enter select, Esc close) ",
            CompletionKind::WorkspaceFile => " @File Mention (Tab/Enter select, Esc close) ",
            CompletionKind::SkillName => " Skills (/skill <name>, Tab/Enter select, Esc close) ",
        };
        let widget = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(Color::Cyan)),
        );
        frame.render_widget(widget, popup_area);
    }

    fn render_model_picker(&self, frame: &mut Frame<'_>, picker: &ModelPickerState) {
        let area = centered_rect(72, 55, frame.area());
        frame.render_widget(Clear, area);

        let filtered = picker.filtered();
        let mut lines = vec![Line::from(Span::styled(
            format!(
                "Select model ({} match(es)) — filter: `{}`",
                filtered.len(),
                picker.query
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        if filtered.is_empty() {
            lines.push(Line::from(Span::raw("  (no models match the filter)")));
        } else {
            for (i, (spec, description)) in filtered.iter().enumerate() {
                let sel = i == picker.selected;
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} {spec}", if sel { "▸" } else { " " }),
                        if sel {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    ),
                    Span::styled(
                        format!("  {description}"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }
        lines.push(Line::from(Span::raw(
            "Enter: Switch Model | Up/Down: Navigate | Type: Fuzzy Filter | Esc: Cancel",
        )));

        let widget = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Model Picker (/model) "),
        );
        frame.render_widget(widget, area);
    }

    fn render_sessions_modal(&self, frame: &mut Frame<'_>, sessions: &SessionsOverlayState) {
        let area = centered_rect(82, 65, frame.area());
        frame.render_widget(Clear, area);

        let action_label = if sessions.fork_mode {
            "FORK NEW BRANCH"
        } else {
            "RESUME IN PLACE"
        };
        let mut lines = vec![Line::from(Span::styled(
            format!(
                "Recent Indexed Sessions ({}) — Mode: [{}] (Tab/f toggles Resume/Fork)",
                sessions.sessions.len(),
                action_label
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ))];

        if sessions.sessions.is_empty() {
            lines.push(Line::from(Span::raw("  (no saved sessions in StateIndex)")));
        } else {
            for (i, s) in sessions.sessions.iter().enumerate() {
                let sel = i == sessions.selected;
                let headline = s
                    .title
                    .as_deref()
                    .filter(|t| !t.trim().is_empty())
                    .or_else(|| (!s.preview.trim().is_empty()).then_some(s.preview.as_str()))
                    .unwrap_or("(untitled session)");
                lines.push(Line::from(Span::styled(
                    format!(
                        "{} {} [{}] {}",
                        if sel { "▸" } else { " " },
                        s.id,
                        s.model,
                        headline
                    ),
                    if sel {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                )));
            }
        }
        lines.push(Line::from(Span::raw(
            "Enter: Confirm | Tab/f: Toggle Resume/Fork | Up/Down: Navigate | Esc: Cancel",
        )));

        let widget = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Sessions (/sessions | /resume) "),
        );
        frame.render_widget(widget, area);
    }
}

struct AltScreenGuard;

impl AltScreenGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )?;
        Ok(Self)
    }
}

impl Drop for AltScreenGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

/// Exports `text` to the terminal clipboard via ANSI OSC-52 (base64, clipboard selection).
fn copy_to_clipboard_osc52(text: &str) {
    use base64::Engine as _;
    use std::io::Write as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut stdout = io::stdout();
    let _ = stdout.write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes());
    let _ = stdout.flush();
}

/// Runs the 60 FPS full-screen Alternate-Screen TUI attached to `handle`.
///
/// # Errors
/// Returns [`TuiError`] if terminal setup or session dispatch fails.
pub async fn run_fullscreen_tui(
    mut handle: SessionHandle,
    state_index: Arc<Mutex<StateIndex>>,
    snapshots: WorkspaceSnapshotStore,
    mut hooks: TuiRuntimeHooks,
) -> Result<(), TuiError> {
    let _guard = AltScreenGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let snap = handle.state_snapshot();
    let mut app = TuiApp::new(handle.session_id(), snap.active_model.clone(), snap.mode);
    if !hooks.initial_records.is_empty() {
        app.load_main_rollout(
            handle.session_id().to_string(),
            snap.active_model,
            &hooks.initial_records,
        );
        hooks.initial_records.clear();
    }
    app.set_workspace_files(std::mem::take(&mut hooks.workspace_files));
    hooks.sync_plugin_registrations(&mut app);
    app.set_discovered_skills(std::mem::take(&mut hooks.discovered_skills));
    if let Some(cb) = hooks.on_memory_view.take() {
        app.set_memory_view(cb);
    }
    app.set_model_catalog(
        std::mem::take(&mut hooks.available_models),
        snap.context_window,
    );
    let mut event_rx = handle.subscribe();
    let mut term_events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(16)); // ~60 FPS

    loop {
        terminal.draw(|f| app.render(f))?;

        tokio::select! {
            _ = tick.tick() => {}
            Ok(sev) = event_rx.recv() => {
                app.apply_session_event(sev);
            }
            Some(Ok(ev)) = term_events.next() => {
                match ev {
                    Event::Key(key) => {
                        match app.handle_key(key) {
                        TuiCommand::Noop => {}
                        TuiCommand::Quit => break,
                        TuiCommand::Session(cmd) => {
                            handle.send(cmd).await?;
                        }
                        TuiCommand::SteerSubagent { task_id, prompt, delivery } => {
                            if let Some(mgr) = handle.subagents() {
                                let _ = mgr.steer(&task_id, prompt, delivery).await;
                            }
                        }
                        TuiCommand::OpenTasksOverlay => {
                            let entries = handle.subagents().map_or_else(Vec::new, tekvra_core::SubagentManager::list);
                            if let Some(mgr) = handle.subagents() {
                                for e in &entries {
                                    if let Ok(recs) = mgr.read_persisted_transcript(&e.task_id) {
                                        app.load_subagent_rollout(&e.task_id, &recs);
                                    }
                                }
                            }
                            app.open_tasks_overlay(entries);
                        }
                        TuiCommand::OpenSessionsOverlay => {
                            let sessions = state_index
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .list_sessions(false, 30, 0)
                                .unwrap_or_default();
                            app.open_sessions_overlay(sessions);
                        }
                        TuiCommand::SwitchSession { session_id, fork } => {
                            if let Some(cb) = &mut hooks.on_switch_session {
                                match cb(&session_id, fork) {
                                    Ok((new_handle, records)) => {
                                        handle = new_handle;
                                        event_rx = handle.subscribe();
                                        let new_snap = handle.state_snapshot();
                                        app.mode = new_snap.mode;
                                        app.load_main_rollout(
                                            handle.session_id().to_string(),
                                            new_snap.active_model,
                                            &records,
                                        );
                                        let verb = if fork { "Forked into" } else { "Resumed" };
                                        app.main_transcript.push(TranscriptEntry::SystemBanner {
                                            tag: "session".to_string(),
                                            text: format!("{verb} session `{}`", handle.session_id()),
                                        });
                                    }
                                    Err(err) => {
                                        app.main_transcript.push(TranscriptEntry::SystemBanner {
                                            tag: "error".to_string(),
                                            text: format!("Session switch failed: {err}"),
                                        });
                                    }
                                }
                            }
                        }
                        TuiCommand::RunSearch { query } => {
                            let hits = state_index
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .search_history(&query, 20)
                                .unwrap_or_default();
                            app.overlay = ActiveOverlay::Search(SearchOverlayState {
                                query,
                                hits,
                                selected: 0,
                            });
                        }
                        TuiCommand::OpenRevertPreview { turn_id } => {
                            let manifests = snapshots.list_manifests(handle.session_id()).unwrap_or_default();
                            let turns = vec![turn_id.clone()];
                            app.overlay = ActiveOverlay::Revert(RevertOverlayState::from_manifests(
                                turn_id,
                                &manifests,
                                &turns,
                            ));
                        }
                        TuiCommand::ReloadPlugins => {
                            if let Some(cb) = &mut hooks.on_reload_plugins {
                                let msg = match cb() {
                                    Ok(ok) => {
                                        hooks.sync_plugin_registrations(&mut app);
                                        ok
                                    }
                                    Err(err) => format!("Plugin reload failed: {err}"),
                                };
                                app.main_transcript.push(TranscriptEntry::SystemBanner {
                                    tag: "plugins".to_string(),
                                    text: msg,
                                });
                            }
                        }
                        TuiCommand::ShowMcpStatus => {
                            let msg = hooks
                                .on_mcp_status
                                .as_mut()
                                .map_or_else(|| "No MCP servers connected.".to_string(), |cb| cb());
                            app.apply_mcp_status(msg);
                        }
                        TuiCommand::RunReview { target } => {
                            if let Some(cb) = &mut hooks.on_run_review {
                                match cb(target).await {
                                    Ok((summary, findings)) => {
                                        app.apply_review_findings(&summary, &findings);
                                    }
                                    Err(err) => {
                                        app.main_transcript.push(TranscriptEntry::SystemBanner {
                                            tag: "error".to_string(),
                                            text: format!("Review failed: {err}"),
                                        });
                                    }
                                }
                            }
                        }
                        TuiCommand::InvokePluginCommand { name, args } => {
                            let res = hooks
                                .on_plugin_command
                                .as_mut()
                                .map_or(Ok(None), |cb| cb(&name, &args));
                            app.apply_plugin_command_result(&name, res);
                        }
                        TuiCommand::CopyToClipboard { text, label } => {
                            let byte_len = text.len();
                            copy_to_clipboard_osc52(&text);
                            app.apply_copy_result(&label, byte_len);
                        }
                        TuiCommand::RunTrust { action } => {
                            let msg = match &mut hooks.on_trust_command {
                                Some(cb) => cb(action).unwrap_or_else(|err| err),
                                None => {
                                    "Trust store unavailable: no trust hook registered by the host."
                                        .to_string()
                                }
                            };
                            app.main_transcript.push(TranscriptEntry::SystemBanner {
                                tag: "trust".to_string(),
                                text: msg,
                            });
                        }
                        }
                    }
                    Event::Mouse(mouse) => {
                        app.handle_mouse(mouse);
                    }
                    Event::Paste(text) => {
                        for ch in text.chars() {
                            app.insert_char_at_cursor(ch);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

/// Extracts the last fenced ``` code block body from `markdown`, if present.
#[must_use]
pub fn extract_last_fenced_code_block(markdown: &str) -> Option<String> {
    let mut open: Option<usize> = None;
    let mut last_block: Option<String> = None;
    let mut current: Vec<&str> = Vec::new();
    let mut in_block = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if !in_block && trimmed.starts_with("```") {
            in_block = true;
            open = Some(trimmed.len());
            current.clear();
        } else if in_block && trimmed.starts_with("```") {
            in_block = false;
            let body = current.join("\n");
            if !body.trim().is_empty() {
                last_block = Some(body);
            }
            let _ = open;
        } else if in_block {
            current.push(line);
        }
    }
    // Tolerate an unclosed fence: treat everything after the last opening as the block.
    if in_block && !current.is_empty() {
        let body = current.join("\n");
        if !body.trim().is_empty() {
            last_block = Some(body);
        }
    }
    last_block
}

fn parse_tui_review_target(rest: &str) -> ReviewTarget {
    if rest.is_empty() || rest == "uncommitted" {
        return ReviewTarget::UncommittedChanges;
    }
    if let Some(branch) = rest.strip_prefix("base ") {
        let b = branch.trim();
        if !b.is_empty() {
            return ReviewTarget::BaseBranch {
                branch: b.to_string(),
            };
        }
    }
    if let Some(sha) = rest.strip_prefix("commit ") {
        let s = sha.trim();
        if !s.is_empty() {
            return ReviewTarget::Commit { sha: s.to_string() };
        }
    }
    ReviewTarget::Custom {
        instructions: rest.to_string(),
    }
}

fn rollout_records_to_entries(records: &[RolloutRecord]) -> Vec<TranscriptEntry> {
    let mut entries = Vec::new();
    for rec in records {
        match rec {
            RolloutRecord::Message { message, .. } => {
                for part in &message.parts {
                    if let tekvra_providers_shim::RoleText::User(t) =
                        classify_message_part(message, part)
                    {
                        entries.push(TranscriptEntry::UserMessage(t));
                    } else if let tekvra_providers_shim::RoleText::Assistant(t) =
                        classify_message_part(message, part)
                    {
                        entries.push(TranscriptEntry::AssistantMessage {
                            item_id: "hist".to_string(),
                            reasoning: String::new(),
                            markdown: t,
                        });
                    }
                }
            }
            RolloutRecord::ToolCard {
                call_id,
                tool_name,
                output,
                ..
            } => {
                entries.push(TranscriptEntry::ToolCard(ToolCardState::new(
                    call_id.clone(),
                    tool_name.clone(),
                    output.clone(),
                )));
            }
            RolloutRecord::SteeringNote { kind, text, .. } => {
                entries.push(TranscriptEntry::SystemBanner {
                    tag: kind.clone(),
                    text: text.clone(),
                });
            }
            _ => {}
        }
    }
    entries
}

mod tekvra_providers_shim {
    pub(super) enum RoleText {
        User(String),
        Assistant(String),
        Other,
    }
}

fn classify_message_part<M, P>(msg: &M, part: &P) -> tekvra_providers_shim::RoleText
where
    M: serde::Serialize,
    P: serde::Serialize,
{
    let Ok(m_val) = serde_json::to_value(msg) else {
        return tekvra_providers_shim::RoleText::Other;
    };
    let Ok(p_val) = serde_json::to_value(part) else {
        return tekvra_providers_shim::RoleText::Other;
    };
    let Some(text) = p_val.get("text").and_then(serde_json::Value::as_str) else {
        return tekvra_providers_shim::RoleText::Other;
    };
    match m_val.get("role").and_then(serde_json::Value::as_str) {
        Some("user") => tekvra_providers_shim::RoleText::User(text.to_string()),
        Some("assistant") => tekvra_providers_shim::RoleText::Assistant(text.to_string()),
        _ => tekvra_providers_shim::RoleText::Other,
    }
}

fn event_thread_id(ev: &SessionEvent) -> Option<&str> {
    match ev {
        SessionEvent::TurnStarted { thread_id, .. }
        | SessionEvent::ItemStarted { thread_id, .. }
        | SessionEvent::ItemDelta { thread_id, .. }
        | SessionEvent::ItemCompleted { thread_id, .. }
        | SessionEvent::SteeringInjected { thread_id, .. }
        | SessionEvent::ApprovalRequested { thread_id, .. }
        | SessionEvent::UserInputRequested { thread_id, .. }
        | SessionEvent::TurnRefused { thread_id, .. }
        | SessionEvent::DiffUpdated { thread_id, .. }
        | SessionEvent::PlanUpdated { thread_id, .. }
        | SessionEvent::ReviewCompleted { thread_id, .. }
        | SessionEvent::TokenUsageUpdated { thread_id, .. }
        | SessionEvent::CompactionCompleted { thread_id, .. }
        | SessionEvent::McpProgress { thread_id, .. }
        | SessionEvent::TurnCompleted { thread_id, .. } => Some(thread_id.as_str()),
        SessionEvent::ConfigWarning { .. } => None,
    }
}

/// Formats the MCP progress status line: `mcp server/tool: progress[/total][ message]`.
pub fn format_mcp_progress(
    server: &str,
    tool: &str,
    progress: f64,
    total: Option<f64>,
    message: Option<&str>,
) -> String {
    let mut text = format!("mcp {server}/{tool}: {}", fmt_progress_number(progress));
    if let Some(total) = total {
        text.push('/');
        text.push_str(&fmt_progress_number(total));
    }
    if let Some(message) = message.filter(|message| !message.is_empty()) {
        text.push(' ');
        text.push_str(message);
    }
    text
}

/// Renders a progress value without a decimal part when it is integral (`1`, not `1.0`).
fn fmt_progress_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn infer_tool_label(out: &tekvra_protocol::ToolOutput) -> &'static str {
    match out {
        tekvra_protocol::ToolOutput::Diff { .. } => "apply_patch",
        tekvra_protocol::ToolOutput::Terminal { .. } => "exec_command",
        tekvra_protocol::ToolOutput::FileSlice { .. } => "file_read",
        tekvra_protocol::ToolOutput::Outline { .. } => "file_index",
        tekvra_protocol::ToolOutput::Search { .. } => "file_grep",
        tekvra_protocol::ToolOutput::Plan { .. } => "plan",
        tekvra_protocol::ToolOutput::Question { .. } => "question",
        tekvra_protocol::ToolOutput::SubagentCard { .. } => "task",
        tekvra_protocol::ToolOutput::CodeExecution { .. } => "code",
        _ => "tool",
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vert[1])[1]
}

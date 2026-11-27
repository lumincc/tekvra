//! Full-screen Alternate-Screen `ratatui` application state, key dispatcher, layout renderer,
//! and 60 FPS `crossterm` event loop.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    ApprovalResolution, InputDeliveryMode, PermissionMode, RevertScope, ReviewFinding,
    SessionCommand, SessionEvent, TokenUsage,
};
use tekvra_storage::{RolloutRecord, StateIndex, WorkspaceSnapshotStore};

use crate::TuiError;
use crate::cards::{ToolCardState, render_tool_card};
use crate::markdown::render_markdown_lines;
use crate::modals::{
    ApprovalModalState, RevertOverlayState, SearchOverlayState, TasksOverlayState,
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
    /// `/revert` workspace file-restoration preview.
    Revert(RevertOverlayState),
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
}

/// Boxed future returned by the host `/review` callback in [`TuiRuntimeHooks`].
pub type BoxReviewFuture =
    Pin<Box<dyn Future<Output = Result<(String, Vec<ReviewFinding>), String>> + Send>>;

/// Boxed callback invoked on `/<name> <args>` for opt-in Luau plugin commands (`Ok(None)` = unknown command).
pub type PluginCommandHandler = Box<dyn FnMut(&str, &str) -> Result<Option<String>, String> + Send>;

/// Optional host callbacks wired into [`run_fullscreen_tui`].
#[derive(Default)]
pub struct TuiRuntimeHooks {
    /// Invoked on `/reload` to rebuild opt-in Luau plugins.
    pub on_reload_plugins: Option<Box<dyn FnMut() -> Result<String, String> + Send>>,
    /// Invoked on `/<name> <args>` to execute a plugin-registered slash command (`Ok(None)` = unknown command).
    pub on_plugin_command: Option<PluginCommandHandler>,
    /// Invoked on `/mcp` to format live MCP server and tool status.
    pub on_mcp_status: Option<Box<dyn FnMut() -> String + Send>>,
    /// Invoked on `/review` to execute a read-only diff review pass.
    pub on_run_review: Option<Box<dyn FnMut(ReviewTarget) -> BoxReviewFuture + Send>>,
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
            viewing_subagent: None,
            main_transcript: Vec::new(),
            subagent_transcripts: BTreeMap::new(),
            selected_tool_card: None,
            scroll_offset: 0,
            input_buffer: String::new(),
            cursor_byte_pos: 0,
            overlay: ActiveOverlay::None,
        }
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
    }

    /// Moves the composer cursor one Unicode scalar value to the right.
    pub fn move_cursor_right(&mut self) {
        let pos = self.clamped_cursor();
        if let Some(ch) = self.input_buffer[pos..].chars().next() {
            self.cursor_byte_pos = pos + ch.len_utf8();
        }
    }

    /// Moves the composer cursor to the start of `input_buffer`.
    pub fn move_cursor_home(&mut self) {
        self.cursor_byte_pos = 0;
    }

    /// Moves the composer cursor to the end of `input_buffer`.
    pub fn move_cursor_end(&mut self) {
        self.cursor_byte_pos = self.input_buffer.len();
    }

    /// Inserts `ch` at the current UTF-8 cursor boundary.
    pub fn insert_char_at_cursor(&mut self, ch: char) {
        let pos = self.clamped_cursor();
        self.input_buffer.insert(pos, ch);
        self.cursor_byte_pos = pos + ch.len_utf8();
    }

    /// Deletes the Unicode scalar immediately preceding the cursor.
    pub fn backspace_at_cursor(&mut self) {
        let pos = self.clamped_cursor();
        if let Some((prev_idx, _)) = self.input_buffer[..pos].char_indices().next_back() {
            self.input_buffer.remove(prev_idx);
            self.cursor_byte_pos = prev_idx;
        }
    }

    /// Deletes the Unicode scalar under the cursor.
    pub fn delete_at_cursor(&mut self) {
        let pos = self.clamped_cursor();
        if pos < self.input_buffer.len() {
            self.input_buffer.remove(pos);
            self.cursor_byte_pos = pos;
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
        self.subagent_transcripts
            .insert(task_id.to_string(), entries);
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
            SessionEvent::TokenUsageUpdated { usage, .. } if !is_subagent => {
                self.usage = usage;
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
                    self.overlay = ActiveOverlay::None;
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
                let submitted = std::mem::take(&mut self.input_buffer);
                self.cursor_byte_pos = 0;
                let trimmed = submitted.trim();
                if trimmed.is_empty() {
                    return TuiCommand::Noop;
                }
                if trimmed == "/quit" || trimmed == "/exit" {
                    return TuiCommand::Quit;
                }
                if trimmed == "/help" {
                    self.main_transcript.push(TranscriptEntry::SystemBanner {
                        tag: "help".to_string(),
                        text: "Commands: /help, /model <provider/model>, /mode <plan|build|auto|yolo>, /goal <cond|clear>, /review [uncommitted|base <branch>|commit <sha>], /mcp, /tasks (Ctrl-X), /search <query>, /revert <turn_id>, /reload, /quit | Keys: Left/Right/Home/End/Delete, Ctrl-J newline, Shift-Tab cycle mode, r Raw/Filtered".to_string(),
                    });
                    return TuiCommand::Noop;
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
                if trimmed == "/model" || trimmed.starts_with("/model ") {
                    let arg = trimmed.strip_prefix("/model").unwrap_or_default().trim();
                    if arg.is_empty() {
                        self.main_transcript.push(TranscriptEntry::SystemBanner {
                            tag: "model".to_string(),
                            text: format!("Active model: `{}`", self.active_model),
                        });
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
                    if key.modifiers.contains(KeyModifiers::ALT) {
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
                })
            }
            _ => TuiCommand::Noop,
        }
    }

    /// Renders the full-screen Alternate-Screen interface onto `frame`.
    pub fn render(&self, frame: &mut Frame<'_>) {
        let composer_lines = (self.input_buffer.lines().count().max(1) as u16)
            .saturating_add(2)
            .clamp(3, 6);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(5),
                Constraint::Length(composer_lines),
            ])
            .split(frame.area());

        self.render_status_bar(frame, chunks[0]);
        self.render_transcript(frame, chunks[1]);
        self.render_composer(frame, chunks[2]);

        match &self.overlay {
            ActiveOverlay::None => {}
            ActiveOverlay::Approval(modal) => self.render_approval_modal(frame, modal),
            ActiveOverlay::Tasks(tasks) => self.render_tasks_modal(frame, tasks),
            ActiveOverlay::Search(search) => self.render_search_modal(frame, search),
            ActiveOverlay::Revert(rev) => self.render_revert_modal(frame, rev),
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
        let status_text = format!(
            " TEKVRA | {} | mode={:?} | model={} | in={} (cache {:.0}%) out={} ${:.4}{}",
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
        frame.render_widget(bar, area);
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
                    all_lines.push(Line::from(Span::styled(
                        format!("  ⚡ [{tag}] {text}"),
                        Style::default().fg(Color::Yellow),
                    )));
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
            lines.push(Line::from(Span::styled(
                format!(
                    "{} {} [role={} model={} status={}] {}",
                    if sel { "▸" } else { " " },
                    entry.task_id,
                    entry.role,
                    entry.model,
                    entry.status,
                    entry.summary
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
}

struct AltScreenGuard;

impl AltScreenGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for AltScreenGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// Runs the 60 FPS full-screen Alternate-Screen TUI attached to `handle`.
///
/// # Errors
/// Returns [`TuiError`] if terminal setup or session dispatch fails.
pub async fn run_fullscreen_tui(
    handle: SessionHandle,
    state_index: Arc<Mutex<StateIndex>>,
    snapshots: WorkspaceSnapshotStore,
    mut hooks: TuiRuntimeHooks,
) -> Result<(), TuiError> {
    let _guard = AltScreenGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let snap = handle.state_snapshot();
    let mut app = TuiApp::new(handle.session_id(), snap.active_model, snap.mode);
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
                if let Event::Key(key) = ev {
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
                                    Ok(ok) => ok,
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
                    }
                }
            }
        }
    }

    Ok(())
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
        | SessionEvent::DiffUpdated { thread_id, .. }
        | SessionEvent::PlanUpdated { thread_id, .. }
        | SessionEvent::ReviewCompleted { thread_id, .. }
        | SessionEvent::TokenUsageUpdated { thread_id, .. }
        | SessionEvent::TurnCompleted { thread_id, .. } => Some(thread_id.as_str()),
        SessionEvent::ConfigWarning { .. } => None,
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

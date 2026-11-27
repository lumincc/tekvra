//! Event-driven `SessionHandle` actor loop (`Next`, `Guide`, `Replace`), interactive approval
//! and question bridges, rollout + SQLite persistence, and atomic turn revert.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use tekvra_config::ResolvedConfig;
use tekvra_protocol::{
    ApprovalPrompt, ApprovalResolution, FileDiffEntry, InputDeliveryMode, InputImage,
    PermissionMode, RevertScope, SessionCommand, SessionEvent, TokenUsage, ToolOutput,
};
use tekvra_providers::{
    CompletionRequest, JobRole, MessagePart, MessageRole, ModelRouter, ProviderDriver,
    ProviderMessage, StreamDelta, StreamDeltaSink, ToolDefinition,
};
use tekvra_storage::{
    RolloutHeader, RolloutLog, RolloutRecord, SessionSummary, StateIndex, WorkspaceSnapshotStore,
};
use tekvra_tools::{
    ApprovalCallback, McpGateway, MontyCodeSandbox, NativeToolRegistry, PermissionEvaluator,
    PtyCompletionNotice, PtySessionManager, QuestionCallback, SafeWebFetcher, ToolExecutionContext,
    WorkspacePathLocks, build_tool_definitions_for_model,
};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::CoreError;
use crate::context::{SkillInvocation, expand_prompt_attachments, expand_skill_invocation};
use crate::frame::{CompactionConfig, Frame, extract_assistant_text};
use crate::goal_and_review::{accumulate_usage, evaluate_goal};
use crate::hooks::{HookEvent, HookEventPayload, HookOutcome};
use crate::steering::{SteeringDetector, ToolFailureTracker};
use crate::subagents::{
    BackgroundSubagentNotice, SubagentManager, SubagentManagerDeps, SubagentSpawnRequest,
};

type QuestionAnswers = BTreeMap<String, Vec<String>>;
type PendingApprovalsMap = Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalResolution>>>>;
/// Queued user input: prompt text plus pre-validated image attachments.
type PendingUserInput = (String, Vec<InputImage>);
type PendingQuestionsMap = Arc<Mutex<HashMap<String, oneshot::Sender<QuestionAnswers>>>>;

/// Optional external plugin tool executor (`tekvra-lua` registers here ONLY when `experimental.lua_plugins = true`).
#[async_trait]
pub trait PluginToolExecutor: Send + Sync {
    /// Executes a plugin-registered tool by name.
    async fn execute_plugin_tool(
        &self,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<ToolOutput, String>;

    /// Dispatches a lifecycle hook to all handlers registered for `event`
    /// (`tekvra.api.register_hook`), in registration order. The default outcome
    /// is empty so hosts without hook support stay no-ops.
    fn dispatch_hook(&self, event: HookEvent, payload: HookEventPayload) -> HookOutcome {
        let _ = (event, payload);
        HookOutcome::default()
    }
}

/// Snapshot of live session metadata inspectable synchronously by TUI and `app-server`.
#[derive(Debug, Clone)]
pub struct SessionRuntimeState {
    /// Session/thread identifier.
    pub session_id: String,
    /// Canonical workspace directory.
    pub cwd: PathBuf,
    /// Active `provider/model` spec.
    pub active_model: String,
    /// Active permission mode (`Plan`, `ReadOnly`, `Build`, `Auto`, `Yolo`).
    pub mode: PermissionMode,
    /// Active `/goal` condition, if set.
    pub goal: Option<String>,
    /// Current session plan Markdown.
    pub plan_markdown: String,
    /// Cumulative token/cache/cost usage.
    pub cumulative_usage: TokenUsage,
    /// Context window of the active model (tokens) for utilization telemetry.
    pub context_window: u64,
    /// Latest `Frame` prefix snapshot for testing and diagnostics.
    pub last_frame_snapshot: Option<crate::frame::FrameSnapshot>,
}

/// Bootstrap parameters for spawning or resuming a [`SessionHandle`].
pub struct SessionBootstrap {
    /// Explicit session id to create or resume (`None` generates `thr_<uuidv7>`).
    pub session_id: Option<String>,
    /// Optional parent session id when spawned as a fork or subagent.
    pub parent_session_id: Option<String>,
    /// Workspace root directory.
    pub workspace_root: PathBuf,
    /// State directory (`<state_dir>/rollouts`, `<state_dir>/state.db`, `<state_dir>/snapshots`).
    pub state_dir: PathBuf,
    /// Resolved declarative configuration.
    pub config: ResolvedConfig,
    /// Role-based model router.
    pub router: ModelRouter,
    /// Async LLM provider driver (`HttpProviderDriver` or deterministic mock).
    pub driver: Arc<dyn ProviderDriver>,
    /// Shared SQLite state index (`None` opens `<state_dir>/state.db` automatically).
    pub state_index: Option<Arc<Mutex<StateIndex>>>,
    /// Automatic context compaction settings.
    pub compaction: CompactionConfig,
    /// Optional connected MCP gateway.
    pub mcp_gateway: Option<Arc<dyn McpGateway>>,
    /// Optional opt-in Luau plugin tool executor.
    pub plugin_executor: Option<Arc<dyn PluginToolExecutor>>,
    /// Extra tool declarations contributed by opt-in Luau plugins.
    pub extra_plugin_tools: Vec<ToolDefinition>,
    /// Whether this session exposes the `task` subagent tool.
    pub allow_subagents: bool,
    /// Nesting depth of this session (`0` = top-level); children spawn at `depth + 1`.
    pub subagent_depth: u32,
}

/// Cloneable client handle to a running session actor (`tekvra-tui`, `tekvra-app-server`, `tekvra-acp`, `tekvra exec`).
#[derive(Clone)]
pub struct SessionHandle {
    session_id: String,
    cmd_tx: mpsc::Sender<SessionCommand>,
    event_tx: broadcast::Sender<SessionEvent>,
    shared_state: Arc<Mutex<SessionRuntimeState>>,
    subagents: Option<SubagentManager>,
    pty_manager: PtySessionManager,
}

impl SessionHandle {
    /// Stable session/thread identifier.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Sends a [`SessionCommand`] to the actor.
    ///
    /// # Errors
    /// Returns [`CoreError::SessionClosed`] if the actor has shut down.
    pub async fn send(&self, cmd: SessionCommand) -> Result<(), CoreError> {
        self.cmd_tx
            .send(cmd)
            .await
            .map_err(|_| CoreError::SessionClosed {
                session_id: self.session_id.clone(),
            })
    }

    /// Subscribes to broadcast [`SessionEvent`] notifications.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.event_tx.subscribe()
    }

    /// Reads a synchronous snapshot of [`SessionRuntimeState`].
    #[must_use]
    pub fn state_snapshot(&self) -> SessionRuntimeState {
        self.shared_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Accesses the session's [`SubagentManager`] (`task` subagents), if enabled.
    #[must_use]
    pub const fn subagents(&self) -> Option<&SubagentManager> {
        self.subagents.as_ref()
    }

    /// Accesses the session's interactive [`PtySessionManager`].
    #[must_use]
    pub const fn pty_manager(&self) -> &PtySessionManager {
        &self.pty_manager
    }
}

enum RunnerWork {
    UserTurn {
        prompt: String,
        images: Vec<InputImage>,
    },
    RevertTo {
        target_id: String,
        scope: RevertScope,
    },
    UndoLastTurn {
        scope: RevertScope,
    },
    Compact,
    SetGoal(Option<String>),
    SetMode(PermissionMode),
    SwitchModel(String),
}

struct InteractiveBridge {
    session_id: String,
    turn_id: Mutex<String>,
    event_tx: broadcast::Sender<SessionEvent>,
    pending_approvals: PendingApprovalsMap,
    pending_questions: PendingQuestionsMap,
}

#[async_trait]
impl ApprovalCallback for InteractiveBridge {
    async fn request_approval(&self, prompt: ApprovalPrompt) -> ApprovalResolution {
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self
                .pending_approvals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.insert(prompt.approval_id.clone(), tx);
        }
        let turn_id = self
            .turn_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let _ = self.event_tx.send(SessionEvent::ApprovalRequested {
            thread_id: self.session_id.clone(),
            turn_id,
            prompt,
        });
        rx.await.unwrap_or(ApprovalResolution::Deny {
            reason: Some("approval channel closed".to_string()),
        })
    }
}

#[async_trait]
impl QuestionCallback for InteractiveBridge {
    async fn ask_questions(&self, questions: Vec<String>) -> Vec<(String, String)> {
        let item_id = format!("q_{}", Uuid::now_v7().simple());
        let pairs: Vec<(String, String)> = questions
            .iter()
            .enumerate()
            .map(|(i, q)| (format!("q{i}"), q.clone()))
            .collect();
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self
                .pending_questions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.insert(item_id.clone(), tx);
        }
        let turn_id = self
            .turn_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let _ = self.event_tx.send(SessionEvent::UserInputRequested {
            thread_id: self.session_id.clone(),
            turn_id,
            item_id,
            questions: pairs.clone(),
        });
        let ans_map = rx.await.unwrap_or_default();
        pairs
            .into_iter()
            .map(|(qid, prompt)| {
                let ans = ans_map.get(&qid).map(|v| v.join(", ")).unwrap_or_default();
                (prompt, ans)
            })
            .collect()
    }
}

/// Spawns (or resumes, if `<state_dir>/rollouts/<session_id>.jsonl` already exists) a live
/// [`SessionHandle`] backed by a single-writer lease, `RolloutLog`, `StateIndex`, and `WorkspaceSnapshotStore`.
///
/// # Errors
/// Returns [`CoreError`] if session lease acquisition, storage opening, or initial model resolution fails.
pub fn spawn_session(bootstrap: SessionBootstrap) -> Result<SessionHandle, CoreError> {
    let canonical_ws = std::fs::canonicalize(&bootstrap.workspace_root)
        .unwrap_or_else(|_| bootstrap.workspace_root.clone());
    std::fs::create_dir_all(&bootstrap.state_dir).map_err(|source| {
        tekvra_storage::StorageError::Io {
            path: bootstrap.state_dir.clone(),
            source,
        }
    })?;

    let state_index = match bootstrap.state_index {
        Some(idx) => idx,
        None => Arc::new(Mutex::new(StateIndex::open(&bootstrap.state_dir)?)),
    };

    let session_id = bootstrap
        .session_id
        .unwrap_or_else(|| format!("thr_{}", Uuid::now_v7().simple()));
    let now_epoch = jiff::Timestamp::now().as_second();

    let mode = bootstrap.config.settings.agent.default_mode;
    let (_, initial_caps) = bootstrap.router.route_for_mode(mode)?;

    let existing_rollout = tekvra_storage::rollout_path(&bootstrap.state_dir, &session_id);
    let (mut rollout_log, mut rollout_records) = if existing_rollout.is_file() {
        RolloutLog::open_and_recover(&bootstrap.state_dir, &session_id)?
    } else {
        let hdr = RolloutHeader {
            format_version: 1,
            session_id: session_id.clone(),
            parent_session_id: bootstrap.parent_session_id.clone(),
            forked_from_turn_id: None,
            cwd: canonical_ws.clone(),
            model: initial_caps.spec.clone(),
            created_at_epoch_secs: now_epoch,
        };
        let log = RolloutLog::create(&bootstrap.state_dir, &hdr)?;
        (log, vec![RolloutRecord::Header(hdr)])
    };

    {
        let idx = state_index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        idx.upsert_session(&SessionSummary {
            id: session_id.clone(),
            parent_id: bootstrap.parent_session_id.clone(),
            forked_from_turn_id: None,
            cwd: canonical_ws.clone(),
            project_id: bootstrap.config.project_root.to_string_lossy().into_owned(),
            model: initial_caps.spec.clone(),
            title: None,
            preview: String::new(),
            archived: false,
            created_at: now_epoch,
            updated_at: now_epoch,
        })?;
    }

    let persisted_grants: Vec<_> = {
        let idx = state_index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        idx.load_permission_grants(&canonical_ws)?
            .into_iter()
            .map(|g| g.scope)
            .collect()
    };
    // Windows has no HOME; fall back to USERPROFILE so the broad-grant
    // safety confirmation tied to the home directory still applies there.
    let home_dir = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let evaluator = Arc::new(Mutex::new(PermissionEvaluator::new(
        mode,
        canonical_ws.clone(),
        home_dir,
        bootstrap.config.permission_rules.clone(),
        persisted_grants,
    )));

    let mut base_tools = build_tool_definitions_for_model(&initial_caps);
    if !bootstrap.allow_subagents {
        base_tools.retain(|t| t.name != "task");
    }
    base_tools.extend(bootstrap.extra_plugin_tools.clone());

    let developer_instructions = build_developer_instructions(&bootstrap.config, &canonical_ws);
    // Plugin `session_start` hook: rollout notes only — no live subscribers exist
    // before the actor loop starts, so transcript events would be lost.
    if let Some(exec) = &bootstrap.plugin_executor {
        let outcome = exec.dispatch_hook(HookEvent::SessionStart, HookEventPayload::default());
        let push_note = |kind: &str,
                         text: &str,
                         rollout_log: &mut tekvra_storage::RolloutLog,
                         rollout_records: &mut Vec<RolloutRecord>| {
            let record = RolloutRecord::SteeringNote {
                turn_id: "session_start".to_string(),
                kind: kind.to_string(),
                text: text.to_string(),
            };
            if rollout_log.append(&record).is_ok() {
                rollout_records.push(record);
            }
        };
        for note in &outcome.notes {
            push_note("plugin", note, &mut rollout_log, &mut rollout_records);
        }
        for failure in &outcome.failures {
            push_note(
                "plugin_error",
                failure,
                &mut rollout_log,
                &mut rollout_records,
            );
        }
    }
    // Resume fidelity: replay persisted turn usage and the latest `plan` tool
    // output so `cumulative_usage` and `plan_markdown` survive a restart.
    let mut restored_usage = TokenUsage::default();
    let mut restored_plan = String::new();
    for record in &rollout_records {
        match record {
            RolloutRecord::TurnCompleted { usage, .. } => {
                accumulate_usage(&mut restored_usage, usage);
            }
            RolloutRecord::ToolCard {
                output: ToolOutput::Plan { markdown },
                ..
            } => restored_plan = markdown.clone(),
            _ => {}
        }
    }
    let restored_messages: Vec<ProviderMessage> = rollout_records
        .iter()
        .filter_map(|r| match r {
            RolloutRecord::Message { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect();
    let frame = Frame::from_messages(
        0,
        developer_instructions.clone(),
        restored_messages,
        base_tools,
        initial_caps.uses_apply_patch,
    );

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<SessionCommand>(128);
    let (event_tx, _) = broadcast::channel::<SessionEvent>(512);
    let (work_tx, mut work_rx) = mpsc::unbounded_channel::<RunnerWork>();
    let (pty_done_tx, mut pty_done_rx) = mpsc::unbounded_channel::<PtyCompletionNotice>();
    let (subagent_done_tx, mut subagent_done_rx) =
        mpsc::unbounded_channel::<BackgroundSubagentNotice>();

    let pty_manager = PtySessionManager::new(Some(pty_done_tx));
    let subagents = bootstrap.allow_subagents.then(|| {
        SubagentManager::new(SubagentManagerDeps {
            parent_session_id: session_id.clone(),
            workspace_root: canonical_ws.clone(),
            state_dir: bootstrap.state_dir.clone(),
            config: bootstrap.config.clone(),
            router: bootstrap.router.clone(),
            driver: Arc::clone(&bootstrap.driver),
            state_index: Some(Arc::clone(&state_index)),
            parent_wakeup_tx: subagent_done_tx,
            depth: bootstrap.subagent_depth,
        })
    });

    let shared_state = Arc::new(Mutex::new(SessionRuntimeState {
        session_id: session_id.clone(),
        cwd: canonical_ws.clone(),
        active_model: initial_caps.spec.clone(),
        mode,
        goal: None,
        plan_markdown: restored_plan.clone(),
        cumulative_usage: restored_usage,
        context_window: initial_caps.context_window,
        last_frame_snapshot: Some(frame.snapshot()),
    }));

    let is_turn_running = Arc::new(AtomicBool::new(false));
    let current_cancel: Arc<Mutex<CancellationToken>> =
        Arc::new(Mutex::new(CancellationToken::new()));
    let guide_inbox: Arc<Mutex<VecDeque<PendingUserInput>>> = Arc::new(Mutex::new(VecDeque::new()));
    let replace_slot: Arc<Mutex<Option<PendingUserInput>>> = Arc::new(Mutex::new(None));
    let pending_approvals: PendingApprovalsMap = Arc::new(Mutex::new(HashMap::new()));
    let pending_questions: PendingQuestionsMap = Arc::new(Mutex::new(HashMap::new()));

    // Spawn lightweight Command Router Task so `Guide`, `Replace`, `InterruptTurn`,
    // `ResolveApproval`, and `ResolveUserInput` take effect IMMEDIATELY during an active turn!
    {
        let router_running = Arc::clone(&is_turn_running);
        let router_cancel = Arc::clone(&current_cancel);
        let router_guide = Arc::clone(&guide_inbox);
        let router_replace = Arc::clone(&replace_slot);
        let router_approvals = Arc::clone(&pending_approvals);
        let router_questions = Arc::clone(&pending_questions);
        let router_work_tx = work_tx;

        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    SessionCommand::ResolveApproval {
                        approval_id,
                        resolution,
                    } => {
                        let tx_opt = router_approvals
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&approval_id);
                        if let Some(tx) = tx_opt {
                            let _ = tx.send(resolution);
                        }
                    }
                    SessionCommand::ResolveUserInput { item_id, answers } => {
                        let tx_opt = router_questions
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&item_id);
                        if let Some(tx) = tx_opt {
                            let _ = tx.send(answers);
                        }
                    }
                    SessionCommand::InterruptTurn => {
                        router_cancel
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .cancel();
                    }
                    SessionCommand::SubmitInput {
                        prompt,
                        delivery,
                        images,
                    } => {
                        if router_running.load(Ordering::SeqCst) {
                            match delivery {
                                InputDeliveryMode::Next => {
                                    let _ = router_work_tx
                                        .send(RunnerWork::UserTurn { prompt, images });
                                }
                                InputDeliveryMode::Guide => {
                                    router_guide
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .push_back((prompt, images));
                                }
                                InputDeliveryMode::Replace => {
                                    *router_replace
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                        Some((prompt, images));
                                    router_cancel
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .cancel();
                                }
                            }
                        } else {
                            let _ = router_work_tx.send(RunnerWork::UserTurn { prompt, images });
                        }
                    }
                    SessionCommand::RevertTo { target_id, scope } => {
                        let _ = router_work_tx.send(RunnerWork::RevertTo { target_id, scope });
                    }
                    SessionCommand::UndoLastTurn { scope } => {
                        let _ = router_work_tx.send(RunnerWork::UndoLastTurn { scope });
                    }
                    SessionCommand::Compact => {
                        let _ = router_work_tx.send(RunnerWork::Compact);
                    }
                    SessionCommand::SetGoal { condition } => {
                        let _ = router_work_tx.send(RunnerWork::SetGoal(condition));
                    }
                    SessionCommand::SetMode { mode } => {
                        let _ = router_work_tx.send(RunnerWork::SetMode(mode));
                    }
                    SessionCommand::SwitchModel { model } => {
                        let _ = router_work_tx.send(RunnerWork::SwitchModel(model));
                    }
                }
            }
        });
    }

    // Spawn single-owner Turn Runner Task owning `Frame` and `RolloutLog`
    let runner_session_id = session_id.clone();
    let runner_event_tx = event_tx.clone();
    let runner_shared_state = Arc::clone(&shared_state);
    let runner_subagents = subagents.clone();
    let runner_pty = pty_manager.clone();

    tokio::spawn(async move {
        let mut actor = SessionActor {
            session_id: runner_session_id.clone(),
            workspace_root: canonical_ws,
            developer_instructions,
            config: bootstrap.config,
            router: bootstrap.router,
            driver: bootstrap.driver,
            compaction: bootstrap.compaction,
            rollout_log,
            rollout_records,
            state_index,
            snapshots: WorkspaceSnapshotStore::new(&bootstrap.state_dir),
            permissions: evaluator,
            path_locks: WorkspacePathLocks::new(),
            pty_manager: runner_pty,
            mcp_gateway: bootstrap.mcp_gateway,
            plugin_executor: bootstrap.plugin_executor,
            extra_plugin_tools: bootstrap.extra_plugin_tools,
            allow_subagents: bootstrap.allow_subagents,
            subagents: runner_subagents,
            frame,
            mode,
            active_goal: None,
            plan_markdown: Arc::new(Mutex::new(restored_plan)),
            cumulative_usage: restored_usage,
            bridge: Arc::new(InteractiveBridge {
                session_id: runner_session_id,
                turn_id: Mutex::new(String::new()),
                event_tx: runner_event_tx.clone(),
                pending_approvals,
                pending_questions,
            }),
            event_tx: runner_event_tx,
            shared_state: runner_shared_state,
            is_turn_running,
            current_cancel,
            guide_inbox,
            replace_slot,
            title_done: false,
        };

        loop {
            // Priority 1: if a `Replace` prompt preempted the previous turn, run it immediately!
            let replaced = actor
                .replace_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some((prompt, images)) = replaced {
                actor.run_user_turn(prompt, images).await;
                continue;
            }

            tokio::select! {
                biased;
                maybe_work = work_rx.recv() => {
                    let Some(work) = maybe_work else { break; };
                    actor.handle_work(work).await;
                }
                Some(bg_notice) = subagent_done_rx.recv() => {
                    // Idle parent session immediately wakes up on background subagent completion!
                    let mut reminder = format!(
                        "Background subagent `{}` (role `{}`) finished with status `{}`:\n{}",
                        bg_notice.task_id, bg_notice.role, bg_notice.status, bg_notice.summary
                    );
                    if let Some(branch) = &bg_notice.worktree_branch {
                        reminder.push_str(&format!(
                            "\n\nThe task ran isolated in worktree branch `{branch}`; merge its work into the parent tree with `task {{ task_id: \"{}\", merge: \"merge\" }}` or drop it with `merge: \"discard\"`.",
                            bg_notice.task_id
                        ));
                        if let Some(diff) = &bg_notice.diff_summary {
                            reminder.push_str(&format!("\n\nchanged files:\n{diff}"));
                        }
                    }
                    actor.run_wakeup_turn("background_subagent", reminder).await;
                }
                Some(pty_notice) = pty_done_rx.recv() => {
                    let reminder = format!(
                        "Background PTY session `{}` (`{}`) exited with status {:?}.",
                        pty_notice.session_id, pty_notice.command, pty_notice.exit_code
                    );
                    actor.run_wakeup_turn("background_pty", reminder).await;
                }
            }
        }
    });

    Ok(SessionHandle {
        session_id,
        cmd_tx,
        event_tx,
        shared_state,
        subagents,
        pty_manager,
    })
}

struct SessionActor {
    session_id: String,
    workspace_root: PathBuf,
    developer_instructions: String,
    config: ResolvedConfig,
    router: ModelRouter,
    driver: Arc<dyn ProviderDriver>,
    compaction: CompactionConfig,
    rollout_log: RolloutLog,
    rollout_records: Vec<RolloutRecord>,
    state_index: Arc<Mutex<StateIndex>>,
    snapshots: WorkspaceSnapshotStore,
    permissions: Arc<Mutex<PermissionEvaluator>>,
    path_locks: WorkspacePathLocks,
    pty_manager: PtySessionManager,
    mcp_gateway: Option<Arc<dyn McpGateway>>,
    plugin_executor: Option<Arc<dyn PluginToolExecutor>>,
    extra_plugin_tools: Vec<ToolDefinition>,
    allow_subagents: bool,
    subagents: Option<SubagentManager>,
    frame: Frame,
    mode: PermissionMode,
    active_goal: Option<String>,
    plan_markdown: Arc<Mutex<String>>,
    cumulative_usage: TokenUsage,
    bridge: Arc<InteractiveBridge>,
    event_tx: broadcast::Sender<SessionEvent>,
    shared_state: Arc<Mutex<SessionRuntimeState>>,
    is_turn_running: Arc<AtomicBool>,
    current_cancel: Arc<Mutex<CancellationToken>>,
    guide_inbox: Arc<Mutex<VecDeque<PendingUserInput>>>,
    replace_slot: Arc<Mutex<Option<PendingUserInput>>>,
    /// Set once the opt-in `JobRole::Title` background session title has run.
    title_done: bool,
}

impl SessionActor {
    async fn handle_work(&mut self, work: RunnerWork) {
        match work {
            RunnerWork::UserTurn { prompt, images } => self.run_user_turn(prompt, images).await,
            RunnerWork::SetGoal(condition) => {
                self.active_goal = condition;
                self.sync_shared_state();
            }
            RunnerWork::SetMode(new_mode) => {
                if self.mode != new_mode {
                    let before = self.frame.snapshot();
                    let old_mode = self.mode;
                    self.mode = new_mode;
                    self.permissions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .set_mode(new_mode);
                    let plan_md = self
                        .plan_markdown
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    let msg = self
                        .frame
                        .append_mode_transition(old_mode, new_mode, &plan_md);
                    if let Ok((_, caps)) = self.router.route_for_mode(new_mode) {
                        let allow_sub = self.allow_subagents;
                        let extras = self.extra_plugin_tools.clone();
                        self.frame.sync_tools_for_model(caps.uses_apply_patch, || {
                            let mut defs = build_tool_definitions_for_model(&caps);
                            if !allow_sub {
                                defs.retain(|t| t.name != "task");
                            }
                            defs.extend(extras);
                            defs
                        });
                    }
                    // Strictly verify prior prefix bytes are untouched!
                    if let Err(err) = self.frame.assert_append_only(&before) {
                        let _ = self.event_tx.send(SessionEvent::ConfigWarning {
                            summary: err.to_string(),
                            details: None,
                        });
                    }
                    let _ = self.append_rollout(RolloutRecord::Message {
                        turn_id: "mode_transition".to_string(),
                        message: msg,
                    });
                    self.sync_shared_state();
                }
            }
            RunnerWork::SwitchModel(model_spec) => {
                if let Ok((_, caps)) = self.router.registry().resolve_model_spec(&model_spec) {
                    let before = self.frame.snapshot();
                    self.router.set_active_main_model(caps.spec.clone());
                    let allow_sub = self.allow_subagents;
                    let extras = self.extra_plugin_tools.clone();
                    self.frame.sync_tools_for_model(caps.uses_apply_patch, || {
                        let mut defs = build_tool_definitions_for_model(&caps);
                        if !allow_sub {
                            defs.retain(|t| t.name != "task");
                        }
                        defs.extend(extras);
                        defs
                    });
                    let _ = self.frame.assert_append_only(&before);
                    self.sync_shared_state();
                } else {
                    let _ = self.event_tx.send(SessionEvent::ConfigWarning {
                        summary: format!("Unknown provider in model spec `{model_spec}`"),
                        details: None,
                    });
                }
            }
            RunnerWork::RevertTo { target_id, scope } => {
                self.revert_to_turn(&target_id, scope);
            }
            RunnerWork::UndoLastTurn { scope } => {
                let latest_turn = self.rollout_records.iter().rev().find_map(|r| match r {
                    RolloutRecord::TurnStarted { turn_id, .. } => Some(turn_id.clone()),
                    _ => None,
                });
                match latest_turn {
                    Some(turn_id) => self.revert_to_turn(&turn_id, scope),
                    None => {
                        let _ = self.event_tx.send(SessionEvent::ConfigWarning {
                            summary: "Nothing to undo: no recorded turns in this session yet."
                                .to_string(),
                            details: None,
                        });
                    }
                }
            }
            RunnerWork::Compact => {
                let before_tokens = self.frame.estimated_tokens();
                let compact_route = self
                    .router
                    .route(JobRole::Compact)
                    .map(|(p, m)| (p.clone(), m));
                match compact_route {
                    Ok((compact_provider, compact_model)) => {
                        match self
                            .frame
                            .force_compact(
                                &self.session_id,
                                &compact_provider,
                                &compact_model,
                                self.driver.as_ref(),
                                self.compaction.preserve_recent_messages,
                            )
                            .await
                        {
                            Ok(summary) => {
                                let after_tokens = self.frame.estimated_tokens();
                                let _ = self.event_tx.send(SessionEvent::CompactionCompleted {
                                    thread_id: self.session_id.clone(),
                                    generation: self.frame.generation(),
                                    before_tokens,
                                    after_tokens,
                                });
                                let _ = self.append_rollout(RolloutRecord::SteeringNote {
                                    turn_id: "manual_compaction".to_string(),
                                    kind: "compaction".to_string(),
                                    text: format!(
                                        "Context compacted to generation {} ({before_tokens} -> {after_tokens} est. tokens):\n{summary}",
                                        self.frame.generation()
                                    ),
                                });
                            }
                            Err(err) => {
                                let _ = self.event_tx.send(SessionEvent::ConfigWarning {
                                    summary: format!("Manual compaction failed: {err}"),
                                    details: None,
                                });
                            }
                        }
                    }
                    Err(err) => {
                        let _ = self.event_tx.send(SessionEvent::ConfigWarning {
                            summary: format!(
                                "Manual compaction failed to route compact model: {err}"
                            ),
                            details: None,
                        });
                    }
                }
                self.sync_shared_state();
            }
        }
    }

    /// Shared revert pipeline used by `RevertTo` and `UndoLastTurn`.
    fn revert_to_turn(&mut self, target_id: &str, scope: RevertScope) {
        let now_epoch = jiff::Timestamp::now().as_second();
        let res = {
            let idx = self
                .state_index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.snapshots.revert_from_turn(
                &idx,
                &mut self.rollout_log,
                &self.rollout_records,
                target_id,
                scope,
                now_epoch,
            )
        };
        match res {
            Ok(outcome) => {
                if matches!(
                    scope,
                    RevertScope::ConversationOnly | RevertScope::ConversationAndFiles
                ) {
                    self.rollout_records.truncate(outcome.remaining_records);
                    let restored_msgs: Vec<ProviderMessage> = self
                        .rollout_records
                        .iter()
                        .filter_map(|r| match r {
                            RolloutRecord::Message { message, .. } => Some(message.clone()),
                            _ => None,
                        })
                        .collect();
                    if let Ok((_, caps)) = self.router.route_for_mode(self.mode) {
                        let mut tools = build_tool_definitions_for_model(&caps);
                        if !self.allow_subagents {
                            tools.retain(|t| t.name != "task");
                        }
                        tools.extend(self.extra_plugin_tools.clone());
                        self.frame = Frame::from_messages(
                            self.frame.generation() + 1,
                            self.developer_instructions.clone(),
                            restored_msgs,
                            tools,
                            caps.uses_apply_patch,
                        );
                    }
                }
                self.sync_shared_state();
            }
            Err(err) => {
                let _ = self.event_tx.send(SessionEvent::ConfigWarning {
                    summary: format!("Revert failed: {err}"),
                    details: None,
                });
            }
        }
    }

    async fn run_wakeup_turn(&mut self, reason: &str, reminder_body: String) {
        let turn_id = format!("turn_{}", Uuid::now_v7().simple());
        let before = self.frame.snapshot();
        let msg = self.frame.append_system_reminder(&reminder_body);
        let _ = self.frame.assert_append_only(&before);

        let now = jiff::Timestamp::now().as_second();
        let _ = self.append_rollout(RolloutRecord::TurnStarted {
            turn_id: turn_id.clone(),
            started_at: now,
        });
        self.dispatch_turn_started_hook(&turn_id);
        let _ = self.append_rollout(RolloutRecord::SteeringNote {
            turn_id: turn_id.clone(),
            kind: reason.to_string(),
            text: reminder_body.clone(),
        });
        let _ = self.append_rollout(RolloutRecord::Message {
            turn_id: turn_id.clone(),
            message: msg,
        });
        let _ = self.event_tx.send(SessionEvent::SteeringInjected {
            thread_id: self.session_id.clone(),
            turn_id: turn_id.clone(),
            reason: reason.to_string(),
            message: reminder_body,
        });

        self.execute_turn_loop(turn_id).await;
    }

    async fn run_user_turn(&mut self, prompt: String, images: Vec<InputImage>) {
        let turn_id = format!("turn_{}", Uuid::now_v7().simple());
        let now = jiff::Timestamp::now().as_second();

        // Zero-roundtrip preprocessing: `@path` / `@path#L<n>-L<m>` attachments and `/skill` bodies.
        let expansion = expand_prompt_attachments(&prompt, &self.workspace_root);
        for warning in &expansion.warnings {
            let _ = self.event_tx.send(SessionEvent::ConfigWarning {
                summary: warning.clone(),
                details: None,
            });
        }
        let effective_text = match expand_skill_invocation(&expansion.text, &self.config.skills) {
            SkillInvocation::None => expansion.text,
            SkillInvocation::Expanded { expanded } => expanded,
            SkillInvocation::Unknown { name } => {
                let _ = self.event_tx.send(SessionEvent::ConfigWarning {
                    summary: format!(
                        "Unknown skill `{name}`; continuing without skill instructions."
                    ),
                    details: None,
                });
                expansion.text
            }
        };

        let mut parts: Vec<MessagePart> = Vec::new();
        let has_images = !expansion.images.is_empty() || !images.is_empty();
        if !effective_text.is_empty() || !has_images {
            parts.push(MessagePart::Text {
                text: effective_text,
                cache_breakpoint: false,
            });
        }
        parts.extend(expansion.images);
        for img in images {
            parts.push(MessagePart::Image {
                media_type: img.media_type,
                data_base64: img.data_base64,
                source_path: img.source_path,
            });
        }
        let original_indexed_text = prompt.clone();
        let user_msg = ProviderMessage {
            role: MessageRole::User,
            parts,
        };

        let before = self.frame.snapshot();
        self.frame.push_message(user_msg.clone());
        let _ = self.frame.assert_append_only(&before);

        let _ = self.append_rollout(RolloutRecord::TurnStarted {
            turn_id: turn_id.clone(),
            started_at: now,
        });
        self.dispatch_turn_started_hook(&turn_id);
        let _ = self.append_rollout(RolloutRecord::Message {
            turn_id: turn_id.clone(),
            message: user_msg,
        });

        {
            let idx = self
                .state_index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ =
                idx.index_message_text(&self.session_id, &turn_id, "user", &original_indexed_text);
            if let Ok(Some(mut summary)) = idx.get_session(&self.session_id) {
                if summary.preview.is_empty() {
                    summary.preview = original_indexed_text.chars().take(120).collect();
                }
                summary.updated_at = now;
                let _ = idx.upsert_session(&summary);
            }
        }

        self.execute_turn_loop(turn_id).await;
    }

    async fn execute_turn_loop(&mut self, turn_id: String) {
        let cancel_token = CancellationToken::new();
        *self
            .current_cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cancel_token.clone();
        *self
            .bridge
            .turn_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = turn_id.clone();
        self.is_turn_running.store(true, Ordering::SeqCst);

        let _ = self.event_tx.send(SessionEvent::TurnStarted {
            thread_id: self.session_id.clone(),
            turn_id: turn_id.clone(),
        });

        let mut steering = SteeringDetector::new(self.config.settings.agent.max_steering_nudges);
        let mut failure_tracker = ToolFailureTracker::new();
        let mut had_tool_execution = false;
        let mut goal_iterations = 0u32;
        let mut turn_usage = TokenUsage::default();
        let mut turn_diffs: Vec<FileDiffEntry> = Vec::new();
        let mut final_status = "completed".to_string();
        let mut final_error: Option<String> = None;

        let registry = NativeToolRegistry::new();

        loop {
            if cancel_token.is_cancelled() {
                final_status = "interrupted".to_string();
                break;
            }

            // Drain any pending `Guide` inputs at this safe turn boundary BEFORE calling the provider!
            self.drain_guides_into_frame(&turn_id);

            let (provider, model) = match self.router.route_for_mode(self.mode) {
                Ok((p, m)) => (p.clone(), m),
                Err(err) => {
                    final_status = "failed".to_string();
                    final_error = Some(err.to_string());
                    break;
                }
            };

            // Context compaction check before assembling outbound request
            if let Ok((compact_prov, compact_model)) =
                self.router.route(tekvra_providers::JobRole::Compact)
            {
                let c_prov = compact_prov.clone();
                let _ = self
                    .frame
                    .compact_if_needed(
                        &self.session_id,
                        &model,
                        &c_prov,
                        &compact_model,
                        self.driver.as_ref(),
                        self.compaction,
                    )
                    .await;
            }

            let pre_req_snapshot = self.frame.snapshot();
            let req = self.frame.build_completion_request(
                &self.session_id,
                self.config.settings.agent.reasoning_effort.clone(),
                None,
            );

            // Emit the assistant item start BEFORE dispatching so hosts can render
            // live deltas against a stable item id while the stream decodes.
            let msg_item_id = format!("msg_{}", Uuid::now_v7().simple());
            let _ = self.event_tx.send(SessionEvent::ItemStarted {
                thread_id: self.session_id.clone(),
                turn_id: turn_id.clone(),
                item_id: msg_item_id.clone(),
                kind: "agent_message".to_string(),
            });
            let delta_event_tx = self.event_tx.clone();
            let delta_thread_id = self.session_id.clone();
            let delta_turn_id = turn_id.clone();
            let delta_item_id = msg_item_id.clone();
            let on_delta: StreamDeltaSink = Arc::new(move |delta: StreamDelta| {
                let (channel, text) = match delta {
                    StreamDelta::TextDelta(text) => ("text", text),
                    StreamDelta::ReasoningSummaryDelta(text) => ("reasoning_summary", text),
                };
                let _ = delta_event_tx.send(SessionEvent::ItemDelta {
                    thread_id: delta_thread_id.clone(),
                    turn_id: delta_turn_id.clone(),
                    item_id: delta_item_id.clone(),
                    channel: channel.to_string(),
                    delta: text,
                });
            });

            let complete_res = tokio::select! {
                biased;
                () = cancel_token.cancelled() => {
                    final_status = "interrupted".to_string();
                    break;
                }
                res = self.driver.complete_streaming(&provider, &model, &req, Some(on_delta)) => res,
            };

            let resp = match complete_res {
                Ok(r) => r,
                Err(err) => {
                    final_status = "failed".to_string();
                    final_error = Some(err.to_string());
                    break;
                }
            };

            accumulate_usage(&mut turn_usage, &resp.usage);
            accumulate_usage(&mut self.cumulative_usage, &resp.usage);
            let _ = self.event_tx.send(SessionEvent::TokenUsageUpdated {
                thread_id: self.session_id.clone(),
                turn_id: turn_id.clone(),
                usage: self.cumulative_usage,
            });

            // Live deltas were already forwarded from `complete_streaming`; only the
            // completed item is announced here (no duplicate replay after the fact).
            let assistant_text = extract_assistant_text(&resp.assistant_message);
            let _ = self.event_tx.send(SessionEvent::ItemCompleted {
                thread_id: self.session_id.clone(),
                turn_id: turn_id.clone(),
                item_id: msg_item_id,
                text: assistant_text.clone(),
                tool_output: None,
            });

            // Append assistant message verbatim (including `encrypted_content` reasoning items!)
            self.frame.push_message(resp.assistant_message.clone());
            if let Err(err) = self.frame.assert_append_only(&pre_req_snapshot) {
                final_status = "failed".to_string();
                final_error = Some(err.to_string());
                break;
            }
            let _ = self.append_rollout(RolloutRecord::Message {
                turn_id: turn_id.clone(),
                message: resp.assistant_message.clone(),
            });
            if !assistant_text.trim().is_empty() {
                let idx = self
                    .state_index
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = idx.index_message_text(
                    &self.session_id,
                    &turn_id,
                    "assistant",
                    &assistant_text,
                );
            }

            let tool_calls: Vec<(String, String, String)> = resp
                .assistant_message
                .parts
                .iter()
                .filter_map(|p| match p {
                    MessagePart::ToolCall {
                        call_id,
                        name,
                        arguments_json,
                    } => Some((call_id.clone(), name.clone(), arguments_json.clone())),
                    _ => None,
                })
                .collect();

            if !tool_calls.is_empty() {
                let mut tool_result_parts = Vec::with_capacity(tool_calls.len());
                let mut attached_images: Vec<MessagePart> = Vec::new();
                let mut interrupted_mid_tools = false;

                for (call_id, tool_name, args_json) in tool_calls {
                    if cancel_token.is_cancelled() {
                        interrupted_mid_tools = true;
                        tool_result_parts.push(MessagePart::ToolResult {
                            call_id,
                            output: "[turn interrupted before tool started]".to_string(),
                            is_error: true,
                        });
                        continue;
                    }

                    let _ = self.event_tx.send(SessionEvent::ItemStarted {
                        thread_id: self.session_id.clone(),
                        turn_id: turn_id.clone(),
                        item_id: call_id.clone(),
                        kind: "tool_call".to_string(),
                    });

                    // Check 3-strike identical failing tool call refusal BEFORE executing!
                    if let Some(strike_nudge) =
                        failure_tracker.check_third_strike(&tool_name, &args_json)
                    {
                        let _ = self.event_tx.send(SessionEvent::SteeringInjected {
                            thread_id: self.session_id.clone(),
                            turn_id: turn_id.clone(),
                            reason: strike_nudge.reason.clone(),
                            message: strike_nudge.message.clone(),
                        });
                        let _ = self.append_rollout(RolloutRecord::SteeringNote {
                            turn_id: turn_id.clone(),
                            kind: strike_nudge.reason,
                            text: strike_nudge.message.clone(),
                        });
                        let _ = self.event_tx.send(SessionEvent::ItemCompleted {
                            thread_id: self.session_id.clone(),
                            turn_id: turn_id.clone(),
                            item_id: call_id.clone(),
                            text: strike_nudge.message.clone(),
                            tool_output: None,
                        });
                        tool_result_parts.push(MessagePart::ToolResult {
                            call_id,
                            output: strike_nudge.message,
                            is_error: true,
                        });
                        continue;
                    }

                    // Plugin lifecycle hook: `pre_tool_use` may deny the call before
                    // any permission prompt or side effect (never burns a prompt).
                    let plugin_deny_reason = self
                        .dispatch_hook_with_notes(
                            &turn_id,
                            HookEvent::PreToolUse,
                            HookEventPayload {
                                tool: Some(tool_name.clone()),
                                arguments: Some(args_json.clone()),
                                call_id: Some(call_id.clone()),
                                ..HookEventPayload::default()
                            },
                        )
                        .denied;
                    if let Some(reason) = plugin_deny_reason {
                        let deny_text = format!("denied by plugin: {reason}");
                        self.record_hook_note(
                            &turn_id,
                            "plugin_deny",
                            &format!("{tool_name}: {reason}"),
                        );
                        let _ = self.event_tx.send(SessionEvent::ItemCompleted {
                            thread_id: self.session_id.clone(),
                            turn_id: turn_id.clone(),
                            item_id: call_id.clone(),
                            text: deny_text.clone(),
                            tool_output: None,
                        });
                        tool_result_parts.push(MessagePart::ToolResult {
                            call_id,
                            output: deny_text,
                            is_error: true,
                        });
                        continue;
                    }

                    let exec_res = tokio::select! {
                        biased;
                        () = cancel_token.cancelled() => {
                            interrupted_mid_tools = true;
                            Err("interrupted by user".to_string())
                        }
                        res = self.dispatch_single_tool(&registry, &turn_id, &call_id, &tool_name, &args_json) => res,
                    };

                    had_tool_execution = true;
                    match exec_res {
                        Ok(output) => {
                            let terminal_failed = matches!(
                                &output,
                                ToolOutput::Terminal {
                                    exit_code: Some(code),
                                    ..
                                } if *code != 0
                            );
                            failure_tracker.record_outcome(&tool_name, &args_json, terminal_failed);

                            if let ToolOutput::Diff { files, snapshot_id } = &output {
                                turn_diffs.extend(files.iter().cloned());
                                if let Some(sid) = snapshot_id {
                                    let _ = self.append_rollout(RolloutRecord::SnapshotRecorded {
                                        turn_id: turn_id.clone(),
                                        snapshot_id: sid.clone(),
                                    });
                                }
                                let _ = self.event_tx.send(SessionEvent::DiffUpdated {
                                    thread_id: self.session_id.clone(),
                                    turn_id: turn_id.clone(),
                                    files: turn_diffs.clone(),
                                });
                            } else if let ToolOutput::Plan { markdown } = &output {
                                let _ = self.event_tx.send(SessionEvent::PlanUpdated {
                                    thread_id: self.session_id.clone(),
                                    turn_id: turn_id.clone(),
                                    markdown: markdown.clone(),
                                });
                            }

                            let model_text = output.to_model_text();
                            // Multimodal delivery: remember inspected images so a
                            // vision-capable model receives real image blocks.
                            if let ToolOutput::Image {
                                path,
                                media_type,
                                data_base64,
                                ..
                            } = &output
                            {
                                attached_images.push(MessagePart::Image {
                                    media_type: media_type.clone(),
                                    data_base64: data_base64.clone(),
                                    source_path: Some(path.display().to_string()),
                                });
                            }
                            let _ = self.append_rollout(RolloutRecord::ToolCard {
                                turn_id: turn_id.clone(),
                                call_id: call_id.clone(),
                                tool_name: tool_name.clone(),
                                output: output.clone(),
                            });
                            // Plugin lifecycle hook: `post_tool_use` observes the
                            // completed call (result already recorded; notes only).
                            self.dispatch_hook_with_notes(
                                &turn_id,
                                HookEvent::PostToolUse,
                                HookEventPayload {
                                    tool: Some(tool_name.clone()),
                                    arguments: Some(args_json.clone()),
                                    call_id: Some(call_id.clone()),
                                    ..HookEventPayload::default()
                                },
                            );
                            let _ = self.event_tx.send(SessionEvent::ItemCompleted {
                                thread_id: self.session_id.clone(),
                                turn_id: turn_id.clone(),
                                item_id: call_id.clone(),
                                text: model_text.clone(),
                                tool_output: Some(output),
                            });
                            tool_result_parts.push(MessagePart::ToolResult {
                                call_id,
                                output: model_text,
                                is_error: terminal_failed,
                            });
                        }
                        Err(err_text) => {
                            if !interrupted_mid_tools {
                                failure_tracker.record_outcome(&tool_name, &args_json, true);
                            }
                            let _ = self.event_tx.send(SessionEvent::ItemCompleted {
                                thread_id: self.session_id.clone(),
                                turn_id: turn_id.clone(),
                                item_id: call_id.clone(),
                                text: err_text.clone(),
                                tool_output: None,
                            });
                            tool_result_parts.push(MessagePart::ToolResult {
                                call_id,
                                output: err_text,
                                is_error: true,
                            });
                        }
                    }
                }

                let before_tool_msg = self.frame.snapshot();
                let tool_msg = ProviderMessage {
                    role: MessageRole::Tool,
                    parts: tool_result_parts,
                };
                self.frame.push_message(tool_msg.clone());
                let _ = self.frame.assert_append_only(&before_tool_msg);
                let _ = self.append_rollout(RolloutRecord::Message {
                    turn_id: turn_id.clone(),
                    message: tool_msg,
                });

                // Vision-capable models receive inspected images as a trailing
                // multimodal user attachment (append-only, after the tool result).
                if model.supports_vision && !attached_images.is_empty() {
                    let before_attach = self.frame.snapshot();
                    let attach_msg = ProviderMessage {
                        role: MessageRole::User,
                        parts: attached_images,
                    };
                    self.frame.push_message(attach_msg.clone());
                    let _ = self.frame.assert_append_only(&before_attach);
                    let _ = self.append_rollout(RolloutRecord::Message {
                        turn_id: turn_id.clone(),
                        message: attach_msg,
                    });
                }

                if interrupted_mid_tools {
                    final_status = "interrupted".to_string();
                    break;
                }
                // Continue to next model step (where any `Guide` inputs queued during tool execution will be drained!)
                continue;
            }

            // No tool calls in this response!
            // 1. Check if a `Guide` prompt arrived while the model was generating; if so, inject and continue!
            if self.drain_guides_into_frame(&turn_id) {
                continue;
            }

            // 2. Check Caudra-style automatic steering (`empty_after_tools` / `abandoned_turn`)
            if let Some(nudge) =
                steering.evaluate_no_tool_response(&resp.assistant_message, had_tool_execution)
            {
                let before_nudge = self.frame.snapshot();
                let msg = self.frame.append_system_reminder(&nudge.message);
                let _ = self.frame.assert_append_only(&before_nudge);
                let _ = self.append_rollout(RolloutRecord::SteeringNote {
                    turn_id: turn_id.clone(),
                    kind: nudge.reason.clone(),
                    text: nudge.message.clone(),
                });
                let _ = self.append_rollout(RolloutRecord::Message {
                    turn_id: turn_id.clone(),
                    message: msg,
                });
                let _ = self.event_tx.send(SessionEvent::SteeringInjected {
                    thread_id: self.session_id.clone(),
                    turn_id: turn_id.clone(),
                    reason: nudge.reason,
                    message: nudge.message,
                });
                continue;
            }

            // 3. Check active tool-less `/goal <condition>` evaluator!
            if let Some(goal_cond) = self.active_goal.clone()
                && goal_iterations < self.config.settings.agent.max_goal_iterations
            {
                goal_iterations += 1;
                match evaluate_goal(
                    &self.session_id,
                    &goal_cond,
                    self.frame.messages(),
                    &self.router,
                    self.driver.as_ref(),
                )
                .await
                {
                    Ok(verdict) => {
                        accumulate_usage(&mut turn_usage, &verdict.usage);
                        accumulate_usage(&mut self.cumulative_usage, &verdict.usage);
                        if !verdict.met {
                            let reminder = format!(
                                "Active `/goal` condition is NOT YET MET (`{goal_cond}`):\n{}",
                                verdict.feedback
                            );
                            let before_goal = self.frame.snapshot();
                            let msg = self.frame.append_system_reminder(&reminder);
                            let _ = self.frame.assert_append_only(&before_goal);
                            let _ = self.append_rollout(RolloutRecord::SteeringNote {
                                turn_id: turn_id.clone(),
                                kind: "goal_unmet".to_string(),
                                text: reminder.clone(),
                            });
                            let _ = self.append_rollout(RolloutRecord::Message {
                                turn_id: turn_id.clone(),
                                message: msg,
                            });
                            let _ = self.event_tx.send(SessionEvent::SteeringInjected {
                                thread_id: self.session_id.clone(),
                                turn_id: turn_id.clone(),
                                reason: "goal_unmet".to_string(),
                                message: reminder,
                            });
                            continue;
                        }
                    }
                    Err(err) => {
                        let _ = self.event_tx.send(SessionEvent::ConfigWarning {
                            summary: format!("Goal evaluation failed: {err}"),
                            details: None,
                        });
                    }
                }
            }

            break;
        }

        // Persist turn usage & completion
        let now = jiff::Timestamp::now().as_second();
        {
            let active_model = self
                .router
                .route_for_mode(self.mode)
                .map(|(_, c)| c.spec)
                .unwrap_or_else(|_| self.config.settings.model.clone());
            let idx = self
                .state_index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = idx.record_usage(&self.session_id, &turn_id, &active_model, &turn_usage, now);
        }
        let _ = self.append_rollout(RolloutRecord::TurnCompleted {
            turn_id: turn_id.clone(),
            status: final_status.clone(),
            usage: turn_usage,
        });
        let active_model_for_hook = self
            .router
            .route_for_mode(self.mode)
            .map(|(_, c)| c.spec)
            .unwrap_or_else(|_| self.config.settings.model.clone());
        self.dispatch_hook_with_notes(
            &turn_id,
            HookEvent::TurnCompleted,
            HookEventPayload {
                turn_id: Some(turn_id.clone()),
                status: Some(final_status.clone()),
                model: Some(active_model_for_hook),
                ..HookEventPayload::default()
            },
        );

        self.is_turn_running.store(false, Ordering::SeqCst);
        self.sync_shared_state();

        if final_status == "completed" {
            self.spawn_auto_title_once();
        }

        let _ = self.event_tx.send(SessionEvent::TurnCompleted {
            thread_id: self.session_id.clone(),
            turn_id,
            status: final_status,
            error: final_error,
        });
    }

    /// Opt-in `auto_title`: one background `JobRole::Title` call that names the
    /// session from the first user prompt + first assistant reply and persists
    /// the title via `StateIndex::set_session_title`. Failures are silent.
    fn spawn_auto_title_once(&mut self) {
        if self.title_done || !self.config.settings.agent.auto_title {
            return;
        }
        self.title_done = true;

        let first_user = self
            .frame
            .messages()
            .iter()
            .find_map(|m| (m.role == MessageRole::User).then(|| message_text(m)));
        let Some(user_text) = first_user.filter(|text| !text.is_empty()) else {
            return;
        };
        let assistant_text = self
            .frame
            .messages()
            .iter()
            .rev()
            .find_map(|m| (m.role == MessageRole::Assistant).then(|| message_text(m)))
            .unwrap_or_default();
        let (provider, model) = match self.router.route(JobRole::Title) {
            Ok(pair) => (pair.0.clone(), pair.1.clone()),
            Err(_) => return,
        };

        let driver = Arc::clone(&self.driver);
        let state_index = Arc::clone(&self.state_index);
        let session_id = self.session_id.clone();
        let summary = format!(
            "User: {}\n\nAssistant: {}",
            user_text.chars().take(600).collect::<String>(),
            assistant_text.chars().take(400).collect::<String>(),
        );
        tokio::spawn(async move {
            let request = CompletionRequest {
                conversation_id: format!("{session_id}:title"),
                messages: vec![ProviderMessage::user_text(format!(
                    "Generate a concise 3-6 word title for this coding session. \
                     Reply with ONLY the title text, no quotes:\n\n{summary}"
                ))],
                tools: Vec::new(),
                reasoning_effort: None,
                output_schema: None,
            };
            let Ok(resp) = driver.complete(&provider, &model, &request).await else {
                return;
            };
            let title: String = message_text(&resp.assistant_message)
                .trim()
                .trim_matches('"')
                .chars()
                .take(80)
                .collect();
            if title.is_empty() {
                return;
            }
            let _ = state_index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_session_title(&session_id, &title);
        });
    }

    fn drain_guides_into_frame(&mut self, turn_id: &str) -> bool {
        let drained: Vec<PendingUserInput> = {
            let mut q = self
                .guide_inbox
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            q.drain(..).collect()
        };
        if drained.is_empty() {
            return false;
        }
        let before = self.frame.snapshot();
        for (guide_text, images) in drained {
            let mut parts: Vec<MessagePart> = Vec::new();
            if !guide_text.is_empty() || images.is_empty() {
                parts.push(MessagePart::Text {
                    text: guide_text.clone(),
                    cache_breakpoint: false,
                });
            }
            for img in images {
                parts.push(MessagePart::Image {
                    media_type: img.media_type,
                    data_base64: img.data_base64,
                    source_path: img.source_path,
                });
            }
            let msg = ProviderMessage {
                role: MessageRole::User,
                parts,
            };
            self.frame.push_message(msg.clone());
            let _ = self.append_rollout(RolloutRecord::Message {
                turn_id: turn_id.to_string(),
                message: msg,
            });
            let idx = self
                .state_index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = idx.index_message_text(&self.session_id, turn_id, "user", &guide_text);
        }
        let _ = self.frame.assert_append_only(&before);
        true
    }

    async fn dispatch_single_tool(
        &self,
        registry: &NativeToolRegistry,
        turn_id: &str,
        call_id: &str,
        tool_name: &str,
        args_json: &str,
    ) -> Result<ToolOutput, String> {
        let args: Value = serde_json::from_str(args_json)
            .map_err(|e| format!("invalid JSON arguments for `{tool_name}`: {e}"))?;

        if tool_name == "task" {
            let Some(mgr) = &self.subagents else {
                return Err("subagent `task` tool is disabled in this session".to_string());
            };
            if let Some(existing_id) = args.get("task_id").and_then(Value::as_str) {
                // Worktree merge-back verbs: `merge` mutates the parent tree, so the
                // changed paths go through the WriteWorkspace permission ladder first.
                if let Some(merge_mode) = args.get("merge").and_then(Value::as_str) {
                    let paths = self
                        .task_merge_paths(mgr, existing_id)
                        .await
                        .map_err(|e| e.to_string())?;
                    match merge_mode {
                        "merge" => {
                            let action = tekvra_tools::ToolAction::WriteWorkspace {
                                tool_name: "task".to_string(),
                                paths,
                            };
                            tekvra_tools::run_permission_ladder(
                                &self.permissions,
                                Some(&(Arc::clone(&self.bridge) as Arc<dyn ApprovalCallback>)),
                                Some(&self.state_index),
                                call_id,
                                &action,
                            )
                            .await
                            .map_err(|e| e.to_string())?;
                            return mgr.merge_task(existing_id).await.map_err(|e| e.to_string());
                        }
                        "discard" => {
                            return mgr
                                .discard_task(existing_id)
                                .await
                                .map_err(|e| e.to_string());
                        }
                        other => {
                            return Err(format!(
                                "invalid `merge` value `{other}`; expected `merge` or `discard`"
                            ));
                        }
                    }
                }
                let prompt = args
                    .get("prompt")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let delivery = match args.get("delivery").and_then(Value::as_str) {
                    Some("replace") => InputDeliveryMode::Replace,
                    _ => InputDeliveryMode::Guide,
                };
                mgr.steer(existing_id, prompt, delivery)
                    .await
                    .map_err(|e| e.to_string())?;
                return Ok(ToolOutput::SubagentCard {
                    task_id: existing_id.to_string(),
                    role: "subagent".to_string(),
                    status: "steered".to_string(),
                    summary: format!("Delivered steering update to `{existing_id}`."),
                    worktree_branch: None,
                    diff_summary: None,
                });
            }
            let prompt = args
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| "`task` requires `prompt`".to_string())?
                .to_string();
            let role = args.get("role").and_then(Value::as_str).map(str::to_string);
            let model_override = args
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
            let background = args
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let worktree = args
                .get("worktree")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            return mgr
                .spawn(SubagentSpawnRequest {
                    prompt,
                    role,
                    model_override,
                    background,
                    worktree,
                })
                .await
                .map_err(|e| e.to_string());
        }

        let ctx = ToolExecutionContext {
            session_id: self.session_id.clone(),
            turn_id: turn_id.to_string(),
            workspace_root: self.workspace_root.clone(),
            settings: self.config.settings.agent.clone(),
            path_locks: self.path_locks.clone(),
            pty_manager: self.pty_manager.clone(),
            web_fetcher: SafeWebFetcher::new(
                &self.config.settings.agent.webfetch_allow_private_hosts,
            ),
            code_sandbox: MontyCodeSandbox,
            permissions: Arc::clone(&self.permissions),
            state_index: Some(Arc::clone(&self.state_index)),
            snapshots: Some(self.snapshots.clone()),
            mcp_gateway: self.mcp_gateway.clone(),
            approval_cb: Some(Arc::clone(&self.bridge) as Arc<dyn ApprovalCallback>),
            question_cb: Some(Arc::clone(&self.bridge) as Arc<dyn QuestionCallback>),
            plan_markdown: Arc::clone(&self.plan_markdown),
        };

        match registry.execute(&ctx, call_id, tool_name, &args).await {
            Ok(out) => Ok(out),
            Err(tekvra_tools::ToolsError::UnknownTool { .. }) => {
                if let Some(plugin_exec) = &self.plugin_executor {
                    plugin_exec.execute_plugin_tool(tool_name, &args).await
                } else {
                    Err(format!("unknown tool `{tool_name}`"))
                }
            }
            Err(err) => Err(err.to_string()),
        }
    }

    /// Dispatches the `turn_started` hook with the turn id and routed model spec.
    fn dispatch_turn_started_hook(&mut self, turn_id: &str) {
        let model = self
            .router
            .route_for_mode(self.mode)
            .map(|(_, c)| c.spec)
            .unwrap_or_else(|_| self.config.settings.model.clone());
        self.dispatch_hook_with_notes(
            turn_id,
            HookEvent::TurnStarted,
            HookEventPayload {
                turn_id: Some(turn_id.to_string()),
                model: Some(model),
                ..HookEventPayload::default()
            },
        );
    }

    /// Dispatches a hook event and records notes/failures as steering notes
    /// plus `SteeringInjected` transcript events (never model-frame messages).
    fn dispatch_hook_with_notes(
        &mut self,
        turn_id: &str,
        event: HookEvent,
        payload: HookEventPayload,
    ) -> HookOutcome {
        let Some(exec) = self.plugin_executor.clone() else {
            return HookOutcome::default();
        };
        let outcome = exec.dispatch_hook(event, payload);
        for note in &outcome.notes {
            self.record_hook_note(turn_id, "plugin", note);
        }
        for failure in &outcome.failures {
            self.record_hook_note(turn_id, "plugin_error", failure);
        }
        outcome
    }

    /// Records one hook-derived note in the rollout and the live transcript.
    fn record_hook_note(&mut self, turn_id: &str, kind: &str, text: &str) {
        let _ = self.append_rollout(RolloutRecord::SteeringNote {
            turn_id: turn_id.to_string(),
            kind: kind.to_string(),
            text: text.to_string(),
        });
        let _ = self.event_tx.send(SessionEvent::SteeringInjected {
            thread_id: self.session_id.clone(),
            turn_id: turn_id.to_string(),
            reason: kind.to_string(),
            message: text.to_string(),
        });
    }

    /// Absolute parent-side paths a worktree task would change on merge-back.
    async fn task_merge_paths(
        &self,
        mgr: &SubagentManager,
        task_id: &str,
    ) -> Result<Vec<PathBuf>, String> {
        let entry = mgr
            .list()
            .into_iter()
            .find(|e| e.task_id == task_id)
            .ok_or_else(|| format!("unknown subagent `{task_id}`"))?;
        let wt = crate::worktree::TaskWorktree {
            path: entry
                .worktree_path
                .ok_or_else(|| format!("task `{task_id}` did not run in a worktree"))?,
            branch: entry.worktree_branch.clone().unwrap_or_default(),
            base_commit: entry.base_commit.clone().unwrap_or_default(),
        };
        let rel = crate::worktree::worktree_changes(&wt)
            .await
            .map_err(|e| e.to_string())?;
        Ok(rel
            .into_iter()
            .map(|p| self.workspace_root.join(p))
            .collect())
    }

    fn append_rollout(&mut self, record: RolloutRecord) -> Result<(), CoreError> {
        self.rollout_log.append(&record)?;
        self.rollout_records.push(record);
        Ok(())
    }

    fn sync_shared_state(&self) {
        let active_model = self
            .router
            .route_for_mode(self.mode)
            .map(|(_, c)| c.spec)
            .unwrap_or_else(|_| self.config.settings.model.clone());
        let context_window = self
            .router
            .route_for_mode(self.mode)
            .map(|(_, c)| c.context_window)
            .unwrap_or(0);
        let plan_md = self
            .plan_markdown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut st = self
            .shared_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.active_model = active_model;
        st.mode = self.mode;
        st.goal.clone_from(&self.active_goal);
        st.plan_markdown = plan_md;
        st.cumulative_usage = self.cumulative_usage;
        st.context_window = context_window;
        st.last_frame_snapshot = Some(self.frame.snapshot());
    }
}

/// Joins the plain `Text` parts of a provider message (used by auto-titling).
fn message_text(message: &ProviderMessage) -> String {
    message
        .parts
        .iter()
        .filter_map(|p| match p {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_developer_instructions(
    config: &ResolvedConfig,
    workspace_root: &std::path::Path,
) -> String {
    let mut out = format!(
        "You are Tekvra, a native Rust coding agent operating in workspace `{}`.\nPrefer concise, verified changes backed by concrete tool outputs.",
        workspace_root.display()
    );
    if !config.instructions.is_empty() {
        out.push_str("\n\nRepository instructions (`AGENTS.md`):\n");
        for doc in &config.instructions {
            out.push_str(doc.content.trim());
            out.push('\n');
        }
    }
    // Deterministic skill metadata: dedupe by name, sort by name so the
    // developer-instruction prefix stays byte-stable for prompt caching.
    if !config.skills.is_empty() {
        let mut skills = config.skills.clone();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills.dedup_by(|a, b| a.name == b.name);
        out.push_str("\n\n<available_skills>\n");
        for skill in &skills {
            out.push_str(&format!("- {}: {}\n", skill.name, skill.description));
        }
        out.push_str("</available_skills>");
    }
    out
}

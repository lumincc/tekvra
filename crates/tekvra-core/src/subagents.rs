//! Durable, steerable foreground and background `task` subagents (`SubagentManager`)
//! with isolated `.jsonl` rollouts, role/model routing, and push-based parent wakeup.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tekvra_config::ResolvedConfig;
use tekvra_protocol::{
    InputDeliveryMode, PermissionMode, SessionCommand, SessionEvent, ToolOutput,
};
use tekvra_providers::{JobRole, ModelRouter, ProviderDriver};
use tekvra_storage::{RolloutLog, RolloutRecord, StateIndex};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::CoreError;
use crate::session::{SessionBootstrap, SessionHandle, spawn_session};

/// Notification pushed directly into the parent `SessionActor` when a background subagent completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundSubagentNotice {
    /// Subagent identifier (`task_<uuidv7>`).
    pub task_id: String,
    /// Assigned role label.
    pub role: String,
    /// Terminal status (`completed`, `failed`, `interrupted`).
    pub status: String,
    /// Final assistant message or error summary.
    pub summary: String,
}

/// Snapshot metadata for a spawned subagent (`task`) exposed to `/tasks` (`Ctrl-X`) and `app-server`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentEntry {
    /// Stable subagent session/task id (`task_<uuidv7>`).
    pub task_id: String,
    /// Parent session id.
    pub parent_session_id: String,
    /// Role tier (`fast`, `subagent`, `main`, `review`).
    pub role: String,
    /// Resolved `provider/model` spec.
    pub model: String,
    /// Whether launched in background.
    pub background: bool,
    /// Lifecycle status (`running`, `completed`, `failed`, `interrupted`).
    pub status: String,
    /// Latest summary text.
    pub summary: String,
}

/// Arguments for spawning a foreground or background subagent.
#[derive(Debug, Clone)]
pub struct SubagentSpawnRequest {
    /// Prompt assigned to the subagent.
    pub prompt: String,
    /// Optional role alias (`"weak"`/`"fast"`, `"medium"`/`"subagent"`, `"strong"`/`"main"`, `"review"`).
    pub role: Option<String>,
    /// Optional explicit `"provider/model"` override.
    pub model_override: Option<String>,
    /// Run asynchronously in background and notify parent on completion when `true`.
    pub background: bool,
}

struct LiveSubagent {
    entry: SubagentEntry,
    handle: SessionHandle,
}

/// Manages foreground and background `task` subagent lifecycles for a parent session.
#[derive(Clone)]
pub struct SubagentManager {
    parent_session_id: String,
    workspace_root: PathBuf,
    state_dir: PathBuf,
    config: ResolvedConfig,
    router: ModelRouter,
    driver: Arc<dyn ProviderDriver>,
    state_index: Option<Arc<Mutex<StateIndex>>>,
    parent_wakeup_tx: mpsc::UnboundedSender<BackgroundSubagentNotice>,
    tasks: Arc<Mutex<BTreeMap<String, LiveSubagent>>>,
}

/// Construction dependencies for [`SubagentManager`].
pub struct SubagentManagerDeps {
    /// Parent session identifier.
    pub parent_session_id: String,
    /// Canonical workspace root.
    pub workspace_root: PathBuf,
    /// State directory for durable `.jsonl` rollouts.
    pub state_dir: PathBuf,
    /// Resolved configuration.
    pub config: ResolvedConfig,
    /// Role-based model router.
    pub router: ModelRouter,
    /// Async LLM provider driver.
    pub driver: Arc<dyn ProviderDriver>,
    /// Optional shared SQLite state index.
    pub state_index: Option<Arc<Mutex<StateIndex>>>,
    /// Push channel waking the parent session when background subagents finish.
    pub parent_wakeup_tx: mpsc::UnboundedSender<BackgroundSubagentNotice>,
}

impl SubagentManager {
    /// Creates a [`SubagentManager`] wired to `deps.parent_wakeup_tx` for zero-polling background completion notices.
    #[must_use]
    pub fn new(deps: SubagentManagerDeps) -> Self {
        Self {
            parent_session_id: deps.parent_session_id,
            workspace_root: deps.workspace_root,
            state_dir: deps.state_dir,
            config: deps.config,
            router: deps.router,
            driver: deps.driver,
            state_index: deps.state_index,
            parent_wakeup_tx: deps.parent_wakeup_tx,
            tasks: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Spawns a subagent `SessionHandle` with its own durable `.jsonl` rollout and executes `req`.
    ///
    /// # Errors
    /// Returns [`CoreError`] if model resolution or session creation fails.
    pub async fn spawn(&self, req: SubagentSpawnRequest) -> Result<ToolOutput, CoreError> {
        let role_label = req
            .role
            .as_deref()
            .unwrap_or("subagent")
            .trim()
            .to_ascii_lowercase();
        let (job_role, child_mode) = match role_label.as_str() {
            "weak" | "fast" => (JobRole::Fast, self.config.settings.agent.default_mode),
            "strong" | "main" | "best" => (JobRole::Main, self.config.settings.agent.default_mode),
            "review" | "readonly" | "read_only" => (JobRole::Review, PermissionMode::ReadOnly),
            "plan" => (JobRole::Plan, PermissionMode::Plan),
            _ => (JobRole::Subagent, self.config.settings.agent.default_mode),
        };

        let mut child_router = self.router.clone();
        let resolved_spec = if let Some(explicit) = &req.model_override {
            let (_, caps) = child_router.registry().resolve_model_spec(explicit)?;
            child_router.set_active_main_model(caps.spec.clone());
            caps.spec
        } else {
            let (_, caps) = child_router.route(job_role)?;
            child_router.set_active_main_model(caps.spec.clone());
            caps.spec
        };

        let task_id = format!("task_{}", Uuid::now_v7().simple());
        let mut child_cfg = self.config.clone();
        child_cfg.settings.model.clone_from(&resolved_spec);
        child_cfg.settings.agent.default_mode = child_mode;

        let bootstrap = SessionBootstrap {
            session_id: Some(task_id.clone()),
            parent_session_id: Some(self.parent_session_id.clone()),
            workspace_root: self.workspace_root.clone(),
            state_dir: self.state_dir.clone(),
            config: child_cfg,
            router: child_router,
            driver: Arc::clone(&self.driver),
            state_index: self.state_index.clone(),
            compaction: crate::frame::CompactionConfig::default(),
            mcp_gateway: None,
            plugin_executor: None,
            extra_plugin_tools: Vec::new(),
            allow_subagents: false, // Prevent unbounded recursive subagent storms by default
        };

        let handle = spawn_session(bootstrap)?;
        let mut event_rx = handle.subscribe();

        let entry = SubagentEntry {
            task_id: task_id.clone(),
            parent_session_id: self.parent_session_id.clone(),
            role: role_label.clone(),
            model: resolved_spec,
            background: req.background,
            status: "running".to_string(),
            summary: String::new(),
        };
        {
            let mut map = self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.insert(
                task_id.clone(),
                LiveSubagent {
                    entry,
                    handle: handle.clone(),
                },
            );
        }

        handle
            .send(SessionCommand::SubmitInput {
                prompt: req.prompt,
                delivery: InputDeliveryMode::Next,
            })
            .await?;

        if !req.background {
            let (status, summary) = collect_first_turn_completion(&mut event_rx).await;
            self.mark_finished(&task_id, &status, &summary);
            return Ok(ToolOutput::SubagentCard {
                task_id,
                role: role_label,
                status,
                summary,
            });
        }

        let tasks_map = Arc::clone(&self.tasks);
        let wakeup_tx = self.parent_wakeup_tx.clone();
        let bg_task_id = task_id.clone();
        let bg_role = role_label.clone();

        tokio::spawn(async move {
            let (status, summary) = collect_first_turn_completion(&mut event_rx).await;
            {
                let mut map = tasks_map
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(live) = map.get_mut(&bg_task_id) {
                    live.entry.status.clone_from(&status);
                    live.entry.summary.clone_from(&summary);
                }
            }
            let _ = wakeup_tx.send(BackgroundSubagentNotice {
                task_id: bg_task_id,
                role: bg_role,
                status,
                summary,
            });
        });

        Ok(ToolOutput::SubagentCard {
            task_id: task_id.clone(),
            role: role_label,
            status: "running".to_string(),
            summary: format!("Background subagent `{task_id}` started."),
        })
    }

    /// Steers a live running subagent via `Guide` or `Replace` (`Ctrl-X` / `/tasks` or parent `task` tool).
    ///
    /// # Errors
    /// Returns [`CoreError::Subagent`] if `task_id` is unknown or its actor has stopped.
    pub async fn steer(
        &self,
        task_id: &str,
        prompt: String,
        delivery: InputDeliveryMode,
    ) -> Result<(), CoreError> {
        let handle = {
            let map = self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.get(task_id)
                .map(|l| l.handle.clone())
                .ok_or_else(|| CoreError::Subagent {
                    message: format!("unknown subagent `{task_id}`"),
                })?
        };
        handle
            .send(SessionCommand::SubmitInput { prompt, delivery })
            .await
    }

    /// Lists all subagents spawned by this parent session.
    #[must_use]
    pub fn list(&self) -> Vec<SubagentEntry> {
        let map = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.values().map(|l| l.entry.clone()).collect()
    }

    /// Subscribes to live [`SessionEvent`]s of a spawned subagent.
    #[must_use]
    pub fn subscribe_task(
        &self,
        task_id: &str,
    ) -> Option<tokio::sync::broadcast::Receiver<SessionEvent>> {
        let map = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(task_id).map(|l| l.handle.subscribe())
    }

    /// Reads the durable persisted `.jsonl` rollout records of any subagent `task_id` (even after restart!).
    ///
    /// # Errors
    /// Returns [`CoreError`] if the rollout file does not exist or cannot be read.
    pub fn read_persisted_transcript(
        &self,
        task_id: &str,
    ) -> Result<Vec<RolloutRecord>, CoreError> {
        Ok(RolloutLog::read_records_readonly(&self.state_dir, task_id)?)
    }

    fn mark_finished(&self, task_id: &str, status: &str, summary: &str) {
        let mut map = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(live) = map.get_mut(task_id) {
            live.entry.status = status.to_string();
            live.entry.summary = summary.to_string();
        }
    }
}

async fn collect_first_turn_completion(
    rx: &mut tokio::sync::broadcast::Receiver<SessionEvent>,
) -> (String, String) {
    let mut latest_text = String::new();
    loop {
        match rx.recv().await {
            Ok(SessionEvent::ItemCompleted {
                text,
                tool_output: None,
                ..
            }) => {
                if !text.trim().is_empty() {
                    latest_text = text;
                }
            }
            Ok(SessionEvent::TurnCompleted { status, error, .. }) => {
                let summary = if let Some(err) = error {
                    err
                } else {
                    latest_text
                };
                return (status, summary);
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                return (
                    "failed".to_string(),
                    "subagent event channel closed".to_string(),
                );
            }
        }
    }
}

//! Stateful Codex v2 JSON-RPC 2.0 dispatcher, policy mapper, event-to-notification bridge,
//! and bidirectional server-to-client approval/question correlation.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use jiff::Timestamp;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tekvra_config::{CURRENT_CONFIG_VERSION, ResolvedConfig};
use tekvra_core::{
    CompactionConfig, PluginToolExecutor, ReviewTarget, SessionBootstrap, SessionHandle,
    run_code_review, spawn_session,
};
use tekvra_protocol::codex_v2::{
    CodexApprovalPolicy, CodexApprovalsReviewer, CodexFileUpdateChange, CodexItemExecutionStatus,
    CodexReviewTarget, CodexSandboxMode, CodexSandboxPolicy, CodexThread, CodexThreadItem,
    CodexThreadStatus, CodexTurn, CodexTurnStatus, CodexUserInput,
    CommandExecutionRequestApprovalParams, FileChangeRequestApprovalParams, InitializeParams,
    InitializeResponse, JsonRpcErrorBody, JsonRpcErrorResponse, JsonRpcId, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResponse, ToolRequestUserInputParams, ToolUserInputQuestion,
    map_codex_policy,
};
use tekvra_protocol::{
    ApprovalResolution, GrantLifetime, InputDeliveryMode, PermissionMode, RevertScope,
    SessionCommand, SessionEvent, ToolOutput,
};
use tekvra_providers::{
    CredentialStore, ModelRouter, ProviderDriver, ProviderRegistry, SecretToken,
    StoredOAuthAccount, ToolDefinition,
};
use tekvra_storage::{
    RolloutLog, SessionSummary, StateIndex, WorkspaceSnapshotStore, rollout_path,
};
use tekvra_tools::{McpGateway, PtyCommandRequest, PtySessionManager};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::fuzzy::fuzzy_file_search;

/// Construction bundle for [`AppServerState`].
pub struct AppServerStateInit {
    /// Resolved Tekvra configuration.
    pub config: ResolvedConfig,
    /// Provider catalog.
    pub provider_registry: ProviderRegistry,
    /// Model router.
    pub router: ModelRouter,
    /// Async LLM provider driver.
    pub driver: Arc<dyn ProviderDriver>,
    /// Shared SQLite state index.
    pub state_index: Arc<Mutex<StateIndex>>,
    /// Workspace file snapshot store.
    pub snapshots: WorkspaceSnapshotStore,
    /// Optional MCP gateway.
    pub mcp_gateway: Option<Arc<dyn McpGateway>>,
    /// Optional opt-in Luau plugin executor.
    pub plugin_executor: Option<Arc<dyn PluginToolExecutor>>,
    /// Optional extra plugin tool definitions.
    pub extra_plugin_tools: Vec<ToolDefinition>,
}

struct LoadedThread {
    handle: SessionHandle,
    thread: CodexThread,
    approval_policy: CodexApprovalPolicy,
    approvals_reviewer: CodexApprovalsReviewer,
    model: String,
}

/// Shared multi-thread server state across stdio, Unix socket, or WebSocket connections.
#[derive(Clone)]
pub struct AppServerState {
    config: Arc<Mutex<ResolvedConfig>>,
    provider_registry: ProviderRegistry,
    router: ModelRouter,
    driver: Arc<dyn ProviderDriver>,
    state_index: Arc<Mutex<StateIndex>>,
    snapshots: WorkspaceSnapshotStore,
    pty_manager: PtySessionManager,
    mcp_gateway: Option<Arc<dyn McpGateway>>,
    plugin_executor: Option<Arc<dyn PluginToolExecutor>>,
    extra_plugin_tools: Vec<ToolDefinition>,
    loaded: Arc<Mutex<BTreeMap<String, LoadedThread>>>,
}

impl AppServerState {
    /// Builds a new [`AppServerState`].
    #[must_use]
    pub fn new(init: AppServerStateInit) -> Self {
        Self {
            config: Arc::new(Mutex::new(init.config)),
            provider_registry: init.provider_registry,
            router: init.router,
            driver: init.driver,
            state_index: init.state_index,
            snapshots: init.snapshots,
            pty_manager: PtySessionManager::new(None),
            mcp_gateway: init.mcp_gateway,
            plugin_executor: init.plugin_executor,
            extra_plugin_tools: init.extra_plugin_tools,
            loaded: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Borrows the workspace file snapshot store.
    #[must_use]
    pub const fn snapshots(&self) -> &WorkspaceSnapshotStore {
        &self.snapshots
    }
}

type PendingClientRpcMap = Arc<Mutex<HashMap<JsonRpcId, oneshot::Sender<Value>>>>;

/// Per-connection JSON-RPC 2.0 state machine enforcing `initialize` handshake ordering
/// and correlating server-originated approval/question/elicitation requests.
pub struct JsonRpcConnection {
    state: AppServerState,
    out_tx: mpsc::UnboundedSender<String>,
    initialized: bool,
    pending_client_rpcs: PendingClientRpcMap,
}

impl JsonRpcConnection {
    /// Creates a new connection bound to `out_tx` (each item is one serialized JSON frame without trailing `\n`).
    #[must_use]
    pub fn new(state: AppServerState, out_tx: mpsc::UnboundedSender<String>) -> Self {
        Self {
            state,
            out_tx,
            initialized: false,
            pending_client_rpcs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Processes one inbound raw JSON frame (request, notification, or client response to a server request).
    pub async fn handle_raw_frame(&mut self, raw: &str) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        let parsed: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                self.send_error(JsonRpcId::Number(0), -32700, format!("Parse error: {e}"));
                return;
            }
        };

        // 1. Client response to a server-initiated request (`id` + `result` or `error`, no `method`)
        if parsed.get("method").is_none()
            && let Some(id_val) = parsed.get("id")
            && let Ok(rpc_id) = serde_json::from_value::<JsonRpcId>(id_val.clone())
        {
            let sender = self
                .pending_client_rpcs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&rpc_id);
            if let Some(tx) = sender {
                let res_payload = parsed.get("result").cloned().unwrap_or(Value::Null);
                let _ = tx.send(res_payload);
            }
            return;
        }

        // 2. Notification (`method` without `id`)
        if parsed.get("id").is_none() {
            if let Ok(notif) = serde_json::from_value::<JsonRpcNotification>(parsed)
                && notif.method == "initialized"
            {
                self.initialized = true;
            }
            return;
        }

        // 3. Client request (`id` + `method`)
        let req: JsonRpcRequest = match serde_json::from_value(parsed) {
            Ok(r) => r,
            Err(e) => {
                self.send_error(
                    JsonRpcId::Number(0),
                    -32600,
                    format!("Invalid Request: {e}"),
                );
                return;
            }
        };

        if req.method == "initialize" {
            match self.handle_initialize(req.params) {
                Ok(val) => {
                    self.initialized = true;
                    self.send_result(req.id, val);
                }
                Err((code, msg)) => self.send_error(req.id, code, msg),
            }
            return;
        }

        if !self.initialized {
            self.send_error(
                req.id,
                -32600,
                "Not initialized: call `initialize` before invoking domain methods".to_string(),
            );
            return;
        }

        let params = req.params.unwrap_or_else(|| json!({}));
        match self.dispatch_method(&req.method, params).await {
            Ok(val) => self.send_result(req.id, val),
            Err((code, msg)) => self.send_error(req.id, code, msg),
        }
    }

    /// Sends a `mcpServer/elicitation/request` server->client RPC and awaits the client's response.
    pub async fn request_mcp_elicitation(
        &self,
        server_name: &str,
        thread_id: &str,
        turn_id: Option<&str>,
    ) -> Value {
        send_server_request(
            &self.out_tx,
            &self.pending_client_rpcs,
            "mcpServer/elicitation/request",
            json!({
                "serverName": server_name,
                "threadId": thread_id,
                "turnId": turn_id,
            }),
        )
        .await
    }

    fn handle_initialize(&self, params: Option<Value>) -> Result<Value, (i64, String)> {
        let p_val = params.ok_or_else(|| (-32602, "missing initialize params".to_string()))?;
        let _init: InitializeParams =
            serde_json::from_value(p_val).map_err(|e| (-32602, e.to_string()))?;
        let cfg = self
            .state
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let resp = InitializeResponse {
            codex_home: cfg.paths.global_config_dir.to_string_lossy().into_owned(),
            platform_family: std::env::consts::FAMILY.to_string(),
            platform_os: std::env::consts::OS.to_string(),
            user_agent: format!("tekvra/{}", env!("CARGO_PKG_VERSION")),
        };
        serde_json::to_value(resp).map_err(|e| (-32603, e.to_string()))
    }

    #[allow(clippy::too_many_lines)]
    async fn dispatch_method(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Value, (i64, String)> {
        match method {
            "thread/start" => self.rpc_thread_start(params).await,
            "thread/resume" => self.rpc_thread_resume(params).await,
            "thread/fork" => self.rpc_thread_fork(params).await,
            "thread/list" => self.rpc_thread_list(params),
            "thread/loaded/list" => self.rpc_thread_loaded_list(),
            "thread/read" => self.rpc_thread_read(params),
            "thread/archive" => self.rpc_thread_archive(params, true),
            "thread/unarchive" => self.rpc_thread_archive(params, false),
            "thread/rollback" => self.rpc_thread_rollback(params).await,
            "thread/compact/start" => self.rpc_thread_compact_start(params).await,
            "thread/name/set" => self.rpc_thread_set_name(params),
            "turn/start" => self.rpc_turn_start(params).await,
            "turn/steer" => self.rpc_turn_steer(params).await,
            "turn/interrupt" => self.rpc_turn_interrupt(params).await,
            "review/start" => self.rpc_review_start(params).await,
            "command/exec" => self.rpc_command_exec(params).await,
            "command/exec/write" => self.rpc_command_exec_write(params).await,
            "command/exec/resize" => self.rpc_command_exec_resize(params),
            "command/exec/terminate" => self.rpc_command_exec_terminate(params),
            "model/list" => self.rpc_model_list(),
            "modelProvider/list" => self.rpc_provider_list(),
            "skills/list" => self.rpc_skills_list(),
            "mcpServerStatus/list" => self.rpc_mcp_status_list(),
            "config/read" => self.rpc_config_read(),
            "config/value/write" => self.rpc_config_write_single(params),
            "config/batchWrite" => self.rpc_config_write_batch(params),
            "account/read" => self.rpc_account_read(),
            "account/login/start" => self.rpc_account_login_start(params),
            "account/logout" => self.rpc_account_logout(params),
            "account/rateLimits/read" => self.rpc_account_rate_limits(),
            "fuzzyFileSearch" => self.rpc_fuzzy_file_search(params),
            other => Err((-32601, format!("Method not found: `{other}`"))),
        }
    }

    async fn rpc_thread_start(&self, params: Value) -> Result<Value, (i64, String)> {
        let sandbox = parse_opt_field::<CodexSandboxMode>(&params, "sandbox")
            .unwrap_or(CodexSandboxMode::WorkspaceWrite);
        let approval_policy =
            parse_opt_field::<CodexApprovalPolicy>(&params, "approvalPolicy").unwrap_or_default();
        let reviewer = parse_opt_field::<CodexApprovalsReviewer>(&params, "approvalsReviewer")
            .unwrap_or_default();
        let mapped = map_codex_policy(Some(sandbox), None, Some(&approval_policy), Some(reviewer));

        let cfg_snapshot = self
            .state
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map_or_else(|| cfg_snapshot.cwd.clone(), PathBuf::from);
        let ephemeral = params
            .get("ephemeral")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let requested_model = params
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);

        let handle = self
            .spawn_loaded_session(None, None, cwd.clone(), mapped.mode, requested_model)
            .await?;
        let snap = handle.state_snapshot();
        let (provider_id, raw_model) = split_model_spec(&snap.active_model);
        let now_secs = Timestamp::now().as_second();

        let codex_thread = CodexThread {
            id: handle.session_id().to_string(),
            session_id: handle.session_id().to_string(),
            project_id: project_hash(&cwd),
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            created_at: now_secs,
            updated_at: now_secs,
            cwd: cwd.to_string_lossy().into_owned(),
            ephemeral,
            model_provider: provider_id.clone(),
            preview: String::new(),
            source: "appServer".to_string(),
            status: CodexThreadStatus::Idle,
            turns: Vec::new(),
            forked_from_id: None,
            name: None,
        };

        self.attach_event_forwarder(&handle);

        {
            let mut map = self
                .state
                .loaded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.insert(
                codex_thread.id.clone(),
                LoadedThread {
                    handle,
                    thread: codex_thread.clone(),
                    approval_policy: approval_policy.clone(),
                    approvals_reviewer: reviewer,
                    model: raw_model.clone(),
                },
            );
        }

        self.emit_notification("thread/started", json!({ "thread": codex_thread }));
        if let Some(w) = mapped.warning {
            self.emit_notification("configWarning", serde_json::to_value(w).unwrap_or_default());
        }

        Ok(json!({
            "approvalPolicy": approval_policy,
            "approvalsReviewer": reviewer,
            "cwd": cwd.to_string_lossy(),
            "model": raw_model,
            "modelProvider": provider_id,
            "sandbox": sandbox_policy_from_mode(sandbox),
            "thread": codex_thread,
        }))
    }

    async fn rpc_thread_resume(&self, params: Value) -> Result<Value, (i64, String)> {
        let thread_id = req_str(&params, "threadId")?;
        let sandbox = parse_opt_field::<CodexSandboxMode>(&params, "sandbox")
            .unwrap_or(CodexSandboxMode::WorkspaceWrite);
        let approval_policy =
            parse_opt_field::<CodexApprovalPolicy>(&params, "approvalPolicy").unwrap_or_default();
        let reviewer = parse_opt_field::<CodexApprovalsReviewer>(&params, "approvalsReviewer")
            .unwrap_or_default();
        let mapped = map_codex_policy(Some(sandbox), None, Some(&approval_policy), Some(reviewer));

        if let Some(w) = mapped.warning {
            self.emit_notification("configWarning", serde_json::to_value(w).unwrap_or_default());
        }

        if let Some(existing) = self
            .state
            .loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
        {
            let (provider_id, raw_model) = split_model_spec(&existing.model);
            return Ok(json!({
                "approvalPolicy": approval_policy,
                "approvalsReviewer": reviewer,
                "cwd": existing.thread.cwd,
                "model": raw_model,
                "modelProvider": provider_id,
                "sandbox": sandbox_policy_from_mode(sandbox),
                "thread": existing.thread,
            }));
        }

        let cfg_snapshot = self
            .state
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let handle = self
            .spawn_loaded_session(
                Some(thread_id.clone()),
                None,
                cfg_snapshot.cwd.clone(),
                mapped.mode,
                None,
            )
            .await?;
        self.attach_event_forwarder(&handle);
        let snap = handle.state_snapshot();
        let (provider_id, raw_model) = split_model_spec(&snap.active_model);
        let now_secs = Timestamp::now().as_second();
        let codex_thread = CodexThread {
            id: thread_id.clone(),
            session_id: thread_id.clone(),
            project_id: project_hash(&snap.cwd),
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            created_at: now_secs,
            updated_at: now_secs,
            cwd: snap.cwd.to_string_lossy().into_owned(),
            ephemeral: false,
            model_provider: provider_id.clone(),
            preview: String::new(),
            source: "appServer".to_string(),
            status: CodexThreadStatus::Idle,
            turns: Vec::new(),
            forked_from_id: None,
            name: None,
        };
        self.state
            .loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                thread_id,
                LoadedThread {
                    handle,
                    thread: codex_thread.clone(),
                    approval_policy: approval_policy.clone(),
                    approvals_reviewer: reviewer,
                    model: raw_model.clone(),
                },
            );

        Ok(json!({
            "approvalPolicy": approval_policy,
            "approvalsReviewer": reviewer,
            "cwd": codex_thread.cwd,
            "model": raw_model,
            "modelProvider": provider_id,
            "sandbox": sandbox_policy_from_mode(sandbox),
            "thread": codex_thread,
        }))
    }

    async fn rpc_thread_fork(&self, params: Value) -> Result<Value, (i64, String)> {
        let parent_id = req_str(&params, "threadId")?;
        let _last_turn = params.get("lastTurnId").and_then(Value::as_str);
        let new_id = format!("thr_{}", Uuid::now_v7().simple());
        let state_dir = self
            .state
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .paths
            .state_dir
            .clone();

        if let Ok(records) = RolloutLog::read_records_readonly(&state_dir, &parent_id) {
            let dst = rollout_path(&state_dir, &new_id);
            if let Some(parent_dir) = dst.parent() {
                let _ = fs::create_dir_all(parent_dir);
            }
            let mut body = String::new();
            for rec in records {
                if let Ok(line) = serde_json::to_string(&rec) {
                    body.push_str(&line);
                    body.push('\n');
                }
            }
            let _ = fs::write(dst, body);
        }

        let cfg_snapshot = self
            .state
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let handle = self
            .spawn_loaded_session(
                Some(new_id.clone()),
                Some(parent_id.clone()),
                cfg_snapshot.cwd.clone(),
                PermissionMode::Build,
                None,
            )
            .await?;
        self.attach_event_forwarder(&handle);
        let snap = handle.state_snapshot();
        let (provider_id, raw_model) = split_model_spec(&snap.active_model);
        let now_secs = Timestamp::now().as_second();
        let forked_thread = CodexThread {
            id: new_id.clone(),
            session_id: new_id.clone(),
            project_id: project_hash(&snap.cwd),
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            created_at: now_secs,
            updated_at: now_secs,
            cwd: snap.cwd.to_string_lossy().into_owned(),
            ephemeral: false,
            model_provider: provider_id.clone(),
            preview: String::new(),
            source: "appServer".to_string(),
            status: CodexThreadStatus::Idle,
            turns: Vec::new(),
            forked_from_id: Some(parent_id),
            name: None,
        };

        self.state
            .loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                new_id,
                LoadedThread {
                    handle,
                    thread: forked_thread.clone(),
                    approval_policy: CodexApprovalPolicy::default(),
                    approvals_reviewer: CodexApprovalsReviewer::User,
                    model: raw_model.clone(),
                },
            );

        self.emit_notification("thread/started", json!({ "thread": forked_thread }));

        Ok(json!({
            "approvalPolicy": CodexApprovalPolicy::default(),
            "approvalsReviewer": CodexApprovalsReviewer::User,
            "cwd": forked_thread.cwd,
            "model": raw_model,
            "modelProvider": provider_id,
            "sandbox": sandbox_policy_from_mode(CodexSandboxMode::WorkspaceWrite),
            "thread": forked_thread,
        }))
    }

    fn rpc_thread_list(&self, params: Value) -> Result<Value, (i64, String)> {
        let archived = params
            .get("archived")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .min(200) as usize;
        let rows = self
            .state
            .state_index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list_sessions(archived, limit, 0)
            .map_err(|e| (-32603, e.to_string()))?;

        let data: Vec<CodexThread> = rows.into_iter().map(summary_to_codex_thread).collect();
        Ok(json!({ "data": data }))
    }

    fn rpc_thread_loaded_list(&self) -> Result<Value, (i64, String)> {
        let ids: Vec<String> = self
            .state
            .loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        Ok(json!({ "data": ids }))
    }

    fn rpc_thread_read(&self, params: Value) -> Result<Value, (i64, String)> {
        let thread_id = req_str(&params, "threadId")?;
        if let Some(loaded) = self
            .state
            .loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
        {
            return Ok(json!({ "thread": loaded.thread }));
        }
        let summary = self
            .state
            .state_index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_session(&thread_id)
            .map_err(|e| (-32603, e.to_string()))?
            .ok_or_else(|| (-32602, format!("unknown thread `{thread_id}`")))?;
        Ok(json!({ "thread": summary_to_codex_thread(summary) }))
    }

    fn rpc_thread_archive(&self, params: Value, archive: bool) -> Result<Value, (i64, String)> {
        let thread_id = req_str(&params, "threadId")?;
        let idx = self
            .state
            .state_index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        idx.set_archived(&thread_id, archive)
            .map_err(|e| (-32603, e.to_string()))?;
        if archive {
            Ok(json!({}))
        } else {
            let summary = idx
                .get_session(&thread_id)
                .map_err(|e| (-32603, e.to_string()))?
                .ok_or_else(|| (-32602, format!("unknown thread `{thread_id}`")))?;
            Ok(json!({ "thread": summary_to_codex_thread(summary) }))
        }
    }

    async fn rpc_thread_rollback(&self, params: Value) -> Result<Value, (i64, String)> {
        let thread_id = req_str(&params, "threadId")?;
        let target_turn_id = params
            .get("turnId")
            .or_else(|| params.get("targetTurnId"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let (handle, thread_val) = {
            let map = self
                .state
                .loaded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = map
                .get(&thread_id)
                .ok_or_else(|| (-32602, format!("thread `{thread_id}` not loaded")))?;
            (entry.handle.clone(), entry.thread.clone())
        };
        if !target_turn_id.is_empty() {
            handle
                .send(SessionCommand::RevertTo {
                    target_id: target_turn_id,
                    scope: RevertScope::ConversationAndFiles,
                })
                .await
                .map_err(|e| (-32603, e.to_string()))?;
        }
        Ok(json!({ "thread": thread_val }))
    }

    /// `thread/compact/start`: queues `SessionCommand::Compact` on the loaded
    /// thread; completion surfaces later via the forwarded compaction events.
    async fn rpc_thread_compact_start(&self, params: Value) -> Result<Value, (i64, String)> {
        let thread_id = req_str(&params, "threadId")?;
        let handle = self.lookup_handle(&thread_id)?;
        handle
            .send(SessionCommand::Compact)
            .await
            .map_err(|e| (-32603, e.to_string()))?;
        Ok(json!({}))
    }

    /// `account/rateLimits/read`: live multi-account quota snapshot from the
    /// active provider driver (empty `accounts` list for deterministic drivers).
    fn rpc_account_rate_limits(&self) -> Result<Value, (i64, String)> {
        let rows = self.state.driver.account_pool_snapshot().summary_rows();
        let accounts: Vec<Value> = rows
            .iter()
            .map(|(provider, name, ratio, exhausted)| {
                json!({
                    "provider": provider,
                    "account": name,
                    "remainingRatio": ratio,
                    "exhausted": exhausted,
                })
            })
            .collect();
        let worst = rows
            .iter()
            .filter(|(_, _, _, exhausted)| !exhausted)
            .map(|(_, _, ratio, _)| *ratio)
            .fold(1.0_f64, f64::min);
        let (spend_limit, spend_window) = {
            let config = self
                .state
                .config
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                config.settings.agent.spend_limit_usd,
                config.settings.agent.spend_limit_window,
            )
        };
        // Config-driven OR term: a configured spend cap that is already reached marks the
        // session spend-control as active even while accounts still have quota. The
        // account-scoped RPC has no thread identity, so the session window conservatively
        // sums the whole ledger (may only over-report, never under-report).
        let spend_spent = spend_limit.map(|_| {
            let index = self
                .state
                .state_index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match spend_window {
                tekvra_config::SpendWindow::Session => index
                    .usage_totals_since(0)
                    .map(|t| t.cost_usd)
                    .unwrap_or_default(),
                tekvra_config::SpendWindow::Day => {
                    let now = jiff::Timestamp::now().as_second();
                    index
                        .usage_totals_since(now.div_euclid(86_400) * 86_400)
                        .map(|t| t.cost_usd)
                        .unwrap_or_default()
                }
            }
        });
        let spend_reached = match (spend_limit, spend_spent) {
            (Some(limit), Some(spent)) => spent >= limit,
            _ => false,
        };
        Ok(json!({
            "rateLimits": {
                "primaryWindow": {
                    "remainingRatio": worst,
                    "usedPercent": ((1.0 - worst) * 100.0).round() as i64,
                },
                "spendControlReached": (!rows.is_empty()
                    && rows.iter().all(|(_, _, _, exhausted)| *exhausted))
                    || spend_reached,
            },
            "accounts": accounts,
        }))
    }

    fn rpc_thread_set_name(&self, params: Value) -> Result<Value, (i64, String)> {
        let thread_id = req_str(&params, "threadId")?;
        let name = req_str(&params, "name")?;
        let _ = self
            .state
            .state_index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_session_title(&thread_id, &name);
        if let Some(entry) = self
            .state
            .loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&thread_id)
        {
            entry.thread.name = Some(name);
        }
        Ok(json!({}))
    }

    async fn rpc_turn_start(&self, params: Value) -> Result<Value, (i64, String)> {
        let thread_id = req_str(&params, "threadId")?;
        let inputs: Vec<CodexUserInput> = params
            .get("input")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| (-32602, format!("invalid `input`: {e}")))?
            .ok_or_else(|| (-32602, "missing `input`".to_string()))?;

        let sandbox_policy = parse_opt_field::<CodexSandboxPolicy>(&params, "sandboxPolicy");
        let approval_policy = parse_opt_field::<CodexApprovalPolicy>(&params, "approvalPolicy");
        let reviewer = parse_opt_field::<CodexApprovalsReviewer>(&params, "approvalsReviewer");

        let handle = {
            let mut map = self
                .state
                .loaded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = map
                .get_mut(&thread_id)
                .ok_or_else(|| (-32602, format!("thread `{thread_id}` is not loaded")))?;
            if let Some(ap) = &approval_policy {
                entry.approval_policy = ap.clone();
            }
            if let Some(rv) = reviewer {
                entry.approvals_reviewer = rv;
            }
            entry.handle.clone()
        };

        if sandbox_policy.is_some() || approval_policy.is_some() || reviewer.is_some() {
            let mapped = map_codex_policy(
                None,
                sandbox_policy.as_ref(),
                approval_policy.as_ref(),
                reviewer,
            );
            handle
                .send(SessionCommand::SetMode { mode: mapped.mode })
                .await
                .map_err(|e| (-32603, e.to_string()))?;
            if let Some(w) = mapped.warning {
                self.emit_notification(
                    "configWarning",
                    serde_json::to_value(w).unwrap_or_default(),
                );
            }
        }

        if let Some(model_override) = params.get("model").and_then(Value::as_str) {
            let _ = handle
                .send(SessionCommand::SwitchModel {
                    model: model_override.to_string(),
                })
                .await;
        }

        let prompt_text = extract_prompt_text(&inputs);
        let turn_stub = CodexTurn {
            id: format!("turn_{}", Uuid::now_v7().simple()),
            items: vec![CodexThreadItem::UserMessage {
                id: format!("item_{}", Uuid::now_v7().simple()),
                content: inputs,
            }],
            status: CodexTurnStatus::InProgress,
            error: None,
        };

        handle
            .send(SessionCommand::SubmitInput {
                prompt: prompt_text,
                delivery: InputDeliveryMode::Next,
                images: Vec::new(),
            })
            .await
            .map_err(|e| (-32603, e.to_string()))?;

        Ok(json!({ "turn": turn_stub }))
    }

    async fn rpc_turn_steer(&self, params: Value) -> Result<Value, (i64, String)> {
        let thread_id = req_str(&params, "threadId")?;
        let expected_turn_id = req_str(&params, "expectedTurnId")?;
        let inputs: Vec<CodexUserInput> = params
            .get("input")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| (-32602, e.to_string()))?
            .unwrap_or_default();
        let handle = self.lookup_handle(&thread_id)?;
        handle
            .send(SessionCommand::SubmitInput {
                prompt: extract_prompt_text(&inputs),
                delivery: InputDeliveryMode::Guide,
                images: Vec::new(),
            })
            .await
            .map_err(|e| (-32603, e.to_string()))?;
        Ok(json!({ "turnId": expected_turn_id }))
    }

    async fn rpc_turn_interrupt(&self, params: Value) -> Result<Value, (i64, String)> {
        let thread_id = req_str(&params, "threadId")?;
        let handle = self.lookup_handle(&thread_id)?;
        handle
            .send(SessionCommand::InterruptTurn)
            .await
            .map_err(|e| (-32603, e.to_string()))?;
        Ok(json!({}))
    }

    async fn rpc_review_start(&self, params: Value) -> Result<Value, (i64, String)> {
        let thread_id = req_str(&params, "threadId")?;
        let target_val = params
            .get("target")
            .cloned()
            .ok_or_else(|| (-32602, "missing `target`".to_string()))?;
        let codex_target: CodexReviewTarget =
            serde_json::from_value(target_val).map_err(|e| (-32602, e.to_string()))?;

        let core_target = match codex_target {
            CodexReviewTarget::UncommittedChanges => ReviewTarget::UncommittedChanges,
            CodexReviewTarget::BaseBranch { branch } => ReviewTarget::BaseBranch { branch },
            CodexReviewTarget::Commit { sha, .. } => ReviewTarget::Commit { sha },
        };
        let (cwd, agent_settings) = {
            let g = self
                .state
                .config
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (g.cwd.clone(), g.settings.agent.clone())
        };

        let outcome = run_code_review(
            &thread_id,
            &cwd,
            &core_target,
            &self.state.router,
            self.state.driver.as_ref(),
            &agent_settings,
        )
        .await
        .map_err(|e| (-32603, e.to_string()))?;

        let turn_id = format!("turn_rev_{}", Uuid::now_v7().simple());
        let turn = CodexTurn {
            id: turn_id,
            items: vec![CodexThreadItem::AgentMessage {
                id: format!("item_rev_{}", Uuid::now_v7().simple()),
                text: outcome.summary,
            }],
            status: CodexTurnStatus::Completed,
            error: None,
        };

        Ok(json!({
            "reviewThreadId": thread_id,
            "turn": turn,
        }))
    }

    async fn rpc_command_exec(&self, params: Value) -> Result<Value, (i64, String)> {
        let cmd_str = match params.get("command") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            _ => return Err((-32602, "missing `command`".to_string())),
        };
        let cfg = self
            .state
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map_or_else(|| cfg.cwd.clone(), PathBuf::from);
        let timeout_ms = params
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .unwrap_or(10_000);
        let out = self
            .state
            .pty_manager
            .exec_command(
                PtyCommandRequest {
                    command: cmd_str,
                    cwd,
                    yield_time_ms: timeout_ms,
                    background: false,
                    rows: 24,
                    cols: 80,
                    conversation_id: "app_server_exec".to_string(),
                },
                &cfg.settings.agent,
                Some(&self.state.state_index),
            )
            .await
            .map_err(|e| (-32603, e.to_string()))?;

        if let ToolOutput::Terminal {
            exit_code,
            raw_output,
            ..
        } = out
        {
            Ok(json!({
                "exitCode": exit_code.unwrap_or(0),
                "stdout": raw_output,
                "stderr": "",
            }))
        } else {
            Ok(json!({ "exitCode": 0, "stdout": "", "stderr": "" }))
        }
    }

    async fn rpc_command_exec_write(&self, params: Value) -> Result<Value, (i64, String)> {
        let proc_id = req_str(&params, "processId")?;
        let chars = if let Some(b64) = params.get("deltaBase64").and_then(Value::as_str) {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| (-32602, format!("invalid deltaBase64: {e}")))?;
            String::from_utf8_lossy(&bytes).into_owned()
        } else {
            String::new()
        };
        self.state
            .pty_manager
            .write_stdin(&proc_id, &chars, 200, Some(&self.state.state_index))
            .await
            .map_err(|e| (-32603, e.to_string()))?;
        Ok(json!({}))
    }

    fn rpc_command_exec_resize(&self, params: Value) -> Result<Value, (i64, String)> {
        let proc_id = req_str(&params, "processId")?;
        let rows = params
            .get("size")
            .and_then(|s| s.get("rows"))
            .and_then(Value::as_u64)
            .unwrap_or(24) as u16;
        let cols = params
            .get("size")
            .and_then(|s| s.get("cols"))
            .and_then(Value::as_u64)
            .unwrap_or(80) as u16;
        self.state
            .pty_manager
            .resize(&proc_id, rows, cols)
            .map_err(|e| (-32603, e.to_string()))?;
        Ok(json!({}))
    }

    fn rpc_command_exec_terminate(&self, params: Value) -> Result<Value, (i64, String)> {
        let proc_id = req_str(&params, "processId")?;
        self.state
            .pty_manager
            .terminate(&proc_id)
            .map_err(|e| (-32603, e.to_string()))?;
        Ok(json!({}))
    }

    fn rpc_model_list(&self) -> Result<Value, (i64, String)> {
        let default_spec = self
            .state
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .settings
            .model
            .clone();
        let mut models = Vec::new();
        for prov in self.state.provider_registry.iter() {
            for model_id in &prov.featured_models {
                let spec = if model_id.contains('/') {
                    model_id.clone()
                } else {
                    format!("{}/{model_id}", prov.id)
                };
                let Ok((_, caps)) = self.state.provider_registry.resolve_model_spec(&spec) else {
                    continue;
                };
                let efforts: Vec<Value> = if caps.supported_efforts.is_empty() {
                    vec![json!({
                        "reasoningEffort": "medium",
                        "description": "Default balanced reasoning effort"
                    })]
                } else {
                    caps.supported_efforts
                        .iter()
                        .map(|e| {
                            json!({
                                "reasoningEffort": e,
                                "description": format!("{e} reasoning effort")
                            })
                        })
                        .collect()
                };
                let default_eff = caps
                    .supported_efforts
                    .iter()
                    .find(|e| e.as_str() == "medium")
                    .or_else(|| caps.supported_efforts.first())
                    .cloned()
                    .unwrap_or_else(|| "medium".to_string());
                models.push(json!({
                    "id": spec,
                    "model": model_id,
                    "displayName": format!("{} / {}", prov.display_name, model_id),
                    "description": format!("{} model via {}", model_id, prov.display_name),
                    "defaultReasoningEffort": default_eff,
                    "supportedReasoningEfforts": efforts,
                    "hidden": false,
                    "isDefault": spec == default_spec || model_id == &default_spec,
                }));
            }
        }
        Ok(json!({ "data": models }))
    }

    fn rpc_provider_list(&self) -> Result<Value, (i64, String)> {
        let data: Vec<Value> = self
            .state
            .provider_registry
            .iter()
            .map(|p| {
                json!({
                    "id": p.id,
                    "displayName": p.display_name,
                    "baseUrl": p.base_url,
                })
            })
            .collect();
        Ok(json!({ "data": data }))
    }

    fn rpc_skills_list(&self) -> Result<Value, (i64, String)> {
        let cfg = self
            .state
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let skills: Vec<Value> = cfg
            .skills
            .iter()
            .map(|s| {
                let is_repo = s.path.starts_with(&cfg.project_root);
                json!({
                    "name": s.name,
                    "description": s.description,
                    "path": s.path.to_string_lossy(),
                    "scope": if is_repo { "repo" } else { "user" },
                    "enabled": true,
                })
            })
            .collect();
        Ok(json!({
            "data": [{
                "cwd": cfg.cwd.to_string_lossy(),
                "errors": [],
                "skills": skills,
            }]
        }))
    }

    fn rpc_mcp_status_list(&self) -> Result<Value, (i64, String)> {
        let cfg = self
            .state
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let live_tools = self
            .state
            .mcp_gateway
            .as_ref()
            .map_or_else(Vec::new, |gw| gw.list_tools());

        let mut server_names: Vec<String> = cfg.settings.mcp_servers.keys().cloned().collect();
        for desc in &live_tools {
            if !server_names.contains(&desc.server) {
                server_names.push(desc.server.clone());
            }
        }

        let mut data = Vec::with_capacity(server_names.len());
        for name in server_names {
            let mut tools_map = serde_json::Map::new();
            for desc in live_tools.iter().filter(|d| d.server == name) {
                tools_map.insert(
                    desc.tool.clone(),
                    json!({
                        "name": desc.tool,
                        "description": desc.description,
                        "inputSchema": desc.input_schema,
                    }),
                );
            }
            data.push(json!({
                "name": name,
                "authStatus": "unsupported",
                "tools": Value::Object(tools_map),
                "resources": [],
                "resourceTemplates": [],
            }));
        }
        Ok(json!({ "data": data }))
    }

    fn rpc_config_read(&self) -> Result<Value, (i64, String)> {
        let cfg = self
            .state
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config_json = serde_json::to_value(&cfg.settings).unwrap_or_else(|_| json!({}));
        Ok(json!({
            "config": config_json,
            "origins": {},
        }))
    }

    fn rpc_config_write_single(&self, params: Value) -> Result<Value, (i64, String)> {
        let key_path = req_str(&params, "keyPath")?;
        let val = params.get("value").cloned().unwrap_or(Value::Null);
        self.apply_config_edits(&[(key_path, val)])
    }

    fn rpc_config_write_batch(&self, params: Value) -> Result<Value, (i64, String)> {
        let edits_arr = params
            .get("edits")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut pairs = Vec::new();
        for item in edits_arr {
            if let Some(kp) = item.get("keyPath").and_then(Value::as_str) {
                let v = item.get("value").cloned().unwrap_or(Value::Null);
                pairs.push((kp.to_string(), v));
            }
        }
        self.apply_config_edits(&pairs)
    }

    fn apply_config_edits(&self, edits: &[(String, Value)]) -> Result<Value, (i64, String)> {
        let mut cfg = self
            .state
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let file_path = cfg.paths.global_config_dir.join("tekvra.toml");
        let existing = fs::read_to_string(&file_path)
            .unwrap_or_else(|_| format!("version = {CURRENT_CONFIG_VERSION}\n"));
        let mut doc = existing
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| (-32603, e.to_string()))?;

        for (key_path, json_val) in edits {
            if let Some(s) = json_val.as_str() {
                doc[key_path.as_str()] = toml_edit::value(s);
                if key_path == "model" {
                    cfg.settings.model = s.to_string();
                }
            } else if let Some(b) = json_val.as_bool() {
                doc[key_path.as_str()] = toml_edit::value(b);
            } else if let Some(i) = json_val.as_i64() {
                doc[key_path.as_str()] = toml_edit::value(i);
            }
        }

        if let Some(parent) = file_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(&file_path, doc.to_string()).map_err(|e| (-32603, e.to_string()))?;

        Ok(json!({
            "filePath": file_path.to_string_lossy(),
            "status": "ok",
            "version": CURRENT_CONFIG_VERSION.to_string(),
        }))
    }

    fn rpc_account_read(&self) -> Result<Value, (i64, String)> {
        let state_dir = self
            .state
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .paths
            .state_dir
            .clone();
        let store = CredentialStore::load(&state_dir).map_err(|e| (-32603, e.to_string()))?;
        let openai_accts = store.list_for_provider("openai");
        let has_env = std::env::var("OPENAI_API_KEY")
            .ok()
            .is_some_and(|s| !s.trim().is_empty());
        let account_json = if let Some(first) = openai_accts.first() {
            json!({
                "type": "chatgpt",
                "email": format!("{}@openai.local", first.account_name),
                "planType": first.plan_type.clone().unwrap_or_else(|| "plus".to_string()),
            })
        } else if has_env {
            json!({ "type": "apiKey" })
        } else {
            Value::Null
        };
        Ok(json!({
            "requiresOpenaiAuth": openai_accts.is_empty() && !has_env,
            "account": account_json,
        }))
    }

    fn rpc_account_login_start(&self, params: Value) -> Result<Value, (i64, String)> {
        let state_dir = self
            .state
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .paths
            .state_dir
            .clone();
        if let Some(api_key) = params.get("apiKey").and_then(Value::as_str) {
            let mut store =
                CredentialStore::load(&state_dir).map_err(|e| (-32603, e.to_string()))?;
            store
                .upsert(StoredOAuthAccount {
                    provider: "openai".to_string(),
                    account_name: "default".to_string(),
                    account_id: None,
                    plan_type: None,
                    access_token: SecretToken::new(api_key),
                    refresh_token: None,
                    expires_at_epoch_secs: None,
                    remaining_quota_ratio: 1.0,
                })
                .map_err(|e| (-32603, e.to_string()))?;
            return Ok(json!({ "type": "apiKey" }));
        }
        Ok(json!({
            "type": "chatgpt",
            "loginId": Uuid::now_v7().to_string(),
            "authUrl": "https://auth.openai.com/authorize",
        }))
    }

    fn rpc_account_logout(&self, params: Value) -> Result<Value, (i64, String)> {
        let state_dir = self
            .state
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .paths
            .state_dir
            .clone();
        let provider = params
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("openai");
        let account_name = params
            .get("account")
            .and_then(Value::as_str)
            .unwrap_or("default");
        let mut store = CredentialStore::load(&state_dir).map_err(|e| (-32603, e.to_string()))?;
        let _ = store.remove(provider, account_name);
        Ok(json!({}))
    }

    fn rpc_fuzzy_file_search(&self, params: Value) -> Result<Value, (i64, String)> {
        let query = req_str(&params, "query")?;
        let roots: Vec<String> = params
            .get("roots")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| (-32602, e.to_string()))?
            .unwrap_or_default();
        let out = fuzzy_file_search(&query, &roots, 100);
        serde_json::to_value(out).map_err(|e| (-32603, e.to_string()))
    }

    fn lookup_handle(&self, thread_id: &str) -> Result<SessionHandle, (i64, String)> {
        self.state
            .loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(thread_id)
            .map(|e| e.handle.clone())
            .ok_or_else(|| (-32602, format!("thread `{thread_id}` is not loaded")))
    }

    async fn spawn_loaded_session(
        &self,
        session_id: Option<String>,
        parent_session_id: Option<String>,
        workspace_root: PathBuf,
        initial_mode: PermissionMode,
        model_override: Option<String>,
    ) -> Result<SessionHandle, (i64, String)> {
        let mut cfg = self
            .state
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        cfg.settings.agent.default_mode = initial_mode;
        if let Some(m) = model_override {
            cfg.settings.model = m;
        }
        let state_dir = cfg.paths.state_dir.clone();
        spawn_session(SessionBootstrap {
            session_id,
            parent_session_id,
            workspace_root,
            state_dir,
            config: cfg,
            router: self.state.router.clone(),
            driver: Arc::clone(&self.state.driver),
            state_index: Some(Arc::clone(&self.state.state_index)),
            compaction: CompactionConfig::default(),
            mcp_gateway: self.state.mcp_gateway.clone(),
            plugin_executor: self.state.plugin_executor.clone(),
            extra_plugin_tools: self.state.extra_plugin_tools.clone(),
            allow_subagents: true,
            subagent_depth: 0,
        })
        .map_err(|e| (-32603, e.to_string()))
    }

    #[allow(clippy::too_many_lines)]
    fn attach_event_forwarder(&self, handle: &SessionHandle) {
        let mut rx = handle.subscribe();
        let out_tx = self.out_tx.clone();
        let pending_rpcs = Arc::clone(&self.pending_client_rpcs);
        let handle_for_replies = handle.clone();
        let forwarder_config = Arc::clone(&self.state.config);
        let forwarder_state_index = Arc::clone(&self.state.state_index);

        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                let now_ms = Timestamp::now().as_millisecond();
                match ev {
                    SessionEvent::TurnStarted { thread_id, turn_id } => {
                        emit_notif_raw(
                            &out_tx,
                            "turn/started",
                            json!({
                                "threadId": thread_id,
                                "turn": {
                                    "id": turn_id,
                                    "items": [],
                                    "status": "inProgress",
                                }
                            }),
                        );
                    }
                    SessionEvent::ItemStarted {
                        thread_id,
                        turn_id,
                        item_id,
                        kind,
                    } => {
                        let item = stub_thread_item(&item_id, &kind);
                        emit_notif_raw(
                            &out_tx,
                            "item/started",
                            json!({
                                "threadId": thread_id,
                                "turnId": turn_id,
                                "item": item,
                                "startedAtMs": now_ms,
                            }),
                        );
                    }
                    SessionEvent::ItemDelta {
                        thread_id,
                        turn_id,
                        item_id,
                        channel,
                        delta,
                    } => match channel.as_str() {
                        "reasoning_summary" => emit_notif_raw(
                            &out_tx,
                            "item/reasoning/summaryTextDelta",
                            json!({
                                "threadId": thread_id,
                                "turnId": turn_id,
                                "itemId": item_id,
                                "summaryIndex": 0,
                                "delta": delta,
                            }),
                        ),
                        "terminal_output" => emit_notif_raw(
                            &out_tx,
                            "item/commandExecution/outputDelta",
                            json!({
                                "threadId": thread_id,
                                "turnId": turn_id,
                                "itemId": item_id,
                                "delta": delta,
                            }),
                        ),
                        _ => emit_notif_raw(
                            &out_tx,
                            "item/agentMessage/delta",
                            json!({
                                "threadId": thread_id,
                                "turnId": turn_id,
                                "itemId": item_id,
                                "delta": delta,
                            }),
                        ),
                    },
                    SessionEvent::ItemCompleted {
                        thread_id,
                        turn_id,
                        item_id,
                        text,
                        tool_output,
                    } => {
                        let item = completed_thread_item(&item_id, &text, tool_output.as_ref());
                        emit_notif_raw(
                            &out_tx,
                            "item/completed",
                            json!({
                                "threadId": thread_id,
                                "turnId": turn_id,
                                "item": item,
                                "completedAtMs": now_ms,
                            }),
                        );
                    }
                    SessionEvent::DiffUpdated {
                        thread_id,
                        turn_id,
                        files,
                    } => {
                        let joined = files
                            .iter()
                            .map(|f| f.unified_diff.as_str())
                            .collect::<Vec<_>>()
                            .join("\n");
                        emit_notif_raw(
                            &out_tx,
                            "turn/diff/updated",
                            json!({
                                "threadId": thread_id,
                                "turnId": turn_id,
                                "diff": joined,
                            }),
                        );
                    }
                    SessionEvent::PlanUpdated {
                        thread_id,
                        turn_id,
                        markdown,
                    } => {
                        let steps: Vec<Value> = markdown
                            .lines()
                            .map(str::trim)
                            .filter(|l| !l.is_empty() && !l.starts_with('#'))
                            .map(|l| {
                                json!({
                                    "step": l.trim_start_matches("- ").trim_start_matches("* "),
                                    "status": "inProgress",
                                })
                            })
                            .collect();
                        emit_notif_raw(
                            &out_tx,
                            "turn/plan/updated",
                            json!({
                                "threadId": thread_id,
                                "turnId": turn_id,
                                "explanation": markdown,
                                "plan": steps,
                            }),
                        );
                    }
                    SessionEvent::TokenUsageUpdated {
                        thread_id,
                        turn_id,
                        usage,
                    } => {
                        let total_toks = usage.input_tokens + usage.output_tokens;
                        let breakdown = json!({
                            "inputTokens": usage.input_tokens,
                            "cachedInputTokens": usage.cached_input_tokens,
                            "outputTokens": usage.output_tokens,
                            "reasoningOutputTokens": usage.reasoning_tokens,
                            "totalTokens": total_toks,
                        });
                        // Live remaining-budget surface for a configured spend cap.
                        let (spend_limit, spend_window) = {
                            let config = forwarder_config
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            (
                                config.settings.agent.spend_limit_usd,
                                config.settings.agent.spend_limit_window,
                            )
                        };
                        let remaining_budget = spend_limit.map(|limit| {
                            let spent = {
                                let index = forwarder_state_index
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                match spend_window {
                                    tekvra_config::SpendWindow::Session => index
                                        .session_usage_totals(&thread_id)
                                        .map(|t| t.cost_usd)
                                        .unwrap_or_default(),
                                    tekvra_config::SpendWindow::Day => {
                                        let now = jiff::Timestamp::now().as_second();
                                        index
                                            .usage_totals_since(now.div_euclid(86_400) * 86_400)
                                            .map(|t| t.cost_usd)
                                            .unwrap_or_default()
                                    }
                                }
                            };
                            json!({
                                "limitUsd": limit,
                                "spentUsd": spent,
                                "remainingUsd": (limit - spent).max(0.0),
                            })
                        });
                        emit_notif_raw(
                            &out_tx,
                            "thread/tokenUsage/updated",
                            json!({
                                "threadId": thread_id,
                                "turnId": turn_id,
                                "tokenUsage": {
                                    "last": breakdown,
                                    "total": breakdown,
                                    "remainingBudget": remaining_budget,
                                }
                            }),
                        );
                    }
                    SessionEvent::TurnRefused { thread_id, reason } => {
                        emit_notif_raw(
                            &out_tx,
                            "turn/refused",
                            json!({
                                "threadId": thread_id,
                                "reason": reason,
                            }),
                        );
                    }
                    SessionEvent::ApprovalRequested {
                        thread_id,
                        turn_id,
                        prompt,
                    } => {
                        let tx_clone = out_tx.clone();
                        let map_clone = Arc::clone(&pending_rpcs);
                        let h_clone = handle_for_replies.clone();
                        tokio::spawn(async move {
                            let is_file = matches!(
                                prompt.tool_name.as_str(),
                                "apply_patch" | "file_write" | "file_edit"
                            );
                            let (rpc_method, req_params) = if is_file {
                                let p = FileChangeRequestApprovalParams {
                                    thread_id,
                                    turn_id,
                                    item_id: prompt.item_id.clone(),
                                    started_at_ms: now_ms,
                                    reason: Some(prompt.summary.clone()),
                                    grant_root: prompt
                                        .cwd
                                        .as_ref()
                                        .map(|c| c.to_string_lossy().into_owned()),
                                };
                                (
                                    "item/fileChange/requestApproval",
                                    serde_json::to_value(p).unwrap_or_default(),
                                )
                            } else {
                                let p = CommandExecutionRequestApprovalParams {
                                    thread_id,
                                    turn_id,
                                    item_id: prompt.item_id.clone(),
                                    started_at_ms: now_ms,
                                    approval_id: Some(prompt.approval_id.clone()),
                                    command: Some(prompt.summary.clone()),
                                    cwd: prompt
                                        .cwd
                                        .as_ref()
                                        .map(|c| c.to_string_lossy().into_owned()),
                                    reason: Some(format!(
                                        "Tool `{}` requires approval",
                                        prompt.tool_name
                                    )),
                                };
                                (
                                    "item/commandExecution/requestApproval",
                                    serde_json::to_value(p).unwrap_or_default(),
                                )
                            };

                            let client_reply =
                                send_server_request(&tx_clone, &map_clone, rpc_method, req_params)
                                    .await;
                            let decision_str = client_reply
                                .get("decision")
                                .and_then(Value::as_str)
                                .unwrap_or("decline");
                            let resolution = match decision_str {
                                "accept" => ApprovalResolution::Approve {
                                    scope: prompt.ladder.first().map(|r| r.scope.clone()),
                                    lifetime: GrantLifetime::Once,
                                    confirmed_broad: false,
                                },
                                "acceptForSession" => ApprovalResolution::Approve {
                                    scope: prompt.ladder.first().map(|r| r.scope.clone()),
                                    lifetime: GrantLifetime::Conversation,
                                    confirmed_broad: true,
                                },
                                _ => ApprovalResolution::Deny {
                                    reason: Some(format!("Client decision: {decision_str}")),
                                },
                            };
                            let _ = h_clone
                                .send(SessionCommand::ResolveApproval {
                                    approval_id: prompt.approval_id,
                                    resolution,
                                })
                                .await;
                        });
                    }
                    SessionEvent::UserInputRequested {
                        thread_id,
                        turn_id,
                        item_id,
                        questions,
                    } => {
                        let tx_clone = out_tx.clone();
                        let map_clone = Arc::clone(&pending_rpcs);
                        let h_clone = handle_for_replies.clone();
                        tokio::spawn(async move {
                            let req_payload = ToolRequestUserInputParams {
                                thread_id,
                                turn_id,
                                item_id: item_id.clone(),
                                is_blocking: true,
                                questions: questions
                                    .into_iter()
                                    .map(|(qid, qtext)| ToolUserInputQuestion {
                                        id: qid.clone(),
                                        header: qid,
                                        question: qtext,
                                    })
                                    .collect(),
                            };
                            let reply = send_server_request(
                                &tx_clone,
                                &map_clone,
                                "item/tool/requestUserInput",
                                serde_json::to_value(req_payload).unwrap_or_default(),
                            )
                            .await;
                            let mut parsed_answers: BTreeMap<String, Vec<String>> = BTreeMap::new();
                            if let Some(ans_obj) = reply.get("answers").and_then(Value::as_object) {
                                for (qid, val) in ans_obj {
                                    if let Some(arr) = val
                                        .get("answers")
                                        .and_then(Value::as_array)
                                        .or_else(|| val.as_array())
                                    {
                                        parsed_answers.insert(
                                            qid.clone(),
                                            arr.iter()
                                                .filter_map(Value::as_str)
                                                .map(str::to_string)
                                                .collect(),
                                        );
                                    }
                                }
                            }
                            let _ = h_clone
                                .send(SessionCommand::ResolveUserInput {
                                    item_id,
                                    answers: parsed_answers,
                                })
                                .await;
                        });
                    }
                    SessionEvent::ConfigWarning { summary, details } => {
                        emit_notif_raw(
                            &out_tx,
                            "configWarning",
                            json!({
                                "summary": summary,
                                "details": details,
                            }),
                        );
                    }
                    SessionEvent::TurnCompleted {
                        thread_id,
                        turn_id,
                        status,
                        error,
                    } => {
                        emit_notif_raw(
                            &out_tx,
                            "turn/completed",
                            json!({
                                "threadId": thread_id,
                                "turn": {
                                    "id": turn_id,
                                    "items": [],
                                    "status": status,
                                    "error": error.map(|m| json!({ "message": m })),
                                }
                            }),
                        );
                    }
                    _ => {}
                }
            }
        });
    }

    fn emit_notification(&self, method: &str, params: Value) {
        emit_notif_raw(&self.out_tx, method, params);
    }

    fn send_result(&self, id: JsonRpcId, result: Value) {
        let resp = JsonRpcResponse {
            jsonrpc: Some("2.0".to_string()),
            id,
            result,
        };
        if let Ok(line) = serde_json::to_string(&resp) {
            let _ = self.out_tx.send(line);
        }
    }

    fn send_error(&self, id: JsonRpcId, code: i64, message: String) {
        let resp = JsonRpcErrorResponse {
            jsonrpc: Some("2.0".to_string()),
            id,
            error: JsonRpcErrorBody {
                code,
                message,
                data: None,
            },
        };
        if let Ok(line) = serde_json::to_string(&resp) {
            let _ = self.out_tx.send(line);
        }
    }
}

async fn send_server_request(
    out_tx: &mpsc::UnboundedSender<String>,
    pending_map: &PendingClientRpcMap,
    method: &str,
    params: Value,
) -> Value {
    let req_id = JsonRpcId::String(format!("srv_req_{}", Uuid::now_v7().simple()));
    let (tx, rx) = oneshot::channel();
    pending_map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(req_id.clone(), tx);

    let frame = JsonRpcRequest {
        jsonrpc: Some("2.0".to_string()),
        id: req_id,
        method: method.to_string(),
        params: Some(params),
    };
    if let Ok(line) = serde_json::to_string(&frame) {
        let _ = out_tx.send(line);
    }
    rx.await.unwrap_or(Value::Null)
}

fn emit_notif_raw(out_tx: &mpsc::UnboundedSender<String>, method: &str, params: Value) {
    let frame = JsonRpcNotification {
        jsonrpc: Some("2.0".to_string()),
        method: method.to_string(),
        params: Some(params),
    };
    if let Ok(line) = serde_json::to_string(&frame) {
        let _ = out_tx.send(line);
    }
}

fn stub_thread_item(item_id: &str, kind: &str) -> CodexThreadItem {
    match kind {
        "reasoning" => CodexThreadItem::Reasoning {
            id: item_id.to_string(),
            summary: Vec::new(),
            content: Vec::new(),
        },
        "plan" => CodexThreadItem::Plan {
            id: item_id.to_string(),
            text: String::new(),
        },
        "tool_call" => CodexThreadItem::CommandExecution {
            id: item_id.to_string(),
            command: String::new(),
            cwd: ".".to_string(),
            command_actions: Vec::new(),
            status: CodexItemExecutionStatus::InProgress,
            aggregated_output: None,
            exit_code: None,
        },
        _ => CodexThreadItem::AgentMessage {
            id: item_id.to_string(),
            text: String::new(),
        },
    }
}

fn completed_thread_item(
    item_id: &str,
    text: &str,
    tool_output: Option<&ToolOutput>,
) -> CodexThreadItem {
    match tool_output {
        Some(ToolOutput::Terminal {
            command,
            exit_code,
            filtered_output,
            ..
        }) => CodexThreadItem::CommandExecution {
            id: item_id.to_string(),
            command: command.clone(),
            cwd: ".".to_string(),
            command_actions: Vec::new(),
            status: if exit_code.unwrap_or(0) == 0 {
                CodexItemExecutionStatus::Completed
            } else {
                CodexItemExecutionStatus::Failed
            },
            aggregated_output: Some(filtered_output.clone()),
            exit_code: *exit_code,
        },
        Some(ToolOutput::Diff { files, .. }) => CodexThreadItem::FileChange {
            id: item_id.to_string(),
            changes: files
                .iter()
                .map(|f| CodexFileUpdateChange {
                    path: f.path.to_string_lossy().into_owned(),
                    diff: f.unified_diff.clone(),
                    kind: json!({ "type": format!("{:?}", f.change_kind).to_ascii_lowercase() }),
                })
                .collect(),
            status: CodexItemExecutionStatus::Completed,
        },
        Some(ToolOutput::Plan { markdown, .. }) => CodexThreadItem::Plan {
            id: item_id.to_string(),
            text: markdown.clone(),
        },
        _ => CodexThreadItem::AgentMessage {
            id: item_id.to_string(),
            text: text.to_string(),
        },
    }
}

fn req_str(params: &Value, key: &str) -> Result<String, (i64, String)> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| (-32602, format!("missing string parameter `{key}`")))
}

fn parse_opt_field<T: serde::de::DeserializeOwned>(params: &Value, key: &str) -> Option<T> {
    params
        .get(key)
        .filter(|v| !v.is_null())
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

fn split_model_spec(spec: &str) -> (String, String) {
    match spec.split_once('/') {
        Some((p, m)) => (p.to_string(), m.to_string()),
        None => ("openai".to_string(), spec.to_string()),
    }
}

fn project_hash(cwd: &std::path::Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cwd.to_string_lossy().as_bytes());
    let bytes = hasher.finalize();
    format!(
        "proj_{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    )
}

fn sandbox_policy_from_mode(mode: CodexSandboxMode) -> CodexSandboxPolicy {
    match mode {
        CodexSandboxMode::ReadOnly => CodexSandboxPolicy::ReadOnly {
            network_access: false,
        },
        CodexSandboxMode::WorkspaceWrite => CodexSandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: true,
        },
        CodexSandboxMode::DangerFullAccess => CodexSandboxPolicy::DangerFullAccess,
    }
}

fn extract_prompt_text(inputs: &[CodexUserInput]) -> String {
    let mut parts = Vec::new();
    for item in inputs {
        match item {
            CodexUserInput::Text { text, .. } => parts.push(text.clone()),
            CodexUserInput::LocalImage { path } => parts.push(format!("[image: {path}]")),
        }
    }
    parts.join("\n")
}

fn summary_to_codex_thread(s: SessionSummary) -> CodexThread {
    let (provider, _) = split_model_spec(&s.model);
    CodexThread {
        id: s.id.clone(),
        session_id: s.id,
        project_id: s.project_id,
        cli_version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: s.created_at,
        updated_at: s.updated_at,
        cwd: s.cwd.to_string_lossy().into_owned(),
        ephemeral: false,
        model_provider: provider,
        preview: s.preview,
        source: "appServer".to_string(),
        status: CodexThreadStatus::NotLoaded,
        turns: Vec::new(),
        forked_from_id: s.parent_id,
        name: s.title,
    }
}

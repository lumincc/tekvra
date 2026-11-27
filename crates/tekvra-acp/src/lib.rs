//! Agent Client Protocol (ACP) JSON-RPC 2.0 server over stdio for editor integrations (Zed, Neovim, JetBrains).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};
use tekvra_config::ResolvedConfig;
use tekvra_core::{
    CompactionConfig, PluginToolExecutor, SessionBootstrap, SessionHandle, spawn_session,
};
use tekvra_protocol::codex_v2::{JsonRpcId, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use tekvra_protocol::{
    ApprovalResolution, GrantLifetime, InputDeliveryMode, PermissionMode, SessionCommand,
    SessionEvent,
};
use tekvra_protocol::{FileDiffEntry, ToolOutput};
use tekvra_providers::{ModelRouter, ProviderDriver, ToolDefinition};
use tekvra_storage::StateIndex;
use tekvra_tools::McpGateway;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Pending server->client `session/request_permission` requests keyed by serialized JSON-RPC id.
type PendingPermissionsMap = Arc<Mutex<BTreeMap<String, (String, oneshot::Sender<Value>)>>>;

/// Errors returned by `tekvra-acp`.
#[derive(Debug, Error)]
pub enum AcpError {
    /// Underlying stream I/O failure.
    #[error("ACP stdio error: {0}")]
    Io(#[from] std::io::Error),
    /// Core session error.
    #[error("ACP session error: {0}")]
    Core(#[from] tekvra_core::CoreError),
}

/// Initialization dependencies for [`serve_acp_stream`] / [`serve_acp_stdio`].
#[derive(Clone)]
pub struct AcpServerDeps {
    /// Resolved Tekvra config.
    pub config: ResolvedConfig,
    /// Role-based model router.
    pub router: ModelRouter,
    /// Async LLM provider driver.
    pub driver: Arc<dyn ProviderDriver>,
    /// Shared SQLite state index.
    pub state_index: Arc<Mutex<StateIndex>>,
    /// Optional MCP gateway.
    pub mcp_gateway: Option<Arc<dyn McpGateway>>,
    /// Optional opt-in Luau plugin executor.
    pub plugin_executor: Option<Arc<dyn PluginToolExecutor>>,
    /// Extra plugin tools.
    pub extra_plugin_tools: Vec<ToolDefinition>,
}

/// Serves ACP JSON-RPC 2.0 on process stdin/stdout.
///
/// # Errors
/// Returns [`AcpError`] on stdio I/O failure.
pub async fn serve_acp_stdio(deps: AcpServerDeps) -> Result<(), AcpError> {
    serve_acp_stream(tokio::io::stdin(), tokio::io::stdout(), deps).await
}

/// Serves ACP JSON-RPC 2.0 over any async `reader` + `writer` stream (`initialize`, `session/new`,
/// `session/load`, `session/prompt`, `session/cancel`, `session/set_mode`, and `session/update` notifications).
///
/// # Errors
/// Returns [`AcpError`] on stream read/write failure.
#[allow(clippy::too_many_lines)]
pub async fn serve_acp_stream<R, W>(
    reader: R,
    mut writer: W,
    deps: AcpServerDeps,
) -> Result<(), AcpError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if writer.write_all(frame.as_bytes()).await.is_err() {
                break;
            }
            if writer.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = writer.flush().await;
        }
    });

    let sessions: Arc<Mutex<BTreeMap<String, SessionHandle>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    // Server -> client `session/request_permission` requests awaiting the editor's response.
    // Keyed by the serialized JSON-RPC id; each entry carries the owning session id so
    // `session/cancel` can fail-closed everything pending for that session.
    let pending_permissions: PendingPermissionsMap = Arc::new(Mutex::new(BTreeMap::new()));
    let next_outbound_rpc_id = Arc::new(AtomicI64::new(100_000));
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // A client *response* (has `id`, `result`/`error`, no `method`) resolves a pending
        // bidirectional request instead of being treated as a new call.
        if let Ok(v) = serde_json::from_str::<Value>(trimmed)
            && v.get("id").is_some()
            && v.get("method").is_none()
            && (v.get("result").is_some() || v.get("error").is_some())
        {
            let key =
                serde_json::to_string(v.get("id").unwrap_or(&Value::Null)).unwrap_or_default();
            if let Some((_sid, resp_tx)) = pending_permissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&key)
            {
                let _ = resp_tx.send(v.get("result").cloned().unwrap_or(Value::Null));
            }
            continue;
        }
        let Ok(req) = serde_json::from_str::<JsonRpcRequest>(trimmed) else {
            continue;
        };
        let params = req.params.unwrap_or_else(|| json!({}));

        match req.method.as_str() {
            "initialize" => {
                send_acp_ok(
                    &out_tx,
                    req.id,
                    json!({
                        "protocolVersion": 1,
                        "agentInfo": {
                            "name": "tekvra",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "agentCapabilities": {
                            "loadSession": true,
                            "promptCapabilities": { "embeddedContext": true }
                        }
                    }),
                );
            }
            "session/new" | "session/load" => {
                let requested_id = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map_or_else(|| deps.config.cwd.clone(), PathBuf::from);
                let handle = spawn_session(SessionBootstrap {
                    session_id: requested_id,
                    parent_session_id: None,
                    workspace_root: cwd,
                    state_dir: deps.config.paths.state_dir.clone(),
                    config: deps.config.clone(),
                    router: deps.router.clone(),
                    driver: Arc::clone(&deps.driver),
                    state_index: Some(Arc::clone(&deps.state_index)),
                    compaction: CompactionConfig::default(),
                    mcp_gateway: deps.mcp_gateway.clone(),
                    plugin_executor: deps.plugin_executor.clone(),
                    extra_plugin_tools: deps.extra_plugin_tools.clone(),
                    allow_subagents: true,
                })?;
                let sid = handle.session_id().to_string();
                sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(sid.clone(), handle);
                send_acp_ok(&out_tx, req.id, json!({ "sessionId": sid }));
            }
            "session/set_mode" => {
                if let Some(sid) = params.get("sessionId").and_then(Value::as_str)
                    && let Some(mode_str) = params.get("modeId").and_then(Value::as_str)
                {
                    let handle_opt = sessions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(sid)
                        .cloned();
                    if let Some(h) = handle_opt {
                        let mode = match mode_str {
                            "plan" => PermissionMode::Plan,
                            "auto" => PermissionMode::Auto,
                            "yolo" => PermissionMode::Yolo,
                            _ => PermissionMode::Build,
                        };
                        let _ = h.send(SessionCommand::SetMode { mode }).await;
                    }
                }
                send_acp_ok(&out_tx, req.id, json!({}));
            }
            "session/cancel" => {
                if let Some(sid) = params.get("sessionId").and_then(Value::as_str) {
                    let handle_opt = sessions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(sid)
                        .cloned();
                    if let Some(h) = handle_opt {
                        let _ = h.send(SessionCommand::InterruptTurn).await;
                    }
                    // Fail closed every pending permission request for this session.
                    let mut pending = pending_permissions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let stale: Vec<String> = pending
                        .iter()
                        .filter(|(_, (owner, _))| owner == sid)
                        .map(|(k, _)| k.clone())
                        .collect();
                    for key in stale {
                        if let Some((_, resp_tx)) = pending.remove(&key) {
                            drop(resp_tx); // receiver observes Err -> Deny
                        }
                    }
                }
                send_acp_ok(&out_tx, req.id, json!({}));
            }
            "session/prompt" => {
                let sid = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let prompt_text = extract_acp_prompt(&params);
                let handle_opt = sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&sid)
                    .cloned();
                if let Some(h) = handle_opt {
                    let tx_clone = out_tx.clone();
                    let req_id = req.id;
                    let pending_permissions = Arc::clone(&pending_permissions);
                    let next_outbound_rpc_id = Arc::clone(&next_outbound_rpc_id);
                    tokio::spawn(async move {
                        let mut rx = h.subscribe();
                        let _ = h
                            .send(SessionCommand::SubmitInput {
                                prompt: prompt_text,
                                delivery: InputDeliveryMode::Next,
                                images: Vec::new(),
                            })
                            .await;
                        let mut stop_reason = "end_turn";
                        while let Ok(ev) = rx.recv().await {
                            match ev {
                                SessionEvent::ItemDelta { channel, delta, .. } => {
                                    let kind = if channel == "reasoning_summary" {
                                        "agent_thought_chunk"
                                    } else {
                                        "agent_message_chunk"
                                    };
                                    emit_acp_update(
                                        &tx_clone,
                                        &sid,
                                        json!({
                                            "sessionUpdate": kind,
                                            "content": { "type": "text", "text": delta }
                                        }),
                                    );
                                }
                                SessionEvent::ItemCompleted {
                                    item_id,
                                    text,
                                    tool_output,
                                    ..
                                } => match tool_output {
                                    Some(ToolOutput::Diff { files, .. }) => {
                                        for FileDiffEntry {
                                            path,
                                            change_kind,
                                            old_text,
                                            new_text,
                                            ..
                                        } in files
                                        {
                                            emit_acp_update(
                                                &tx_clone,
                                                &sid,
                                                json!({
                                                    "sessionUpdate": "tool_call_update",
                                                    "toolCallId": item_id,
                                                    "status": "completed",
                                                    "kind": match change_kind {
                                                        tekvra_protocol::FileChangeKind::Delete => "delete",
                                                        _ => "edit",
                                                    },
                                                    "content": [{
                                                        "type": "diff",
                                                        "path": path.display().to_string(),
                                                        "oldText": old_text,
                                                        "newText": new_text,
                                                    }],
                                                    "rawOutput": &text,
                                                }),
                                            );
                                        }
                                    }
                                    Some(_) => {
                                        emit_acp_update(
                                            &tx_clone,
                                            &sid,
                                            json!({
                                                "sessionUpdate": "tool_call_update",
                                                "toolCallId": item_id,
                                                "status": "completed",
                                                "rawOutput": text,
                                            }),
                                        );
                                    }
                                    None => {}
                                },
                                SessionEvent::ApprovalRequested { prompt, .. } => {
                                    // Bidirectional `session/request_permission` (ACP 3.1):
                                    // ask the editor, fail closed on EOF/cancel or an
                                    // unrecognized outcome.
                                    let rpc_id =
                                        next_outbound_rpc_id.fetch_add(1, AtomicOrdering::SeqCst);
                                    let (resp_tx, resp_rx) = oneshot::channel::<Value>();
                                    let pending_key =
                                        serde_json::to_string(&json!(rpc_id)).unwrap_or_default();
                                    pending_permissions
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .insert(pending_key, (sid.clone(), resp_tx));
                                    let raw = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": rpc_id,
                                        "method": "session/request_permission",
                                        "params": {
                                            "sessionId": sid,
                                            "toolCall": {
                                                "toolCallId": prompt.approval_id,
                                                "title": prompt.summary,
                                            },
                                            "options": [
                                                { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                                                { "optionId": "allow_always", "name": "Allow always", "kind": "allow_always" },
                                                { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" }
                                            ]
                                        }
                                    });
                                    tx_clone.send(raw.to_string()).ok();
                                    let resolution = match resp_rx.await {
                                        Ok(result) => map_acp_permission_result(&result),
                                        Err(_) => ApprovalResolution::Deny { reason: None },
                                    };
                                    let _ = h
                                        .send(SessionCommand::ResolveApproval {
                                            approval_id: prompt.approval_id,
                                            resolution,
                                        })
                                        .await;
                                }
                                SessionEvent::TurnCompleted { status, .. } => {
                                    if status == "interrupted" {
                                        stop_reason = "cancelled";
                                    }
                                    break;
                                }
                                _ => {}
                            }
                        }
                        send_acp_ok(&tx_clone, req_id, json!({ "stopReason": stop_reason }));
                    });
                }
            }
            _ => {}
        }
    }

    let _ = writer_task.await;
    Ok(())
}

/// Maps an ACP `session/request_permission` response into a core [`ApprovalResolution`].
///
/// Recognizes `{"outcome":{"outcome":"selected","optionId":"allow_once|allow_always|..."}}`
/// (and a bare `optionId` fallback). Anything else — including malformed results — denies.
#[must_use]
pub fn map_acp_permission_result(result: &Value) -> ApprovalResolution {
    let outcome = result.get("outcome").unwrap_or(result);
    let option_id = outcome
        .get("optionId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match option_id {
        "allow_once" => ApprovalResolution::Approve {
            scope: None,
            lifetime: GrantLifetime::Once,
            confirmed_broad: false,
        },
        "allow_always" => ApprovalResolution::Approve {
            scope: None,
            lifetime: GrantLifetime::Project,
            confirmed_broad: false,
        },
        _ => ApprovalResolution::Deny { reason: None },
    }
}

fn extract_acp_prompt(params: &Value) -> String {
    if let Some(s) = params.get("prompt").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(arr) = params.get("prompt").and_then(Value::as_array) {
        return arr
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

fn send_acp_ok(out_tx: &mpsc::UnboundedSender<String>, id: JsonRpcId, result: Value) {
    let resp = JsonRpcResponse {
        jsonrpc: Some("2.0".to_string()),
        id,
        result,
    };
    if let Ok(s) = serde_json::to_string(&resp) {
        let _ = out_tx.send(s);
    }
}

fn emit_acp_update(out_tx: &mpsc::UnboundedSender<String>, session_id: &str, update: Value) {
    let notif = JsonRpcNotification {
        jsonrpc: Some("2.0".to_string()),
        method: "session/update".to_string(),
        params: Some(json!({
            "sessionId": session_id,
            "update": update,
        })),
    };
    if let Ok(s) = serde_json::to_string(&notif) {
        let _ = out_tx.send(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tekvra_config::{CliConfigOverrides, ConfigPaths, load_config};
    use tekvra_protocol::TokenUsage;
    use tekvra_providers::{
        CompletionRequest, CompletionResponse, MessagePart, MessageRole, ModelCapabilities,
        ProviderDefinition, ProviderError, ProviderMessage, ProviderRegistry, StreamDelta,
    };
    use tempfile::TempDir;

    struct MockAcpDriver;

    #[async_trait]
    impl ProviderDriver for MockAcpDriver {
        async fn complete(
            &self,
            _provider: &ProviderDefinition,
            _model: &ModelCapabilities,
            _request: &CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            Ok(CompletionResponse {
                assistant_message: ProviderMessage {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::Text {
                        text: "ACP_STREAM_OK".to_string(),
                        cache_breakpoint: false,
                    }],
                },
                stream_deltas: vec![StreamDelta::TextDelta("ACP_STREAM_OK".to_string())],
                usage: TokenUsage::default(),
                served_by_account: "mock".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn acp_initialize_session_new_and_streaming_prompt_roundtrip() {
        let tmp = TempDir::new().expect("tempdir");
        let ws = tmp.path().join("ws");
        let cfg_dir = tmp.path().join("cfg");
        let state_dir = tmp.path().join("state");
        std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
        std::fs::create_dir_all(&state_dir).expect("mkdir state");

        let resolved = load_config(
            ConfigPaths {
                global_config_dir: cfg_dir,
                state_dir: state_dir.clone(),
                home_dir: Some(ws.clone()),
            },
            &ws,
            &CliConfigOverrides::default(),
        )
        .expect("config");
        let registry = ProviderRegistry::new();
        let router = ModelRouter::from_config(&resolved, registry);
        let state_index = Arc::new(Mutex::new(StateIndex::open(&state_dir).expect("state db")));

        let deps = AcpServerDeps {
            config: resolved,
            router,
            driver: Arc::new(MockAcpDriver),
            state_index,
            mcp_gateway: None,
            plugin_executor: None,
            extra_plugin_tools: Vec::new(),
        };

        let (cli, srv) = tokio::io::duplex(32 * 1024);
        let (srv_r, srv_w) = tokio::io::split(srv);
        tokio::spawn(async move {
            let _ = serve_acp_stream(srv_r, srv_w, deps).await;
        });

        let (cli_r, mut cli_w) = tokio::io::split(cli);
        let mut lines = BufReader::new(cli_r).lines();

        cli_w
            .write_all(b"{\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .expect("init");
        let init_v: Value =
            serde_json::from_str(&lines.next_line().await.expect("io").expect("l")).expect("j");
        assert_eq!(init_v["result"]["agentInfo"]["name"], "tekvra");

        cli_w
            .write_all(b"{\"id\":2,\"method\":\"session/new\",\"params\":{}}\n")
            .await
            .expect("new");
        let new_v: Value =
            serde_json::from_str(&lines.next_line().await.expect("io").expect("l")).expect("j");
        let sid = new_v["result"]["sessionId"]
            .as_str()
            .expect("sid")
            .to_string();

        let prompt_req = json!({
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": sid,
                "prompt": [{ "type": "text", "text": "Hello ACP" }]
            }
        });
        cli_w
            .write_all(format!("{prompt_req}\n").as_bytes())
            .await
            .expect("prompt");

        let mut saw_chunk = false;
        loop {
            let frame: Value =
                serde_json::from_str(&lines.next_line().await.expect("io").expect("l")).expect("j");
            if frame.get("method").and_then(Value::as_str) == Some("session/update")
                && frame["params"]["update"]["content"]["text"] == "ACP_STREAM_OK"
            {
                saw_chunk = true;
            }
            if frame.get("id") == Some(&json!(3)) {
                assert_eq!(frame["result"]["stopReason"], "end_turn");
                break;
            }
        }
        assert!(saw_chunk);
    }

    #[test]
    fn acp_permission_results_map_to_core_resolutions() {
        let allow_once = map_acp_permission_result(&json!({
            "outcome": { "outcome": "selected", "optionId": "allow_once" }
        }));
        assert_eq!(
            allow_once,
            ApprovalResolution::Approve {
                scope: None,
                lifetime: GrantLifetime::Once,
                confirmed_broad: false,
            }
        );

        let allow_always = map_acp_permission_result(&json!({
            "outcome": { "outcome": "selected", "optionId": "allow_always" }
        }));
        assert!(matches!(
            allow_always,
            ApprovalResolution::Approve {
                lifetime: GrantLifetime::Project,
                ..
            }
        ));

        let reject = map_acp_permission_result(&json!({
            "outcome": { "outcome": "selected", "optionId": "reject_once" }
        }));
        assert!(matches!(reject, ApprovalResolution::Deny { .. }));

        // Malformed / empty results fail closed.
        let malformed = map_acp_permission_result(&json!({ "unexpected": true }));
        assert!(matches!(malformed, ApprovalResolution::Deny { .. }));
        let empty = map_acp_permission_result(&json!({}));
        assert!(matches!(empty, ApprovalResolution::Deny { .. }));
    }

    #[tokio::test]
    async fn acp_unknown_client_response_frames_do_not_break_the_stream() {
        let tmp = TempDir::new().expect("tempdir");
        let ws = tmp.path().join("ws");
        let cfg_dir = tmp.path().join("cfg");
        let state_dir = tmp.path().join("state");
        std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
        std::fs::create_dir_all(&state_dir).expect("mkdir state");

        let resolved = load_config(
            ConfigPaths {
                global_config_dir: cfg_dir,
                state_dir,
                home_dir: Some(ws.clone()),
            },
            &ws,
            &CliConfigOverrides::default(),
        )
        .expect("config");
        let registry = ProviderRegistry::new();
        let router = ModelRouter::from_config(&resolved, registry);
        let state_index = Arc::new(Mutex::new(
            StateIndex::open(&tmp.path().join("state")).expect("state db"),
        ));

        let deps = AcpServerDeps {
            config: resolved,
            router,
            driver: Arc::new(MockAcpDriver),
            state_index,
            mcp_gateway: None,
            plugin_executor: None,
            extra_plugin_tools: Vec::new(),
        };

        let (cli, srv) = tokio::io::duplex(32 * 1024);
        let (srv_r, srv_w) = tokio::io::split(srv);
        tokio::spawn(async move {
            let _ = serve_acp_stream(srv_r, srv_w, deps).await;
        });

        let (cli_r, mut cli_w) = tokio::io::split(cli);
        let mut lines = BufReader::new(cli_r).lines();

        cli_w
            .write_all(b"{\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .expect("init");
        let _ = lines.next_line().await.expect("io").expect("line");

        // A stray response frame with no pending bidirectional request must be swallowed
        // (routed to the pending-permission map, no reply, stream stays alive).
        cli_w
            .write_all(b"{\"id\":4242,\"result\":{\"outcome\":{\"optionId\":\"allow_once\"}}}\n")
            .await
            .expect("stray response");
        cli_w
            .write_all(b"{\"id\":2,\"method\":\"session/new\",\"params\":{}}\n")
            .await
            .expect("new");
        let new_v: Value =
            serde_json::from_str(&lines.next_line().await.expect("io").expect("l")).expect("j");
        assert!(new_v["result"]["sessionId"].is_string());
    }
}

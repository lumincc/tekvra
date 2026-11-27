//! Specification and JSON Schema conformance tests for `tekvra-app-server` (Tasks 7.1 & 7.2).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tekvra_app_server::{AppServerState, AppServerStateInit, serve_read_write, serve_websocket};
use tekvra_config::{CliConfigOverrides, ConfigPaths, TrustStore, load_config};
use tekvra_protocol::TokenUsage;
use tekvra_providers::{
    CompletionRequest, CompletionResponse, MessagePart, MessageRole, ModelCapabilities,
    ModelRouter, ProviderDefinition, ProviderDriver, ProviderError, ProviderMessage,
    ProviderRegistry, StreamDelta,
};
use tekvra_storage::{StateIndex, WorkspaceSnapshotStore};
use tekvra_tools::{McpGateway, McpToolDescriptor, ToolsError};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct StaticMcpGateway;

#[async_trait]
impl McpGateway for StaticMcpGateway {
    fn list_tools(&self) -> Vec<McpToolDescriptor> {
        vec![McpToolDescriptor {
            server: "repo_docs".to_string(),
            tool: "search_symbols".to_string(),
            description: "Search indexed documentation symbols".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
        }]
    }

    async fn call_tool(
        &self,
        _server: &str,
        _tool: &str,
        arguments: Value,
    ) -> Result<Value, ToolsError> {
        Ok(json!({ "echo": arguments }))
    }
}

struct ScriptedAppDriver {
    responses: Mutex<VecDeque<CompletionResponse>>,
}

impl ScriptedAppDriver {
    fn new(list: Vec<CompletionResponse>) -> Self {
        Self {
            responses: Mutex::new(list.into()),
        }
    }
}

#[async_trait]
impl ProviderDriver for ScriptedAppDriver {
    async fn complete(
        &self,
        _provider: &ProviderDefinition,
        _model: &ModelCapabilities,
        _request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let mut q = self
            .responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(q.pop_front().unwrap_or_else(|| CompletionResponse {
            assistant_message: ProviderMessage {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    text: "Fallback done.".to_string(),
                    cache_breakpoint: false,
                }],
            },
            stream_deltas: vec![StreamDelta::TextDelta("Fallback done.".to_string())],
            usage: TokenUsage {
                input_tokens: 50,
                cached_input_tokens: 25,
                output_tokens: 10,
                reasoning_tokens: 0,
                cost_usd: 0.0005,
            },
            served_by_account: "mock".to_string(),
        }))
    }
}

fn build_app_state(tmp: &TempDir, scripted: Vec<CompletionResponse>) -> (AppServerState, PathBuf) {
    let ws = tmp.path().join("repo");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(ws.join("src")).expect("mkdir src");
    std::fs::write(ws.join("src/auth_handler.rs"), "pub fn verify() {}\n").expect("write src");
    std::fs::write(ws.join(".env.secret"), "SECRET=1\n").expect("write .env.secret");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");

    let mut trust = TrustStore::load(&state_dir).expect("trust");
    trust.grant_trust(&ws).expect("grant trust");

    let resolved = load_config(
        ConfigPaths {
            global_config_dir: cfg_dir,
            state_dir: state_dir.clone(),
            home_dir: Some(ws.clone()),
        },
        &ws,
        &CliConfigOverrides::default(),
    )
    .expect("load config");

    let registry = ProviderRegistry::new();
    let router = ModelRouter::from_config(&resolved, registry.clone());
    let state_index = Arc::new(Mutex::new(StateIndex::open(&state_dir).expect("state db")));
    let snapshots = WorkspaceSnapshotStore::new(&state_dir);
    let driver: Arc<dyn ProviderDriver> = Arc::new(ScriptedAppDriver::new(scripted));

    let state = AppServerState::new(AppServerStateInit {
        config: resolved,
        provider_registry: registry,
        router,
        driver,
        state_index,
        snapshots,
        mcp_gateway: Some(Arc::new(StaticMcpGateway)),
        plugin_executor: None,
        extra_plugin_tools: Vec::new(),
    });
    (state, ws)
}

fn validate_against_codex_def(bundle: &Value, def_name: &str, instance: &Value) {
    let mut sub = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "definitions": bundle["definitions"],
        "$ref": format!("#/definitions/{def_name}"),
    });
    if bundle.get("$defs").is_some() && bundle.get("definitions").is_none() {
        sub = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$defs": bundle["$defs"],
            "$ref": format!("#/$defs/{def_name}"),
        });
    }
    let validator = jsonschema::validator_for(&sub)
        .unwrap_or_else(|e| panic!("compile schema for {def_name}: {e}"));
    if let Err(err) = validator.validate(instance) {
        panic!(
            "Payload for `{def_name}` failed Codex JSON Schema validation: {err}\nPayload: {instance}"
        );
    }
}

#[tokio::test]
async fn stdio_schema_conformance_handshake_guard_fuzzy_search_and_config_warning() {
    let tmp = TempDir::new().expect("tempdir");
    let (state, ws) = build_app_state(&tmp, Vec::new());

    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let (srv_r, srv_w) = tokio::io::split(server_stream);
    tokio::spawn(async move {
        let _ = serve_read_write(srv_r, srv_w, state).await;
    });

    let (cli_r, mut cli_w) = tokio::io::split(client_stream);
    let mut lines = BufReader::new(cli_r).lines();

    // 1. Calling `thread/start` BEFORE `initialize` MUST return a JSON-RPC error!
    cli_w
        .write_all(b"{\"id\":1,\"method\":\"thread/start\",\"params\":{}}\n")
        .await
        .expect("write pre-init");
    let pre_resp: Value =
        serde_json::from_str(&lines.next_line().await.expect("io").expect("line")).expect("json");
    assert!(
        pre_resp.get("error").is_some(),
        "expected pre-initialize rejection, got: {pre_resp}"
    );

    // 2. `initialize` + `initialized`
    cli_w
        .write_all(
            b"{\"id\":2,\"method\":\"initialize\",\"params\":{\"clientInfo\":{\"name\":\"codex-test\",\"version\":\"0.1.0\"}}}\n",
        )
        .await
        .expect("write init");
    let init_resp: Value =
        serde_json::from_str(&lines.next_line().await.expect("io").expect("line")).expect("json");
    cli_w
        .write_all(b"{\"method\":\"initialized\"}\n")
        .await
        .expect("write initialized");

    // 3. `thread/start` with `sandbox = "read-only"` -> emits `thread/started` AND `configWarning`!
    cli_w
        .write_all(
            b"{\"id\":3,\"method\":\"thread/start\",\"params\":{\"sandbox\":\"read-only\"}}\n",
        )
        .await
        .expect("write thread/start");

    let mut thread_started_notif = None;
    let mut config_warning_notif = None;
    let mut thread_start_result = None;
    for _ in 0..3 {
        let msg: Value =
            serde_json::from_str(&lines.next_line().await.expect("io").expect("frame"))
                .expect("json");
        if msg.get("method").and_then(Value::as_str) == Some("thread/started") {
            thread_started_notif = Some(msg["params"].clone());
        } else if msg.get("method").and_then(Value::as_str) == Some("configWarning") {
            config_warning_notif = Some(msg["params"].clone());
        } else if msg.get("id") == Some(&json!(3)) {
            thread_start_result = Some(msg["result"].clone());
        }
    }
    let thread_started_notif = thread_started_notif.expect("thread/started emitted");
    let config_warning_notif = config_warning_notif.expect("configWarning emitted on read-only");
    let thread_start_result = thread_start_result.expect("thread/start response");

    // 4. Query `model/list`, `skills/list`, `mcpServerStatus/list`, `config/read`, `account/read`, `fuzzyFileSearch`
    let queries = [
        (4, "model/list", json!({})),
        (5, "skills/list", json!({})),
        (6, "mcpServerStatus/list", json!({})),
        (7, "config/read", json!({})),
        (8, "account/read", json!({})),
        (
            9,
            "fuzzyFileSearch",
            json!({ "query": "auth", "roots": [ws.to_string_lossy()] }),
        ),
    ];
    let mut results = std::collections::BTreeMap::new();
    for (id, method, params) in &queries {
        let line = serde_json::to_string(&json!({ "id": id, "method": method, "params": params }))
            .expect("ser");
        cli_w
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("send");
        let reply: Value =
            serde_json::from_str(&lines.next_line().await.expect("io").expect("line"))
                .expect("json");
        results.insert(*id, reply["result"].clone());
    }

    // Verify `fuzzyFileSearch` found `src/auth_handler.rs` and skipped `.env.secret`!
    let fuzzy_files = results[&9]["files"].as_array().expect("files array");
    assert_eq!(fuzzy_files.len(), 1);
    assert!(
        fuzzy_files[0]["path"]
            .as_str()
            .unwrap_or_default()
            .contains("auth_handler.rs")
    );

    // Verify `mcpServerStatus/list` reports live `repo_docs` MCP tools!
    let mcp_data = results[&6]["data"].as_array().expect("mcp data array");
    assert_eq!(mcp_data.len(), 1);
    assert_eq!(mcp_data[0]["name"], "repo_docs");
    assert_eq!(
        mcp_data[0]["tools"]["search_symbols"]["name"],
        "search_symbols"
    );

    // 5. Validate against generated Codex v1 & v2 JSON Schemas if present!
    let v1_path = PathBuf::from(
        "/home/jx03/.gemini/antigravity-cli/brain/6a8ba37d-5090-44d1-ada1-e77361ed465e/scratch/codex-schema/codex_app_server_protocol.schemas.json",
    );
    let v2_path = PathBuf::from(
        "/home/jx03/.gemini/antigravity-cli/brain/6a8ba37d-5090-44d1-ada1-e77361ed465e/scratch/codex-schema/codex_app_server_protocol.v2.schemas.json",
    );
    if v1_path.is_file() && v2_path.is_file() {
        let v1_bundle: Value =
            serde_json::from_str(&std::fs::read_to_string(v1_path).expect("read v1")).expect("v1");
        let v2_bundle: Value =
            serde_json::from_str(&std::fs::read_to_string(v2_path).expect("read v2")).expect("v2");

        validate_against_codex_def(&v1_bundle, "InitializeResponse", &init_resp["result"]);
        validate_against_codex_def(&v1_bundle, "FuzzyFileSearchResponse", &results[&9]);
        validate_against_codex_def(&v2_bundle, "ThreadStartResponse", &thread_start_result);
        validate_against_codex_def(
            &v2_bundle,
            "ThreadStartedNotification",
            &thread_started_notif,
        );
        validate_against_codex_def(
            &v2_bundle,
            "ConfigWarningNotification",
            &config_warning_notif,
        );
        validate_against_codex_def(&v2_bundle, "ModelListResponse", &results[&4]);
        validate_against_codex_def(&v2_bundle, "SkillsListResponse", &results[&5]);
        validate_against_codex_def(&v2_bundle, "ListMcpServerStatusResponse", &results[&6]);
        validate_against_codex_def(&v2_bundle, "ConfigReadResponse", &results[&7]);
        validate_against_codex_def(&v2_bundle, "GetAccountResponse", &results[&8]);
    }
}

#[tokio::test]
async fn websocket_bearer_auth_and_bidirectional_command_approval_and_denial() {
    let tmp = TempDir::new().expect("tempdir");
    // Windows runners expose `python`, not `python3`.
    let python = if cfg!(windows) { "python" } else { "python3" };
    // Turn 1: model calls `exec_command` (python printing a marker — triggers interactive `Ask` in Build mode!) -> approved -> finishes.
    // Turn 2: model calls `exec_command` (second python print) -> declined -> finishes reporting denial.
    let scripted = vec![
        CompletionResponse {
            assistant_message: ProviderMessage {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::ToolCall {
                    call_id: "call_ws_1".to_string(),
                    name: "exec_command".to_string(),
                    arguments_json: json!({
                        "command": format!("{python} -c \"print('WS_APPROVED_OK')\"")
                    })
                    .to_string(),
                }],
            },
            stream_deltas: Vec::new(),
            usage: TokenUsage::default(),
            served_by_account: "mock".to_string(),
        },
        CompletionResponse {
            assistant_message: ProviderMessage {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    text: "Command approved and executed!".to_string(),
                    cache_breakpoint: false,
                }],
            },
            stream_deltas: vec![StreamDelta::TextDelta(
                "Command approved and executed!".to_string(),
            )],
            usage: TokenUsage::default(),
            served_by_account: "mock".to_string(),
        },
        CompletionResponse {
            assistant_message: ProviderMessage {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::ToolCall {
                    call_id: "call_ws_2".to_string(),
                    name: "exec_command".to_string(),
                    arguments_json: json!({
                        "command": format!("{python} -c \"print('MUST_NOT_RUN')\"")
                    })
                    .to_string(),
                }],
            },
            stream_deltas: Vec::new(),
            usage: TokenUsage::default(),
            served_by_account: "mock".to_string(),
        },
        CompletionResponse {
            assistant_message: ProviderMessage {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    text: "Command was declined by user.".to_string(),
                    cache_breakpoint: false,
                }],
            },
            stream_deltas: vec![StreamDelta::TextDelta(
                "Command was declined by user.".to_string(),
            )],
            usage: TokenUsage::default(),
            served_by_account: "mock".to_string(),
        },
    ];
    let (state, _ws) = build_app_state(&tmp, scripted);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ws");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = serve_websocket(listener, Some("cap_secret_123".to_string()), state).await;
    });

    // 1. Unauthenticated WebSocket upgrade MUST fail (`401 Unauthorized`)!
    let unauth_err = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect_err("unauthenticated ws upgrade must fail");
    assert!(
        unauth_err.to_string().contains("401"),
        "expected HTTP 401 Unauthorized, got: {unauth_err}"
    );

    // 2. Authenticated WebSocket upgrade with `Authorization: Bearer cap_secret_123` succeeds!
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut ws_req = format!("ws://{addr}/ws")
        .into_client_request()
        .expect("ws req");
    ws_req.headers_mut().insert(
        "Authorization",
        http::HeaderValue::from_static("Bearer cap_secret_123"),
    );
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(ws_req)
        .await
        .expect("authenticated ws connect");

    // Handshake + `thread/start`
    ws_stream
        .send(
            json!({
                "id": 1,
                "method": "initialize",
                "params": { "clientInfo": { "name": "ws-ide", "version": "1.0.0" } }
            })
            .to_string()
            .into(),
        )
        .await
        .expect("send init");
    let _ = ws_stream.next().await.expect("init reply").expect("ok");

    ws_stream
        .send(
            json!({
                "id": 2,
                "method": "thread/start",
                "params": { "sandbox": "workspace-write", "approvalPolicy": "on-request" }
            })
            .to_string()
            .into(),
        )
        .await
        .expect("send thread/start");

    let mut thread_id = String::new();
    while thread_id.is_empty() {
        let txt = ws_stream
            .next()
            .await
            .expect("frame")
            .expect("ok")
            .into_text()
            .expect("text");
        let v: Value = serde_json::from_str(&txt).expect("json");
        if v.get("id") == Some(&json!(2)) {
            thread_id = v["result"]["thread"]["id"]
                .as_str()
                .expect("thread id")
                .to_string();
        }
    }

    // 3. Turn 1: `turn/start` -> receive `item/commandExecution/requestApproval` -> reply `"accept"`!
    ws_stream
        .send(
            json!({
                "id": 3,
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [{ "type": "text", "text": "Run python check" }]
                }
            })
            .to_string()
            .into(),
        )
        .await
        .expect("send turn/start 1");

    let mut saw_approval_req_1 = false;
    let mut saw_ws_approved_output = false;
    // Keep every completed item so a failure shows what actually ran.
    let mut completed_items = Vec::new();
    loop {
        let txt = ws_stream
            .next()
            .await
            .expect("frame")
            .expect("ok")
            .into_text()
            .expect("text");
        let v: Value = serde_json::from_str(&txt).expect("json");
        if v.get("method").and_then(Value::as_str) == Some("item/completed") {
            completed_items.push(v["params"]["item"].clone());
        }
        if v.get("method").and_then(Value::as_str) == Some("item/commandExecution/requestApproval")
        {
            saw_approval_req_1 = true;
            let srv_id = v["id"].clone();
            ws_stream
                .send(
                    json!({
                        "id": srv_id,
                        "result": { "decision": "accept" }
                    })
                    .to_string()
                    .into(),
                )
                .await
                .expect("send accept");
        }
        if v.get("method").and_then(Value::as_str) == Some("item/completed")
            && v["params"]["item"]["aggregatedOutput"]
                .as_str()
                .unwrap_or_default()
                .contains("WS_APPROVED_OK")
        {
            saw_ws_approved_output = true;
        }
        if v.get("method").and_then(Value::as_str) == Some("turn/completed") {
            break;
        }
    }
    assert!(
        saw_approval_req_1,
        "expected server->client requestApproval"
    );
    assert!(
        saw_ws_approved_output,
        "expected approved command to execute; completed items: {completed_items:?}"
    );

    // 4. Turn 2: `turn/start` -> receive `item/commandExecution/requestApproval` -> reply `"decline"`!
    ws_stream
        .send(
            json!({
                "id": 4,
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [{ "type": "text", "text": "Run second python check" }]
                }
            })
            .to_string()
            .into(),
        )
        .await
        .expect("send turn/start 2");

    let mut saw_approval_req_2 = false;
    let mut saw_must_not_run = false;
    loop {
        let txt = ws_stream
            .next()
            .await
            .expect("frame")
            .expect("ok")
            .into_text()
            .expect("text");
        let v: Value = serde_json::from_str(&txt).expect("json");
        if v.get("method").and_then(Value::as_str) == Some("item/commandExecution/requestApproval")
        {
            saw_approval_req_2 = true;
            let srv_id = v["id"].clone();
            ws_stream
                .send(
                    json!({
                        "id": srv_id,
                        "result": { "decision": "decline" }
                    })
                    .to_string()
                    .into(),
                )
                .await
                .expect("send decline");
        }
        if txt.contains("MUST_NOT_RUN")
            && v.get("method").and_then(Value::as_str) == Some("item/completed")
            && let Some(out) = v["params"]["item"]["aggregatedOutput"].as_str()
            && out.contains("MUST_NOT_RUN")
        {
            saw_must_not_run = true;
        }
        if v.get("method").and_then(Value::as_str) == Some("turn/completed") {
            break;
        }
    }
    assert!(saw_approval_req_2);
    assert!(!saw_must_not_run, "declined command must never execute");
}

#[tokio::test]
async fn thread_compact_start_queues_compaction_and_rate_limits_read_reports_pool() {
    let tmp = TempDir::new().expect("tempdir");
    let (state, _ws) = build_app_state(&tmp, Vec::new());

    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let (srv_r, srv_w) = tokio::io::split(server_stream);
    tokio::spawn(async move {
        let _ = serve_read_write(srv_r, srv_w, state).await;
    });

    let (cli_r, mut cli_w) = tokio::io::split(client_stream);
    let mut lines = BufReader::new(cli_r).lines();

    cli_w
        .write_all(
            b"{\"id\":1,\"method\":\"initialize\",\"params\":{\"clientInfo\":{\"name\":\"codex-test\",\"version\":\"0.1.0\"}}}\n",
        )
        .await
        .expect("write init");
    let _: Value =
        serde_json::from_str(&lines.next_line().await.expect("io").expect("line")).expect("j");
    cli_w
        .write_all(b"{\"method\":\"initialized\"}\n")
        .await
        .expect("write initialized");

    cli_w
        .write_all(b"{\"id\":2,\"method\":\"thread/start\",\"params\":{}}\n")
        .await
        .expect("write thread/start");
    let mut thread_id: Option<String> = None;
    while thread_id.is_none() {
        let msg: Value =
            serde_json::from_str(&lines.next_line().await.expect("io").expect("frame"))
                .expect("json");
        if msg.get("id") == Some(&json!(2)) {
            thread_id = Some(
                msg["result"]["thread"]["id"]
                    .as_str()
                    .expect("id")
                    .to_string(),
            );
        }
    }
    let thread_id = thread_id.expect("thread id from thread/start");

    // Live `account/rateLimits/read`: deterministic driver -> empty pool summary.
    cli_w
        .write_all(b"{\"id\":3,\"method\":\"account/rateLimits/read\",\"params\":{}}\n")
        .await
        .expect("write rate limits");
    let limits: Value =
        serde_json::from_str(&lines.next_line().await.expect("io").expect("line")).expect("j");
    assert_eq!(
        limits["result"]["rateLimits"]["primaryWindow"]["remainingRatio"],
        1.0
    );
    assert_eq!(limits["result"]["rateLimits"]["spendControlReached"], false);
    assert_eq!(limits["result"]["accounts"], json!([]));

    // `thread/compact/start` queues `SessionCommand::Compact` and answers `{}`.
    let req = serde_json::to_string(&json!({
        "id": 4,
        "method": "thread/compact/start",
        "params": { "threadId": thread_id }
    }))
    .expect("ser");
    cli_w
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("send compact");
    let compact: Value =
        serde_json::from_str(&lines.next_line().await.expect("io").expect("line")).expect("j");
    assert!(
        compact.get("error").is_none(),
        "compact must succeed: {compact}"
    );
    assert_eq!(compact["result"], json!({}));

    // Unknown thread id -> clean JSON-RPC error, not a panic.
    let req = serde_json::to_string(&json!({
        "id": 5,
        "method": "thread/compact/start",
        "params": { "threadId": "thr_missing" }
    }))
    .expect("ser");
    cli_w
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("send missing");
    let missing: Value =
        serde_json::from_str(&lines.next_line().await.expect("io").expect("line")).expect("j");
    assert!(
        missing.get("error").is_some(),
        "unknown thread must error: {missing}"
    );
}

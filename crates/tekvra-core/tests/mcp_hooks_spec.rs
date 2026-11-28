//! Session-level server→client MCP tests: `sampling/createMessage` through the interactive
//! question bridge + `fast` role routing, `elicitation/create` typed answers, usage ledger
//! attribution, and tool-call continuity after server-initiated round trips.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tekvra_config::{CliConfigOverrides, ConfigPaths, McpServerConfig, TrustStore, load_config};
use tekvra_core::{SessionBootstrap, spawn_session};
use tekvra_protocol::{
    ApprovalResolution, GrantLifetime, InputDeliveryMode, PermissionMode, SessionCommand,
    SessionEvent, TokenUsage,
};
use tekvra_providers::{
    CompletionRequest, CompletionResponse, MessagePart, MessageRole, ModelCapabilities,
    ModelRouter, ProviderDefinition, ProviderDriver, ProviderError, ProviderRegistry, StreamDelta,
};
use tekvra_storage::StateIndex;
use tempfile::TempDir;

/// Driver that answers the *main* conversation frame from a script and sampling sub-requests
/// (identified by their `-mcp-sampling` conversation id) with a fixed text.
struct TurnScriptDriver {
    script: Mutex<std::collections::VecDeque<CompletionResponse>>,
}

impl TurnScriptDriver {
    fn new(responses: Vec<CompletionResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(responses.into_iter().collect()),
        })
    }
}

fn text_resp(text: &str) -> CompletionResponse {
    CompletionResponse {
        assistant_message: tekvra_providers::ProviderMessage {
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                text: text.to_string(),
                cache_breakpoint: false,
            }],
        },
        stream_deltas: vec![StreamDelta::TextDelta(text.to_string())],
        usage: TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 80,
            output_tokens: 20,
            reasoning_tokens: 0,
            cost_usd: 0.001,
        },
        served_by_account: "mock".to_string(),
    }
}

fn tool_call_resp(call_id: &str, name: &str, args: Value) -> CompletionResponse {
    CompletionResponse {
        assistant_message: tekvra_providers::ProviderMessage {
            role: MessageRole::Assistant,
            parts: vec![MessagePart::ToolCall {
                call_id: call_id.to_string(),
                name: name.to_string(),
                arguments_json: args.to_string(),
            }],
        },
        stream_deltas: Vec::new(),
        usage: TokenUsage {
            input_tokens: 150,
            cached_input_tokens: 120,
            output_tokens: 40,
            reasoning_tokens: 15,
            cost_usd: 0.002,
        },
        served_by_account: "mock".to_string(),
    }
}

#[async_trait]
impl ProviderDriver for TurnScriptDriver {
    async fn complete(
        &self,
        provider: &ProviderDefinition,
        model: &ModelCapabilities,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        if request.conversation_id.ends_with("-mcp-sampling") {
            // Server-initiated sampling sub-request: fixed completion.
            return Ok(CompletionResponse {
                assistant_message: tekvra_providers::ProviderMessage {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::Text {
                        text: "SAMPLED_MODEL_REPLY".to_string(),
                        cache_breakpoint: false,
                    }],
                },
                stream_deltas: Vec::new(),
                usage: TokenUsage {
                    input_tokens: 11,
                    cached_input_tokens: 0,
                    output_tokens: 5,
                    reasoning_tokens: 0,
                    cost_usd: 0.0002,
                },
                served_by_account: "mock".to_string(),
            });
        }
        let next = self
            .script
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| text_resp("fallback"));
        let _ = (provider, model);
        Ok(next)
    }
}

fn setup_bootstrap(tmp: &TempDir, driver: Arc<dyn ProviderDriver>) -> SessionBootstrap {
    let ws = tmp.path().join("repo");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&ws).expect("mkdir ws");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");

    let mut trust = TrustStore::load(&state_dir).expect("load trust");
    trust.grant_trust(&ws).expect("grant trust");

    let mut resolved = load_config(
        ConfigPaths {
            global_config_dir: cfg_dir,
            state_dir: state_dir.clone(),
            home_dir: Some(ws.clone()),
        },
        &ws,
        &CliConfigOverrides::default(),
    )
    .expect("load config");
    resolved.settings.agent.default_mode = PermissionMode::Build;
    resolved.settings.experimental.mcp_sampling = true;

    let reg = ProviderRegistry::new();
    let router = ModelRouter::from_config(&resolved, reg);
    let state_index = StateIndex::open(&state_dir).expect("open state");

    SessionBootstrap {
        session_id: None,
        parent_session_id: None,
        workspace_root: ws,
        state_dir,
        config: resolved,
        router,
        driver,
        state_index: Some(Arc::new(std::sync::Mutex::new(state_index))),
        compaction: tekvra_core::CompactionConfig::default(),
        mcp_gateway: None,
        plugin_executor: None,
        extra_plugin_tools: Vec::new(),
        allow_subagents: false,
        subagent_depth: 0,
    }
}

/// Stdio MCP mock: on `tools/call` it pushes a `sampling/createMessage` request followed by an
/// `elicitation/create` request, reads the client's responses, and embeds both outcomes into the
/// tool-call result text.
fn write_hook_stdio_script(path: &std::path::Path) {
    std::fs::write(
        path,
        r#"import json, sys

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    mid = msg.get("id")
    method = msg.get("method")
    if mid is None:
        continue
    if method == "initialize":
        res = {"protocolVersion": "2024-11-05", "capabilities": {"sampling": {}, "elicitation": {}}, "serverInfo": {"name": "hook-demo", "version": "1.0"}}
    elif method == "tools/list":
        res = {"tools": [{"name": "probe", "description": "Triggers sampling+elicitation", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        send({"jsonrpc": "2.0", "id": "srv_sampling_1", "method": "sampling/createMessage",
              "params": {"messages": [{"role": "user", "content": [{"type": "text", "text": "summarize"}]}]}})
        sampling_resp = json.loads(sys.stdin.readline())
        if "result" in sampling_resp:
            sample_text = sampling_resp["result"].get("content", {}).get("text", "")
        else:
            sample_text = "ERR:" + str(sampling_resp.get("error", {}).get("code"))
        send({"jsonrpc": "2.0", "id": "srv_elicit_1", "method": "elicitation/create",
              "params": {"message": "Proceed?", "requestedSchema": {"type": "object", "properties": {"ok": {"type": "boolean"}}}}})
        elicit_resp = json.loads(sys.stdin.readline())
        if "result" in elicit_resp:
            elicit_action = elicit_resp["result"].get("action")
            elicit_ok = elicit_resp["result"].get("content", {}).get("ok")
        else:
            elicit_action = "error"
            elicit_ok = None
        res = {"content": [{"type": "text", "text": f"sample={sample_text}; elicit={elicit_action}:{elicit_ok}"}]}
    else:
        res = {}
    send({"jsonrpc": "2.0", "id": mid, "result": res})
"#,
    )
    .expect("write hook stdio script");
}

/// Stdio MCP mock: `tools/call` streams fifty rapid `notifications/progress` frames on the
/// request's `_meta.progressToken`, waits for the coalesced final value to flush, then returns.
fn write_progress_stdio_script(path: &std::path::Path) {
    std::fs::write(
        path,
        r#"import json, sys, time

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    mid = msg.get("id")
    method = msg.get("method")
    if mid is None:
        continue
    if method == "initialize":
        res = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "progress-demo", "version": "1.0"}}
    elif method == "tools/list":
        res = {"tools": [{"name": "long_job", "description": "Streams progress", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        token = msg["params"]["_meta"]["progressToken"]
        for step in range(1, 51):
            params = {"progressToken": token, "progress": step, "total": 50}
            if step == 50:
                params["message"] = "indexing done"
            send({"jsonrpc": "2.0", "method": "notifications/progress", "params": params})
        time.sleep(0.4)
        res = {"content": [{"type": "text", "text": "job done"}]}
    else:
        res = {}
    send({"jsonrpc": "2.0", "id": mid, "result": res})
"#,
    )
    .expect("write progress stdio script");
}

async fn await_event(
    rx: &mut tokio::sync::broadcast::Receiver<SessionEvent>,
    want: fn(&SessionEvent) -> bool,
) -> SessionEvent {
    loop {
        match rx.recv().await {
            Ok(ev) if want(&ev) => return ev,
            Ok(_) => {}
            Err(e) => panic!("event stream closed: {e}"),
        }
    }
}

#[tokio::test]
async fn sampling_and_elicitation_round_trip_through_session_hooks() {
    let tmp = TempDir::new().expect("tempdir");

    // Scripted main turn: model calls `code` to invoke the MCP probe tool, then wraps up.
    let driver = TurnScriptDriver::new(vec![
        tool_call_resp(
            "call_probe_1",
            "code",
            json!({ "code": "tools = mcp_list()\nr = await mcp_call('hook_srv', 'probe', {})\nprint(r['content'][0]['text'])" }),
        ),
        text_resp("probe done"),
    ]);

    let mut bootstrap = setup_bootstrap(&tmp, Arc::clone(&driver) as Arc<dyn ProviderDriver>);
    let state_dir = bootstrap.state_dir.clone();

    // Stdio hook server with per-server sampling opt-in.
    let script_path = tmp.path().join("hook_stdio_mcp.py");
    write_hook_stdio_script(&script_path);
    let mut servers: indexmap::IndexMap<String, McpServerConfig> = indexmap::IndexMap::new();
    servers.insert(
        "hook_srv".to_string(),
        McpServerConfig {
            command: Some("python3".to_string()),
            args: vec![script_path.to_string_lossy().into_owned()],
            enabled: true,
            sampling: true,
            ..McpServerConfig::default()
        },
    );
    let (gateway, report) = tekvra_tools::ConfiguredMcpGateway::connect_all(
        &servers,
        &bootstrap.workspace_root,
        &bootstrap.config.settings.agent,
        true, // experimental.mcp_sampling
    )
    .await;
    assert_eq!(report.connected_servers, vec!["hook_srv"]);
    bootstrap.mcp_gateway = Some(gateway);

    let handle = spawn_session(bootstrap).expect("spawn session");
    let mut rx = handle.subscribe();

    handle
        .send(SessionCommand::SubmitInput {
            prompt: "probe the mcp server".to_string(),
            delivery: InputDeliveryMode::Next,
            images: Vec::new(),
        })
        .await
        .expect("submit");

    // The `code`-sandbox `mcp_call` is authorized through the interactive ladder first:
    // approve it, then handle the sampling approval prompt the server triggers.
    let mcp_approval = await_event(&mut rx, |e| {
        matches!(
            e,
            SessionEvent::ApprovalRequested { .. } | SessionEvent::UserInputRequested { .. }
        )
    })
    .await;
    match mcp_approval {
        SessionEvent::ApprovalRequested { prompt, .. } => {
            handle
                .send(SessionCommand::ResolveApproval {
                    approval_id: prompt.approval_id,
                    resolution: ApprovalResolution::Approve {
                        scope: None,
                        lifetime: GrantLifetime::Once,
                        confirmed_broad: false,
                    },
                })
                .await
                .expect("approve mcp tool call");
        }
        SessionEvent::UserInputRequested { .. } => {
            panic!("sampling prompt arrived before the mcp tool-call approval");
        }
        _ => unreachable!(),
    }

    // The sampling approval prompt arrives as a user-input request; approve it.
    let ev = await_event(&mut rx, |e| {
        matches!(e, SessionEvent::UserInputRequested { .. })
    })
    .await;
    let SessionEvent::UserInputRequested { item_id, .. } = ev else {
        unreachable!()
    };
    handle
        .send(SessionCommand::ResolveUserInput {
            item_id,
            answers: BTreeMap::from([("q0".to_string(), vec!["y".to_string()])]),
        })
        .await
        .expect("approve sampling");

    // The elicitation prompt arrives as a second user-input request; answer boolean=true.
    let ev = await_event(&mut rx, |e| {
        matches!(e, SessionEvent::UserInputRequested { .. })
    })
    .await;
    let SessionEvent::UserInputRequested { item_id, .. } = ev else {
        unreachable!()
    };
    handle
        .send(SessionCommand::ResolveUserInput {
            item_id,
            answers: BTreeMap::from([("q0".to_string(), vec!["true".to_string()])]),
        })
        .await
        .expect("answer elicitation");

    loop {
        match rx.recv().await {
            Ok(SessionEvent::TurnCompleted { .. }) => break,
            Ok(_) => {}
            Err(e) => panic!("event stream closed: {e}"),
        }
    }

    // Read the final turn output: the `code` tool printed the round-trip summary.
    // Recover it from the rollout: find the CodeExecution output containing the summary.
    let session_id = handle.session_id().to_string();
    let records = tekvra_storage::RolloutLog::read_records_readonly(&state_dir, &session_id)
        .expect("read rollout");
    let mut summary_seen = false;
    for rec in &records {
        if let tekvra_storage::RolloutRecord::ToolCard { output, .. } = rec {
            let text = output.to_model_text();
            if text.contains("sample=SAMPLED_MODEL_REPLY; elicit=accept:True") {
                summary_seen = true;
            }
        }
    }
    assert!(
        summary_seen,
        "expected sampling+elicitation round trip through session hooks; tool outputs: {:?}",
        records
            .iter()
            .filter_map(|rec| match rec {
                tekvra_storage::RolloutRecord::ToolCard { output, .. } => {
                    Some(output.to_model_text())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    );

    // Sampling usage is attributed under `<rpc_id>_mcp_sampling`: the ledger total must
    // exceed the main-turn contributions alone (0.001 + 0.002).
    let index = StateIndex::open(&state_dir).expect("reopen state");
    let totals = index.session_usage_totals(&session_id).expect("totals");
    assert!(
        totals.cost_usd > 0.0031,
        "sampling usage must be recorded in the ledger, got {}",
        totals.cost_usd
    );
    drop(handle);
}

/// One `McpProgress` event as received by a `subscribe()` client.
#[derive(Debug, PartialEq)]
struct SeenProgress {
    thread_id: String,
    turn_id: String,
    server: String,
    tool: String,
    progress: f64,
    total: Option<f64>,
    message: Option<String>,
}

/// `notifications/progress` for an in-flight `tools/call` becomes `SessionEvent::McpProgress`
/// with the running turn's ids: the burst is coalesced (first and final values both arrive,
/// in order), nothing surfaces after `TurnCompleted`, and the rollout never records it.
#[tokio::test]
async fn mcp_progress_events_reach_subscribers_without_entering_the_rollout() {
    let tmp = TempDir::new().expect("tempdir");
    let driver = TurnScriptDriver::new(vec![
        tool_call_resp(
            "call_progress_1",
            "code",
            json!({ "code": "r = await mcp_call('hook_srv', 'long_job', {})\nprint(r['content'][0]['text'])" }),
        ),
        text_resp("job finished"),
    ]);
    let mut bootstrap = setup_bootstrap(&tmp, Arc::clone(&driver) as Arc<dyn ProviderDriver>);
    let state_dir = bootstrap.state_dir.clone();

    let script_path = tmp.path().join("progress_stdio_mcp.py");
    write_progress_stdio_script(&script_path);
    let mut servers: indexmap::IndexMap<String, McpServerConfig> = indexmap::IndexMap::new();
    servers.insert(
        "hook_srv".to_string(),
        McpServerConfig {
            command: Some("python3".to_string()),
            args: vec![script_path.to_string_lossy().into_owned()],
            enabled: true,
            ..McpServerConfig::default()
        },
    );
    let (gateway, report) = tekvra_tools::ConfiguredMcpGateway::connect_all(
        &servers,
        &bootstrap.workspace_root,
        &bootstrap.config.settings.agent,
        false,
    )
    .await;
    assert_eq!(report.connected_servers, vec!["hook_srv"]);
    bootstrap.mcp_gateway = Some(gateway);

    let handle = spawn_session(bootstrap).expect("spawn session");
    let mut rx = handle.subscribe();
    let session_id = handle.session_id().to_string();
    handle
        .send(SessionCommand::SubmitInput {
            prompt: "run the long job".to_string(),
            delivery: InputDeliveryMode::Next,
            images: Vec::new(),
        })
        .await
        .expect("submit");

    let mut progress = Vec::new();
    let completed = loop {
        match rx.recv().await {
            Ok(SessionEvent::ApprovalRequested { prompt, .. }) => {
                // The `code`-sandbox `mcp_call` is authorized through the interactive ladder.
                handle
                    .send(SessionCommand::ResolveApproval {
                        approval_id: prompt.approval_id,
                        resolution: ApprovalResolution::Approve {
                            scope: None,
                            lifetime: GrantLifetime::Once,
                            confirmed_broad: false,
                        },
                    })
                    .await
                    .expect("approve mcp tool call");
            }
            Ok(SessionEvent::McpProgress {
                thread_id,
                turn_id,
                server,
                tool,
                progress: value,
                total,
                message,
            }) => progress.push(SeenProgress {
                thread_id,
                turn_id,
                server,
                tool,
                progress: value,
                total,
                message,
            }),
            Ok(SessionEvent::TurnCompleted {
                turn_id, status, ..
            }) => {
                break (turn_id, status);
            }
            Ok(_) => {}
            Err(e) => panic!("event stream closed: {e}"),
        }
    };
    let (turn_id, status) = completed;
    assert_eq!(status, "completed");
    assert!(turn_id.starts_with("turn_"), "{turn_id}");
    assert!(
        progress.len() >= 2,
        "coalescing must keep the first and the final update: {progress:?}"
    );
    let expected = |value, total, message: Option<&str>| SeenProgress {
        thread_id: session_id.clone(),
        turn_id: turn_id.clone(),
        server: "hook_srv".to_string(),
        tool: "long_job".to_string(),
        progress: value,
        total,
        message: message.map(str::to_string),
    };
    assert_eq!(progress[0], expected(1.0, Some(50.0), None));
    assert_eq!(
        progress.last(),
        Some(&expected(50.0, Some(50.0), Some("indexing done")))
    );
    assert!(
        progress.windows(2).all(|w| w[0].progress <= w[1].progress),
        "progress must not go backwards: {progress:?}"
    );

    // The turn is retired at completion: no late (coalesced) progress may surface afterwards.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    while let Ok(ev) = rx.try_recv() {
        assert!(
            !matches!(ev, SessionEvent::McpProgress { .. }),
            "late progress after TurnCompleted: {ev:?}"
        );
    }

    // Progress is live-only: the rollout holds the tool card, never a progress entry.
    let records = tekvra_storage::RolloutLog::read_records_readonly(&state_dir, &session_id)
        .expect("read rollout");
    assert!(
        records
            .iter()
            .any(|rec| matches!(rec, tekvra_storage::RolloutRecord::ToolCard { .. })),
        "expected the tool card in the rollout"
    );
    assert!(
        records
            .iter()
            .all(|rec| !format!("{rec:?}").contains("indexing done")),
        "progress leaked into the rollout"
    );
    drop(handle);
}

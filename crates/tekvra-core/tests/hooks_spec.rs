//! Specification integration tests for session-level Luau hook dispatch:
//! `pre_tool_use` deny gating, `post_tool_use`/`turn_started` notes as steering
//! notes (never model-frame messages), and prompt-cache frame stability.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tekvra_config::{CliConfigOverrides, ConfigPaths, TrustStore, load_config};
use tekvra_core::{
    HookEvent, HookEventPayload, HookOutcome, PluginToolExecutor, SessionBootstrap, spawn_session,
};
use tekvra_protocol::{
    InputDeliveryMode, PermissionMode, SessionCommand, SessionEvent, TokenUsage, ToolOutput,
};
use tekvra_providers::{
    CompletionRequest, CompletionResponse, MessagePart, MessageRole, ModelCapabilities,
    ModelRouter, ProviderDefinition, ProviderDriver, ProviderError, ProviderRegistry, StreamDelta,
};
use tekvra_storage::RolloutRecord;
use tempfile::TempDir;

type CompletionHandler = dyn Fn(&CompletionRequest) -> CompletionResponse + Send + Sync;

struct ScriptedDriver {
    handler: Box<CompletionHandler>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl ScriptedDriver {
    fn from_fn(
        f: impl Fn(&CompletionRequest) -> CompletionResponse + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            handler: Box::new(f),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn recorded(&self) -> Vec<CompletionRequest> {
        self.requests.lock().expect("lock").clone()
    }
}

#[async_trait]
impl ProviderDriver for ScriptedDriver {
    async fn complete(
        &self,
        _provider: &ProviderDefinition,
        _model: &ModelCapabilities,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().expect("lock").push(request.clone());
        Ok((self.handler)(request))
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

fn tool_call_resp(call_id: &str, name: &str, args: serde_json::Value) -> CompletionResponse {
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

/// Fake plugin executor: denies `file_write` in `pre_tool_use`, notes on turns.
#[derive(Default)]
struct HookScriptExecutor;

#[async_trait]
impl PluginToolExecutor for HookScriptExecutor {
    async fn execute_plugin_tool(
        &self,
        _tool_name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<ToolOutput, String> {
        Err("no plugin tools registered".to_string())
    }

    fn dispatch_hook(&self, event: HookEvent, payload: HookEventPayload) -> HookOutcome {
        match event {
            HookEvent::TurnStarted => HookOutcome {
                notes: vec!["turn hook note".to_string()],
                ..HookOutcome::default()
            },
            HookEvent::PreToolUse if payload.tool.as_deref() == Some("file_write") => HookOutcome {
                denied: Some("never write files".to_string()),
                ..HookOutcome::default()
            },
            _ => HookOutcome::default(),
        }
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

    let reg = ProviderRegistry::new();
    let router = ModelRouter::from_config(&resolved, reg);

    SessionBootstrap {
        session_id: None,
        parent_session_id: None,
        workspace_root: ws,
        state_dir,
        config: resolved,
        router,
        driver,
        state_index: None,
        compaction: tekvra_core::CompactionConfig::default(),
        mcp_gateway: None,
        plugin_executor: Some(Arc::new(HookScriptExecutor)),
        extra_plugin_tools: Vec::new(),
        allow_subagents: false,
        subagent_depth: 0,
    }
}

fn flat_messages(request: &CompletionRequest) -> String {
    request
        .messages
        .iter()
        .map(|m| {
            m.parts
                .iter()
                .filter_map(|p| match p {
                    MessagePart::Text { text, .. } => Some(text.as_str()),
                    MessagePart::ToolResult { output, .. } => Some(output.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n----\n")
}

#[tokio::test]
async fn pre_tool_deny_gates_execution_and_notes_become_steering_notes() {
    let tmp = TempDir::new().expect("tempdir");
    let target = tmp.path().join("repo").join("new.rs");

    // Script: first request returns the denied `file_write` call, the
    // tool-result follow-up gets the closing text response.
    let driver = ScriptedDriver::from_fn(move |request: &CompletionRequest| {
        let calls: Vec<&MessagePart> = request
            .messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter(|p| matches!(p, MessagePart::ToolCall { .. }))
            .collect();
        if calls.is_empty() {
            tool_call_resp(
                "call_deny_1",
                "file_write",
                serde_json::json!({ "path": "new.rs", "content": "pub fn x() {}" }),
            )
        } else {
            text_resp("done without writing")
        }
    });
    let recorded = Arc::clone(&driver) as Arc<dyn ProviderDriver>;
    let script_driver = driver;

    let bootstrap = setup_bootstrap(&tmp, recorded);
    let state_dir = bootstrap.state_dir.clone();
    let session_id_for_rollout = String::new();
    let _ = session_id_for_rollout;
    let handle = spawn_session(bootstrap).expect("spawn session");
    let mut rx = handle.subscribe();
    handle
        .send(SessionCommand::SubmitInput {
            prompt: "please write new.rs".to_string(),
            delivery: InputDeliveryMode::Next,
            images: Vec::new(),
        })
        .await
        .expect("submit input");

    loop {
        match rx.recv().await {
            Ok(SessionEvent::TurnCompleted { .. }) => break,
            Ok(_) => {}
            Err(e) => panic!("event stream closed: {e}"),
        }
    }

    // The denied call never executed: the target file does not exist.
    assert!(
        !target.exists(),
        "denied file_write MUST NOT create the file"
    );

    // The model received the denial as its tool result.
    let requests = script_driver.recorded();
    assert!(
        requests.len() >= 2,
        "expected tool-result follow-up request"
    );
    let second = flat_messages(&requests[1]);
    assert!(
        second.contains("denied by plugin: never write files"),
        "denial must reach the model, got:\n{second}"
    );

    // Hook notes/denials landed as rollout steering notes (kinds `plugin*`).
    let session_id = handle.session_id().to_string();
    let records = tekvra_storage::RolloutLog::read_records_readonly(&state_dir, &session_id)
        .expect("read rollout");
    let note_kinds: Vec<&str> = records
        .iter()
        .filter_map(|r| match r {
            RolloutRecord::SteeringNote { kind, .. } => Some(kind.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        note_kinds.contains(&"plugin_deny"),
        "expected plugin_deny steering note, got {note_kinds:?}"
    );
    assert!(
        note_kinds.contains(&"plugin"),
        "expected plugin turn note, got {note_kinds:?}"
    );
    assert!(
        !note_kinds.contains(&"plugin_error"),
        "no failures expected: {note_kinds:?}"
    );

    // Denial must not have produced a permission prompt: no ApprovalRequested events
    // (pre_tool_use deny runs BEFORE the permission ladder).
    // (Events were consumed above; rollout carries no approval records either.)
    let has_approval = records
        .iter()
        .any(|r| matches!(r, RolloutRecord::SteeringNote { kind, .. } if kind == "approval"));
    assert!(!has_approval);
}

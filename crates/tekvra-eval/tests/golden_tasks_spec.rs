//! Golden-task evals: scripted provider drives a real session bootstrap per fixture;
//! the scorer asserts ordered `(tool, arg-substring)` presence plus final file state.
//!
//! Deterministic + hermetic: no network, providers are in-process mocks, workspaces
//! are tempdir copies of this crate's fixtures.

use std::collections::VecDeque;
use std::sync::Mutex;

use tekvra_config::{CliConfigOverrides, ConfigPaths, TrustStore, load_config};
use tekvra_core::{CompactionConfig, SessionBootstrap, spawn_session};
use tekvra_eval::fixtures::stage_fixture;
use tekvra_eval::scoring::{ExpectedToolCall, assert_final_file, score_tool_sequence};
use tekvra_protocol::{InputDeliveryMode, SessionCommand, SessionEvent, TokenUsage};
use tekvra_providers::{
    CompletionRequest, CompletionResponse, MessagePart, MessageRole, ModelCapabilities,
    ModelRouter, ProviderDefinition, ProviderDriver, ProviderError, ProviderMessage,
    ProviderRegistry, StreamDelta,
};
use tempfile::TempDir;

type CompletionHandler =
    dyn Fn(&ModelCapabilities, &CompletionRequest) -> CompletionResponse + Send + Sync;

/// Queue-based scripted driver: pops one response per request and records every outbound
/// request (the tool calls the model made are visible in its assistant messages).
#[derive(Default)]
struct EvalDriver {
    script: Mutex<VecDeque<CompletionResponse>>,
    handler: Option<Box<CompletionHandler>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl EvalDriver {
    fn from_queue(items: Vec<CompletionResponse>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            script: Mutex::new(items.into()),
            handler: None,
            requests: Mutex::new(Vec::new()),
        })
    }

    /// Ordered `(tool, args_json)` pairs the model emitted (from recorded outbound
    /// requests: each assistant ToolCall appears in the NEXT request's history).
    fn observed_tool_calls(&self) -> Vec<(String, String)> {
        let requests = self.requests.lock().expect("lock");
        let mut calls = Vec::new();
        for req in requests.iter() {
            for msg in &req.messages {
                for part in &msg.parts {
                    if let MessagePart::ToolCall {
                        name,
                        arguments_json,
                        ..
                    } = part
                    {
                        calls.push((name.clone(), arguments_json.clone()));
                    }
                }
            }
        }
        calls
    }
}

#[async_trait::async_trait]
impl ProviderDriver for EvalDriver {
    async fn complete(
        &self,
        _provider: &ProviderDefinition,
        model: &ModelCapabilities,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().expect("lock").push(request.clone());
        if let Some(h) = &self.handler {
            return Ok(h(model, request));
        }
        let next = self
            .script
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| text_resp("eval fallback"));
        Ok(next)
    }
}

fn text_resp(text: &str) -> CompletionResponse {
    CompletionResponse {
        assistant_message: ProviderMessage {
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                text: text.to_string(),
                cache_breakpoint: false,
            }],
        },
        stream_deltas: vec![StreamDelta::TextDelta(text.to_string())],
        usage: TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 0,
            output_tokens: 20,
            reasoning_tokens: 0,
            cost_usd: 0.0005,
        },
        served_by_account: "eval-mock".to_string(),
    }
}

fn tool_call_resp(name: &str, args: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        assistant_message: ProviderMessage {
            role: MessageRole::Assistant,
            parts: vec![MessagePart::ToolCall {
                call_id: format!("call_{name}"),
                name: name.to_string(),
                arguments_json: args.to_string(),
            }],
        },
        stream_deltas: Vec::new(),
        usage: TokenUsage {
            input_tokens: 140,
            cached_input_tokens: 0,
            output_tokens: 35,
            reasoning_tokens: 10,
            cost_usd: 0.0012,
        },
        served_by_account: "eval-mock".to_string(),
    }
}

/// Bootstraps a real session against a staged fixture workspace.
fn bootstrap_for(
    base: &TempDir,
    fixture: &str,
    driver: std::sync::Arc<EvalDriver>,
) -> (SessionBootstrap, std::path::PathBuf) {
    let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures").to_string();
    let fixture_dir = std::path::Path::new(&fixture_dir).join(fixture);
    let workspace = stage_fixture(&fixture_dir, base.path()).expect("stage fixture");

    let cfg_dir = base.path().join("cfg");
    let state_dir = base.path().join("state");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let mut trust = TrustStore::load(&state_dir).expect("load trust");
    trust.grant_trust(&workspace).expect("grant trust");

    let resolved = load_config(
        ConfigPaths {
            global_config_dir: cfg_dir,
            state_dir: state_dir.clone(),
            home_dir: Some(workspace.clone()),
        },
        &workspace,
        &CliConfigOverrides::default(),
    )
    .expect("load config");

    let reg = ProviderRegistry::new();
    let router = ModelRouter::from_config(&resolved, reg);

    let bootstrap = SessionBootstrap {
        session_id: None,
        parent_session_id: None,
        workspace_root: workspace.clone(),
        state_dir,
        config: resolved,
        router,
        driver,
        state_index: None,
        compaction: CompactionConfig::default(),
        mcp_gateway: None,
        plugin_executor: None,
        extra_plugin_tools: Vec::new(),
        allow_subagents: false,
        subagent_depth: 0,
    };
    (bootstrap, workspace)
}

async fn run_turn(
    bootstrap: SessionBootstrap,
    driver: &std::sync::Arc<EvalDriver>,
    prompt: &str,
) -> (String, Vec<(String, String)>, tekvra_core::SessionHandle) {
    let handle = spawn_session(bootstrap).expect("spawn eval session");
    let mut rx = handle.subscribe();
    handle
        .send(SessionCommand::SubmitInput {
            prompt: prompt.to_string(),
            delivery: InputDeliveryMode::Next,
            images: Vec::new(),
        })
        .await
        .expect("submit eval prompt");

    let status = loop {
        if let SessionEvent::TurnCompleted { status, .. } = rx.recv().await.expect("event stream") {
            break status;
        }
    };
    (status, driver.observed_tool_calls(), handle)
}

/// Fixture 1: the model writes a file; scorer checks the ordered `file_write` call and
/// the final file state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn golden_write_file_task() {
    let tmp = TempDir::new().expect("tempdir");
    let driver = EvalDriver::from_queue(vec![
        tool_call_resp(
            "file_write",
            serde_json::json!({
                "path": "src/main.rs",
                "content": "fn main() {\n    println!("
            }),
        ),
        text_resp("WROTE_FILE_OK"),
    ]);
    let (bootstrap, workspace) =
        bootstrap_for(&tmp, "write-file-workspace", std::sync::Arc::clone(&driver));
    let (status, observed, _handle) = run_turn(bootstrap, &driver, "rewrite src/main.rs").await;
    assert_eq!(status, "completed");

    let failures = score_tool_sequence(
        &observed,
        &[ExpectedToolCall {
            tool: "file_write",
            args_contain: "src/main.rs",
        }],
    );
    assert!(failures.is_empty(), "tool-sequence failures: {failures:?}");
    assert_final_file(&workspace.join("src/main.rs"), "println!");
}

/// Fixture 2: the model greps for a marker; scorer checks the ordered `file_grep` call
/// and that the workspace stays untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn golden_grep_task() {
    let tmp = TempDir::new().expect("tempdir");
    let driver = EvalDriver::from_queue(vec![
        tool_call_resp(
            "file_grep",
            serde_json::json!({ "pattern": "NEEDLE_MARKER", "path": "src" }),
        ),
        text_resp("GREP_DONE_OK"),
    ]);
    let (bootstrap, workspace) =
        bootstrap_for(&tmp, "grep-workspace", std::sync::Arc::clone(&driver));
    let (status, observed, _handle) = run_turn(bootstrap, &driver, "find NEEDLE_MARKER").await;
    assert_eq!(status, "completed");

    let failures = score_tool_sequence(
        &observed,
        &[ExpectedToolCall {
            tool: "file_grep",
            args_contain: "NEEDLE_MARKER",
        }],
    );
    assert!(failures.is_empty(), "tool-sequence failures: {failures:?}");
    // Final state: grep must not mutate any fixture file.
    let alpha =
        std::fs::read_to_string(workspace.join("src/alpha.rs")).expect("read alpha fixture");
    assert_eq!(
        alpha,
        "/// fixture module for grep task\npub fn alpha() -> u32 {\n    NEEDLE_MARKER\n}\n"
    );
}

// Final-state note: the fixture content assertions above double as regression guards —
// the eval gate fails if a tool implementation starts corrupting fixture files.

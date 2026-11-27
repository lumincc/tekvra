//! Specification tests for `tekvra-core` (`agent-core-and-sessions` Tasks 5.1, 5.2, 5.3).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tekvra_config::{CliConfigOverrides, ConfigPaths, TrustStore, load_config};
use tekvra_core::{
    CompactionConfig, Frame, ReviewTarget, SessionBootstrap, run_code_review, spawn_session,
};
use tekvra_protocol::{
    InputDeliveryMode, PermissionMode, SessionCommand, SessionEvent, TokenUsage,
};
use tekvra_providers::{
    CompletionRequest, CompletionResponse, MessagePart, MessageRole, ModelCapabilities,
    ModelRouter, ProviderDefinition, ProviderDriver, ProviderError, ProviderMessage,
    ProviderRegistry, StreamDelta,
};
use tempfile::TempDir;
use tokio::process::Command;

type CompletionHandler = dyn Fn(&ProviderDefinition, &ModelCapabilities, &CompletionRequest) -> CompletionResponse
    + Send
    + Sync;

#[derive(Default)]
struct ScriptedDriver {
    requests: Mutex<Vec<(String, CompletionRequest)>>,
    handler: Option<Box<CompletionHandler>>,
    script: Mutex<VecDeque<CompletionResponse>>,
}

impl ScriptedDriver {
    fn from_queue(items: Vec<CompletionResponse>) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            handler: None,
            script: Mutex::new(items.into()),
        })
    }

    fn from_fn(
        f: impl Fn(&ProviderDefinition, &ModelCapabilities, &CompletionRequest) -> CompletionResponse
        + Send
        + Sync
        + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            handler: Some(Box::new(f)),
            script: Mutex::new(VecDeque::new()),
        })
    }

    fn recorded_requests(&self) -> Vec<(String, CompletionRequest)> {
        self.requests.lock().expect("lock").clone()
    }
}

#[async_trait]
impl ProviderDriver for ScriptedDriver {
    async fn complete(
        &self,
        provider: &ProviderDefinition,
        model: &ModelCapabilities,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests
            .lock()
            .expect("lock")
            .push((model.spec.clone(), request.clone()));
        if let Some(h) = &self.handler {
            return Ok(h(provider, model, request));
        }
        let next = self
            .script
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| text_resp("fallback ok"));
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
            input_tokens: 120,
            cached_input_tokens: 100,
            output_tokens: 30,
            reasoning_tokens: 0,
            cost_usd: 0.001,
        },
        served_by_account: "mock".to_string(),
    }
}

fn tool_call_resp(call_id: &str, name: &str, args: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        assistant_message: ProviderMessage {
            role: MessageRole::Assistant,
            parts: vec![
                MessagePart::Reasoning {
                    id: format!("rs_{call_id}"),
                    summary: vec!["planning tool call".to_string()],
                    encrypted_content: Some(format!("ENC_BLOB_{call_id}")),
                },
                MessagePart::ToolCall {
                    call_id: call_id.to_string(),
                    name: name.to_string(),
                    arguments_json: args.to_string(),
                },
            ],
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

fn setup_bootstrap(
    tmp: &TempDir,
    driver: Arc<dyn ProviderDriver>,
    mode: PermissionMode,
) -> SessionBootstrap {
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
    resolved.settings.agent.default_mode = mode;

    let mut reg = ProviderRegistry::new();
    reg.apply_custom_configs(&resolved.providers.providers);
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
        compaction: CompactionConfig::default(),
        mcp_gateway: None,
        plugin_executor: None,
        extra_plugin_tools: Vec::new(),
        allow_subagents: true,
    }
}

async fn wait_for_turn_complete(rx: &mut tokio::sync::broadcast::Receiver<SessionEvent>) -> String {
    loop {
        if let Ok(SessionEvent::TurnCompleted { status, .. }) = rx.recv().await {
            return status;
        }
    }
}

#[tokio::test]
async fn append_only_frame_plan_to_build_prefix_stability_compaction_and_non_destructive_guide() {
    // 1. Direct `Frame::assert_append_only` check + mutation detection
    let mut frame = Frame::new("Developer base prompt", Vec::new(), true);
    frame.push_message(ProviderMessage::user_text("Turn 1 prompt"));
    let snap1 = frame.snapshot();
    frame.append_mode_transition(PermissionMode::Plan, PermissionMode::Build, "- [ ] Step 1");
    frame
        .assert_append_only(&snap1)
        .expect("mode transition appends trailing reminder without mutating prior prefix");

    // 2. End-to-end `SessionHandle`: start in `Plan` mode, record plan, switch to `Build`,
    // and inject a `Guide` input mid-tool-execution (`exec_command` sleeping 200ms) without cancelling!
    let tmp = TempDir::new().expect("tempdir");
    let driver = ScriptedDriver::from_queue(vec![
        // Turn 1 (`Plan` mode): call `plan` tool then conclude
        tool_call_resp(
            "c_plan",
            "plan",
            json!({ "markdown": "1. Implement feature" }),
        ),
        text_resp("Plan drafted."),
        // Turn 2 (`Build` mode -> `Auto`-allowed read/slow command): run `file_read` / slow `exec_command`
        tool_call_resp(
            "c_slow",
            "exec_command",
            json!({ "cmd": "sleep 0.25 && echo IN_FLIGHT_TOOL_COMPLETED", "yield_time_ms": 2000 }),
        ),
        text_resp("Incorporated guide instruction after tool finished."),
    ]);

    let mut bootstrap = setup_bootstrap(&tmp, driver.clone(), PermissionMode::Plan);
    // Pre-authorize `sleep` in permissions so `Build` mode runs it without blocking on prompt
    bootstrap
        .config
        .permission_rules
        .push(tekvra_config::PermissionRule {
            tool: "exec_command".to_string(),
            pattern: "sleep*".to_string(),
            action: tekvra_protocol::RuleAction::Allow,
        });
    bootstrap
        .config
        .permission_rules
        .push(tekvra_config::PermissionRule {
            tool: "exec_command".to_string(),
            pattern: "echo*".to_string(),
            action: tekvra_protocol::RuleAction::Allow,
        });

    let handle = spawn_session(bootstrap).expect("spawn session");
    let mut rx = handle.subscribe();

    // Run Turn 1 in `Plan` mode
    handle
        .send(SessionCommand::SubmitInput {
            prompt: "Draft a plan".to_string(),
            delivery: InputDeliveryMode::Next,
        })
        .await
        .expect("send turn 1");
    assert_eq!(wait_for_turn_complete(&mut rx).await, "completed");

    // Switch `Plan` -> `Build` mid-session
    handle
        .send(SessionCommand::SetMode {
            mode: PermissionMode::Build,
        })
        .await
        .expect("set Build mode");

    // Start Turn 2 (which launches a 250ms PTY command) and send `Guide` while the tool is running!
    handle
        .send(SessionCommand::SubmitInput {
            prompt: "Run slow check".to_string(),
            delivery: InputDeliveryMode::Next,
        })
        .await
        .expect("send turn 2");

    // Wait until `c_slow` tool_call starts, then inject `Guide`!
    loop {
        if let Ok(SessionEvent::ItemStarted { kind, item_id, .. }) = rx.recv().await
            && kind == "tool_call"
            && item_id == "c_slow"
        {
            break;
        }
    }

    handle
        .send(SessionCommand::SubmitInput {
            prompt: "GUIDE_NOTE: prefer BTreeMap over HashMap".to_string(),
            delivery: InputDeliveryMode::Guide,
        })
        .await
        .expect("send Guide mid-tool");

    assert_eq!(wait_for_turn_complete(&mut rx).await, "completed");

    // Inspect outbound provider requests:
    // Every request in Turn 2 MUST retain the exact byte-identical prefix from Turn 1 (`Developer` + `Draft a plan` + `rs_c_plan` w/ `ENC_BLOB_c_plan`!),
    // the slow tool MUST have completed (`IN_FLIGHT_TOOL_COMPLETED`), AND `GUIDE_NOTE` MUST appear right before the final request!
    let reqs = driver.recorded_requests();
    assert_eq!(reqs.len(), 4);
    let req1_msgs = &reqs[1].1.messages; // End of Turn 1
    let req3_msgs = &reqs[3].1.messages; // Final step of Turn 2 after Guide

    for (idx, earlier_msg) in req1_msgs.iter().enumerate() {
        assert_eq!(
            &req3_msgs[idx], earlier_msg,
            "message prefix at index {idx} must remain byte-identical across Plan->Build and Guide!"
        );
    }

    let serialized_req3 = serde_json::to_string(req3_msgs).expect("json");
    assert!(
        serialized_req3.contains("ENC_BLOB_c_plan"),
        "stateless encrypted reasoning must survive verbatim"
    );
    assert!(
        serialized_req3.contains("IN_FLIGHT_TOOL_COMPLETED"),
        "Guide must NOT cancel the in-flight tool execution"
    );
    assert!(
        serialized_req3.contains("GUIDE_NOTE: prefer BTreeMap over HashMap"),
        "Guide input must be injected before the next provider call"
    );
}

#[tokio::test]
async fn automatic_steering_abandoned_turn_three_strike_refusal_and_idle_parent_subagent_wakeup() {
    let tmp = TempDir::new().expect("tempdir");

    // Part A: `abandoned_turn` nudge + 3-strike identical failing tool call refusal
    let driver_a = ScriptedDriver::from_queue(vec![
        // Step 1: Model announces prose-only intent without calling a tool -> triggers `abandoned_turn`!
        text_resp("I will now read the missing file to check its contents."),
        // Step 2 (after `abandoned_turn` nudge): Model calls `file_read` on nonexistent file #1 -> fails (strike 1)
        tool_call_resp("f1", "file_read", json!({ "path": "does_not_exist.rs" })),
        // Step 3: Model calls exact same `file_read` #2 -> fails (strike 2)
        tool_call_resp("f2", "file_read", json!({ "path": "does_not_exist.rs" })),
        // Step 4: Model calls exact same `file_read` #3 -> REFUSED before execution (`repeated_failing_tool`)!
        tool_call_resp("f3", "file_read", json!({ "path": "does_not_exist.rs" })),
        // Step 5: Model adapts and finishes cleanly
        text_resp("Diagnosed that `does_not_exist.rs` is absent; no further retries needed."),
    ]);

    let handle_a = spawn_session(setup_bootstrap(&tmp, driver_a, PermissionMode::Auto))
        .expect("spawn session A");
    let mut rx_a = handle_a.subscribe();

    handle_a
        .send(SessionCommand::SubmitInput {
            prompt: "Inspect does_not_exist.rs".to_string(),
            delivery: InputDeliveryMode::Next,
        })
        .await
        .expect("submit");

    let mut steering_reasons = Vec::new();
    loop {
        match rx_a.recv().await.expect("event") {
            SessionEvent::SteeringInjected { reason, .. } => steering_reasons.push(reason),
            SessionEvent::TurnCompleted { status, .. } => {
                assert_eq!(status, "completed");
                break;
            }
            _ => {}
        }
    }
    assert!(
        steering_reasons.iter().any(|r| r == "abandoned_turn"),
        "expected abandoned_turn steering nudge, got {steering_reasons:?}"
    );
    assert!(
        steering_reasons
            .iter()
            .any(|r| r == "repeated_failing_tool"),
        "expected repeated_failing_tool 3rd-strike refusal, got {steering_reasons:?}"
    );

    // Part B: Background `task` subagent completes AFTER parent goes idle -> wakes parent via push channel!
    let tmp_b = TempDir::new().expect("tempdir b");
    let driver_b = ScriptedDriver::from_fn(|_prov, _model, req| {
        let joined = serde_json::to_string(&req.messages).unwrap_or_default();
        if req.conversation_id.starts_with("task_") {
            // Subagent work
            std::thread::sleep(Duration::from_millis(120));
            return text_resp("SUBAGENT_AUDIT_RESULT_OK");
        }
        if joined.contains("SUBAGENT_AUDIT_RESULT_OK") {
            return text_resp("Parent received background subagent audit result!");
        }
        if joined.contains("Background subagent `task_") && joined.contains("started") {
            return text_resp("Waiting quietly while background subagent works.");
        }
        tool_call_resp(
            "spawn_bg",
            "task",
            json!({
                "prompt": "Audit module safety",
                "role": "fast",
                "background": true
            }),
        )
    });

    let handle_b = spawn_session(setup_bootstrap(&tmp_b, driver_b, PermissionMode::Auto))
        .expect("spawn session B");
    let mut rx_b = handle_b.subscribe();

    handle_b
        .send(SessionCommand::SubmitInput {
            prompt: "Launch background audit".to_string(),
            delivery: InputDeliveryMode::Next,
        })
        .await
        .expect("submit bg");

    // 1st TurnCompleted: parent finishes initial turn and goes IDLE while background subagent is still running
    assert_eq!(wait_for_turn_complete(&mut rx_b).await, "completed");

    // Without sending any new user command, parent MUST wake up when the background subagent finishes
    // and complete a wakeup turn mentioning `Parent received background subagent audit result!`!
    let mut saw_wakeup_message = false;
    let wakeup_res = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx_b.recv().await.expect("wakeup event") {
                SessionEvent::ItemCompleted { text, .. }
                    if text.contains("Parent received background subagent audit result!") =>
                {
                    saw_wakeup_message = true;
                }
                SessionEvent::TurnCompleted { status, .. } if saw_wakeup_message => {
                    return status;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("idle parent woke up without polling");
    assert_eq!(wakeup_res, "completed");

    // Verify subagent transcript is persisted on disk and reloadable!
    let mgr = handle_b.subagents().expect("subagent manager");
    let tasks = mgr.list();
    assert_eq!(tasks.len(), 1);
    let persisted = mgr
        .read_persisted_transcript(&tasks[0].task_id)
        .expect("reload subagent rollout");
    assert!(persisted.len() >= 3);
}

#[tokio::test]
async fn goal_evaluator_rejects_unverified_test_claim_and_review_extracts_structured_git_findings()
{
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    std::fs::create_dir_all(ws.join("src")).expect("mkdir");
    std::fs::write(
        ws.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a - b // BUG: subtracts instead of adding\n}\n",
    )
    .expect("write lib.rs");

    // Initialize a temporary Git repository & commit baseline, then introduce a buggy diff on a feature branch
    for args in [
        &["init"][..],
        &["config", "user.email", "dev@tekvra.local"][..],
        &["config", "user.name", "Tekvra Test"][..],
        &["checkout", "-b", "main"][..],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(&ws)
            .status()
            .await
            .expect("git init");
        assert!(status.success());
    }
    std::fs::write(
        ws.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .expect("write clean");
    assert!(
        Command::new("git")
            .args(["add", "."])
            .current_dir(&ws)
            .status()
            .await
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&ws)
            .status()
            .await
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["checkout", "-b", "feat/buggy"])
            .current_dir(&ws)
            .status()
            .await
            .unwrap()
            .success()
    );
    std::fs::write(
        ws.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
    )
    .expect("write buggy");
    assert!(
        Command::new("git")
            .args(["commit", "-am", "introduce bug"])
            .current_dir(&ws)
            .status()
            .await
            .unwrap()
            .success()
    );

    // 1. `/goal` test: Turn step 1 claims "All tests pass!" with NO tool execution -> `/goal` evaluator returns `met: false`;
    //    Step 2 actually executes `echo CARGO_TEST_ALL_PASSED` -> `/goal` evaluator sees tool output evidence and returns `met: true`!
    let goal_driver = ScriptedDriver::from_fn(|_prov, _model, req| {
        let body = serde_json::to_string(&req.messages).unwrap_or_default();
        if req.conversation_id.ends_with(":goal") {
            assert!(
                req.tools.is_empty(),
                "`/goal` evaluator MUST be strictly tool-less (`tools: []`)"
            );
            if body.contains("CARGO_TEST_ALL_PASSED") {
                return text_resp(
                    r#"{"met": true, "feedback": "Verified test output in transcript."}"#,
                );
            }
            return text_resp(
                r#"{"met": false, "feedback": "No test execution output found in transcript; run the test command."}"#,
            );
        }
        if body.contains("No test execution output found in transcript") {
            if body.contains("CARGO_TEST_ALL_PASSED") {
                return text_resp("Tests executed and verified.");
            }
            return tool_call_resp(
                "run_t",
                "exec_command",
                json!({ "cmd": "echo CARGO_TEST_ALL_PASSED", "yield_time_ms": 1000 }),
            );
        }
        // First response: unverified prose claim without running any command!
        text_resp("All tests pass!")
    });

    let bootstrap = setup_bootstrap(&tmp, goal_driver, PermissionMode::Auto);
    let review_router = bootstrap.router.clone();
    let review_settings = bootstrap.config.settings.agent.clone();
    let handle = spawn_session(bootstrap).expect("spawn goal session");
    let mut rx = handle.subscribe();

    handle
        .send(SessionCommand::SetGoal {
            condition: Some("All tests pass with verified test command output".to_string()),
        })
        .await
        .expect("set goal");
    handle
        .send(SessionCommand::SubmitInput {
            prompt: "Finish the feature and ensure tests pass".to_string(),
            delivery: InputDeliveryMode::Next,
        })
        .await
        .expect("submit");

    let mut saw_goal_unmet = false;
    loop {
        match rx.recv().await.expect("ev") {
            SessionEvent::SteeringInjected { reason, .. } if reason == "goal_unmet" => {
                saw_goal_unmet = true;
            }
            SessionEvent::TurnCompleted { status, .. } => {
                assert_eq!(status, "completed");
                break;
            }
            _ => {}
        }
    }
    assert!(
        saw_goal_unmet,
        "unverified prose claim must be rejected by tool-less `/goal` evaluator until test output exists"
    );

    // 2. Read-only `/review` against `BaseBranch { branch: "main" }`
    let review_driver = ScriptedDriver::from_fn(|_prov, _model, req| {
        let body = serde_json::to_string(&req.messages).unwrap_or_default();
        assert!(
            body.contains("a - b"),
            "collected Git branch diff must include `a - b`"
        );
        // Ensure mutating tools (`apply_patch`, `file_edit`, `file_write`, `exec_command`) are NOT exposed in `/review`!
        for t in &req.tools {
            assert!(
                matches!(
                    t.name.as_str(),
                    "file_read" | "file_grep" | "file_glob" | "file_index"
                ),
                "unexpected mutating tool `{}` in read-only `/review`",
                t.name
            );
        }
        text_resp(
            r#"{
                "summary": "Found arithmetic regression in add().",
                "findings": [
                    {
                        "severity": "high",
                        "path": "src/lib.rs",
                        "start_line": 2,
                        "end_line": 2,
                        "title": "Subtraction used instead of addition",
                        "body": "`a - b` reverses the sign of `b`; restore `a + b`."
                    }
                ]
            }"#,
        )
    });

    let outcome = run_code_review(
        "thr_rev_01",
        &ws,
        &ReviewTarget::BaseBranch {
            branch: "main".to_string(),
        },
        &review_router,
        review_driver.as_ref(),
        &review_settings,
    )
    .await
    .expect("run_code_review");

    assert_eq!(outcome.findings.len(), 1);
    assert_eq!(outcome.findings[0].severity, "high");
    assert_eq!(outcome.findings[0].path, PathBuf::from("src/lib.rs"));
    assert_eq!(outcome.findings[0].start_line, 2);
}

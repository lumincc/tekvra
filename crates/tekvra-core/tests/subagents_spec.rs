//! Specification integration tests for worktree-isolated `task` subagents
//! (spawn → isolated checkout → merge-back / rejection outside Git repositories).

use std::process::Command as StdCommand;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tekvra_config::{CliConfigOverrides, ConfigPaths, TrustStore, load_config};
use tekvra_core::{CoreError, SessionBootstrap, SubagentSpawnRequest, spawn_session};
use tekvra_protocol::{PermissionMode, SessionEvent, TokenUsage};
use tekvra_providers::{
    CompletionRequest, CompletionResponse, MessagePart, MessageRole, ModelCapabilities,
    ModelRouter, ProviderDefinition, ProviderDriver, ProviderError, ProviderRegistry, StreamDelta,
};
use tempfile::TempDir;

fn git(cwd: &std::path::Path, args: &[&str]) {
    let out = StdCommand::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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

/// Driver serving exactly one scripted completion, then falling back to plain text.
struct OneShotDriver {
    script: Mutex<std::collections::VecDeque<CompletionResponse>>,
}

#[async_trait]
impl ProviderDriver for OneShotDriver {
    async fn complete(
        &self,
        _provider: &ProviderDefinition,
        _model: &ModelCapabilities,
        _request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let next = self
            .script
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| text_resp("fallback ok"));
        Ok(next)
    }
}

fn setup_bootstrap(tmp: &TempDir, git_init: bool) -> SessionBootstrap {
    let ws = tmp.path().join("repo");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&ws).expect("mkdir ws");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");

    if git_init {
        git(&ws, &["init", "-q", "-b", "main"]);
        git(&ws, &["config", "user.email", "t@tekvra.local"]);
        git(&ws, &["config", "user.name", "Tekvra Test"]);
        // Windows runners default to `core.autocrlf=true`, which would rewrite merged
        // file contents to CRLF and break byte-exact assertions.
        git(&ws, &["config", "core.autocrlf", "false"]);
        std::fs::write(ws.join("lib.rs"), "fn a() {}\n").expect("seed file");
        git(&ws, &["add", "-A"]);
        git(&ws, &["commit", "-q", "-m", "init"]);
    }

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
        driver: Arc::new(OneShotDriver {
            script: Mutex::new(vec![text_resp("subagent finished the work")].into()),
        }),
        state_index: None,
        compaction: tekvra_core::CompactionConfig::default(),
        mcp_gateway: None,
        plugin_executor: None,
        extra_plugin_tools: Vec::new(),
        allow_subagents: true,
        subagent_depth: 0,
    }
}

async fn wait_for_turn_complete(rx: &mut tokio::sync::broadcast::Receiver<SessionEvent>) {
    loop {
        if let Ok(SessionEvent::TurnCompleted { .. }) = rx.recv().await {
            return;
        }
    }
}

#[tokio::test]
async fn worktree_task_lifecycle_reports_branch_merges_and_cleans_up() {
    let tmp = TempDir::new().expect("tempdir");
    let bootstrap = setup_bootstrap(&tmp, true);

    let handle = spawn_session(bootstrap).expect("spawn parent session");
    let mgr = handle.subagents().expect("subagent manager").clone();
    let parent_root = parent_workspace(&tmp);

    // Spawn a foreground worktree-isolated task.
    let card = mgr
        .spawn(SubagentSpawnRequest {
            prompt: "do isolated work".to_string(),
            role: None,
            model_override: None,
            background: false,
            worktree: true,
        })
        .await
        .expect("spawn worktree task");
    let tekvra_protocol::ToolOutput::SubagentCard {
        task_id,
        status,
        worktree_branch,
        diff_summary,
        ..
    } = card
    else {
        panic!("expected SubagentCard, got {card:?}");
    };
    assert_eq!(status, "completed");
    let branch = worktree_branch.expect("worktree branch reported");
    assert!(branch.starts_with("tekvra/task_"), "{branch}");
    assert!(diff_summary.is_some());

    // The child worktree checkout exists on disk, on its own branch.
    let entry = mgr
        .list()
        .into_iter()
        .find(|e| e.task_id == task_id)
        .expect("task entry");
    let wt_path = entry.worktree_path.clone().expect("worktree path");
    assert!(wt_path.join(".git").exists());
    assert_eq!(entry.depth, 1);

    // Parent tree untouched before merge-back.
    assert!(!parent_root.join("feature.rs").exists());

    // The task's "work": a file written directly into the worktree checkout.
    std::fs::write(wt_path.join("feature.rs"), "pub fn f() {}\n").expect("task work");
    std::fs::write(wt_path.join("lib.rs"), "fn edited() {}\n").expect("task edit");

    let merged = mgr.merge_task(&task_id).await.expect("merge back");
    let tekvra_protocol::ToolOutput::SubagentCard {
        status: merged_status,
        summary,
        ..
    } = merged
    else {
        panic!("expected merged SubagentCard");
    };
    assert_eq!(merged_status, "merged");
    assert!(summary.contains("Merged"), "{summary}");

    // Parent tree now contains the task's committed work; worktree is gone.
    assert!(
        parent_root.join("feature.rs").exists(),
        "merged feature file must land in parent tree"
    );
    let lib = std::fs::read_to_string(parent_root.join("lib.rs")).expect("read merged lib.rs");
    assert_eq!(lib, "fn edited() {}\n");
    assert!(!wt_path.exists(), "worktree removed after merge");
}

/// Rebuilds the manager-independent parent workspace root for assertions.
fn parent_workspace(tmp: &TempDir) -> std::path::PathBuf {
    tmp.path().join("repo")
}

#[tokio::test]
async fn worktree_task_rejected_outside_git_repository() {
    let tmp = TempDir::new().expect("tempdir");
    let bootstrap = setup_bootstrap(&tmp, false);
    let parent_root = parent_workspace(&tmp);
    let state_dir = bootstrap.state_dir.clone();
    let handle = spawn_session(bootstrap).expect("spawn parent session");
    let mgr = handle.subagents().expect("subagent manager").clone();

    let err = mgr
        .spawn(SubagentSpawnRequest {
            prompt: "isolate me".to_string(),
            role: None,
            model_override: None,
            background: false,
            worktree: true,
        })
        .await
        .expect_err("must reject outside a git repository");
    let CoreError::Subagent { message } = err else {
        panic!("expected CoreError::Subagent, got {err:?}");
    };
    assert!(message.contains("Git repository"), "{message}");
    // No worktree directory was created under the state dir.
    let wt_root = state_dir.join("worktrees");
    assert!(
        !wt_root.exists(),
        "no worktrees dir must be created: {:?}",
        wt_root
    );
    assert!(!parent_root.join(".git").exists());
}

#[tokio::test]
async fn depth_budget_gates_nested_subagent_manager() {
    let tmp = TempDir::new().expect("tempdir");
    let mut bootstrap = setup_bootstrap(&tmp, true);
    bootstrap.config.settings.agent.subagent_max_depth = 1;

    let handle = spawn_session(bootstrap).expect("spawn parent session");
    assert!(
        handle.subagents().is_some(),
        "top-level session manages subagents"
    );
    let mut rx = handle.subscribe();
    let mgr = handle.subagents().expect("subagent manager").clone();

    // Depth-1 child (parent at depth 0, max 1): allowed to spawn, but the child
    // itself must NOT get a subagent manager (budget exhausted at depth 1).
    let card = mgr
        .spawn(SubagentSpawnRequest {
            prompt: "level one".to_string(),
            role: None,
            model_override: None,
            background: true,
            worktree: false,
        })
        .await
        .expect("spawn depth-1 task");
    let tekvra_protocol::ToolOutput::SubagentCard { task_id, .. } = card else {
        panic!("expected SubagentCard");
    };
    wait_for_turn_complete(&mut rx).await;
    let entry = mgr
        .list()
        .into_iter()
        .find(|e| e.task_id == task_id)
        .expect("entry");
    assert_eq!(entry.depth, 1);
    assert_eq!(entry.status, "completed");
}

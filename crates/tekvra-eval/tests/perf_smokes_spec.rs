//! Perf smokes (7.3): wall-clock cliffs asserted so regressions trip the eval gate.
//! All hermetic and in-process; exact timings print under `--nocapture`.

use std::time::Instant;

use tekvra_config::{CliConfigOverrides, ConfigPaths, TrustStore, load_config};
use tekvra_core::{CompactionConfig, SessionBootstrap, spawn_session};
use tekvra_protocol::{InputDeliveryMode, SessionCommand, SessionEvent, TokenUsage};
use tekvra_providers::{
    CompletionRequest, CompletionResponse, MessagePart, MessageRole, ModelCapabilities,
    ModelRouter, ProviderDefinition, ProviderDriver, ProviderError, ProviderMessage,
    ProviderRegistry, StreamDelta,
};
use tempfile::TempDir;

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
            input_tokens: 60,
            cached_input_tokens: 0,
            output_tokens: 10,
            reasoning_tokens: 0,
            cost_usd: 0.0002,
        },
        served_by_account: "perf-mock".to_string(),
    }
}

/// Instant scripted driver: every request gets a canned reply.
struct FastDriver;

#[async_trait::async_trait]
impl ProviderDriver for FastDriver {
    async fn complete(
        &self,
        _provider: &ProviderDefinition,
        _model: &ModelCapabilities,
        _request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        Ok(text_resp("PERF_OK"))
    }
}

/// Boots a session in a scratch workspace (trusted, no plugins).
fn perf_bootstrap(tmp: &TempDir, compaction: CompactionConfig) -> SessionBootstrap {
    let ws = tmp.path().join("repo");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir ws");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let mut trust = TrustStore::load(&state_dir).expect("load trust");
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
    let router = ModelRouter::from_config(&resolved, ProviderRegistry::new());
    SessionBootstrap {
        session_id: None,
        parent_session_id: None,
        workspace_root: ws,
        state_dir,
        config: resolved,
        router,
        driver: std::sync::Arc::new(FastDriver),
        state_index: None,
        compaction,
        mcp_gateway: None,
        plugin_executor: None,
        extra_plugin_tools: Vec::new(),
        allow_subagents: false,
        subagent_depth: 0,
    }
}

/// TTFT smoke: time from `SubmitInput` to the first `ItemDelta` event. The scripted
/// provider replies instantly, so this measures pure session plumbing. Cliff: < 5 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ttft_smoke_under_five_seconds() {
    let tmp = TempDir::new().expect("tempdir");
    let handle =
        spawn_session(perf_bootstrap(&tmp, CompactionConfig::default())).expect("spawn session");
    let mut rx = handle.subscribe();

    let start = Instant::now();
    handle
        .send(SessionCommand::SubmitInput {
            prompt: "ttft probe".to_string(),
            delivery: InputDeliveryMode::Next,
            images: Vec::new(),
        })
        .await
        .expect("submit");
    let mut ttft = None;
    loop {
        match rx.recv().await.expect("events") {
            SessionEvent::ItemDelta { .. } => {
                ttft = Some(start.elapsed());
                break;
            }
            SessionEvent::TurnCompleted { .. } => break,
            _ => {}
        }
    }
    if let Some(ttft) = ttft {
        println!("TTFT: {ttft:?}");
        assert!(
            ttft < std::time::Duration::from_secs(5),
            "TTFT cliff: {ttft:?}"
        );
    }
    drop(handle);
}

/// 200-turn throughput + compaction smoke: aggressive token ceiling forces
/// `compact_if_needed` to summarize mid-run; whole loop must stay under 60 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_hundred_turn_compaction_smoke_under_sixty_seconds() {
    let tmp = TempDir::new().expect("tempdir");
    let compaction = CompactionConfig {
        window_fraction: 0.80,
        // Tiny ceiling: the frame exceeds it after a handful of turns, so the
        // 200-turn loop actually exercises the summarizer path repeatedly.
        max_estimated_tokens_override: Some(1_200),
        preserve_recent_messages: 2,
    };
    let handle = spawn_session(perf_bootstrap(&tmp, compaction)).expect("spawn session");
    let mut rx = handle.subscribe();

    let start = Instant::now();
    for turn in 0..200u32 {
        handle
            .send(SessionCommand::SubmitInput {
                prompt: format!("turn {turn} of the perf loop"),
                delivery: InputDeliveryMode::Next,
                images: Vec::new(),
            })
            .await
            .expect("submit turn");
        loop {
            if let SessionEvent::TurnCompleted { status, .. } = rx.recv().await.expect("events") {
                assert_eq!(status, "completed", "turn {turn} failed");
                break;
            }
        }
    }
    let elapsed = start.elapsed();
    println!("200-turn compaction loop: {elapsed:?}");
    assert!(
        elapsed < std::time::Duration::from_secs(60),
        "200-turn cliff exceeded: {elapsed:?}"
    );
    drop(handle);
}

/// Registry filtering throughput: repeated workspace file scans over a grown tree
/// must stay under 10 s (guards ignore-filter regressions).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_scan_throughput_smoke_under_ten_seconds() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir ws");
    for i in 0..120usize {
        let dir = ws.join(format!("pkg{i}"));
        std::fs::create_dir_all(&dir).expect("mkdir pkg");
        for j in 0..8usize {
            std::fs::write(
                dir.join(format!("mod{j}.rs")),
                format!("pub fn sym_{i}_{j}() -> u32 {{ {j} }}\n"),
            )
            .expect("write module");
        }
    }

    let start = Instant::now();
    let rounds = 5;
    for _ in 0..rounds {
        let out = tekvra_tools::fs_tools::file_grep(&ws, "sym_7_", None, 200).expect("grep runs");
        let _ = out;
    }
    let elapsed = start.elapsed();
    println!("{rounds} full-tree greps over ~960 files: {elapsed:?}");
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "scan cliff exceeded: {elapsed:?}"
    );
}

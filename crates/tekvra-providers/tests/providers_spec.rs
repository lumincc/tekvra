//! Specification integration tests for `tekvra-providers` (`providers-and-model-routing`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::{Value, json};
use tekvra_config::{CliConfigOverrides, ConfigPaths, load_config};
use tekvra_protocol::PermissionMode;
use tekvra_providers::{
    AccountCandidate, AccountPin, AccountPool, CompletionRequest, CredentialStore,
    HttpProviderDriver, JobRole, LoopbackOAuthListener, MessagePart, MessageRole, ModelRouter,
    ProviderDefinition, ProviderDriver, ProviderMessage, ProviderRegistry, SecretToken,
    ToolDefinition, WireProtocol, build_anthropic_messages_payload, build_chat_completions_payload,
    build_gemini_payload, build_openai_responses_payload, exchange_oauth_token, generate_pkce_pair,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> (String, Value) {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).await.expect("read request chunk");
        assert!(n > 0, "unexpected EOF reading HTTP request");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers_str = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_len = headers_str
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_len {
        let n = stream.read(&mut chunk).await.expect("read body chunk");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body_bytes = &buf[header_end..header_end + content_len];
    let json_body: Value = serde_json::from_slice(body_bytes).unwrap_or(Value::Null);
    (headers_str, json_body)
}

#[test]
fn builtin_catalog_covers_all_required_providers_and_editor_routing() {
    let reg = ProviderRegistry::new();
    for expected in [
        "openai",
        "anthropic",
        "gemini",
        "github-copilot",
        "xai",
        "deepseek",
        "mistral",
        "zai",
        "openrouter",
        "ollama",
        "llamacpp",
    ] {
        assert!(
            reg.get(expected).is_some(),
            "missing built-in provider `{expected}`"
        );
    }

    let (_, codex_caps) = reg
        .resolve_model_spec("openai/gpt-5.4-codex")
        .expect("resolve codex");
    assert_eq!(codex_caps.protocol, WireProtocol::OpenAiResponses);
    assert!(codex_caps.uses_apply_patch);
    assert!(codex_caps.supports_encrypted_reasoning);
    assert_eq!(
        codex_caps
            .align_reasoning_effort(Some("xhigh"))
            .expect("xhigh valid"),
        Some("xhigh".to_string())
    );
    assert!(codex_caps.align_reasoning_effort(Some("ultra")).is_err());

    let (_, claude_caps) = reg
        .resolve_model_spec("anthropic/claude-sonnet-4-5")
        .expect("resolve claude");
    assert_eq!(claude_caps.protocol, WireProtocol::AnthropicMessages);
    assert!(!claude_caps.uses_apply_patch);
}

#[tokio::test]
async fn stateless_openai_responses_replays_encrypted_reasoning_and_preserves_cache_prefix() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("local_addr");
    let captured: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_srv = Arc::clone(&captured);

    let server_task = tokio::spawn(async move {
        // Turn 1: return SSE stream with encrypted reasoning + tool call
        let (mut s1, _) = listener.accept().await.expect("accept 1");
        let req1 = read_http_request(&mut s1).await;
        captured_srv.lock().expect("lock").push(req1);
        let sse_body = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"Checking Cargo.toml\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{",
            "\"output\":[",
            "{\"type\":\"reasoning\",\"id\":\"rs_turn1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Checking Cargo.toml\"}],\"encrypted_content\":\"ENC_BLOB_V1\"},",
            "{\"type\":\"function_call\",\"call_id\":\"call_read1\",\"name\":\"file_read\",\"arguments\":\"{\\\"path\\\":\\\"Cargo.toml\\\"}\"}",
            "],",
            "\"usage\":{\"input_tokens\":1000,\"input_tokens_details\":{\"cached_tokens\":900},\"output_tokens\":50,\"output_tokens_details\":{\"reasoning_tokens\":20}}",
            "}}\n\n"
        );
        let http_resp1 = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            sse_body.len(),
            sse_body
        );
        s1.write_all(http_resp1.as_bytes()).await.expect("write 1");
        drop(s1);

        // Turn 2: verify encrypted reasoning + tool output replayed, return final answer
        let (mut s2, _) = listener.accept().await.expect("accept 2");
        let req2 = read_http_request(&mut s2).await;
        captured_srv.lock().expect("lock").push(req2);
        let json_body = serde_json::to_string(&json!({
            "output": [
                {
                    "type": "message",
                    "content": [{ "type": "output_text", "text": "Workspace verified." }]
                }
            ],
            "usage": {
                "input_tokens": 1200,
                "input_tokens_details": { "cached_tokens": 1150 },
                "output_tokens": 25
            }
        }))
        .expect("json");
        let http_resp2 = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            json_body.len(),
            json_body
        );
        s2.write_all(http_resp2.as_bytes()).await.expect("write 2");
    });

    let mut pool = AccountPool::new(0.01);
    pool.add_account(AccountCandidate {
        provider: "openai".to_string(),
        name: "primary".to_string(),
        token: SecretToken::new("sk-test-123"),
        account_id: Some("acct_xyz".to_string()),
        remaining_quota_ratio: 1.0,
        exhausted: false,
    });
    let driver = HttpProviderDriver::new(pool);

    let provider = ProviderDefinition {
        id: "openai".to_string(),
        display_name: "OpenAI".to_string(),
        base_url: format!("http://{addr}/v1"),
        default_protocol: WireProtocol::OpenAiResponses,
        default_api_key_env: None,
        supports_vision_override: None,
        headers: BTreeMap::new(),
        featured_models: Vec::new(),
    };
    let caps = tekvra_providers::ModelCapabilities::resolve("openai", "gpt-5.4-codex", None);

    let mut history = vec![
        ProviderMessage::developer_cached("System instructions for Tekvra."),
        ProviderMessage::user_text("Inspect Cargo.toml"),
    ];
    let tools = vec![ToolDefinition {
        name: "file_read".to_string(),
        description: "Read file".to_string(),
        parameters: json!({"type": "object"}),
    }];

    let resp1 = driver
        .complete(
            &provider,
            &caps,
            &CompletionRequest {
                conversation_id: "conv_stable_01".to_string(),
                messages: history.clone(),
                tools: tools.clone(),
                reasoning_effort: Some("high".to_string()),
                output_schema: None,
            },
        )
        .await
        .expect("turn 1 complete");

    assert!((resp1.usage.cache_hit_ratio() - 0.90).abs() < 1e-6);

    // Append assistant turn (containing encrypted reasoning + tool call) and tool output verbatim
    history.push(resp1.assistant_message);
    history.push(ProviderMessage {
        role: MessageRole::Tool,
        parts: vec![MessagePart::ToolResult {
            call_id: "call_read1".to_string(),
            output: "[workspace]\nresolver = \"3\"".to_string(),
            is_error: false,
        }],
    });

    let resp2 = driver
        .complete(
            &provider,
            &caps,
            &CompletionRequest {
                conversation_id: "conv_stable_01".to_string(),
                messages: history,
                tools,
                reasoning_effort: Some("high".to_string()),
                output_schema: None,
            },
        )
        .await
        .expect("turn 2 complete");
    assert_eq!(resp2.served_by_account, "primary");

    server_task.await.expect("server task");
    let recorded = captured.lock().expect("lock");
    assert_eq!(recorded.len(), 2);

    let (hdr1, body1) = &recorded[0];
    let (_, body2) = &recorded[1];
    assert!(
        hdr1.to_ascii_lowercase()
            .contains("session-id: conv_stable_01")
    );
    assert!(
        hdr1.to_ascii_lowercase()
            .contains("chatgpt-account-id: acct_xyz")
    );
    assert_eq!(body1["store"], false);
    assert_eq!(body1["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body1["prompt_cache_key"], "conv_stable_01");
    assert_eq!(body1["reasoning"]["effort"], "high");
    assert_eq!(body1["reasoning"]["summary"], "auto");
    assert_eq!(
        body1["input"][0]["content"][0]["prompt_cache_breakpoint"],
        true
    );

    // Prefix items 0 and 1 MUST be byte-identical between Turn 1 and Turn 2!
    assert_eq!(body1["input"][0], body2["input"][0]);
    assert_eq!(body1["input"][1], body2["input"][1]);
    // And Turn 2 item 2 MUST replay `encrypted_content: "ENC_BLOB_V1"` verbatim!
    assert_eq!(body2["input"][2]["type"], "reasoning");
    assert_eq!(body2["input"][2]["encrypted_content"], "ENC_BLOB_V1");
}

#[tokio::test]
async fn multi_account_quota_exhaustion_triggers_automatic_failover_unless_pinned() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    let server = tokio::spawn(async move {
        // First request (primary account) -> 429 quota_exceeded
        let (mut s1, _) = listener.accept().await.expect("accept 1");
        let (hdr1, _) = read_http_request(&mut s1).await;
        assert!(hdr1.contains("Bearer sk-primary"));
        let err_body = r#"{"error":{"code":"insufficient_quota"}}"#;
        let resp1 = format!(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            err_body.len(),
            err_body
        );
        s1.write_all(resp1.as_bytes()).await.expect("write 429");
        drop(s1);

        // Automatic retry (secondary account) -> 200 OK
        let (mut s2, _) = listener.accept().await.expect("accept 2");
        let (hdr2, _) = read_http_request(&mut s2).await;
        assert!(hdr2.contains("Bearer sk-secondary"));
        let ok_body = serde_json::to_string(&json!({
            "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "Failover ok" }] }]
        }))
        .expect("json");
        let resp2 = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            ok_body.len(),
            ok_body
        );
        s2.write_all(resp2.as_bytes()).await.expect("write 200");
    });

    let mut pool = AccountPool::new(0.02);
    pool.add_account(AccountCandidate {
        provider: "openai".to_string(),
        name: "primary".to_string(),
        token: SecretToken::new("sk-primary"),
        account_id: None,
        remaining_quota_ratio: 1.0,
        exhausted: false,
    });
    pool.add_account(AccountCandidate {
        provider: "openai".to_string(),
        name: "secondary".to_string(),
        token: SecretToken::new("sk-secondary"),
        account_id: None,
        remaining_quota_ratio: 1.0,
        exhausted: false,
    });

    // Verify pinning rejects exhausted pinned account cleanly
    let mut pinned_pool = pool.clone();
    pinned_pool.set_pin(Some("openai:primary".parse::<AccountPin>().expect("pin")));
    pinned_pool.record_quota_state("openai", "primary", Some(0.0), true);
    assert!(pinned_pool.select("openai").is_err());

    let driver = HttpProviderDriver::new(pool);
    let provider = ProviderDefinition {
        id: "openai".to_string(),
        display_name: "OpenAI".to_string(),
        base_url: format!("http://{addr}/v1"),
        default_protocol: WireProtocol::OpenAiResponses,
        default_api_key_env: None,
        supports_vision_override: None,
        headers: BTreeMap::new(),
        featured_models: Vec::new(),
    };
    let caps = tekvra_providers::ModelCapabilities::resolve("openai", "gpt-5.4-codex", None);
    let resp = driver
        .complete(
            &provider,
            &caps,
            &CompletionRequest {
                conversation_id: "conv_failover".to_string(),
                messages: vec![ProviderMessage::user_text("ping")],
                tools: Vec::new(),
                reasoning_effort: None,
                output_schema: None,
            },
        )
        .await
        .expect("failover succeeds");

    assert_eq!(resp.served_by_account, "secondary");
    server.await.expect("server");
}

#[tokio::test]
async fn oauth_loopback_pkce_token_exchange_and_plan_to_build_model_routing() {
    let tmp = TempDir::new().expect("tempdir");

    // 1. Verify PKCE + Loopback OAuth callback + JWT claim extraction + secure CredentialStore
    let (verifier, challenge) = generate_pkce_pair();
    let (loopback, auth_url) = LoopbackOAuthListener::bind(
        "https://auth.openai.com/oauth/authorize",
        "tekvra-cli",
        &challenge,
        "openid profile offline_access",
    )
    .await
    .expect("bind loopback");

    let parsed_auth = url::Url::parse(&auth_url).expect("parse auth_url");
    let state = parsed_auth
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .expect("state query param");
    let callback_url = format!(
        "{}?code=auth_code_777&state={state}",
        loopback.redirect_uri()
    );

    let client_sim = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let r = client
            .get(&callback_url)
            .send()
            .await
            .expect("callback GET");
        assert!(r.status().is_success());
    });
    let code = loopback
        .accept_authorization_code()
        .await
        .expect("receive auth code");
    assert_eq!(code, "auth_code_777");
    client_sim.await.expect("client_sim");

    // Mock OAuth token endpoint returning a JWT with `chatgpt_account_id` and `chatgpt_plan_type`
    let token_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind token");
    let token_addr = token_listener.local_addr().expect("addr");
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "org_codex_99",
                "chatgpt_plan_type": "pro"
            }
        }))
        .expect("claims"),
    );
    let fake_jwt = format!("eyJhbGciOiJub25lIn0.{claims}.sig");

    let token_srv = tokio::spawn(async move {
        let (mut sock, _) = token_listener.accept().await.expect("accept");
        let _ = read_http_request(&mut sock).await;
        let body = serde_json::to_string(&json!({
            "access_token": fake_jwt,
            "refresh_token": "rt_refresh_1",
            "expires_in": 3600
        }))
        .expect("json");
        let http = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        sock.write_all(http.as_bytes()).await.expect("write");
    });

    let http = reqwest::Client::new();
    let stored = exchange_oauth_token(
        &http,
        &format!("http://{token_addr}/oauth/token"),
        "openai",
        "codex-sub",
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("code_verifier", &verifier),
        ],
        1_700_000_000,
    )
    .await
    .expect("exchange_oauth_token");
    token_srv.await.expect("token_srv");

    assert_eq!(stored.account_id.as_deref(), Some("org_codex_99"));
    assert_eq!(stored.plan_type.as_deref(), Some("pro"));
    assert!(format!("{stored:?}").contains("[REDACTED]"));

    let mut cred_store = CredentialStore::load(tmp.path()).expect("load creds");
    cred_store.upsert(stored).expect("upsert creds");
    let reloaded = CredentialStore::load(tmp.path()).expect("reload creds");
    assert_eq!(reloaded.list_for_provider("openai").len(), 1);

    // 2. Verify role-based model routing (`Plan` vs `Build` mode without mutating prior history)
    let global_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&global_dir).expect("mkdir");
    std::fs::create_dir_all(&state_dir).expect("mkdir");
    std::fs::create_dir_all(&repo_dir).expect("mkdir");

    std::fs::write(
        global_dir.join("tekvra.toml"),
        r#"
version = 2
model = "openai/gpt-5.4-codex"

[models]
main = "openai/gpt-5.4-codex"
plan = "anthropic/claude-opus-4-5"
fast = "openai/gpt-5.4-mini"
"#,
    )
    .expect("write tekvra.toml");

    let cfg = load_config(
        ConfigPaths {
            global_config_dir: global_dir,
            state_dir,
            home_dir: None,
        },
        &repo_dir,
        &CliConfigOverrides::default(),
    )
    .expect("load_config");

    let router = ModelRouter::from_config(&cfg, ProviderRegistry::new());
    let (_, plan_model) = router
        .route_for_mode(PermissionMode::Plan)
        .expect("route Plan");
    assert_eq!(plan_model.spec, "anthropic/claude-opus-4-5");

    let (_, build_model) = router
        .route_for_mode(PermissionMode::Build)
        .expect("route Build");
    assert_eq!(build_model.spec, "openai/gpt-5.4-codex");

    let (_, review_fallback) = router.route(JobRole::Review).expect("route Review");
    assert_eq!(review_fallback.spec, "openai/gpt-5.4-codex");

    let (_, fast_model) = router.route(JobRole::Fast).expect("route Fast");
    assert_eq!(fast_model.spec, "openai/gpt-5.4-mini");
}

#[tokio::test]
async fn device_code_polling_through_pending_and_slow_down_plus_refresh_token_renewal() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tekvra_providers::{
        build_oauth_http_client, poll_device_code_token, refresh_oauth_account, request_device_code,
    };

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind oauth mock");
    let addr = listener.local_addr().expect("addr");
    let step = Arc::new(AtomicUsize::new(0));
    let step_srv = Arc::clone(&step);

    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let step_inner = Arc::clone(&step_srv);
            tokio::spawn(async move {
                let (headers, _) = read_http_request(&mut sock).await;
                let first_line = headers.lines().next().unwrap_or_default();
                let (status, payload) = if first_line.contains("/device/code") {
                    (
                        "200 OK",
                        json!({
                            "device_code": "dc_secret_001",
                            "user_code": "TKVR-2026",
                            "verification_uri": "https://auth.openai.com/codex/device",
                            "interval": 0
                        }),
                    )
                } else {
                    match step_inner.fetch_add(1, Ordering::SeqCst) {
                        0 => (
                            "400 Bad Request",
                            json!({ "error": "authorization_pending" }),
                        ),
                        1 => ("400 Bad Request", json!({ "error": "slow_down" })),
                        2 => {
                            let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                                serde_json::to_vec(&json!({
                                    "https://api.openai.com/auth": {
                                        "chatgpt_account_id": "org_device_42",
                                        "chatgpt_plan_type": "team"
                                    }
                                }))
                                .expect("claims"),
                            );
                            (
                                "200 OK",
                                json!({
                                    "access_token": format!("eyJhbGciOiJub25lIn0.{claims}.sig"),
                                    "refresh_token": "rt_device_v1",
                                    "expires_in": 60
                                }),
                            )
                        }
                        _ => (
                            "200 OK",
                            json!({
                                "access_token": "at_refreshed_v2",
                                "expires_in": 7200
                            }),
                        ),
                    }
                };
                let body = payload.to_string();
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    let base = format!("http://{addr}");
    let http = build_oauth_http_client(&base).expect("oauth client");

    let dc_session =
        request_device_code(&http, &format!("{base}/device/code"), "cli_test", "openid")
            .await
            .expect("request_device_code");
    assert_eq!(dc_session.user_code, "TKVR-2026");

    let account = poll_device_code_token(
        &http,
        &format!("{base}/oauth/token"),
        "openai",
        "team-seat",
        "cli_test",
        &dc_session,
        6,
    )
    .await
    .expect("poll_device_code_token");
    assert_eq!(account.account_id.as_deref(), Some("org_device_42"));
    assert_eq!(account.plan_type.as_deref(), Some("team"));
    assert_eq!(step.load(Ordering::SeqCst), 3);

    // Refresh expired token (`refresh_token` omitted by server -> preserved from `account`)
    let renewed = refresh_oauth_account(
        &http,
        &format!("{base}/oauth/token"),
        "cli_test",
        &account,
        2_000_000_000,
    )
    .await
    .expect("refresh_oauth_account");
    assert_eq!(renewed.access_token.expose(), "at_refreshed_v2");
    assert_eq!(renewed.account_id.as_deref(), Some("org_device_42"));
    assert_eq!(renewed.plan_type.as_deref(), Some("team"));
    assert!(
        renewed.refresh_token.is_some(),
        "refresh_token must be preserved"
    );
    assert_eq!(renewed.expires_at_epoch_secs, Some(2_000_007_200));
}

fn multimodal_request() -> CompletionRequest {
    CompletionRequest {
        conversation_id: "conv_vision_01".to_string(),
        messages: vec![
            ProviderMessage::developer_cached("You inspect screenshots."),
            ProviderMessage {
                role: MessageRole::User,
                parts: vec![
                    MessagePart::Text {
                        text: "Identify the misaligned button".to_string(),
                        cache_breakpoint: false,
                    },
                    MessagePart::Image {
                        media_type: "image/png".to_string(),
                        data_base64: "aGVsbG8=".to_string(),
                        source_path: Some("ui-bug.png".to_string()),
                    },
                ],
            },
        ],
        tools: Vec::new(),
        reasoning_effort: None,
        output_schema: None,
    }
}

#[test]
fn multimodal_image_wire_encoding_across_all_four_protocols() {
    let model = tekvra_providers::ModelCapabilities::resolve("openai", "gpt-5.4-codex", None);
    assert!(model.supports_vision, "codex models must be vision-capable");

    // 1. OpenAI /responses -> input_image data URL inside user message content
    let responses_body =
        build_openai_responses_payload(&model, &multimodal_request()).expect("responses payload");
    let user_items: Vec<&Value> = responses_body["input"]
        .as_array()
        .expect("input array")
        .iter()
        .filter(|i| i["role"] == "user")
        .collect();
    assert_eq!(user_items.len(), 1);
    assert_eq!(
        user_items[0]["content"][1]["type"],
        json!("input_image"),
        "image must ride alongside the prompt text block"
    );
    assert_eq!(
        user_items[0]["content"][1]["image_url"],
        json!("data:image/png;base64,aGVsbG8=")
    );
    assert_eq!(user_items[0]["content"][0]["type"], json!("input_text"));

    // 2. OpenAI /chat/completions -> array content with image_url block
    let chat_body = build_chat_completions_payload(&model, &multimodal_request());
    let chat_user = chat_body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|m| m["role"] == "user")
        .expect("chat user message");
    assert_eq!(chat_user["content"][0]["type"], json!("text"));
    assert_eq!(
        chat_user["content"][1]["image_url"]["url"],
        json!("data:image/png;base64,aGVsbG8=")
    );

    // 3. Anthropic /v1/messages -> base64 image source block
    let anthropic_body = build_anthropic_messages_payload(&model, &multimodal_request());
    let anthropic_user = anthropic_body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|m| m["role"] == "user")
        .expect("anthropic user message");
    assert_eq!(anthropic_user["content"][1]["type"], json!("image"));
    assert_eq!(
        anthropic_user["content"][1]["source"],
        json!({ "type": "base64", "media_type": "image/png", "data": "aGVsbG8=" })
    );

    // 4. Native Gemini -> inlineData mimeType/data block
    let gemini_model =
        tekvra_providers::ModelCapabilities::resolve("gemini", "gemini-2.5-pro", None);
    assert!(gemini_model.supports_vision);
    let gemini_body = build_gemini_payload(&gemini_model, &multimodal_request());
    assert_eq!(
        gemini_body["contents"][0]["parts"][1]["inlineData"],
        json!({ "mimeType": "image/png", "data": "aGVsbG8=" })
    );
    assert_eq!(
        gemini_body["contents"][0]["parts"][0]["text"],
        json!("Identify the misaligned button")
    );
    assert_eq!(
        gemini_body["systemInstruction"]["parts"][0]["text"],
        json!("You inspect screenshots.")
    );

    // Gemini tool mapping: assistant functionCall + user functionResponse round-trips by name
    let gemini_tools_req = CompletionRequest {
        conversation_id: "conv_vision_tools".to_string(),
        messages: vec![
            ProviderMessage::developer_cached("Use tools."),
            ProviderMessage {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::ToolCall {
                    call_id: "call_read_1".to_string(),
                    name: "file_read".to_string(),
                    arguments_json: r#"{"path":"Cargo.toml"}"#.to_string(),
                }],
            },
            ProviderMessage {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    call_id: "call_read_1".to_string(),
                    output: "[workspace]".to_string(),
                    is_error: false,
                }],
            },
        ],
        tools: vec![ToolDefinition {
            name: "file_read".to_string(),
            description: "Read file".to_string(),
            parameters: json!({"type": "object"}),
        }],
        reasoning_effort: None,
        output_schema: None,
    };
    let tools_body = build_gemini_payload(&gemini_model, &gemini_tools_req);
    // The developer message lands in `systemInstruction`, so contents[0] is the assistant
    // functionCall and contents[1] the user functionResponse.
    assert_eq!(
        tools_body["contents"][0]["parts"][0]["functionCall"]["name"],
        json!("file_read")
    );
    assert_eq!(
        tools_body["contents"][1]["parts"][0]["functionResponse"]["name"],
        json!("file_read")
    );
    assert!(
        tools_body["tools"][0]["functionDeclarations"]
            .as_array()
            .is_some()
    );

    // Parsing a chunked `streamGenerateContent` JSON array response
    let gemini_raw = json!([
        {
            "candidates": [{ "content": { "parts": [{ "text": "The button " }] } }],
            "usageMetadata": { "promptTokenCount": 300, "cachedContentTokenCount": 100, "candidatesTokenCount": 12 }
        },
        {
            "candidates": [{ "content": { "parts": [{ "text": "is misaligned." }] } }],
            "usageMetadata": { "promptTokenCount": 300, "cachedContentTokenCount": 100, "candidatesTokenCount": 30, "thoughtsTokenCount": 5 }
        }
    ])
    .to_string();
    let parsed = tekvra_providers::parse_gemini_body(&gemini_raw, &gemini_model, "mock")
        .expect("parse gemini");
    // Each streamed candidate chunk becomes its own assistant text part in order.
    assert_eq!(
        tekvra_providers::MessagePart::Text {
            text: "The button ".to_string(),
            cache_breakpoint: false,
        },
        parsed.assistant_message.parts[0],
    );
    assert_eq!(
        tekvra_providers::MessagePart::Text {
            text: "is misaligned.".to_string(),
            cache_breakpoint: false,
        },
        parsed.assistant_message.parts[1],
    );
    assert_eq!(parsed.usage.input_tokens, 300);
    assert_eq!(parsed.usage.cached_input_tokens, 100);
    assert_eq!(parsed.usage.output_tokens, 30);
    assert_eq!(parsed.usage.reasoning_tokens, 5);
    assert!(parsed.usage.cost_usd > 0.0);

    // Text-only chat-completions requests must keep plain string `content` for local servers.
    let text_only = CompletionRequest {
        conversation_id: "conv_plain".to_string(),
        messages: vec![ProviderMessage::user_text("ping")],
        tools: Vec::new(),
        reasoning_effort: None,
        output_schema: None,
    };
    let plain_body = build_chat_completions_payload(&model, &text_only);
    assert_eq!(plain_body["messages"][0]["content"], json!("ping"));
}

#[tokio::test]
async fn non_vision_model_rejects_image_locally_before_network_io() {
    // Model whose heuristics yield `supports_vision == false`.
    let model = tekvra_providers::ModelCapabilities::resolve("openai", "gpt-3.5-turbo", None);
    assert!(!model.supports_vision);

    let driver = HttpProviderDriver::new(AccountPool::new(0.01));
    let provider = ProviderDefinition {
        id: "openai".to_string(),
        display_name: "OpenAI".to_string(),
        base_url: "http://127.0.0.1:9/never-listen".to_string(),
        default_protocol: WireProtocol::OpenAiResponses,
        default_api_key_env: None,
        supports_vision_override: None,
        headers: BTreeMap::new(),
        featured_models: Vec::new(),
    };
    let err = driver
        .complete(&provider, &model, &multimodal_request())
        .await
        .expect_err("image on non-vision model must be rejected");
    let rendered = err.to_string();
    assert!(
        rendered.contains("does not support image input"),
        "unexpected error: {rendered}"
    );
    if let tekvra_providers::ProviderError::UnsupportedModality { model, modality } = err {
        assert_eq!(model, "openai/gpt-3.5-turbo");
        assert_eq!(modality, "image");
    } else {
        panic!("expected UnsupportedModality, got: {err}");
    }
}

#[test]
fn providers_toml_vision_override_applies_to_resolved_models() {
    use indexmap::IndexMap;
    use tekvra_config::{ProviderAccountConfig, ProviderEntryConfig};

    let mut providers = IndexMap::new();
    providers.insert(
        "ollama".to_string(),
        ProviderEntryConfig {
            api_key_env: None,
            supports_vision: Some(true),
            accounts: Vec::<ProviderAccountConfig>::new(),
            ..Default::default()
        },
    );
    let mut registry = ProviderRegistry::new();
    registry.apply_custom_configs(&providers);

    let (_, caps) = registry
        .resolve_model_spec("ollama/qwen3-coder:30b")
        .expect("resolve ollama model");
    assert!(
        caps.supports_vision,
        "providers.toml `supports_vision = true` must override per-model heuristics"
    );
    let (def, _) = registry
        .resolve_model_spec("ollama/qwen3-coder:30b")
        .expect("def");
    assert_eq!(def.supports_vision_override, Some(true));
}

// ---------------------------------------------------------------------------
// Incremental SSE streaming, usage/cost extraction, and transient retry backoff
// ---------------------------------------------------------------------------

fn chat_account(provider: &str, name: &str, token: &str) -> AccountCandidate {
    AccountCandidate {
        provider: provider.to_string(),
        name: name.to_string(),
        token: SecretToken::new(token),
        account_id: None,
        remaining_quota_ratio: 1.0,
        exhausted: false,
    }
}

fn chat_tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "file_edit".to_string(),
        description: "Edit a file".to_string(),
        parameters: json!({"type": "object"}),
    }]
}

#[tokio::test]
async fn streaming_chat_completions_emits_live_deltas_and_assembles_tool_calls() {
    use tekvra_providers::StreamDelta;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_srv = Arc::clone(&captured);
    let write2_at: Arc<Mutex<Option<tokio::time::Instant>>> = Arc::new(Mutex::new(None));
    let write2_srv = Arc::clone(&write2_at);

    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let (hdr, body) = read_http_request(&mut sock).await;
        captured_srv.lock().expect("lock").push((hdr, body));
        // First delta flushes immediately; the rest arrives only after a visible gap
        // so the test can prove deltas decode live instead of after EOF.
        let head = "data: {\"choices\":[{\"delta\":{\"content\":\"Planning \"}}]}\n\n";
        sock.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{head}"
            )
            .as_bytes(),
        )
        .await
        .expect("write head");
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let rest = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"edit.\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_z\",\"function\":{\"name\":\"file_edit\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":120,\"prompt_tokens_details\":{\"cached_tokens\":100},\"completion_tokens\":40,\"completion_tokens_details\":{\"reasoning_tokens\":7}}}\n\n",
            "data: [DONE]\n\n",
        );
        sock.write_all(rest.as_bytes()).await.expect("write rest");
        *write2_srv.lock().expect("lock") = Some(tokio::time::Instant::now());
    });

    let mut pool = AccountPool::new(0.02);
    pool.add_account(chat_account("xai", "main", "sk-xai"));
    let driver = HttpProviderDriver::new(pool);
    let provider = ProviderDefinition {
        id: "xai".to_string(),
        display_name: "xAI".to_string(),
        base_url: format!("http://{addr}/v1"),
        default_protocol: WireProtocol::OpenAiChatCompletions,
        default_api_key_env: None,
        supports_vision_override: None,
        headers: BTreeMap::new(),
        featured_models: Vec::new(),
    };
    let caps = tekvra_providers::ModelCapabilities::resolve("xai", "grok-4", None);

    let arrivals: Arc<Mutex<Vec<(tokio::time::Instant, StreamDelta)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let arrivals_sink = Arc::clone(&arrivals);
    let sink: tekvra_providers::StreamDeltaSink =
        Arc::new(move |delta: tekvra_providers::StreamDelta| {
            arrivals_sink
                .lock()
                .expect("lock")
                .push((tokio::time::Instant::now(), delta));
        });

    let resp = driver
        .complete_streaming(
            &provider,
            &caps,
            &CompletionRequest {
                conversation_id: "conv_stream_chat".to_string(),
                messages: vec![ProviderMessage::user_text("edit the file")],
                tools: chat_tools(),
                reasoning_effort: None,
                output_schema: None,
            },
            Some(sink),
        )
        .await
        .expect("streaming chat turn");

    server.await.expect("server");
    let wrote_rest = write2_at
        .lock()
        .expect("lock")
        .expect("rest write recorded");
    let arrived = arrivals.lock().expect("lock");
    assert!(
        arrived.first().expect("live delta").0 < wrote_rest,
        "first delta must decode before the server finished its second write"
    );
    assert_eq!(arrived.len(), 2, "two content deltas expected: {arrived:?}");
    drop(arrived);

    // Assembled message: merged prose + fully concatenated tool-call arguments.
    let parts = &resp.assistant_message.parts;
    assert_eq!(
        parts[0],
        tekvra_providers::MessagePart::Text {
            text: "Planning edit.".to_string(),
            cache_breakpoint: false,
        }
    );
    assert_eq!(
        parts[1],
        tekvra_providers::MessagePart::ToolCall {
            call_id: "call_z".to_string(),
            name: "file_edit".to_string(),
            arguments_json: r#"{"path":"a.rs"}"#.to_string(),
        }
    );
    // `stream_options.include_usage` must surface the final usage chunk.
    assert_eq!(resp.usage.input_tokens, 120);
    assert_eq!(resp.usage.cached_input_tokens, 100);
    assert_eq!(resp.usage.output_tokens, 40);
    assert_eq!(resp.usage.reasoning_tokens, 7);
    assert!(resp.usage.cost_usd > 0.0);
    // The outbound request must have asked for streaming + usage.
    let (hdr, body) = &captured.lock().expect("lock")[0];
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"]["include_usage"], json!(true));
    assert!(hdr.contains("POST /v1/chat/completions"));
}

#[tokio::test]
async fn streaming_anthropic_assembles_thinking_signature_tool_use_and_cache_usage() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_srv = Arc::clone(&captured);

    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let (hdr, body) = read_http_request(&mut sock).await;
        captured_srv.lock().expect("lock").push((hdr, body));
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1000,\"cache_creation_input_tokens\":50,\"cache_read_input_tokens\":800}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"id\":\"thg_1\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Reasoning step.\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"SIG==\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Here is the plan.\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_9\",\"name\":\"file_read\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"pa\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"th\\\":\\\"x\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":42}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        sock.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{sse}",
                sse.len()
            )
            .as_bytes(),
        )
        .await
        .expect("write sse");
    });

    let mut pool = AccountPool::new(0.02);
    pool.add_account(chat_account("anthropic", "claude-main", "sk-ant-test"));
    let driver = HttpProviderDriver::new(pool);
    let provider = ProviderDefinition {
        id: "anthropic".to_string(),
        display_name: "Anthropic".to_string(),
        base_url: format!("http://{addr}/v1"),
        default_protocol: WireProtocol::AnthropicMessages,
        default_api_key_env: None,
        supports_vision_override: None,
        headers: BTreeMap::new(),
        featured_models: Vec::new(),
    };
    let caps = tekvra_providers::ModelCapabilities::resolve("anthropic", "claude-sonnet-4-5", None);

    let deltas: Arc<Mutex<Vec<tekvra_providers::StreamDelta>>> = Arc::new(Mutex::new(Vec::new()));
    let deltas_sink = Arc::clone(&deltas);
    let sink: tekvra_providers::StreamDeltaSink = Arc::new(move |d| {
        deltas_sink.lock().expect("lock").push(d);
    });

    let resp = driver
        .complete_streaming(
            &provider,
            &caps,
            &CompletionRequest {
                conversation_id: "conv_stream_anthropic".to_string(),
                messages: vec![
                    ProviderMessage::developer_cached("Be careful."),
                    ProviderMessage::user_text("plan it"),
                ],
                tools: chat_tools(),
                reasoning_effort: None,
                output_schema: None,
            },
            Some(sink),
        )
        .await
        .expect("streaming anthropic turn");
    server.await.expect("server");

    // Signed thinking block + text + tool call, in wire order.
    let parts = &resp.assistant_message.parts;
    assert_eq!(
        parts[0],
        tekvra_providers::MessagePart::Reasoning {
            id: "thg_1".to_string(),
            summary: vec!["Reasoning step.".to_string()],
            encrypted_content: None,
            signature: Some("SIG==".to_string()),
        }
    );
    assert_eq!(
        parts[1],
        tekvra_providers::MessagePart::Text {
            text: "Here is the plan.".to_string(),
            cache_breakpoint: false,
        }
    );
    assert_eq!(
        parts[2],
        tekvra_providers::MessagePart::ToolCall {
            call_id: "toolu_9".to_string(),
            name: "file_read".to_string(),
            arguments_json: r#"{"path":"x"}"#.to_string(),
        }
    );
    // Usage: input tokens include cache-creation; cached counts cache reads.
    assert_eq!(resp.usage.input_tokens, 1850);
    assert_eq!(resp.usage.cached_input_tokens, 800);
    assert_eq!(resp.usage.output_tokens, 42);
    assert!(resp.usage.cost_usd > 0.0);
    // Live delta ordering: thinking first, then text.
    let deltas = deltas.lock().expect("lock");
    assert!(matches!(
        deltas.first(),
        Some(tekvra_providers::StreamDelta::ReasoningSummaryDelta(_))
    ));
    assert!(matches!(
        deltas.get(1),
        Some(tekvra_providers::StreamDelta::TextDelta(t)) if t == "Here is the plan."
    ));
    drop(deltas);

    // Request carried `stream: true` with Anthropic auth headers.
    let (hdr, body) = &captured.lock().expect("lock")[0];
    assert_eq!(body["stream"], json!(true));
    assert!(hdr.contains("x-api-key: sk-ant-test"));
    // Rolling ephemeral cache breakpoint lands on the trailing user turn block.
    let messages = body["messages"].as_array().expect("messages");
    let last_msg = messages.last().expect("trailing message");
    assert_eq!(
        last_msg["content"]
            .as_array()
            .expect("content")
            .last()
            .expect("last block")["cache_control"],
        json!({"type": "ephemeral"})
    );
    // Developer system block keeps its own breakpoint.
    assert_eq!(
        body["system"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
}

#[tokio::test]
async fn streaming_gemini_alt_sse_streams_chunks_and_usage() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_srv = Arc::clone(&captured);

    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let (hdr, body) = read_http_request(&mut sock).await;
        captured_srv.lock().expect("lock").push((hdr, body));
        let sse = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hel\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"lo!\"}]}}],\"usageMetadata\":{\"promptTokenCount\":300,\"cachedContentTokenCount\":100,\"candidatesTokenCount\":12,\"thoughtsTokenCount\":5}}\n\n",
        );
        sock.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{sse}",
                sse.len()
            )
            .as_bytes(),
        )
        .await
        .expect("write sse");
    });

    let mut pool = AccountPool::new(0.02);
    pool.add_account(chat_account("gemini", "g-main", "sk-g"));
    let driver = HttpProviderDriver::new(pool);
    let provider = ProviderDefinition {
        id: "gemini".to_string(),
        display_name: "Gemini".to_string(),
        base_url: format!("http://{addr}/v1beta"),
        default_protocol: WireProtocol::GeminiGenerateContent,
        default_api_key_env: None,
        supports_vision_override: None,
        headers: BTreeMap::new(),
        featured_models: Vec::new(),
    };
    let caps = tekvra_providers::ModelCapabilities::resolve("gemini", "gemini-2.5-pro", None);

    let resp = driver
        .complete_streaming(
            &provider,
            &caps,
            &CompletionRequest {
                conversation_id: "conv_stream_gemini".to_string(),
                messages: vec![ProviderMessage::user_text("hi")],
                tools: Vec::new(),
                reasoning_effort: None,
                output_schema: None,
            },
            None,
        )
        .await
        .expect("streaming gemini turn");
    server.await.expect("server");

    // One part per chunk (matches the buffered JSON-array parser behavior).
    assert_eq!(
        resp.assistant_message.parts,
        vec![
            tekvra_providers::MessagePart::Text {
                text: "Hel".to_string(),
                cache_breakpoint: false,
            },
            tekvra_providers::MessagePart::Text {
                text: "lo!".to_string(),
                cache_breakpoint: false,
            },
        ]
    );
    assert_eq!(resp.stream_deltas.len(), 2);
    assert_eq!(resp.usage.input_tokens, 300);
    assert_eq!(resp.usage.cached_input_tokens, 100);
    assert_eq!(resp.usage.output_tokens, 12);
    assert_eq!(resp.usage.reasoning_tokens, 5);
    assert!(resp.usage.cost_usd > 0.0);

    let (hdr, _) = &captured.lock().expect("lock")[0];
    assert!(
        hdr.contains(":streamGenerateContent?alt=sse"),
        "gemini streaming must append alt=sse: {hdr}"
    );
}

#[tokio::test]
async fn streaming_openai_responses_delivers_live_deltas_and_encrypted_reasoning() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_srv = Arc::clone(&captured);

    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let (hdr, body) = read_http_request(&mut sock).await;
        captured_srv.lock().expect("lock").push((hdr, body));
        let sse = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"Checking\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello \"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"world\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{",
            "\"output\":[",
            "{\"type\":\"reasoning\",\"id\":\"rs_s1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Checking\"}],\"encrypted_content\":\"ENC_S1\"},",
            "{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello world\"}]}",
            "],",
            "\"usage\":{\"input_tokens\":100,\"input_tokens_details\":{\"cached_tokens\":60},\"output_tokens\":20,\"output_tokens_details\":{\"reasoning_tokens\":8}}",
            "}}\n\n"
        );
        sock.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{sse}",
                sse.len()
            )
            .as_bytes(),
        )
        .await
        .expect("write sse");
    });

    let mut pool = AccountPool::new(0.01);
    pool.add_account(chat_account("openai", "primary", "sk-test-123"));
    let driver = HttpProviderDriver::new(pool);
    let provider = ProviderDefinition {
        id: "openai".to_string(),
        display_name: "OpenAI".to_string(),
        base_url: format!("http://{addr}/v1"),
        default_protocol: WireProtocol::OpenAiResponses,
        default_api_key_env: None,
        supports_vision_override: None,
        headers: BTreeMap::new(),
        featured_models: Vec::new(),
    };
    let caps = tekvra_providers::ModelCapabilities::resolve("openai", "gpt-5.4-codex", None);

    let deltas: Arc<Mutex<Vec<tekvra_providers::StreamDelta>>> = Arc::new(Mutex::new(Vec::new()));
    let deltas_sink = Arc::clone(&deltas);
    let sink: tekvra_providers::StreamDeltaSink = Arc::new(move |d| {
        deltas_sink.lock().expect("lock").push(d);
    });

    let resp = driver
        .complete_streaming(
            &provider,
            &caps,
            &CompletionRequest {
                conversation_id: "conv_stream_resp".to_string(),
                messages: vec![ProviderMessage::user_text("hi")],
                tools: Vec::new(),
                reasoning_effort: Some("high".to_string()),
                output_schema: None,
            },
            Some(sink),
        )
        .await
        .expect("streaming responses turn");
    server.await.expect("server");

    // Live deltas arrive in wire order (reasoning first, then text fragments).
    let deltas = deltas.lock().expect("lock");
    assert_eq!(
        &*deltas,
        &vec![
            tekvra_providers::StreamDelta::ReasoningSummaryDelta("Checking".to_string()),
            tekvra_providers::StreamDelta::TextDelta("Hello ".to_string()),
            tekvra_providers::StreamDelta::TextDelta("world".to_string()),
        ]
    );
    drop(deltas);
    // Final assembly keeps encrypted reasoning verbatim for stateless replay.
    let parts = &resp.assistant_message.parts;
    assert_eq!(
        parts[0],
        tekvra_providers::MessagePart::Reasoning {
            id: "rs_s1".to_string(),
            summary: vec!["Checking".to_string()],
            encrypted_content: Some("ENC_S1".to_string()),
            signature: None,
        }
    );
    assert_eq!(resp.usage.input_tokens, 100);
    assert_eq!(resp.usage.cached_input_tokens, 60);
    assert_eq!(resp.usage.reasoning_tokens, 8);
    assert_eq!(
        resp.stream_deltas.len(),
        3,
        "no duplicate deltas after completion"
    );

    let (_, body) = &captured.lock().expect("lock")[0];
    assert_eq!(body["stream"], json!(true));
}

#[tokio::test]
async fn streaming_falls_back_to_json_when_server_ignores_stream_hint() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_srv = Arc::clone(&captured);

    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let (_, body) = read_http_request(&mut sock).await;
        captured_srv.lock().expect("lock").push(body);
        let json_body = serde_json::to_string(&json!({
            "choices": [{
                "message": { "content": "Buffered answer." },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 55,
                "prompt_tokens_details": { "cached_tokens": 40 },
                "completion_tokens": 9,
                "completion_tokens_details": { "reasoning_tokens": 2 }
            }
        }))
        .expect("json");
        sock.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{json_body}",
                json_body.len()
            )
            .as_bytes(),
        )
        .await
        .expect("write json");
    });

    let mut pool = AccountPool::new(0.02);
    pool.add_account(chat_account("deepseek", "ds", "sk-ds"));
    let driver = HttpProviderDriver::new(pool);
    let provider = ProviderDefinition {
        id: "deepseek".to_string(),
        display_name: "DeepSeek".to_string(),
        base_url: format!("http://{addr}/v1"),
        default_protocol: WireProtocol::OpenAiChatCompletions,
        default_api_key_env: None,
        supports_vision_override: None,
        headers: BTreeMap::new(),
        featured_models: Vec::new(),
    };
    let caps = tekvra_providers::ModelCapabilities::resolve("deepseek", "deepseek-chat", None);

    let resp = driver
        .complete_streaming(
            &provider,
            &caps,
            &CompletionRequest {
                conversation_id: "conv_fallback".to_string(),
                messages: vec![ProviderMessage::user_text("hi")],
                tools: Vec::new(),
                reasoning_effort: None,
                output_schema: None,
            },
            None,
        )
        .await
        .expect("json fallback turn");
    server.await.expect("server");

    assert_eq!(
        resp.assistant_message.parts[0],
        tekvra_providers::MessagePart::Text {
            text: "Buffered answer.".to_string(),
            cache_breakpoint: false,
        }
    );
    // Real usage must surface even through the JSON fallback (no more zero-token rows).
    assert_eq!(resp.usage.input_tokens, 55);
    assert_eq!(resp.usage.cached_input_tokens, 40);
    assert_eq!(resp.usage.output_tokens, 9);
    assert_eq!(resp.usage.reasoning_tokens, 2);
    assert!(resp.usage.cost_usd > 0.0);
    assert_eq!(captured.lock().expect("lock")[0]["stream"], json!(true));
}

#[tokio::test]
async fn transient_503_and_temporary_429_retries_succeed_within_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let request_count = Arc::new(Mutex::new(0u32));
    let count_srv = Arc::clone(&request_count);

    let server = tokio::spawn(async move {
        // 1. Transient upstream 503 with an immediate Retry-After hint.
        let (mut s1, _) = listener.accept().await.expect("accept 503");
        let _ = read_http_request(&mut s1).await;
        let body503 = "{\"error\":{\"message\":\"upstream overloaded\"}}";
        s1.write_all(
            format!(
                "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body503}",
                body503.len()
            )
            .as_bytes(),
        )
        .await
        .expect("write 503");
        drop(s1);

        // 2. Temporary single-account 429 without permanent billing markers.
        let (mut s2, _) = listener.accept().await.expect("accept 429");
        let _ = read_http_request(&mut s2).await;
        let body429 = "{\"error\":{\"message\":\"slow down\"}}";
        s2.write_all(
            format!(
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body429}",
                body429.len()
            )
            .as_bytes(),
        )
        .await
        .expect("write 429");
        drop(s2);

        // 3. Success.
        let (mut s3, _) = listener.accept().await.expect("accept 200");
        let (_, req_body) = read_http_request(&mut s3).await;
        *count_srv.lock().expect("lock") += 3;
        let _ = req_body;
        let ok_body = serde_json::to_string(&json!({
            "choices": [{ "message": { "content": "Recovered." }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 3 }
        }))
        .expect("json");
        s3.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{ok_body}",
                ok_body.len()
            )
            .as_bytes(),
        )
        .await
        .expect("write 200");
    });

    let mut pool = AccountPool::new(0.02);
    pool.add_account(chat_account("xai", "only", "sk-x"));
    let driver = HttpProviderDriver::new(pool);
    let provider = ProviderDefinition {
        id: "xai".to_string(),
        display_name: "xAI".to_string(),
        base_url: format!("http://{addr}/v1"),
        default_protocol: WireProtocol::OpenAiChatCompletions,
        default_api_key_env: None,
        supports_vision_override: None,
        headers: BTreeMap::new(),
        featured_models: Vec::new(),
    };
    let caps = tekvra_providers::ModelCapabilities::resolve("xai", "grok-4", None);

    let started = tokio::time::Instant::now();
    let resp = driver
        .complete(
            &provider,
            &caps,
            &CompletionRequest {
                conversation_id: "conv_retry".to_string(),
                messages: vec![ProviderMessage::user_text("hi")],
                tools: Vec::new(),
                reasoning_effort: None,
                output_schema: None,
            },
        )
        .await
        .expect("retry recovers");
    let elapsed = started.elapsed();

    server.await.expect("server");
    assert_eq!(*request_count.lock().expect("lock"), 3);
    assert_eq!(
        resp.assistant_message.parts[0],
        tekvra_providers::MessagePart::Text {
            text: "Recovered.".to_string(),
            cache_breakpoint: false,
        }
    );
    assert_eq!(resp.usage.input_tokens, 10);
    assert_eq!(resp.usage.output_tokens, 3);
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "Retry-After: 0 must be honored over exponential backoff; took {elapsed:?}"
    );
    // The single account must NOT have been marked exhausted by a soft 429.
    assert!(driver.account_pool_snapshot().select("xai").is_ok());
}

//! Process-level CLI integration tests for `tekvra` (Task 7.3):
//! - `tekvra exec --output-schema <schema.json>` exits `0` on conforming JSON and `2` on schema mismatch
//! - `tekvra --no-plugins exec ...` suppresses Lua VM startup even when `experimental.lua_plugins = true`
//!   and `init.lua` throws an unconditional error

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::Command;

#[derive(Clone)]
struct MockResponsesState {
    bodies: Arc<Mutex<VecDeque<Value>>>,
}

async fn handle_responses(
    State(st): State<MockResponsesState>,
    axum::Json(_req_body): axum::Json<Value>,
) -> axum::Json<Value> {
    let next = st
        .bodies
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pop_front()
        .unwrap_or_else(|| {
            json!({
                "id": "resp_default",
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "{\"summary\":\"ok\",\"passed\":true}" }]
                }],
                "usage": { "input_tokens": 20, "output_tokens": 10 }
            })
        });
    axum::Json(next)
}

async fn spawn_mock_openai_server(replies: Vec<&str>) -> String {
    let queue: VecDeque<Value> = replies
        .into_iter()
        .enumerate()
        .map(|(i, text)| {
            json!({
                "id": format!("resp_{i}"),
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": text }]
                }],
                "usage": {
                    "input_tokens": 40,
                    "input_tokens_details": { "cached_tokens": 20 },
                    "output_tokens": 12
                }
            })
        })
        .collect();

    let app = Router::new()
        .route("/v1/responses", post(handle_responses))
        .with_state(MockResponsesState {
            bodies: Arc::new(Mutex::new(queue)),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock openai");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/v1")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_exec_output_schema_validation_and_no_plugins_flag() {
    let base_url = spawn_mock_openai_server(vec![
        // 1. Valid JSON matching schema
        r#"{"summary":"All 21 OpenSpec tasks verified","passed":true}"#,
        // 2. Invalid JSON violating schema (`passed` is string instead of boolean!)
        r#"{"summary":"Broken schema","passed":"not_a_bool"}"#,
        // 3. Valid reply for `--no-plugins` invocation
        "NO_PLUGINS_EXEC_SUCCESS",
    ])
    .await;

    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");

    // Configure global `tekvra.toml` with `experimental.lua_plugins = true` AND a poisonous `init.lua`!
    std::fs::write(
        cfg_dir.join("tekvra.toml"),
        "version = 2\nmodel = \"openai/gpt-5.4-codex\"\n\n[experimental]\nlua_plugins = true\n",
    )
    .expect("write tekvra.toml");
    std::fs::write(
        cfg_dir.join("providers.toml"),
        format!("version = 2\n\n[providers.openai]\nbase_url = \"{base_url}\"\n"),
    )
    .expect("write providers.toml");
    std::fs::write(
        cfg_dir.join("init.lua"),
        "error('POISON_INIT_LUA_WAS_EXECUTED')\n",
    )
    .expect("write poison init.lua");

    let schema_path = tmp.path().join("report.schema.json");
    std::fs::write(
        &schema_path,
        serde_json::to_string_pretty(&json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "required": ["summary", "passed"],
            "properties": {
                "summary": { "type": "string" },
                "passed": { "type": "boolean" }
            },
            "additionalProperties": false
        }))
        .expect("schema json"),
    )
    .expect("write schema");

    let bin = env!("CARGO_BIN_EXE_tekvra");

    // 0. Sanity check: WITHOUT `--no-plugins`, `init.lua` runs and aborts startup!
    let poison_out = Command::new(bin)
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .env("OPENAI_API_KEY", "sk-test-cli")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(["exec", "test"])
        .output()
        .await
        .expect("run poison check");
    assert!(
        !poison_out.status.success(),
        "expected poison init.lua to fail when --no-plugins is NOT passed"
    );

    // 1. `tekvra --no-plugins exec --output-schema <schema.json>` with valid response -> exit 0!
    let ok_out = Command::new(bin)
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .env("OPENAI_API_KEY", "sk-test-cli")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args([
            "--no-plugins",
            "exec",
            "--output-schema",
            schema_path.to_str().expect("utf8"),
            "Generate structured summary",
        ])
        .output()
        .await
        .expect("run valid exec");
    assert!(
        ok_out.status.success(),
        "expected exit 0, stderr={}",
        String::from_utf8_lossy(&ok_out.stderr)
    );
    let parsed_stdout: Value =
        serde_json::from_slice(&ok_out.stdout).expect("stdout must be valid JSON");
    assert_eq!(parsed_stdout["passed"], true);

    // 2. `tekvra --no-plugins exec --output-schema <schema.json>` with schema-violating response -> exit 2!
    let bad_out = Command::new(bin)
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .env("OPENAI_API_KEY", "sk-test-cli")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args([
            "--no-plugins",
            "exec",
            "--output-schema",
            schema_path.to_str().expect("utf8"),
            "Generate broken summary",
        ])
        .output()
        .await
        .expect("run invalid schema exec");
    assert_eq!(
        bad_out.status.code(),
        Some(2),
        "expected exit code 2 on JSON Schema validation failure, stderr={}",
        String::from_utf8_lossy(&bad_out.stderr)
    );

    // 3. Plain `tekvra --no-plugins exec "Say hi"` -> exit 0 and prints response!
    let plain_out = Command::new(bin)
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .env("OPENAI_API_KEY", "sk-test-cli")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(["--no-plugins", "exec", "Say hi"])
        .output()
        .await
        .expect("run plain --no-plugins exec");
    assert!(plain_out.status.success());
    assert!(
        String::from_utf8_lossy(&plain_out.stdout).contains("NO_PLUGINS_EXEC_SUCCESS"),
        "stdout={}",
        String::from_utf8_lossy(&plain_out.stdout)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_auth_login_device_code_loopback_pkce_and_startup_refresh_token_renewal() {
    use axum::http::StatusCode;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[derive(Clone)]
    struct OAuthMockState {
        polls: Arc<AtomicUsize>,
        refreshes: Arc<AtomicUsize>,
    }

    let oauth_state = OAuthMockState {
        polls: Arc::new(AtomicUsize::new(0)),
        refreshes: Arc::new(AtomicUsize::new(0)),
    };

    let app = Router::new()
        .route(
            "/device/code",
            post(|| async {
                axum::Json(json!({
                    "device_code": "dc_cli_999",
                    "user_code": "CLI-CODE",
                    "verification_uri": "http://127.0.0.1/verify",
                    "interval": 0
                }))
            }),
        )
        .route(
            "/oauth/token",
            post({
                let st = oauth_state.clone();
                move |body: String| {
                    let st = st.clone();
                    async move {
                        if body.contains("grant_type=refresh_token") {
                            st.refreshes.fetch_add(1, Ordering::SeqCst);
                            return (
                                StatusCode::OK,
                                axum::Json(json!({
                                    "access_token": "at_auto_refreshed_from_cli",
                                    "refresh_token": "rt_kept_v2",
                                    "expires_in": 3600
                                })),
                            );
                        }
                        if body.contains("device_code") {
                            let n = st.polls.fetch_add(1, Ordering::SeqCst);
                            if n == 0 {
                                return (
                                    StatusCode::BAD_REQUEST,
                                    axum::Json(json!({ "error": "authorization_pending" })),
                                );
                            }
                            return (
                                StatusCode::OK,
                                axum::Json(json!({
                                    "access_token": "at_expired_soon",
                                    "refresh_token": "rt_cli_device",
                                    "expires_in": -120
                                })),
                            );
                        }
                        // Browser PKCE `authorization_code` exchange
                        (
                            StatusCode::OK,
                            axum::Json(json!({
                                "access_token": "at_pkce_browser_ok",
                                "refresh_token": "rt_pkce_browser",
                                "expires_in": 3600
                            })),
                        )
                    }
                }
            }),
        );

    let oauth_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind oauth server");
    let oauth_addr = oauth_listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(oauth_listener, app).await;
    });
    let oauth_base = format!("http://{oauth_addr}");

    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");

    let bin = env!("CARGO_BIN_EXE_tekvra");

    // 1. `tekvra auth login --device-code` polls through `authorization_pending` and saves credentials.json!
    let dev_out = Command::new(bin)
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .env(
            "TEKVRA_OAUTH_DEVICE_URL",
            format!("{oauth_base}/device/code"),
        )
        .env(
            "TEKVRA_OAUTH_TOKEN_URL",
            format!("{oauth_base}/oauth/token"),
        )
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args([
            "auth",
            "login",
            "--provider",
            "openai",
            "--account-name",
            "seat1",
            "--device-code",
        ])
        .output()
        .await
        .expect("run device-code login");
    assert!(
        dev_out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&dev_out.stderr)
    );
    let dev_stdout = String::from_utf8_lossy(&dev_out.stdout);
    assert!(dev_stdout.contains("CLI-CODE"));
    assert!(dev_stdout.contains("Authenticated `openai:seat1`"));

    // 2. `tekvra auth login --no-browser` binds loopback PKCE listener, accepts browser callback, and exchanges code!
    let mut child = Command::new(bin)
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .env(
            "TEKVRA_OAUTH_AUTHORIZE_URL",
            format!("{oauth_base}/oauth/authorize"),
        )
        .env(
            "TEKVRA_OAUTH_TOKEN_URL",
            format!("{oauth_base}/oauth/token"),
        )
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args([
            "auth",
            "login",
            "--provider",
            "openai",
            "--account-name",
            "browser_seat",
            "--no-browser",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn loopback login");

    let stdout_pipe = child.stdout.take().expect("stdout pipe");
    let mut reader = BufReader::new(stdout_pipe).lines();
    let first_line = reader
        .next_line()
        .await
        .expect("read line")
        .expect("authorize line");
    let auth_url_str = first_line
        .strip_prefix("Authorize in browser: ")
        .expect("Authorize prefix")
        .trim();
    let parsed_auth = tekvra_providers_url_query(auth_url_str);
    let redirect_uri = parsed_auth.get("redirect_uri").expect("redirect_uri");
    let state_param = parsed_auth.get("state").expect("state");

    let cb_client = tekvra_providers::build_oauth_http_client(redirect_uri).expect("cb client");
    let cb_resp = cb_client
        .get(format!(
            "{redirect_uri}?code=pkce_code_123&state={state_param}"
        ))
        .send()
        .await
        .expect("send callback");
    assert!(cb_resp.status().is_success());

    let wait_status = child.wait().await.expect("wait loopback login");
    assert!(wait_status.success());

    // 3. Verify startup auto-refresh of `seat1` (`expires_in = -120`) during `tekvra exec`!
    let mock_openai = spawn_mock_openai_server(vec!["AUTO_REFRESH_EXEC_OK"]).await;
    std::fs::write(
        cfg_dir.join("providers.toml"),
        format!("version = 2\n\n[providers.openai]\nbase_url = \"{mock_openai}\"\n"),
    )
    .expect("write providers.toml");

    let exec_out = Command::new(bin)
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .env(
            "TEKVRA_OAUTH_TOKEN_URL",
            format!("{oauth_base}/oauth/token"),
        )
        .env_remove("OPENAI_API_KEY")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(["--no-plugins", "exec", "Verify refreshed token"])
        .output()
        .await
        .expect("run exec with auto-refresh");
    assert!(
        exec_out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&exec_out.stderr)
    );
    assert!(
        oauth_state.refreshes.load(Ordering::SeqCst) >= 1,
        "expected expired OAuth credential to auto-refresh on startup"
    );
    let creds_raw =
        std::fs::read_to_string(state_dir.join("credentials.json")).expect("read credentials.json");
    assert!(creds_raw.contains("at_auto_refreshed_from_cli"));
    assert!(creds_raw.contains("at_pkce_browser_ok"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_exec_output_file_and_resume_last_and_fork_preserve_conversation() {
    use tekvra_storage::StateIndex;

    let base_url = spawn_mock_openai_server(vec![
        "FIRST_TURN_SAVED",
        "RESUMED_SECOND_TURN",
        "FORKED_TURN",
    ])
    .await;

    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    let out_file = tmp.path().join("nested").join("last_msg.txt");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");

    std::fs::write(
        cfg_dir.join("providers.toml"),
        format!("version = 2\n\n[providers.openai]\nbase_url = \"{base_url}\"\n"),
    )
    .expect("write providers.toml");

    let bin = env!("CARGO_BIN_EXE_tekvra");

    // 1. `tekvra exec -o <file> "Initial prompt"` writes `-o` file and persists session in StateIndex!
    let turn1 = Command::new(bin)
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .env("OPENAI_API_KEY", "sk-test-resume")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args([
            "--no-plugins",
            "exec",
            "-o",
            out_file.to_str().expect("utf8"),
            "Initial prompt",
        ])
        .output()
        .await
        .expect("run turn 1");
    assert!(turn1.status.success());
    assert_eq!(
        std::fs::read_to_string(&out_file).expect("read -o file"),
        "FIRST_TURN_SAVED"
    );

    let idx = StateIndex::open(&state_dir).expect("open state index");
    let sessions_after_1 = idx.list_sessions(false, 10, 0).expect("list sessions");
    assert_eq!(sessions_after_1.len(), 1);
    let orig_id = sessions_after_1[0].id.clone();

    // 2. `tekvra resume --last --prompt "Follow-up prompt"` resumes `orig_id`!
    let turn2 = Command::new(bin)
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .env("OPENAI_API_KEY", "sk-test-resume")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args([
            "--no-plugins",
            "resume",
            "--last",
            "--prompt",
            "Follow-up prompt",
        ])
        .output()
        .await
        .expect("run resume --last");
    assert!(
        turn2.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&turn2.stderr)
    );
    assert!(String::from_utf8_lossy(&turn2.stdout).contains("RESUMED_SECOND_TURN"));

    // 3. `tekvra fork <orig_id> --prompt "Branch prompt"` creates a second session with `parent_id = Some(orig_id)`!
    let forked = Command::new(bin)
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .env("OPENAI_API_KEY", "sk-test-resume")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args([
            "--no-plugins",
            "fork",
            &orig_id,
            "--prompt",
            "Branch prompt",
        ])
        .output()
        .await
        .expect("run fork");
    assert!(
        forked.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&forked.stderr)
    );
    let fork_stdout = String::from_utf8_lossy(&forked.stdout);
    assert!(fork_stdout.contains("Forked session: thr_"));
    assert!(fork_stdout.contains("FORKED_TURN"));

    let sessions_after_fork = idx.list_sessions(false, 10, 0).expect("list sessions 2");
    assert_eq!(sessions_after_fork.len(), 2);
    assert!(
        sessions_after_fork
            .iter()
            .any(|s| s.parent_id.as_deref() == Some(orig_id.as_str())),
        "expected forked session to record parent_id={orig_id}"
    );
}

fn tekvra_providers_url_query(raw_url: &str) -> std::collections::BTreeMap<String, String> {
    let (_, query) = raw_url.split_once('?').unwrap_or((raw_url, ""));
    let mut out = std::collections::BTreeMap::new();
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let decoded = v.replace("%3A", ":").replace("%2F", "/");
            out.insert(k.to_string(), decoded);
        }
    }
    out
}

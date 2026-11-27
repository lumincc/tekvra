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
    spawn_mock_openai_bodies(queue).await
}

async fn spawn_mock_openai_bodies(bodies: VecDeque<Value>) -> String {
    let app = Router::new()
        .route("/v1/responses", post(handle_responses))
        .with_state(MockResponsesState {
            bodies: Arc::new(Mutex::new(bodies)),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_trust_status_allow_revoke_roundtrip() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");

    let bin = env!("CARGO_BIN_EXE_tekvra");
    let run = async |args: &[&str]| -> (i32, String) {
        let out = Command::new(bin)
            .args(args)
            .current_dir(&ws)
            .env("TEKVRA_CONFIG_DIR", &cfg_dir)
            .env("TEKVRA_STATE_DIR", &state_dir)
            .output()
            .await
            .expect("run tekvra trust");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string(),
        )
    };

    let (code, out) = run(&["trust", "status"]).await;
    assert_eq!(code, 0);
    assert!(out.contains("not trusted"), "initial status: {out}");

    let (code, out) = run(&["trust", "allow"]).await;
    assert_eq!(code, 0);
    assert!(out.contains("granted trust"), "allow: {out}");

    let (code, out) = run(&["trust", "status"]).await;
    assert_eq!(code, 0);
    assert!(
        out.contains("trusted:") && !out.contains("not trusted"),
        "post-allow: {out}"
    );

    let (code, out) = run(&["trust", "revoke"]).await;
    assert_eq!(code, 0);
    assert!(out.contains("revoked trust"), "revoke: {out}");

    // Trust state persists in TEKVRA_STATE_DIR (trusted_folders.toml), independent of config dir.
    assert!(
        state_dir.join("trusted_folders.toml").exists(),
        "trust store file must exist"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_mcp_add_list_remove_preserves_toml_comments() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");

    let project_toml = ws.join(".tekvra").join("tekvra.toml");
    std::fs::create_dir_all(ws.join(".tekvra")).expect("mkdir .tekvra");
    std::fs::write(
        &project_toml,
        "# model preferences\nversion = 2\nmodel = \"openai/gpt-5.4-codex\"\n\n# alpha stdio gateway\n[mcp_servers.alpha]\ncommand = \"alpha-cmd\"\nenabled = true\n",
    )
    .expect("write project toml");

    let bin = env!("CARGO_BIN_EXE_tekvra");
    let run = async |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(bin)
            .args(args)
            .current_dir(&ws)
            .env("OPENAI_API_KEY", "sk-test-cli")
            .env("TEKVRA_CONFIG_DIR", &cfg_dir)
            .env("TEKVRA_STATE_DIR", &state_dir)
            .output()
            .await
            .expect("run tekvra mcp");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };

    // Trust the workspace first: untrusted projects withhold MCP servers from merged config.
    let (code, _, err) = run(&["trust", "allow"]).await;
    assert_eq!(code, 0, "trust allow failed: {err}");

    // add stdio with `--` trailing args
    let (code, out, err) = run(&[
        "mcp",
        "add",
        "beta",
        "--command",
        "uvx",
        "--",
        "-y",
        "mcp-server-git",
    ])
    .await;
    assert_eq!(code, 0, "add failed: {out} {err}");

    let after_add = std::fs::read_to_string(&project_toml).expect("read toml");
    assert!(
        after_add.contains("# model preferences"),
        "document-level comment must survive: {after_add}"
    );
    assert!(after_add.contains("[mcp_servers.alpha]"));
    assert!(after_add.contains("[mcp_servers.beta]"));
    assert!(after_add.contains("command = \"uvx\""));
    assert!(after_add.contains("\"-y\"") && after_add.contains("\"mcp-server-git\""));

    // list shows both
    let (code, out, err) = run(&["mcp", "list"]).await;
    assert_eq!(code, 0, "list failed: {out} {err}");
    assert!(out.contains("alpha") && out.contains("beta"));

    // add http with bearer env var
    let (code, _, err) = run(&[
        "mcp",
        "add",
        "remote",
        "--url",
        "http://127.0.0.1:9/mcp",
        "--bearer-env",
        "REMOTE_TOKEN",
    ])
    .await;
    assert_eq!(code, 0, "http add failed: {err}");
    let toml_now = std::fs::read_to_string(&project_toml).expect("read toml");
    assert!(toml_now.contains("url = \"http://127.0.0.1:9/mcp\""));
    assert!(toml_now.contains("bearer_token_env_var = \"REMOTE_TOKEN\""));

    // command + url together is rejected
    let (code, _, _) = run(&["mcp", "add", "both", "--command", "x", "--url", "http://x"]).await;
    assert_ne!(code, 0, "command+url must be mutually exclusive");

    // remove keeps the comment and the untouched entries
    let (code, out, err) = run(&["mcp", "remove", "alpha"]).await;
    assert_eq!(code, 0, "remove failed: {out} {err}");
    let after_remove = std::fs::read_to_string(&project_toml).expect("read toml");
    assert!(
        after_remove.contains("# model preferences"),
        "document-level comment must survive removal: {after_remove}"
    );
    // The comment decorating the removed table goes with it (owned decor).
    assert!(!after_remove.contains("# alpha stdio gateway"));
    assert!(!after_remove.contains("[mcp_servers.alpha]"));
    assert!(after_remove.contains("[mcp_servers.beta]"));

    // removing a missing server errors
    let (code, _, _) = run(&["mcp", "remove", "ghost"]).await;
    assert_ne!(code, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_mcp_test_performs_oneshot_handshake_against_http_mock() {
    use std::net::SocketAddr;

    async fn mcp_dispatch(axum::Json(body): axum::Json<Value>) -> axum::response::Response {
        let method = body
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "serverInfo": { "name": "mock-mcp", "version": "0.0.1" }
            }),
            "tools/list" => json!({
                "tools": [
                    { "name": "echo", "description": "Echo a message", "inputSchema": { "type": "object" } },
                    { "name": "ping", "description": "Ping", "inputSchema": { "type": "object" } }
                ]
            }),
            _ => json!({}),
        };
        let payload = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        axum::response::Response::builder()
            .header("content-type", "application/json")
            .body(axum::body::Body::from(payload.to_string()))
            .expect("response")
    }

    let app: Router<()> = Router::new().route("/mcp", axum::routing::post(mcp_dispatch));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    std::fs::create_dir_all(ws.join(".tekvra")).expect("mkdir .tekvra");
    std::fs::write(
        ws.join(".tekvra").join("tekvra.toml"),
        format!("version = 2\n\n[mcp_servers.mock]\nurl = \"http://{addr}/mcp\"\nenabled = true\n"),
    )
    .expect("write toml");

    let bin = env!("CARGO_BIN_EXE_tekvra");
    // Untrusted projects withhold MCP servers from merged config: grant trust first.
    let pre = Command::new(bin)
        .args(["trust", "allow"])
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .output()
        .await
        .expect("trust allow");
    assert_eq!(pre.status.code(), Some(0));

    let out = Command::new(env!("CARGO_BIN_EXE_tekvra"))
        .args(["mcp", "test", "mock"])
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .output()
        .await
        .expect("run mcp test");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("connected, 2 tool(s)"), "{stdout}");
    assert!(stdout.contains("mock/echo") && stdout.contains("mock/ping"));
}

#[test]
fn cli_completion_scripts_for_all_four_shells() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");

    let bin = env!("CARGO_BIN_EXE_tekvra");
    let cases: [(&str, Vec<&str>); 4] = [
        (
            "bash",
            vec!["_tekvra()", "complete -F _tekvra tekvra", "--worktree"],
        ),
        ("zsh", vec!["#compdef tekvra", "_tekvra()"]),
        ("fish", vec!["complete -c tekvra", "__fish_use_subcommand"]),
        (
            "powershell",
            vec!["Register-ArgumentCompleter", "tekvra", "ParameterValue"],
        ),
    ];
    for (shell, needles) in &cases {
        let out = std::process::Command::new(bin)
            .args(["completion", shell])
            .current_dir(&ws)
            .env("TEKVRA_CONFIG_DIR", &cfg_dir)
            .env("TEKVRA_STATE_DIR", &state_dir)
            .output()
            .expect("run completion");
        assert_eq!(out.status.code(), Some(0), "completion {shell} failed");
        let script = String::from_utf8_lossy(&out.stdout).to_string();
        for needle in needles {
            assert!(
                script.contains(needle),
                "completion {shell} missing `{needle}`:\n{script}"
            );
        }
    }

    // Unsupported shell exits non-zero.
    let bad = std::process::Command::new(bin)
        .args(["completion", "csh"])
        .output()
        .expect("run completion csh");
    assert_ne!(bad.status.code(), Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_exec_image_flag_attaches_input_image_to_provider_payload() {
    use std::sync::Arc as StdArc;

    #[derive(Clone)]
    struct CaptureState(StdArc<Mutex<Vec<Value>>>);

    async fn handle_capture(
        State(st): State<CaptureState>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::Json<Value> {
        st.0.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(body);
        axum::Json(json!({
            "id": "resp_img",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "IMAGE_SEEN" }]
            }],
            "usage": { "input_tokens": 10, "output_tokens": 5 }
        }))
    }

    let captured = StdArc::new(Mutex::new(Vec::new()));
    let app: Router<()> = Router::new()
        .route("/v1/responses", post(handle_capture))
        .with_state(CaptureState(StdArc::clone(&captured)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    std::fs::write(
        cfg_dir.join("providers.toml"),
        format!("version = 2\n\n[providers.openai]\nbase_url = \"http://{addr}/v1\"\n"),
    )
    .expect("write providers.toml");

    // 1x1 PNG (smallest valid payload; only the extension/size matter for the CLI gate).
    let img_path = ws.join("ui-bug.png");
    std::fs::write(
        &img_path,
        [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
    )
    .expect("write png");

    let out = Command::new(env!("CARGO_BIN_EXE_tekvra"))
        .args([
            "exec",
            "--mode",
            "yolo",
            "-i",
            "ui-bug.png",
            "Describe this screenshot",
        ])
        .current_dir(&ws)
        .env("OPENAI_API_KEY", "sk-test-img")
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .output()
        .await
        .expect("run exec");
    assert_eq!(
        out.status.code(),
        Some(0),
        "exec -i failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("IMAGE_SEEN"));

    let serialized = {
        let bodies = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let last = bodies.last().expect("captured request body");
        serde_json::to_string(last).expect("json")
    };
    assert!(
        serialized.contains("input_image"),
        "image must reach the provider payload: {serialized}"
    );
    assert!(serialized.contains("data:image/png;base64,"));

    // Unsupported extension is rejected before any network call.
    std::fs::write(ws.join("notes.bmp"), b"xx").expect("write bmp");
    let out = Command::new(env!("CARGO_BIN_EXE_tekvra"))
        .args(["exec", "--mode", "yolo", "-i", "notes.bmp", "nope"])
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .output()
        .await
        .expect("run exec bmp");
    assert_ne!(out.status.code(), Some(0), "bmp must be rejected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_worktree_flag_routes_writes_into_managed_checkout() {
    let base_url = spawn_mock_openai_server(vec![]).await;
    let _ = base_url; // per-request queue handled by default fallback replies

    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&ws).expect("mkdir ws");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");

    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&ws)
            .output()
            .expect("git")
    };
    let init = git(&["init", "-q"]);
    assert!(
        init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    git(&["config", "user.email", "test@tekvra.local"]);
    git(&["config", "user.name", "Tekvra Test"]);
    std::fs::write(ws.join("README.md"), "seed").expect("seed file");
    git(&["add", "."]);
    let commit = git(&["commit", "-q", "-m", "seed"]);
    assert!(commit.status.success(), "git commit failed");

    std::fs::write(
        cfg_dir.join("providers.toml"),
        "version = 2\n\n[providers.openai]\nbase_url = \"http://127.0.0.1:9/unused\"\n",
    )
    .expect("write providers.toml");

    let bin = env!("CARGO_BIN_EXE_tekvra");

    // 1. `tekvra worktree create demo` creates the managed checkout.
    let out = Command::new(bin)
        .args(["worktree", "create", "demo"])
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .output()
        .await
        .expect("run worktree create");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("created"), "{stdout}");
    let wt_dir = state_dir.join("worktrees").join("repo").join("demo");
    assert!(wt_dir.join(".git").exists(), "worktree checkout must exist");
    assert!(
        wt_dir.join("README.md").exists(),
        "worktree must contain seeded files"
    );

    // 2. `list` shows the managed worktree; second `create` reuses it.
    let out = Command::new(bin)
        .args(["worktree", "list"])
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .output()
        .await
        .expect("run worktree list");
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("demo"));

    let out = Command::new(bin)
        .args(["worktree", "create", "demo"])
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .output()
        .await
        .expect("run worktree reuse");
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("reused"));

    // 3. Branch `tekvra/demo` exists in the repo.
    let branch = git(&["rev-parse", "--verify", "refs/heads/tekvra/demo"]);
    assert!(branch.status.success(), "tekvra/demo branch must exist");

    // 4. Removing a missing worktree errors without side effects; removing an existing one works.
    let out = Command::new(bin)
        .args(["worktree", "remove", "ghost"])
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .output()
        .await
        .expect("run worktree remove ghost");
    assert_ne!(
        out.status.code(),
        Some(0),
        "removing a missing worktree must fail"
    );
    assert!(
        !state_dir
            .join("worktrees")
            .join("repo")
            .join("ghost")
            .exists()
    );

    let out = Command::new(bin)
        .args(["worktree", "remove", "demo"])
        .current_dir(&ws)
        .env("TEKVRA_CONFIG_DIR", &cfg_dir)
        .env("TEKVRA_STATE_DIR", &state_dir)
        .output()
        .await
        .expect("run worktree remove demo");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!wt_dir.join(".git").exists(), "worktree must be removed");
}

#[test]
// install.sh is a POSIX shell script (bash + tar + sha256sum + file:// curl);
// Windows distribution is served by install.ps1, so this smoke test cannot run there.
#[cfg_attr(
    windows,
    ignore = "install.sh is a POSIX shell script; Windows ships install.ps1"
)]
fn installer_sh_verifies_sha256_and_rejects_mismatch() {
    use std::process::Command as StdCommand;

    let tmp = TempDir::new().expect("tempdir");
    let dist = tmp.path().join("dist");
    let prefix = tmp.path().join("prefix");
    // Release layout: <DOWNLOAD_PREFIX>/<VERSION>/<assets>.
    let dist_versioned = dist.join("v0.1.0");
    std::fs::create_dir_all(&dist_versioned).expect("mkdir dist");

    // Package a real release bundle through ci_verify_packaging.py so the
    // fixture mirrors the exact nested layout (`tekvra-<tag>-<target>/tekvra`)
    // published by the release pipeline; any packaging layout drift fails here
    // instead of silently breaking install.sh / install.ps1 for real users.
    let payload_dir = tmp.path().join("payload");
    std::fs::create_dir_all(&payload_dir).expect("mkdir payload");
    std::fs::write(payload_dir.join("tekvra"), "#!/bin/sh\necho FAKE_TEKVRA\n")
        .expect("write fake bin");
    // Mirror install.sh's host detection so the fixture matches what the
    // installer will request on this machine.
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let target = format!(
        "{}-{}",
        std::env::consts::ARCH,
        match os {
            "linux" => "unknown-linux-gnu",
            "darwin" => "apple-darwin",
            other => other,
        }
    );
    let archive_name = format!("tekvra-v0.1.0-{target}.tar.gz");
    let packager = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/ci_verify_packaging.py"
    );
    let python = find_python_interpreter()
        .expect("python3 or python must be available to package the installer fixture");
    let packaged = StdCommand::new(&python)
        .args([
            packager,
            "--package-release",
            "--version",
            "v0.1.0",
            "--target",
            &target,
            "--binary",
        ])
        .arg(payload_dir.join("tekvra"))
        .arg("--out-dir")
        .arg(&dist_versioned)
        .output()
        .expect("run ci_verify_packaging.py --package-release");
    assert!(
        packaged.status.success(),
        "packaging fixture failed: {}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    // The packager emits `<digest>  <archive>` in a `<archive>.sha256` sidecar;
    // the release publish job assembles SHA256SUMS.txt by concatenating those
    // sidecars (`cat *.sha256 | sort -k2`), so mirror that content here.
    let sums_line = std::fs::read_to_string(dist_versioned.join(format!("{archive_name}.sha256")))
        .expect("read packager sha256 sidecar");
    let good_sums = sums_line;
    let bad_sums = format!("{}  {archive_name}\n", "0".repeat(64));

    let installer = concat!(env!("CARGO_MANIFEST_DIR"), "/../../install.sh");
    let download_prefix = format!("file://{}", dist.display());

    // 1. Mismatched digest -> non-zero exit, nothing installed.
    std::fs::write(dist_versioned.join("SHA256SUMS.txt"), &bad_sums).expect("write bad sums");
    let out = StdCommand::new("bash")
        .arg(installer)
        .env("TEKVRA_VERSION", "v0.1.0")
        .env("TEKVRA_INSTALL_PREFIX", &prefix)
        .env("TEKVRA_DOWNLOAD_PREFIX", &download_prefix)
        .output()
        .expect("run install.sh with bad checksum");
    assert_ne!(out.status.code(), Some(0), "mismatched checksum must abort");
    assert!(
        !prefix.join("bin").join("tekvra").exists(),
        "no binary may be installed on mismatch"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("checksum mismatch"), "{stderr}");

    // 2. Matching digest -> exit 0, binary installed and executable.
    std::fs::write(dist_versioned.join("SHA256SUMS.txt"), good_sums).expect("write good sums");
    let out = StdCommand::new("bash")
        .arg(installer)
        .env("TEKVRA_VERSION", "v0.1.0")
        .env("TEKVRA_INSTALL_PREFIX", &prefix)
        .env("TEKVRA_DOWNLOAD_PREFIX", &download_prefix)
        .output()
        .expect("run install.sh with good checksum");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let installed = prefix.join("bin").join("tekvra");
    assert!(installed.exists(), "binary must be installed");
    let run = StdCommand::new(&installed)
        .output()
        .expect("run installed binary");
    assert!(String::from_utf8_lossy(&run.stdout).contains("FAKE_TEKVRA"));
}

/// Probes for a working Python 3 interpreter (`python3`, then `python`),
/// rejecting Windows Store alias stubs that exit non-zero on `--version`.
fn find_python_interpreter() -> Option<String> {
    for candidate in ["python3", "python"] {
        let works = std::process::Command::new(candidate)
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        if works {
            return Some(candidate.to_string());
        }
    }
    None
}

#[test]
// install.ps1 is a Windows PowerShell script (Invoke-WebRequest, Expand-Archive,
// Get-FileHash); Unix distribution is served by install.sh.
#[cfg(windows)]
fn installer_ps1_verifies_sha256_and_installs_nested_bundle() {
    use std::process::Command as StdCommand;

    let tmp = TempDir::new().expect("tempdir");
    let dist = tmp.path().join("dist");
    let prefix = tmp.path().join("prefix");
    // Release layout: <DOWNLOAD_PREFIX>/<VERSION>/<assets>.
    let dist_versioned = dist.join("v0.1.0");
    std::fs::create_dir_all(&dist_versioned).expect("mkdir dist");

    // Package a real release bundle through ci_verify_packaging.py so the
    // fixture mirrors the exact nested layout
    // (`tekvra-<tag>-<target>/tekvra.exe`) published by the release pipeline.
    // The payload must be a valid PE so the installed binary stays executable;
    // a copy of cmd.exe prints on demand without shipping any extra fixture.
    let payload_dir = tmp.path().join("payload");
    std::fs::create_dir_all(&payload_dir).expect("mkdir payload");
    let payload_path = payload_dir.join("tekvra.exe");
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let cmd_exe = std::path::Path::new(&system_root)
        .join("System32")
        .join("cmd.exe");
    std::fs::copy(&cmd_exe, &payload_path).expect("copy cmd.exe as fake binary");

    // The packager fixture only builds the x86_64 archive, so on ARM64 hosts
    // this exercises install.ps1's documented fall-back from aarch64 (probed,
    // missing) to x86_64-pc-windows-msvc.
    let target = "x86_64-pc-windows-msvc";
    let archive_name = format!("tekvra-v0.1.0-{target}.zip");
    let packager = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/ci_verify_packaging.py"
    );
    let python = find_python_interpreter()
        .expect("python3 or python must be available to package the installer fixture");
    let packaged = StdCommand::new(&python)
        .args([
            packager,
            "--package-release",
            "--version",
            "v0.1.0",
            "--target",
            target,
            "--binary",
        ])
        .arg(&payload_path)
        .arg("--out-dir")
        .arg(&dist_versioned)
        .output()
        .expect("run ci_verify_packaging.py --package-release");
    assert!(
        packaged.status.success(),
        "packaging fixture failed: {}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    // Mirror the publish job's `cat *.sha256 | sort -k2 > SHA256SUMS.txt`.
    let sums_line = std::fs::read_to_string(dist_versioned.join(format!("{archive_name}.sha256")))
        .expect("read packager sha256 sidecar");
    let good_sums = sums_line;
    let bad_sums = format!("{}  {archive_name}\n", "0".repeat(64));

    let installer = concat!(env!("CARGO_MANIFEST_DIR"), "/../../install.ps1");
    let download_prefix = format!("file://{}", dist.display());
    // Prefer PowerShell 7; Windows PowerShell 5.1 works too because
    // install.ps1 resolves file:// download prefixes locally.
    let has_pwsh = StdCommand::new("pwsh")
        .args(["-NoProfile", "-Command", "$PSVersionTable.PSVersion.Major"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    let shell = if has_pwsh { "pwsh" } else { "powershell" };
    let installer_cmd = |sums: &str| {
        std::fs::write(dist_versioned.join("SHA256SUMS.txt"), sums).expect("write SHA256SUMS.txt");
        StdCommand::new(shell)
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                installer,
            ])
            .env("TEKVRA_VERSION", "v0.1.0")
            .env("TEKVRA_INSTALL_PREFIX", &prefix)
            .env("TEKVRA_DOWNLOAD_PREFIX", &download_prefix)
            .output()
            .expect("run install.ps1")
    };

    // 1. Mismatched digest -> non-zero exit, nothing installed.
    let out = installer_cmd(&bad_sums);
    assert_ne!(out.status.code(), Some(0), "mismatched checksum must abort");
    assert!(
        !prefix.join("bin").join("tekvra.exe").exists(),
        "no binary may be installed on mismatch"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("checksum mismatch"), "{stderr}");

    // 2. Matching digest -> exit 0, nested-bundle binary installed and runnable.
    let out = installer_cmd(&good_sums);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("installed tekvra"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let installed = prefix.join("bin").join("tekvra.exe");
    let installed_bytes = std::fs::read(&installed).expect("read installed binary");
    let payload_bytes = std::fs::read(&payload_path).expect("read payload binary");
    assert_eq!(
        installed_bytes, payload_bytes,
        "installed binary must match the packaged payload"
    );
    let run = StdCommand::new(&installed)
        .args(["/d", "/c", "echo", "FAKE_TEKVRA"])
        .output()
        .expect("run installed binary");
    assert!(String::from_utf8_lossy(&run.stdout).contains("FAKE_TEKVRA"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_exec_resolves_question_tool_prompts_without_hanging() {
    // Turn 1 asks the `question` tool; headless exec must resolve the pending
    // `UserInputRequested` event itself (empty answers) and continue to turn 2.
    let bodies = VecDeque::from(vec![
        json!({
            "id": "resp_q1",
            "output": [{
                "type": "function_call",
                "call_id": "qcall_1",
                "name": "question",
                "arguments": "{\"questions\":[\"Which database should we use?\"]}"
            }],
            "usage": { "input_tokens": 40, "output_tokens": 12 }
        }),
        json!({
            "id": "resp_q2",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "QUESTION_RESOLVED_HEADLESS" }]
            }],
            "usage": { "input_tokens": 40, "output_tokens": 8 }
        }),
    ]);
    let base_url = spawn_mock_openai_bodies(bodies).await;

    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    std::fs::write(
        cfg_dir.join("tekvra.toml"),
        "version = 2\nmodel = \"openai/gpt-5.4-codex\"\n",
    )
    .expect("write tekvra.toml");
    std::fs::write(
        cfg_dir.join("providers.toml"),
        format!("version = 2\n\n[providers.openai]\nbase_url = \"{base_url}\"\n"),
    )
    .expect("write providers.toml");

    let bin = env!("CARGO_BIN_EXE_tekvra");
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        Command::new(bin)
            .current_dir(&ws)
            .env("TEKVRA_CONFIG_DIR", &cfg_dir)
            .env("TEKVRA_STATE_DIR", &state_dir)
            .env("OPENAI_API_KEY", "sk-test-cli")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .args(["--no-plugins", "exec", "Ask me about databases"])
            .output(),
    )
    .await
    .expect("headless exec must not hang on a `question` turn")
    .expect("run exec");
    assert!(
        out.status.success(),
        "exit 0 expected, stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("QUESTION_RESOLVED_HEADLESS"),
        "turn must continue past the question: {stdout}"
    );
}

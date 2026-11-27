//! Comprehensive specification tests for `tekvra-tools` (`native-tools-and-pty` & `permissions-and-safety`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tekvra_config::{AgentSettings, PermissionRule};
use tekvra_protocol::{
    ApprovalResolution, ApprovalScope, FileChangeKind, GrantLifetime, PermissionMode, RuleAction,
    ToolOutput,
};
use tekvra_providers::ModelCapabilities;
use tekvra_storage::{StateIndex, WorkspaceSnapshotStore};
use tekvra_tools::{
    McpGateway, McpToolDescriptor, MontyCodeSandbox, NativeToolRegistry, PermissionDecision,
    PermissionEvaluator, PtySessionManager, SafeWebFetcher, ToolAction, ToolExecutionContext,
    WorkspacePathLocks, build_tool_definitions_for_model, decompose_bash_command,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn make_ctx(
    ws: PathBuf,
    state_dir: PathBuf,
    mode: PermissionMode,
    rules: Vec<PermissionRule>,
) -> ToolExecutionContext {
    let index = StateIndex::open(&state_dir).expect("open state index");
    let snapshots = WorkspaceSnapshotStore::new(&state_dir);
    let evaluator = PermissionEvaluator::new(
        mode,
        std::fs::canonicalize(&ws).expect("canon ws"),
        Some(ws.clone()),
        rules,
        Vec::new(),
    );
    ToolExecutionContext {
        session_id: "thr_tools_01".to_string(),
        turn_id: "turn_01".to_string(),
        workspace_root: std::fs::canonicalize(&ws).expect("canon ws"),
        settings: AgentSettings::default(),
        path_locks: WorkspacePathLocks::new(),
        pty_manager: PtySessionManager::new(None),
        web_fetcher: SafeWebFetcher::new(&[]),
        code_sandbox: MontyCodeSandbox,
        permissions: Arc::new(Mutex::new(evaluator)),
        state_index: Some(Arc::new(Mutex::new(index))),
        snapshots: Some(snapshots),
        mcp_gateway: None,
        approval_cb: None,
        question_cb: None,
        plan_markdown: Arc::new(Mutex::new(String::new())),
    }
}

#[tokio::test]
async fn zero_lua_grep_skips_env_with_truncation_and_tree_sitter_file_index_extracts_symbols() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(ws.join("src")).expect("mkdir");

    // Write credential files that MUST be skipped during workspace scans
    std::fs::write(ws.join(".env"), "NEEDLE_SECRET=top_secret_123\n").expect("write .env");
    std::fs::write(ws.join(".env.production"), "NEEDLE_SECRET=prod_456\n")
        .expect("write .env.prod");
    std::fs::write(ws.join("deploy.pem"), "NEEDLE_SECRET=pem_key\n").expect("write pem");

    // Write legitimate source files containing NEEDLE and Rust symbols
    std::fs::write(
        ws.join("src/lib.rs"),
        "pub struct Engine {\n    pub active: bool,\n}\n\npub fn run_needle() -> &'static str {\n    \"NEEDLE_1\"\n}\n\npub fn second_needle() -> &'static str {\n    \"NEEDLE_2\"\n}\n",
    )
    .expect("write lib.rs");

    let ctx = make_ctx(ws, state, PermissionMode::Build, Vec::new());
    let reg = NativeToolRegistry::new();

    // 1. `file_grep` with `max_matches = 1` skips `.env*` & `.pem` and reports `truncated = true` + `skipped_sensitive_files = 3`
    let grep_out = reg
        .execute(
            &ctx,
            "c1",
            "file_grep",
            &json!({ "pattern": "NEEDLE", "max_matches": 1 }),
        )
        .await
        .expect("file_grep");

    let ToolOutput::Search {
        matches,
        truncated,
        skipped_sensitive_files,
        ..
    } = grep_out
    else {
        panic!("expected Search output");
    };
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].path, PathBuf::from("src/lib.rs"));
    assert!(truncated, "scan cap should set truncated = true");
    assert_eq!(skipped_sensitive_files, 3);

    // 2. `file_index` extracts exact 1-based line ranges via native tree-sitter without Lua
    let idx_out = reg
        .execute(&ctx, "c2", "file_index", &json!({ "path": "src/lib.rs" }))
        .await
        .expect("file_index");
    let ToolOutput::Outline {
        language, symbols, ..
    } = idx_out
    else {
        panic!("expected Outline");
    };
    assert_eq!(language, "rust");
    assert_eq!(symbols.len(), 3);
    assert_eq!(symbols[0].name, "Engine");
    assert_eq!(symbols[0].kind, "struct");
    assert_eq!((symbols[0].start_line, symbols[0].end_line), (1, 3));
    assert_eq!(symbols[1].name, "run_needle");
    assert_eq!((symbols[1].start_line, symbols[1].end_line), (5, 7));
}

#[tokio::test]
async fn single_editor_model_routing_and_multi_file_apply_patch_with_move_and_snapshots() {
    let codex = ModelCapabilities::resolve("openai", "gpt-5.4-codex", None);
    let claude = ModelCapabilities::resolve("anthropic", "claude-sonnet-4-5", None);

    let codex_tools = build_tool_definitions_for_model(&codex);
    assert!(codex_tools.iter().any(|t| t.name == "apply_patch"));
    assert!(!codex_tools.iter().any(|t| t.name == "file_edit"));
    assert!(!codex_tools.iter().any(|t| t.name == "file_write"));

    let claude_tools = build_tool_definitions_for_model(&claude);
    assert!(!claude_tools.iter().any(|t| t.name == "apply_patch"));
    assert!(claude_tools.iter().any(|t| t.name == "file_edit"));
    assert!(claude_tools.iter().any(|t| t.name == "file_write"));

    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&ws).expect("mkdir");
    std::fs::write(
        ws.join("old_name.txt"),
        "header\n  alpha_trailing   \nfooter\n",
    )
    .expect("write old_name");
    std::fs::write(ws.join("obsolete.txt"), "delete me\n").expect("write obsolete");

    let ctx = make_ctx(ws.clone(), state.clone(), PermissionMode::Build, Vec::new());
    let reg = NativeToolRegistry::new();

    let patch = concat!(
        "*** Begin Patch\n",
        "*** Add File: created/new.txt\n",
        "+hello world\n",
        "*** Update File: old_name.txt\n",
        "*** Move to: renamed/updated.txt\n",
        "@@ header\n",
        "-alpha_trailing\n",
        "+beta_clean\n",
        "*** Delete File: obsolete.txt\n",
        "*** End Patch"
    );

    let out = reg
        .execute(&ctx, "c_patch", "apply_patch", &json!({ "patch": patch }))
        .await
        .expect("apply_patch succeeds");

    let ToolOutput::Diff { files, snapshot_id } = out else {
        panic!("expected Diff");
    };
    assert!(
        snapshot_id.is_some(),
        "pre-mutation snapshot must be recorded"
    );
    assert_eq!(files.len(), 3);
    assert_eq!(files[0].change_kind, FileChangeKind::Add);
    assert_eq!(files[1].change_kind, FileChangeKind::Move);
    assert_eq!(files[1].moved_from, Some(PathBuf::from("old_name.txt")));
    assert_eq!(files[2].change_kind, FileChangeKind::Delete);

    assert_eq!(
        std::fs::read_to_string(ws.join("created/new.txt")).expect("read new"),
        "hello world\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("renamed/updated.txt")).expect("read moved"),
        "header\nbeta_clean\nfooter\n"
    );
    assert!(!ws.join("old_name.txt").exists());
    assert!(!ws.join("obsolete.txt").exists());
}

#[tokio::test]
async fn interactive_pty_prompt_write_stdin_and_failing_test_filter_with_tool_output_paging() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&ws).expect("mkdir");

    let ctx = make_ctx(ws, state, PermissionMode::Auto, Vec::new());
    let reg = NativeToolRegistry::new();

    // 1. Interactive process prompting for stdin without exiting in initial yield window
    let start_out = reg
        .execute(
            &ctx,
            "pty1",
            "exec_command",
            &json!({
                "cmd": "printf 'Enter token: '; read tok; printf 'GOT:%s\\n' \"$tok\"",
                "yield_time_ms": 200
            }),
        )
        .await
        .expect("exec interactive");

    let ToolOutput::Terminal {
        session_id: Some(sid),
        exit_code: None,
        filtered_output: prompt_out,
        ..
    } = start_out
    else {
        panic!("expected running PTY receipt, got {start_out:?}");
    };
    assert!(prompt_out.contains("Enter token:"));

    // Deliver stdin via `write_stdin`
    let finish_out = reg
        .execute(
            &ctx,
            "pty2",
            "write_stdin",
            &json!({
                "session_id": sid,
                "chars": "codex42\n",
                "yield_time_ms": 1500
            }),
        )
        .await
        .expect("write_stdin");

    let ToolOutput::Terminal {
        session_id: None,
        exit_code: Some(0),
        filtered_output: done_text,
        ..
    } = finish_out
    else {
        panic!("expected completed PTY output, got {finish_out:?}");
    };
    assert!(done_text.contains("GOT:codex42"), "got: {done_text}");

    // 2. Failing build/test command with CR progress bars + 400 lines of noise -> non-zero exit preserved,
    // `[filtered: ...]` summary + `output_id`, and raw log searchable via `tool_output`!
    let noisy_fail_cmd = concat!(
        "printf 'Progress 10%%\\rProgress 50%%\\rProgress 100%%\\n'; ",
        "i=1; while [ $i -le 300 ]; do echo \"   Compiling dep_$i v0.1.0\"; i=$((i+1)); done; ",
        "echo 'test auth::critical_case ... FAILED'; ",
        "echo 'CRITICAL_ASSERTION_MARKER_991'; ",
        "exit 101"
    );

    let fail_out = reg
        .execute(
            &ctx,
            "pty3",
            "exec_command",
            &json!({
                "cmd": noisy_fail_cmd,
                "yield_time_ms": 3000
            }),
        )
        .await
        .expect("exec failing test");

    let ToolOutput::Terminal {
        exit_code: Some(code),
        filtered_output,
        output_id: Some(oid),
        was_filtered,
        ..
    } = fail_out
    else {
        panic!("expected filtered failing Terminal output, got {fail_out:?}");
    };
    assert_eq!(code, 101, "non-zero exit code MUST be preserved");
    assert!(was_filtered);
    assert!(
        !filtered_output.contains("Progress 10%"),
        "CR progress bar must collapse to final row"
    );
    assert!(filtered_output.contains("Progress 100%"));
    assert!(filtered_output.contains("CRITICAL_ASSERTION_MARKER_991"));
    assert!(filtered_output.contains("[filtered:"));

    // Page/search raw retained output via `tool_output`
    let paged = reg
        .execute(
            &ctx,
            "out1",
            "tool_output",
            &json!({
                "output_id": oid,
                "regex": "Compiling dep_150|CRITICAL_ASSERTION_MARKER_991"
            }),
        )
        .await
        .expect("tool_output regex search");
    let text = paged.to_model_text();
    assert!(text.contains("Compiling dep_150"));
    assert!(text.contains("CRITICAL_ASSERTION_MARKER_991"));
}

#[derive(Default)]
struct MockMcp {
    calls: AtomicUsize,
}

#[async_trait]
impl McpGateway for MockMcp {
    fn list_tools(&self) -> Vec<McpToolDescriptor> {
        vec![
            McpToolDescriptor {
                server: "docs".to_string(),
                tool: "lookup".to_string(),
                description: "Lookup docs".to_string(),
                input_schema: json!({"type": "object"}),
            },
            McpToolDescriptor {
                server: "docs".to_string(),
                tool: "forbidden".to_string(),
                description: "Forbidden tool".to_string(),
                input_schema: json!({"type": "object"}),
            },
        ]
    }

    async fn call_tool(
        &self,
        _server: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<Value, tekvra_tools::ToolsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({ "tool": tool, "echo": arguments }))
    }
}

#[tokio::test]
async fn bash_ast_decomposition_modes_scope_ladders_ssrf_redirects_and_cache_stable_monty_mcp() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&ws).expect("mkdir");
    let canon_ws = std::fs::canonicalize(&ws).expect("canon");

    // 1. Bash AST decomposition: `git status && rm -rf build/` MUST split and require auth for `rm -rf build/`!
    let decomp = decompose_bash_command("git status && rm -rf build/");
    assert!(!decomp.once_only);
    assert_eq!(decomp.subcommands.len(), 2);
    assert_eq!(decomp.subcommands[0].tokens, vec!["git", "status"]);
    assert_eq!(decomp.subcommands[1].tokens, vec!["rm", "-rf", "build/"]);

    // Dynamic `eval` or `bash -c "$DYNAMIC"` MUST set `once_only = true`!
    assert!(decompose_bash_command("eval \"$DANGEROUS\"").once_only);
    assert!(decompose_bash_command("bash -c \"$DYNAMIC\"").once_only);

    let rules = vec![
        PermissionRule {
            tool: "exec_command".to_string(),
            pattern: "rm -rf /".to_string(),
            action: RuleAction::Deny,
        },
        PermissionRule {
            tool: "mcp".to_string(),
            pattern: "docs/forbidden".to_string(),
            action: RuleAction::Deny,
        },
    ];

    // 2. Plan mode blocks `apply_patch` / `file_write` with guidance to use `plan`
    let mut eval = PermissionEvaluator::new(
        PermissionMode::Plan,
        canon_ws.clone(),
        Some(canon_ws.clone()),
        rules.clone(),
        Vec::new(),
    );
    match eval.evaluate(
        "i1",
        &ToolAction::WriteWorkspace {
            tool_name: "apply_patch".to_string(),
            paths: vec![PathBuf::from("src/main.rs")],
        },
    ) {
        PermissionDecision::Deny { reason } => assert!(reason.contains("plan")),
        other => panic!("expected Deny in Plan mode, got {other:?}"),
    }

    // 3. Build mode prompts specifically on `rm -rf build/` in `git status && rm -rf build/`,
    // and `cargo test -p tekvra-core` produces narrow-to-broad ladder requiring `confirmed_broad` on `cargo *`!
    eval.set_mode(PermissionMode::Build);
    let PermissionDecision::Prompt(rm_prompt) = eval.evaluate(
        "i2",
        &ToolAction::Shell {
            command: "git status && rm -rf build/".to_string(),
            cwd: canon_ws.clone(),
        },
    ) else {
        panic!("expected Prompt for uncovered `rm -rf build/`");
    };
    assert_eq!(rm_prompt.summary, "rm -rf build/");

    let PermissionDecision::Prompt(cargo_prompt) = eval.evaluate(
        "i3",
        &ToolAction::Shell {
            command: "cargo test -p tekvra-core".to_string(),
            cwd: canon_ws.clone(),
        },
    ) else {
        panic!("expected Prompt for `cargo test -p tekvra-core`");
    };
    assert!(
        cargo_prompt
            .ladder
            .iter()
            .any(|r| r.label.contains("cargo test *") && !r.broad_grant)
    );
    let broad_rung = cargo_prompt
        .ladder
        .iter()
        .find(|r| r.label.contains("cargo *"))
        .expect("whole-program cargo * rung");
    assert!(broad_rung.broad_grant);

    // Persisting `cargo *` without `confirmed_broad` MUST fail!
    assert!(
        eval.apply_resolution(
            &cargo_prompt,
            ApprovalResolution::Approve {
                scope: Some(broad_rung.scope.clone()),
                lifetime: GrantLifetime::Conversation,
                confirmed_broad: false,
            },
            None,
        )
        .is_err()
    );
    // Narrower `cargo test *` succeeds without `confirmed_broad`!
    eval.apply_resolution(
        &cargo_prompt,
        ApprovalResolution::Approve {
            scope: Some(ApprovalScope::CommandPrefix {
                tokens: vec!["cargo".to_string(), "test".to_string()],
            }),
            lifetime: GrantLifetime::Conversation,
            confirmed_broad: false,
        },
        None,
    )
    .expect("grant cargo test *");
    assert_eq!(
        eval.evaluate(
            "i4",
            &ToolAction::Shell {
                command: "cargo test --workspace".to_string(),
                cwd: canon_ws.clone(),
            },
        ),
        PermissionDecision::Allow
    );

    // Dynamic `eval` refuses `Conversation` lifetime!
    let PermissionDecision::Prompt(eval_prompt) = eval.evaluate(
        "i5",
        &ToolAction::Shell {
            command: "eval \"$FOO\"".to_string(),
            cwd: canon_ws.clone(),
        },
    ) else {
        panic!("expected once_only prompt for eval");
    };
    assert!(eval_prompt.once_only);
    assert!(
        eval.apply_resolution(
            &eval_prompt,
            ApprovalResolution::Approve {
                scope: None,
                lifetime: GrantLifetime::Conversation,
                confirmed_broad: true,
            },
            None,
        )
        .is_err()
    );

    // 4. Explicit `deny` rule enforced even in `Auto` and `Yolo` modes!
    for mode in [PermissionMode::Auto, PermissionMode::Yolo] {
        eval.set_mode(mode);
        assert!(matches!(
            eval.evaluate(
                "i6",
                &ToolAction::Shell {
                    command: "echo hi && rm -rf /".to_string(),
                    cwd: canon_ws.clone(),
                },
            ),
            PermissionDecision::Deny { .. }
        ));
    }

    // 5. SSRF-safe `webfetch` blocks localhost and metadata IP redirects (`169.254.169.254` & `127.0.0.1`)!
    let redirect_srv = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let srv_addr = redirect_srv.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut sock, _) = redirect_srv.accept().await.expect("accept");
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf).await;
        let resp = "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = sock.write_all(resp.as_bytes()).await;
    });

    // Without allowlist, direct loopback is blocked immediately
    let strict_fetcher = SafeWebFetcher::new(&[]);
    assert!(
        strict_fetcher
            .fetch(&format!("http://{srv_addr}/start"))
            .await
            .is_err()
    );

    // Even when initial host is allowlisted, redirecting to `169.254.169.254` MUST abort before connecting!
    let hop_fetcher = SafeWebFetcher::new(&["127.0.0.1".to_string()]);
    let redir_err = hop_fetcher
        .fetch(&format!("http://{srv_addr}/start"))
        .await
        .expect_err("redirect to cloud metadata IP must fail");
    assert!(redir_err.to_string().contains("169.254.169.254"));

    // 6. Isolated Monty `code` + prompt-cache-stable MCP gateway
    let codex_caps = ModelCapabilities::resolve("openai", "gpt-5.4-codex", None);
    let tools_before_mcp = build_tool_definitions_for_model(&codex_caps);

    let mut ctx = make_ctx(ws, state, PermissionMode::Auto, rules);
    let mock_mcp = Arc::new(MockMcp::default());
    ctx.mcp_gateway = Some(mock_mcp.clone());

    let tools_after_mcp = build_tool_definitions_for_model(&codex_caps);
    assert_eq!(
        tools_before_mcp, tools_after_mcp,
        "top-level LLM tool schemas MUST remain byte-identical when MCP servers connect"
    );

    let reg = NativeToolRegistry::new();
    // Concurrent MCP calls inside one Monty `code` snippet (`ResolveFutures` / `mcp_call_sync`)
    let code_out = reg
        .execute(
            &ctx,
            "code1",
            "code",
            &json!({
                "code": "tools = mcp_list()\nr1 = mcp_call_sync('docs', 'lookup', {'q': 'rust'})\nr2 = mcp_call_sync('docs', 'lookup', {'q': 'tokio'})\nprint(len(tools), r1['echo']['q'], r2['echo']['q'])"
            }),
        )
        .await
        .expect("monty code runs");

    let ToolOutput::CodeExecution { output, mcp_calls } = code_out else {
        panic!("expected CodeExecution");
    };
    assert_eq!(mcp_calls, 2);
    assert_eq!(output.trim(), "2 rust tokio");

    // Denied MCP tool (`docs/forbidden`) inside `code` MUST be rejected by PermissionEvaluator!
    assert!(
        reg.execute(
            &ctx,
            "code2",
            "code",
            &json!({ "code": "mcp_call_sync('docs', 'forbidden', {})" }),
        )
        .await
        .is_err()
    );

    // Direct OS/filesystem access inside `code` MUST be blocked by Monty sandbox!
    assert!(
        reg.execute(
            &ctx,
            "code3",
            "code",
            &json!({ "code": "open('/etc/passwd', 'r')" }),
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn configured_mcp_gateway_stdio_and_http_roundtrip_via_monty_code() {
    use indexmap::IndexMap;
    use std::fs;
    use tekvra_config::{AgentSettings, McpServerConfig};
    use tekvra_tools::ConfiguredMcpGateway;

    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    let state = tmp.path().join("state");
    fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    fs::create_dir_all(&state).expect("mkdir state");

    // 1. Write a minimal stdio JSON-RPC 2.0 MCP server script using python3
    let stdio_script = tmp.path().join("mock_mcp_stdio.py");
    fs::write(
        &stdio_script,
        r#"import json, sys
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
        res = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "stdio-demo", "version": "1.0"}}
    elif method == "tools/list":
        res = {"tools": [{"name": "upper", "description": "Uppercase text", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        txt = msg["params"]["arguments"].get("text", "")
        res = {"content": [{"type": "text", "text": txt.upper()}], "upper": txt.upper()}
    else:
        res = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": res}) + "\n")
    sys.stdout.flush()
"#,
    )
    .expect("write stdio mcp script");

    // 2. Spawn an in-process HTTP JSON-RPC 2.0 MCP endpoint over TcpListener
    let http_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind http mcp");
    let http_addr = http_listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = http_listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let req_str = String::from_utf8_lossy(&buf[..n]);
                let body_str = req_str.split("\r\n\r\n").nth(1).unwrap_or("{}");
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(body_str) else {
                    return;
                };
                let Some(mid) = msg.get("id").cloned() else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                };
                let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
                let result = match method {
                    "initialize" => json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "http-demo", "version": "1.0" }
                    }),
                    "tools/list" => json!({
                        "tools": [{
                            "name": "add",
                            "description": "Add two integers",
                            "inputSchema": { "type": "object" }
                        }]
                    }),
                    "tools/call" => {
                        let args = &msg["params"]["arguments"];
                        let sum = args["a"].as_i64().unwrap_or(0) + args["b"].as_i64().unwrap_or(0);
                        json!({
                            "content": [{ "type": "text", "text": sum.to_string() }],
                            "sum": sum
                        })
                    }
                    _ => json!({}),
                };
                let resp_json =
                    json!({ "jsonrpc": "2.0", "id": mid, "result": result }).to_string();
                let http_resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp_json.len(),
                    resp_json
                );
                let _ = stream.write_all(http_resp.as_bytes()).await;
            });
        }
    });

    let mut servers = IndexMap::new();
    servers.insert(
        "stdio_srv".to_string(),
        McpServerConfig {
            command: Some("python3".to_string()),
            args: vec![stdio_script.to_string_lossy().into_owned()],
            enabled: true,
            ..McpServerConfig::default()
        },
    );
    servers.insert(
        "http_srv".to_string(),
        McpServerConfig {
            url: Some(format!("http://{http_addr}/mcp")),
            enabled: true,
            ..McpServerConfig::default()
        },
    );
    servers.insert(
        "broken_srv".to_string(),
        McpServerConfig {
            command: Some("/nonexistent/mcp-binary-xyz".to_string()),
            enabled: true,
            ..McpServerConfig::default()
        },
    );

    let (gw, report) =
        ConfiguredMcpGateway::connect_all(&servers, &ws, &AgentSettings::default()).await;
    assert_eq!(report.connected_servers, vec!["stdio_srv", "http_srv"]);
    assert_eq!(
        report.warnings.len(),
        1,
        "broken server should degrade non-fatally"
    );

    let codex_caps = ModelCapabilities::resolve("openai", "gpt-5.4-codex", None);
    let schema_before = build_tool_definitions_for_model(&codex_caps);

    let mut ctx = make_ctx(ws, state, PermissionMode::Auto, Vec::new());
    ctx.mcp_gateway = Some(gw.clone());

    assert_eq!(
        schema_before,
        build_tool_definitions_for_model(&codex_caps),
        "live MCP gateway must not alter top-level LLM tool schemas"
    );

    let reg = NativeToolRegistry::new();
    let out = reg
        .execute(
            &ctx,
            "live_mcp_1",
            "code",
            &json!({
                "code": "tools = mcp_list()\nu = await mcp_call('stdio_srv', 'upper', {'text': 'tekvra'})\ns = await mcp_call('http_srv', 'add', {'a': 19, 'b': 23})\nprint(len(tools), u['upper'], s['sum'])"
            }),
        )
        .await
        .expect("monty executes live stdio + http mcp calls");

    let ToolOutput::CodeExecution { output, mcp_calls } = out else {
        panic!("expected CodeExecution");
    };
    assert_eq!(mcp_calls, 2);
    assert_eq!(output.trim(), "2 TEKVRA 42");
}

#[tokio::test]
async fn streamable_http_sse_mcp_bearer_auth_and_list_changed_catalog_refresh() {
    use indexmap::IndexMap;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tekvra_config::{AgentSettings, McpServerConfig};
    use tekvra_tools::ConfiguredMcpGateway;

    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    let state = tmp.path().join("state");
    fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    fs::create_dir_all(&state).expect("mkdir state");

    let unlocked_v2 = Arc::new(AtomicBool::new(false));
    let unlocked_srv = Arc::clone(&unlocked_v2);

    let sse_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind sse mcp");
    let sse_addr = sse_listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = sse_listener.accept().await {
            let unlocked_inner = Arc::clone(&unlocked_srv);
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let req_str = String::from_utf8_lossy(&buf[..n]);
                let headers_lower = req_str
                    .split("\r\n\r\n")
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !headers_lower.contains("authorization: bearer mcp-secret-tok") {
                    let _ = stream
                        .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 12\r\nConnection: close\r\n\r\nunauthorized")
                        .await;
                    return;
                }
                let body_str = req_str.split("\r\n\r\n").nth(1).unwrap_or("{}");
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(body_str) else {
                    return;
                };
                let Some(mid) = msg.get("id").cloned() else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                };
                let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
                let mut sse_events = String::new();
                let result = match method {
                    "initialize" => json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": { "listChanged": true } },
                        "serverInfo": { "name": "sse-mcp", "version": "1.0" }
                    }),
                    "tools/list" => {
                        let mut tools = vec![json!({
                            "name": "unlock",
                            "description": "Unlocks dynamic tool",
                            "inputSchema": { "type": "object" }
                        })];
                        if unlocked_inner.load(Ordering::Acquire) {
                            tools.push(json!({
                                "name": "multiply",
                                "description": "Multiplies two ints",
                                "inputSchema": { "type": "object" }
                            }));
                        }
                        json!({ "tools": tools })
                    }
                    "tools/call" => {
                        let name = msg["params"]["name"].as_str().unwrap_or("");
                        if name == "unlock" {
                            unlocked_inner.store(true, Ordering::Release);
                            sse_events.push_str(
                                ": heartbeat\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\r\n\r\n",
                            );
                            json!({ "unlocked": true })
                        } else {
                            let args = &msg["params"]["arguments"];
                            let prod =
                                args["x"].as_i64().unwrap_or(0) * args["y"].as_i64().unwrap_or(0);
                            json!({ "product": prod })
                        }
                    }
                    _ => json!({}),
                };
                let rpc_resp = json!({ "jsonrpc": "2.0", "id": mid, "result": result }).to_string();
                sse_events.push_str(&format!("event: message\r\ndata: {rpc_resp}\r\n\r\n"));
                let http_resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse_events.len(),
                    sse_events
                );
                let _ = stream.write_all(http_resp.as_bytes()).await;
            });
        }
    });

    let mut env_map = std::collections::BTreeMap::new();
    env_map.insert(
        "TEST_SSE_MCP_TOKEN".to_string(),
        "mcp-secret-tok".to_string(),
    );

    let mut servers = IndexMap::new();
    servers.insert(
        "sse_srv".to_string(),
        McpServerConfig {
            url: Some(format!("http://{sse_addr}/sse")),
            bearer_token_env_var: Some("TEST_SSE_MCP_TOKEN".to_string()),
            env: env_map,
            enabled: true,
            ..McpServerConfig::default()
        },
    );

    let (gw, report) =
        ConfiguredMcpGateway::connect_all(&servers, &ws, &AgentSettings::default()).await;
    assert_eq!(report.connected_servers, vec!["sse_srv"]);
    assert_eq!(gw.list_tools().len(), 1);

    let codex_caps = ModelCapabilities::resolve("openai", "gpt-5.4-codex", None);
    let schema_before = build_tool_definitions_for_model(&codex_caps);

    let mut ctx = make_ctx(ws, state, PermissionMode::Auto, Vec::new());
    ctx.mcp_gateway = Some(gw.clone());

    let reg = NativeToolRegistry::new();
    let out = reg
        .execute(
            &ctx,
            "sse_mcp_1",
            "code",
            &json!({
                "code": "before = len(mcp_list())\nawait mcp_call('sse_srv', 'unlock', {})\nafter = len(mcp_list())\nm = await mcp_call('sse_srv', 'multiply', {'x': 6, 'y': 7})\nprint(before, after, m['product'])"
            }),
        )
        .await
        .expect("streamable HTTP SSE + list_changed refresh in monty");

    let ToolOutput::CodeExecution { output, mcp_calls } = out else {
        panic!("expected CodeExecution");
    };
    assert_eq!(mcp_calls, 2);
    assert_eq!(output.trim(), "1 2 42");
    assert_eq!(
        schema_before,
        build_tool_definitions_for_model(&codex_caps),
        "tools/list_changed catalog refresh must never mutate top-level LLM tool schemas"
    );
}

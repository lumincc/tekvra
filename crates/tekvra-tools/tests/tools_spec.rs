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
    MEMORY_ENTRY_LIMIT, MEMORY_FILE_LIMIT, McpGateway, McpToolDescriptor, MemoryStore,
    MontyCodeSandbox, NativeToolRegistry, PermissionDecision, PermissionEvaluator,
    PtySessionManager, SafeWebFetcher, ShellSyntax, ToolAction, ToolExecutionContext,
    WorkspacePathLocks, active_shell_syntax, build_tool_definitions_for_model,
    decompose_bash_command, decompose_powershell_command, decompose_shell_command_for, memory_slug,
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
        lsp_manager: None,
        memory_store: None,
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

    let reg = NativeToolRegistry::new();

    // Windows runs payloads through PowerShell; shell startup is slower there, so use a
    // generous yield window and poll write_stdin until exit instead of a single call.
    // Auto mode's safe-command allowlist is bash-oriented (`Write-Host` is unknown and
    // prompts, but this ctx has no approver), so the Windows branch runs under Yolo —
    // this test verifies PTY mechanics, not permission policy.
    let (mode, prompt_cmd, prompt_yield_ms) = if cfg!(windows) {
        (
            PermissionMode::Yolo,
            "Write-Host -NoNewline 'Enter token: '; $tok = [Console]::ReadLine(); Write-Output \"GOT:$tok\"",
            10_000_u64,
        )
    } else {
        (
            PermissionMode::Auto,
            "printf 'Enter token: '; read tok; printf 'GOT:%s\\n' \"$tok\"",
            200_u64,
        )
    };

    let ctx = make_ctx(ws, state, mode, Vec::new());

    // 1. Interactive process prompting for stdin without exiting in initial yield window
    let start_out = reg
        .execute(
            &ctx,
            "pty1",
            "exec_command",
            &json!({
                "cmd": prompt_cmd,
                "yield_time_ms": prompt_yield_ms
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
    assert!(
        prompt_out.contains("Enter token:"),
        "prompt not captured; receipt output was {prompt_out:?}"
    );

    // Deliver stdin via `write_stdin` (CR terminates the line on both platforms: Unix
    // line discipline maps it to NL, the Windows console treats it as Enter), polling
    // until the session exits since the shell may emit output over several wakeups.
    let mut done_exit = None;
    let mut done_text = String::new();
    let mut chars = "codex42\r".to_string();
    for _ in 0..50 {
        let out = reg
            .execute(
                &ctx,
                "pty2",
                "write_stdin",
                &json!({
                    "session_id": sid.clone(),
                    "chars": chars,
                    "yield_time_ms": 1500
                }),
            )
            .await
            .expect("write_stdin");
        chars = String::new();
        let ToolOutput::Terminal {
            session_id,
            exit_code,
            filtered_output,
            ..
        } = out
        else {
            panic!("expected PTY terminal output");
        };
        done_text.push_str(&filtered_output);
        if session_id.is_none() {
            done_exit = exit_code;
            break;
        }
    }
    let Some(exit_code) = done_exit else {
        panic!("PTY session never exited; collected: {done_text}");
    };
    assert_eq!(exit_code, 0);
    assert!(done_text.contains("GOT:codex42"), "got: {done_text}");

    // 2. Failing build/test command with CR progress bars + 400 lines of noise -> non-zero exit preserved,
    // `[filtered: ...]` summary + `output_id`, and raw log searchable via `tool_output`!
    let noisy_fail_cmd = if cfg!(windows) {
        concat!(
            "Write-Host \"Progress 10%`rProgress 50%`rProgress 100%\"; ",
            "for ($i = 1; $i -le 300; $i++) { Write-Output \"   Compiling dep_$i v0.1.0\" }; ",
            "Write-Output 'test auth::critical_case ... FAILED'; ",
            "Write-Output 'CRITICAL_ASSERTION_MARKER_991'; ",
            "exit 101"
        )
    } else {
        concat!(
            "printf 'Progress 10%%\\rProgress 50%%\\rProgress 100%%\\n'; ",
            "i=1; while [ $i -le 300 ]; do echo \"   Compiling dep_$i v0.1.0\"; i=$((i+1)); done; ",
            "echo 'test auth::critical_case ... FAILED'; ",
            "echo 'CRITICAL_ASSERTION_MARKER_991'; ",
            "exit 101"
        )
    };

    let fail_out = reg
        .execute(
            &ctx,
            "pty3",
            "exec_command",
            &json!({
                "cmd": noisy_fail_cmd,
                "yield_time_ms": if cfg!(windows) { 20_000 } else { 3_000 }
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

    // 2. Plan mode blocks `apply_patch` / `file_write` with guidance to use `plan`.
    //    This test asserts bash decomposition semantics (`eval "$FOO"` → once_only),
    //    so pin the syntax explicitly instead of following the platform shell.
    let mut eval = PermissionEvaluator::new(
        PermissionMode::Plan,
        canon_ws.clone(),
        Some(canon_ws.clone()),
        rules.clone(),
        Vec::new(),
    )
    .with_shell_syntax(ShellSyntax::Bash);
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
        ConfiguredMcpGateway::connect_all(&servers, &ws, &AgentSettings::default(), false).await;
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
        ConfiguredMcpGateway::connect_all(&servers, &ws, &AgentSettings::default(), false).await;
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

/// Windows PowerShell decomposition and evaluator decisions mirror the bash contract:
/// readonly pipelines allow in `Plan`, destructive compounds and dynamic constructs
/// degrade to prompts/once-only, and Unix behavior stays byte-identical.
#[tokio::test]
async fn powershell_decomposition_and_permission_decisions_mirror_bash_contract() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    std::fs::create_dir_all(&ws).expect("mkdir");
    let canon_ws = std::fs::canonicalize(&ws).expect("canon");

    // 1. Native decomposition: pipeline splits, redirect targets surface, dynamic
    //    constructs (`iex`, `& $var`, nested `-Command`) force once_only.
    let decomp = decompose_powershell_command(
        "Get-ChildItem src -Recurse | Select-String pattern; Remove-Item temp/",
    );
    assert!(!decomp.once_only);
    assert_eq!(decomp.subcommands.len(), 3);
    assert_eq!(
        decomp.subcommands[0].tokens,
        vec!["Get-ChildItem", "src", "-Recurse"]
    );
    assert_eq!(
        decomp.subcommands[1].tokens,
        vec!["Select-String", "pattern"]
    );
    assert_eq!(decomp.subcommands[2].tokens, vec!["Remove-Item", "temp/"]);

    let redirected = decompose_powershell_command("Get-Content log.txt *> C:\\out\\all.txt");
    assert_eq!(
        redirected.subcommands[0].write_redirects,
        vec!["C:\\out\\all.txt"]
    );

    for dynamic in [
        "Invoke-Expression \"$user_input\"",
        "& $scriptPath -Force",
        "pwsh -Command \"Write-Output nested\"",
    ] {
        assert!(
            decompose_powershell_command(dynamic).once_only,
            "expected once_only for `{dynamic}`"
        );
    }

    // 2. Facade regression: bash syntax delegation must equal `decompose_bash_command`;
    //    on the platform shell (`active_shell_syntax`) it applies to the facade too.
    let bash_decomp = decompose_shell_command_for(ShellSyntax::Bash, "git status && rm -rf build/");
    assert_eq!(
        bash_decomp,
        decompose_bash_command("git status && rm -rf build/")
    );

    // 3. Plan mode: PS readonly pipelines allow; destructive compounds and dynamic
    //    commands deny with guidance.
    let ps_eval = || {
        PermissionEvaluator::new(
            PermissionMode::Plan,
            canon_ws.clone(),
            Some(canon_ws.clone()),
            Vec::new(),
            Vec::new(),
        )
        .with_shell_syntax(ShellSyntax::PowerShell)
    };
    for readonly in [
        "Get-ChildItem src -Recurse | Select-String pattern",
        "1..300 | ForEach-Object { $_ }",
        "git status",
    ] {
        match ps_eval().evaluate(
            "ps-plan",
            &ToolAction::Shell {
                command: readonly.to_string(),
                cwd: canon_ws.clone(),
            },
        ) {
            PermissionDecision::Allow => {}
            other => panic!("expected Allow for PS readonly `{readonly}`, got {other:?}"),
        }
    }
    for blocked in [
        "Get-ChildItem src; Remove-Item temp/",
        "iex $payload",
        "Get-Content log.txt > out.txt",
    ] {
        match ps_eval().evaluate(
            "ps-plan",
            &ToolAction::Shell {
                command: blocked.to_string(),
                cwd: canon_ws.clone(),
            },
        ) {
            PermissionDecision::Deny { reason } => {
                assert!(
                    reason.contains("plan"),
                    "denial must guide to plan: {reason}"
                );
            }
            other => panic!("expected Deny for PS `{blocked}` in Plan, got {other:?}"),
        }
    }

    // 4. Build mode: `Remove-Item` in a compound prompts on the specific subcommand;
    //    once_only write redirects also prompt (never silently allowed).
    let build_eval = PermissionEvaluator::new(
        PermissionMode::Build,
        canon_ws.clone(),
        Some(canon_ws.clone()),
        Vec::new(),
        Vec::new(),
    )
    .with_shell_syntax(ShellSyntax::PowerShell);
    let PermissionDecision::Prompt(rm_prompt) = build_eval.evaluate(
        "ps-build",
        &ToolAction::Shell {
            command: "Get-ChildItem src; Remove-Item temp/".to_string(),
            cwd: canon_ws.clone(),
        },
    ) else {
        panic!("expected Prompt for PS compound with `Remove-Item`");
    };
    assert_eq!(rm_prompt.summary, "Remove-Item temp/");

    let PermissionDecision::Prompt(redirect_prompt) = build_eval.evaluate(
        "ps-build",
        &ToolAction::Shell {
            command: "Get-Content log.txt > out.txt".to_string(),
            cwd: canon_ws.clone(),
        },
    ) else {
        panic!("expected Prompt for PS write redirect");
    };
    assert!(!redirect_prompt.ladder.is_empty());

    // 5. Explicit deny rules decompose PS commands: `Remove-Item /` matches the subcommand.
    let deny_rules = vec![PermissionRule {
        tool: "exec_command".to_string(),
        pattern: "Remove-Item /".to_string(),
        action: RuleAction::Deny,
    }];
    let deny_eval = PermissionEvaluator::new(
        PermissionMode::Yolo,
        canon_ws.clone(),
        Some(canon_ws.clone()),
        deny_rules,
        Vec::new(),
    )
    .with_shell_syntax(ShellSyntax::PowerShell);
    match deny_eval.evaluate(
        "ps-deny",
        &ToolAction::Shell {
            command: "Get-ChildItem /; Remove-Item /".to_string(),
            cwd: canon_ws.clone(),
        },
    ) {
        PermissionDecision::Deny { reason } => {
            assert!(
                reason.contains("Remove-Item /"),
                "explicit deny must name the rule: {reason}"
            );
        }
        other => panic!("expected explicit Deny under Yolo for PS rule match, got {other:?}"),
    }

    // 6. Unix regression: on a bash platform the facade must keep producing bash
    //    decomposition for the same command (byte-identical behavior guarantee).
    if active_shell_syntax() == ShellSyntax::Bash {
        let facade = decompose_shell_command_for(active_shell_syntax(), "cargo build --release");
        let direct = decompose_bash_command("cargo build --release");
        assert_eq!(facade, direct);
    }
}

// ---------------------------------------------------------------------------
// Phase 5: multimodal `view_image`, uniform read authorization, concurrent
// read-only `batch`, and async Build-mode MCP approval inside `code`
// ---------------------------------------------------------------------------

struct ApprovingCallback {
    prompts: Mutex<Vec<String>>,
}

#[async_trait]
impl tekvra_tools::ApprovalCallback for ApprovingCallback {
    async fn request_approval(
        &self,
        prompt: tekvra_protocol::ApprovalPrompt,
    ) -> ApprovalResolution {
        self.prompts
            .lock()
            .expect("lock")
            .push(format!("{}: {}", prompt.tool_name, prompt.summary));
        ApprovalResolution::Approve {
            scope: None,
            lifetime: GrantLifetime::Project,
            confirmed_broad: false,
        }
    }
}

/// Minimal valid PNG (signature + stub payload) for MIME sniffing.
fn tiny_png() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    bytes.extend_from_slice(b"fake-ihdr-data");
    bytes
}

#[tokio::test]
async fn view_image_returns_multimodal_image_payload_and_authorizes_reads() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&ws).expect("mkdir");
    let png = tiny_png();
    std::fs::write(ws.join("shot.png"), &png).expect("write png");

    let reg = NativeToolRegistry::new();

    // Build mode with an explicit `view_image` deny rule -> rejected before disk read.
    let deny_rules = vec![PermissionRule {
        tool: "view_image".to_string(),
        pattern: "*".to_string(),
        action: RuleAction::Deny,
    }];
    let denied_ctx = make_ctx(ws.clone(), state.clone(), PermissionMode::Build, deny_rules);
    let err = reg
        .execute(
            &denied_ctx,
            "v0",
            "view_image",
            &json!({ "path": "shot.png" }),
        )
        .await
        .expect_err("deny rule must block view_image");
    assert!(err.to_string().contains("blocked by explicit `deny` rule"));

    // Without the deny rule, the full base64 payload is delivered.
    let ctx = make_ctx(ws, state, PermissionMode::Build, Vec::new());
    let out = reg
        .execute(&ctx, "v1", "view_image", &json!({ "path": "shot.png" }))
        .await
        .expect("view_image");
    let ToolOutput::Image {
        path,
        media_type,
        bytes_len,
        data_base64,
    } = out
    else {
        panic!("expected Image output, got {out:?}");
    };
    assert_eq!(path, PathBuf::from("shot.png"));
    assert_eq!(media_type, "image/png");
    assert_eq!(bytes_len, png.len());
    // Full payload, not the old 64-byte head: standard base64 length formula.
    assert_eq!(
        data_base64.len(),
        bytes_len.div_ceil(3) * 4,
        "expected full base64 payload"
    );
    // Model-facing text stays concise (no multi-megabyte base64 inline).
    let model_text = ToolOutput::Image {
        path: path.clone(),
        media_type: media_type.clone(),
        bytes_len,
        data_base64: data_base64.clone(),
    }
    .to_model_text();
    assert!(model_text.starts_with("Viewed image "));
    assert!(!model_text.contains(data_base64.as_str()));
}

#[tokio::test]
async fn deny_rule_blocks_file_grep_and_batch_rejects_mutating_tools_concurrently() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&ws).expect("mkdir");
    std::fs::write(ws.join("a.txt"), "NEEDLE_A\n").expect("write a");
    std::fs::write(ws.join("b.txt"), "NEEDLE_B\n").expect("write b");

    let rules = vec![PermissionRule {
        tool: "file_grep".to_string(),
        pattern: "*".to_string(),
        action: RuleAction::Deny,
    }];
    let ctx = make_ctx(ws, state, PermissionMode::Build, rules);
    let reg = NativeToolRegistry::new();

    // Direct denied call.
    let err = reg
        .execute(&ctx, "g0", "file_grep", &json!({ "pattern": "NEEDLE" }))
        .await
        .expect_err("deny rule must block file_grep");
    assert!(err.to_string().contains("blocked by explicit `deny` rule"));

    // Denied call inside `batch` (before scanning disk).
    let err = reg
        .execute(
            &ctx,
            "b0",
            "batch",
            &json!({ "calls": [{ "tool": "file_grep", "arguments": { "pattern": "NEEDLE" } }] }),
        )
        .await
        .expect_err("deny rule must hold inside batch");
    assert!(err.to_string().contains("blocked by explicit `deny` rule"));

    // Mutating tools are rejected from `batch` with a validation error.
    let err = reg
        .execute(
            &ctx,
            "b1",
            "batch",
            &json!({ "calls": [{ "tool": "file_write", "arguments": { "path": "x.txt", "content": "nope" } }] }),
        )
        .await
        .expect_err("mutating tool must be rejected in batch");
    assert!(err.to_string().contains("only accepts read/search/index"));

    // Read-only calls run concurrently and return ordered results.
    let out = reg
        .execute(
            &ctx,
            "b2",
            "batch",
            &json!({
                "calls": [
                    { "tool": "file_read", "arguments": { "path": "a.txt" } },
                    { "tool": "file_read", "arguments": { "path": "b.txt" } }
                ]
            }),
        )
        .await
        .expect("batch read");
    let text = out.to_model_text();
    let a_pos = text.find("NEEDLE_A").expect("NEEDLE_A in output");
    let b_pos = text.find("NEEDLE_B").expect("NEEDLE_B in output");
    assert!(a_pos < b_pos, "batch results must stay ordered: {text}");
}

#[tokio::test]
async fn build_mode_mcp_call_inside_code_prompts_interactively() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&ws).expect("mkdir");

    let reg = NativeToolRegistry::new();
    let snippet = json!({
        "code": "r = mcp_call_sync('docs', 'lookup', {'q': 'tekvra'})\nprint(r['echo']['q'])"
    });

    // 1. Build mode with NO approval callback -> fails closed (no hang).
    let ctx_no_cb = make_ctx(ws.clone(), state.clone(), PermissionMode::Build, Vec::new());
    let err = reg
        .execute(&ctx_no_cb, "c0", "code", &snippet)
        .await
        .expect_err("prompt without approval callback must deny");
    assert!(err.to_string().contains("approval required"));

    // 2. Build mode WITH an interactive approval callback -> executes the call.
    let mut ctx = make_ctx(ws, state, PermissionMode::Build, Vec::new());
    let mock_mcp = Arc::new(MockMcp::default());
    ctx.mcp_gateway = Some(mock_mcp.clone());
    let approving = Arc::new(ApprovingCallback {
        prompts: Mutex::new(Vec::new()),
    });
    ctx.approval_cb = Some(approving.clone());

    let out = reg
        .execute(&ctx, "c1", "code", &snippet)
        .await
        .expect("approved mcp call inside code");
    let ToolOutput::CodeExecution { output, mcp_calls } = out else {
        panic!("expected CodeExecution");
    };
    assert_eq!(mcp_calls, 1);
    assert_eq!(output.trim(), "tekvra");
    assert!(
        !approving.prompts.lock().expect("lock").is_empty(),
        "an interactive approval prompt must have been issued"
    );
    assert_eq!(mock_mcp.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mcp_session_header_echo_delete_and_split_sse_streaming() {
    use indexmap::IndexMap;
    use std::collections::BTreeMap;
    use tekvra_config::McpServerConfig;
    use tekvra_tools::ConfiguredMcpGateway;

    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(&state).expect("mkdir state");

    // Server-side log: (method, session_header_present); plus observed DELETEs.
    let seen: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let deletes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen_srv = Arc::clone(&seen);
    let deletes_srv = Arc::clone(&deletes);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let seen_inner = Arc::clone(&seen_srv);
            let deletes_inner = Arc::clone(&deletes_srv);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = vec![0_u8; 4096];
                // Read until end of headers + body (mock server: single small request).
                loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let s = String::from_utf8_lossy(&buf);
                    let Some((headers, body)) = s.split_once("\r\n\r\n") else {
                        continue;
                    };
                    // DELETE carries no body — handle as soon as headers are complete.
                    if headers.to_ascii_lowercase().starts_with("delete") {
                        break;
                    }
                    if serde_json::from_str::<Value>(body.trim()).is_ok() {
                        break;
                    }
                }
                if buf.is_empty() {
                    return;
                }
                let req_str = String::from_utf8_lossy(&buf).into_owned();
                let (headers, body) = req_str.split_once("\r\n\r\n").unwrap_or(("", &req_str));
                let headers_lower = headers.to_ascii_lowercase();
                if headers_lower.starts_with("delete") {
                    deletes_inner
                        .lock()
                        .expect("lock")
                        .push(headers.to_string());
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    return;
                }
                let session_present = headers_lower.contains("mcp-session-id:");
                let Ok(msg) = serde_json::from_str::<Value>(body.trim()) else {
                    return;
                };
                let Some(mid) = msg.get("id").cloned() else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                };
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                seen_inner
                    .lock()
                    .expect("lock")
                    .push((method.to_string(), session_present));
                let result = match method {
                    "initialize" => json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "serverInfo": { "name": "sess-mcp", "version": "1.0" }
                    }),
                    "tools/list" => json!({ "tools": [
                        { "name": "echo", "description": "Echoes", "inputSchema": { "type": "object" } }
                    ]}),
                    _ => json!({}),
                };
                let rpc = json!({ "jsonrpc": "2.0", "id": mid, "result": result }).to_string();
                let mut headers_out =
                    String::from("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n");
                if method == "initialize" {
                    headers_out.push_str("Mcp-Session-Id: sess-123\r\n");
                }
                headers_out.push_str(&format!(
                    "Content-Length: {}\r\nConnection: close\r\n\r\n",
                    format!("data: {rpc}\r\n\r\n").len()
                ));
                let _ = stream.write_all(headers_out.as_bytes()).await;
                // Split the SSE data line across two writes to prove incremental parsing.
                let payload = format!("data: {rpc}\r\n\r\n");
                let (head, tail) = payload.split_at(payload.len() / 2);
                let _ = stream.write_all(head.as_bytes()).await;
                stream.flush().await.ok();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let _ = stream.write_all(tail.as_bytes()).await;
            });
        }
    });

    let mut servers = IndexMap::new();
    let mut env_map = BTreeMap::new();
    env_map.insert("X".to_string(), "1".to_string());
    servers.insert(
        "sess_srv".to_string(),
        McpServerConfig {
            url: Some(format!("http://{addr}/mcp")),
            enabled: true,
            env: env_map,
            ..McpServerConfig::default()
        },
    );

    let (gw, report) =
        ConfiguredMcpGateway::connect_all(&servers, &ws, &AgentSettings::default(), false).await;
    assert_eq!(report.connected_servers, vec!["sess_srv"]);
    assert_eq!(gw.list_tools().len(), 1, "split SSE frame must still parse");

    // Every request after initialize must echo the captured session id.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let seen_now = seen.lock().expect("lock").clone();
    let list_entry = seen_now.iter().find(|(m, _)| m == "tools/list");
    assert!(list_entry.is_some(), "saw {seen_now:?}");
    assert!(
        list_entry.unwrap().1,
        "tools/list must carry Mcp-Session-Id after initialize"
    );

    // Dropping the gateway must fire the best-effort session DELETE.
    drop(gw);
    let mut observed_delete = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if !deletes.lock().expect("lock").is_empty() {
            observed_delete = true;
            break;
        }
    }
    assert!(observed_delete, "expected Mcp-Session-Id DELETE on drop");
}

#[tokio::test]
async fn mcp_server_initiated_requests_fallback_gating_and_unknown_methods() {
    use indexmap::IndexMap;
    use std::collections::BTreeMap;
    use tekvra_config::McpServerConfig;
    use tekvra_tools::{ConfiguredMcpGateway, McpServerHooks};

    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(&state).expect("mkdir state");

    // Log of responses POSTed back by the gateway, keyed by server-request method.
    let responses: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));

    let sampling_hook = Arc::new(SamplingCountHook {
        invocations: AtomicUsize::new(0),
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let responses_srv = Arc::clone(&responses);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let responses_inner = Arc::clone(&responses_srv);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = vec![0_u8; 4096];
                loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let s = String::from_utf8_lossy(&buf);
                    if let Some((_, body)) = s.split_once("\r\n\r\n")
                        && serde_json::from_str::<Value>(body.trim()).is_ok()
                    {
                        break;
                    }
                }
                let req_str = String::from_utf8_lossy(&buf).into_owned();
                let (_, body) = req_str.split_once("\r\n\r\n").unwrap_or(("", ""));
                let Ok(msg) = serde_json::from_str::<Value>(body.trim()) else {
                    return;
                };
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                let mid = msg.get("id").cloned().unwrap_or(Value::Null);
                // The gateway POSTs JSON-RPC RESPONSES back to us (server→client flow) or
                // originates requests (initialize/tools/list) which we answer over SSE.
                let is_response = msg.get("result").is_some() || msg.get("error").is_some();
                if is_response {
                    responses_inner.lock().expect("lock").push(msg.clone());
                    let _ = stream
                        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }
                // Originating request: for `initialize` we answer, then ALSO push a
                // server→client request over the same SSE stream so the client must route it.
                let (result, extra_server_request) = match method {
                    "initialize" => (
                        json!({
                            "protocolVersion": "2024-11-05",
                            "capabilities": { "sampling": {}, "elicitation": {} },
                            "serverInfo": { "name": "srv-init", "version": "1.0" }
                        }),
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": "srv_req_unknown_1",
                            "method": "workspace/writeFile",
                            "params": { "path": "x.txt", "content": "nope" }
                        })),
                    ),
                    "tools/list" => {
                        let mut extra = vec![json!({
                            "jsonrpc": "2.0",
                            "id": "srv_req_sampling_1",
                            "method": "sampling/createMessage",
                            "params": { "messages": [ { "role": "user", "content": { "type": "text", "text": "hi" } } ] }
                        })];
                        // Second client connection (scenario B) also receives the elicitation probe.
                        extra.push(json!({
                            "jsonrpc": "2.0",
                            "id": "srv_req_elicit_1",
                            "method": "elicitation/create",
                            "params": { "message": "Continue?", "requestedSchema": { "type": "object", "properties": { "ok": { "type": "boolean" } } } }
                        }));
                        (
                            json!({ "tools": [
                                { "name": "t", "description": "d", "inputSchema": { "type": "object" } }
                            ]}),
                            Some(Value::Array(extra)),
                        )
                    }
                    _ => (
                        json!({}),
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": "srv_req_unknown_1",
                            "method": "workspace/writeFile",
                            "params": { "path": "x.txt", "content": "nope" }
                        })),
                    ),
                };
                let mut events = String::new();
                if let Some(Value::Array(reqs)) = extra_server_request {
                    for sreq in reqs {
                        events.push_str(&format!("data: {}\r\n\r\n", sreq));
                    }
                } else if let Some(sreq) = extra_server_request {
                    events.push_str(&format!("data: {}\r\n\r\n", sreq));
                }
                let rpc = json!({ "jsonrpc": "2.0", "id": mid, "result": result }).to_string();
                events.push_str(&format!("data: {rpc}\r\n\r\n"));
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
                    events.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let mut servers = IndexMap::new();
    let mut env_map = BTreeMap::new();
    env_map.insert("X".to_string(), "1".to_string());
    servers.insert(
        "srv_init".to_string(),
        McpServerConfig {
            url: Some(format!("http://{addr}/mcp")),
            enabled: true,
            env: env_map,
            sampling: true,
            ..McpServerConfig::default()
        },
    );

    // Scenario A: hooks attached, but the global sampling gate is OFF (`connect_all(..., false)`).
    let (gw, report) =
        ConfiguredMcpGateway::connect_all(&servers, &ws, &AgentSettings::default(), false).await;
    assert_eq!(report.connected_servers, vec!["srv_init"]);
    gw.set_server_hooks(Arc::new(McpServerHooks {
        sampling: Some(Arc::clone(&sampling_hook) as Arc<dyn tekvra_tools::McpSamplingHook>),
        elicitation: None,
        progress: None,
    }));

    // The gateway routed the server→client `sampling/createMessage` and POSTed back
    // an error (gate off); the unknown method got `-32601`.
    let mut saw_unknown_32601 = false;
    let mut saw_sampling_gate_error = false;
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let now = responses.lock().expect("lock").clone();
        for msg in &now {
            if let Some(err) = msg.get("error") {
                let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
                let message = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if code == -32601 && message.contains("workspace/writeFile") {
                    saw_unknown_32601 = true;
                }
                if code == -32603 && message.contains("sampling is not enabled") {
                    saw_sampling_gate_error = true;
                }
            }
        }
        if saw_unknown_32601 && saw_sampling_gate_error {
            break;
        }
    }
    assert!(saw_unknown_32601, "unknown server method must yield -32601");
    assert!(
        saw_sampling_gate_error,
        "sampling with the gate off must yield -32603 even with hooks attached"
    );
    assert_eq!(
        sampling_hook.invocations.load(Ordering::SeqCst),
        0,
        "hook must never run while the gate is off"
    );
    drop(gw);

    // Scenario B: global flag ON (`connect_all(..., true)`) + per-server `sampling: true`
    // → the hook runs and its result is POSTed back; elicitation without a handler declines.
    let (gw2, report2) =
        ConfiguredMcpGateway::connect_all(&servers, &ws, &AgentSettings::default(), true).await;
    assert_eq!(report2.connected_servers, vec!["srv_init"]);
    gw2.set_server_hooks(Arc::new(McpServerHooks {
        sampling: Some(Arc::clone(&sampling_hook) as Arc<dyn tekvra_tools::McpSamplingHook>),
        elicitation: None,
        progress: None,
    }));

    let mut saw_sampled_ok = false;
    let mut saw_elicit_decline = false;
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let now = responses.lock().expect("lock").clone();
        for msg in &now {
            if msg.get("result").and_then(|r| {
                r.get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(Value::as_str)
            }) == Some("sampled-ok")
            {
                saw_sampled_ok = true;
            }
            if msg.get("id").and_then(Value::as_str) == Some("srv_req_elicit_1")
                && msg
                    .get("result")
                    .and_then(|r| r.get("action"))
                    .and_then(Value::as_str)
                    == Some("decline")
            {
                saw_elicit_decline = true;
            }
        }
        if saw_sampled_ok && saw_elicit_decline {
            break;
        }
    }
    assert!(
        saw_sampled_ok,
        "sampling hook must run and POST its result back when the gate is on"
    );
    assert_eq!(
        sampling_hook.invocations.load(Ordering::SeqCst),
        1,
        "exactly one sampling invocation expected"
    );
    assert!(
        saw_elicit_decline,
        "elicitation without a handler must decline explicitly"
    );
}

/// File-scope sampling hook fixture counting invocations.
struct SamplingCountHook {
    invocations: AtomicUsize,
}

#[async_trait::async_trait]
impl tekvra_tools::McpSamplingHook for SamplingCountHook {
    async fn sample(
        &self,
        _server: &str,
        _request_id: &Value,
        _params: &Value,
    ) -> Result<Value, String> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Ok(json!({
            "role": "assistant",
            "content": { "type": "text", "text": "sampled-ok" }
        }))
    }
}

#[tokio::test]
async fn web_search_parses_mock_endpoint_honors_deny_rules_and_caps() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(&state).expect("mkdir state");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 4096];
                let _ = stream.read(&mut buf).await;
                let page = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    SEARCH_PAGE.len(),
                    SEARCH_PAGE
                );
                let _ = stream.write_all(page.as_bytes()).await;
            });
        }
    });

    let endpoint = format!("http://{addr}/search");

    // 1. Deny rule on the search endpoint → the call is refused by the ladder.
    let deny_rules = vec![PermissionRule {
        tool: "webfetch".to_string(),
        pattern: endpoint.clone(),
        action: RuleAction::Deny,
    }];
    let mut ctx = make_ctx(ws.clone(), state.clone(), PermissionMode::Build, deny_rules);
    ctx.settings.websearch_base_url = Some(endpoint.clone());
    let reg = NativeToolRegistry::new();
    let err = reg
        .execute(
            &ctx,
            "ws_deny_1",
            "web_search",
            &json!({ "query": "rust async" }),
        )
        .await
        .expect_err("deny rule must refuse the search endpoint");
    assert!(
        err.to_string().contains("denied") || err.to_string().contains("Denied"),
        "got: {err}"
    );

    // 2. Auto-approving host → prompt is issued once and results parse.
    let mut ctx2 = make_ctx(ws, state, PermissionMode::Build, Vec::new());
    ctx2.settings.websearch_base_url = Some(endpoint);
    // The mock endpoint is loopback: allowlist it for the SSRF-safe fetcher.
    ctx2.web_fetcher = tekvra_tools::SafeWebFetcher::new(&["127.0.0.1".to_string()]);
    ctx2.approval_cb = Some(Arc::new(ApprovingCallback {
        prompts: Mutex::new(Vec::new()),
    }));
    let out = reg
        .execute(
            &ctx2,
            "ws_ok_1",
            "web_search",
            &json!({ "query": "tekvra rust agent", "max_results": 99 }),
        )
        .await
        .expect("approved web_search parses results");
    let ToolOutput::SearchResults { query, results } = out else {
        panic!("expected SearchResults");
    };
    assert_eq!(query, "tekvra rust agent");
    assert_eq!(results.len(), 2, "cap 99 clamps to page content");
    assert_eq!(results[0].url, "https://example.com/docs");
    assert!(results[0].title.contains("Example"));

    // Cap: max_results=1 keeps exactly one.
    let out1 = reg
        .execute(
            &ctx2,
            "ws_ok_2",
            "web_search",
            &json!({ "query": "q", "max_results": 1 }),
        )
        .await
        .expect("capped search");
    let ToolOutput::SearchResults { results: r1, .. } = out1 else {
        panic!("expected SearchResults");
    };
    assert_eq!(r1.len(), 1);
}

/// Minimal DuckDuckGo-HTML-shaped fixture served by the mock endpoint.
const SEARCH_PAGE: &str = r##"<html><body>
<div class="result">
  <h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.test/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs&amp;rut=x"><b>Example</b> Docs</a></h2>
  <a class="result__snippet" href="//duckduckgo.test/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs">The &quot;official&quot; guide &lt;v2&gt;</a>
</div>
<div class="result">
  <h2 class="result__title"><a class="result__a" href="https://direct.example.org/page">Direct Hit</a></h2>
  <a class="result__snippet" href="#">A plain snippet</a>
</div>
</body></html>"##;

#[tokio::test]
async fn memory_store_caps_and_memory_save_ladder() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir ws");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");

    // --- MemoryStore unit behavior: dated append + entry/file caps + slug ---
    let store = MemoryStore::open(&state_dir, &ws);
    store.append("prefers tabs").expect("append 1");
    store.append("uses ruff for lint").expect("append 2");
    let body = std::fs::read_to_string(store.path()).expect("read memory");
    assert!(body.contains("- 20"), "dated entry: {body}");
    assert!(body.contains("prefers tabs") && body.contains("uses ruff for lint"));
    let big = "x".repeat(MEMORY_ENTRY_LIMIT + 1);
    let err = store.append(&big).expect_err("entry cap");
    assert!(err.to_string().contains("the limit is"), "entry cap: {err}");
    // File cap: fill close to the limit, then refuse the next big append.
    std::fs::write(store.path(), "y".repeat(MEMORY_FILE_LIMIT - 8)).expect("fill memory");
    let err = store.append("another entry").expect_err("file cap");
    assert!(
        err.to_string().contains("memory file is full"),
        "file cap: {err}"
    );
    let slug = memory_slug(&ws);
    assert!(
        !slug.is_empty()
            && !slug.contains('/')
            && !slug.contains('\\')
            && slug
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "safe slug: {slug}"
    );

    // --- Registry arm through the ladder ---
    let reg = NativeToolRegistry::new();
    let store_ok = MemoryStore::open(&state_dir.join("mem_ws_state"), &ws);

    // ReadOnly mode: default-deny even with a store attached.
    let ctx_ro = make_ctx(
        ws.clone(),
        state_dir.join("ro_state"),
        PermissionMode::ReadOnly,
        Vec::new(),
    );
    // Attach the store manually (make_ctx passes None).
    let mut ctx_ro_with = ctx_ro;
    ctx_ro_with.memory_store = Some(Arc::new(store_ok.clone()));
    let out = reg
        .execute(
            &ctx_ro_with,
            "mem_ro_1",
            "memory_save",
            &json!({ "content": "should be denied" }),
        )
        .await;
    let err = match out {
        Err(err) => err,
        Ok(other) => panic!("read-only must deny memory_save: {other:?}"),
    };
    assert!(
        err.to_string().contains("require `build`/`auto` mode")
            || err.to_string().contains("permission denied"),
        "read-only denial: {err}"
    );

    // Build mode without an approval host fails closed.
    let ctx_build = make_ctx(
        ws.clone(),
        state_dir.join("build_state"),
        PermissionMode::Build,
        Vec::new(),
    );
    let mut ctx_build_with = ctx_build;
    ctx_build_with.memory_store = Some(Arc::new(store_ok.clone()));
    let out = reg
        .execute(
            &ctx_build_with,
            "mem_build_1",
            "memory_save",
            &json!({ "content": "needs approval" }),
        )
        .await;
    assert!(out.is_err(), "build without approval host must fail closed");

    // Explicit allow rule overrides the ladder in any mode.
    let allow_rules = vec![PermissionRule {
        tool: "memory_save".to_string(),
        pattern: "*".to_string(),
        action: RuleAction::Allow,
    }];
    let store_allow = MemoryStore::open(&state_dir.join("allow_state"), &ws);
    let ctx_allow = make_ctx(
        ws.clone(),
        state_dir.join("allow_ctx_state"),
        PermissionMode::ReadOnly,
        allow_rules,
    );
    let mut ctx_allow_with = ctx_allow;
    ctx_allow_with.memory_store = Some(Arc::new(store_allow.clone()));
    let out = reg
        .execute(
            &ctx_allow_with,
            "mem_allow_1",
            "memory_save",
            &json!({ "content": "allowed by rule" }),
        )
        .await
        .expect("allow-rule override");
    let ToolOutput::Text { text } = out else {
        panic!("expected Text confirmation");
    };
    assert!(text.contains("saved to"), "confirmation: {text}");
    let body = std::fs::read_to_string(store_allow.path()).expect("read allowed memory");
    assert!(body.contains("allowed by rule"));

    // Deny rule wins in every mode.
    let deny_rules = vec![PermissionRule {
        tool: "memory_save".to_string(),
        pattern: "*".to_string(),
        action: RuleAction::Deny,
    }];
    let ctx_deny = make_ctx(
        ws,
        state_dir.join("deny_ctx_state"),
        PermissionMode::Yolo,
        deny_rules,
    );
    let mut ctx_deny_with = ctx_deny;
    ctx_deny_with.memory_store = Some(Arc::new(store_ok));
    let out = reg
        .execute(
            &ctx_deny_with,
            "mem_deny_1",
            "memory_save",
            &json!({ "content": "must be denied" }),
        )
        .await;
    assert!(out.is_err(), "deny rule must refuse memory_save");
}

/// `batch` admits `web_search` (network-bound, read-only) alongside local reads; results
/// stay ordered; per-sub-call endpoint authorization and deny rules hold; process-spawning
/// tools stay excluded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_admits_web_search_orders_mixed_results_and_keeps_lsp_out() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(&state).expect("mkdir state");
    std::fs::write(ws.join("notes.txt"), "NEEDLE_LOCAL\n").expect("write notes");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 4096];
                let _ = stream.read(&mut buf).await;
                let page = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    SEARCH_PAGE.len(),
                    SEARCH_PAGE
                );
                let _ = stream.write_all(page.as_bytes()).await;
            });
        }
    });
    let endpoint = format!("http://{addr}/search");

    let mut ctx = make_ctx(ws.clone(), state, PermissionMode::Build, Vec::new());
    ctx.settings.websearch_base_url = Some(endpoint.clone());
    ctx.web_fetcher = tekvra_tools::SafeWebFetcher::new(&["127.0.0.1".to_string()]);
    ctx.approval_cb = Some(Arc::new(ApprovingCallback {
        prompts: Mutex::new(Vec::new()),
    }));
    let reg = NativeToolRegistry::new();

    // Mixed local + network sub-calls: ordered results.
    let out = reg
        .execute(
            &ctx,
            "bws1",
            "batch",
            &json!({
                "calls": [
                    { "tool": "file_grep", "arguments": { "pattern": "NEEDLE_LOCAL" } },
                    { "tool": "web_search", "arguments": { "query": "tekvra rust agent" } },
                    { "tool": "file_read", "arguments": { "path": "notes.txt" } }
                ]
            }),
        )
        .await
        .expect("mixed batch runs");
    let text = out.to_model_text();
    let local_pos = text
        .find("NEEDLE_LOCAL")
        .expect("local grep result present");
    let search_pos = text.find("Example").expect("search result present");
    let read_pos = text.find("NEEDLE_LOCAL").expect("dup marker");
    let _ = read_pos;
    assert!(
        local_pos < search_pos,
        "results must stay in submission order: {text}"
    );

    // Process-spawning tools stay excluded from `batch`.
    let err = reg
        .execute(
            &ctx,
            "bws2",
            "batch",
            &json!({ "calls": [{ "tool": "lsp_diagnostics", "arguments": { "path": "notes.txt" } }] }),
        )
        .await
        .expect_err("lsp_diagnostics must stay out of batch");
    assert!(err.to_string().contains("only accepts"), "got: {err}");

    // Endpoint deny rules hold inside batch sub-calls.
    let deny_rules = vec![PermissionRule {
        tool: "webfetch".to_string(),
        pattern: endpoint.clone(),
        action: RuleAction::Deny,
    }];
    let mut deny_ctx = make_ctx(
        ws,
        tmp.path().join("state2"),
        PermissionMode::Build,
        deny_rules,
    );
    deny_ctx.settings.websearch_base_url = Some(endpoint);
    deny_ctx.web_fetcher = tekvra_tools::SafeWebFetcher::new(&["127.0.0.1".to_string()]);
    deny_ctx.approval_cb = Some(Arc::new(ApprovingCallback {
        prompts: Mutex::new(Vec::new()),
    }));
    let err = reg
        .execute(
            &deny_ctx,
            "bws3",
            "batch",
            &json!({ "calls": [{ "tool": "web_search", "arguments": { "query": "q" } }] }),
        )
        .await
        .expect_err("endpoint deny rule must hold inside batch");
    assert!(
        err.to_string().contains("denied") || err.to_string().contains("Denied"),
        "got: {err}"
    );
}

fn sse_response(rpc: Value) -> String {
    let body = format!("data: {rpc}\r\n\r\n");
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Per-server MCP HTTP streaming timeouts (`http_idle_timeout_sec`) override the built-in
/// budget: a slower-than-default stream passes with a larger override and still trips the
/// (overridden) idle timeout when the override is smaller.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_http_idle_timeout_is_configurable_per_server() {
    use indexmap::IndexMap;
    use tekvra_config::McpServerConfig;
    use tekvra_tools::ConfiguredMcpGateway;

    const CHUNK_DELAY_MS: u64 = 1200;

    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("mkdir ws");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                // Read headers, then the Content-Length body.
                let mut buf = vec![0_u8; 8192];
                let mut read = 0_usize;
                let body_start = loop {
                    let n = stream.read(&mut buf[read..]).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    read += n;
                    let head = String::from_utf8_lossy(&buf[..read]);
                    if let Some(pos) = head.find("\r\n\r\n") {
                        let len: usize = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if read >= pos + 4 + len {
                            break pos;
                        }
                    }
                };
                let body = String::from_utf8_lossy(&buf[body_start + 4..read]).to_string();
                let req: Value = serde_json::from_str(&body).unwrap_or(json!({}));
                let mid = req.get("id").cloned().unwrap_or(json!(0));
                let method = req.get("method").and_then(Value::as_str).unwrap_or("");
                let result = match method {
                    "initialize" => sse_response(json!({ "jsonrpc": "2.0", "id": mid, "result": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "serverInfo": { "name": "mock-slow", "version": "1" }
                    }})),
                    "tools/list" => {
                        sse_response(json!({ "jsonrpc": "2.0", "id": mid, "result": { "tools": [
                            { "name": "slow", "description": "d", "inputSchema": { "type": "object" } }
                        ]}}))
                    }
                    // tools/call: an immediate chunk, then the result after the delay.
                    // Headers declare the FULL body up front (first event + delayed
                    // result) so hyper streams bytes as they arrive without framing.
                    "tools/call" => {
                        let first = format!(
                            "data: {}\r\n\r\n",
                            json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": {} })
                        );
                        let rpc = json!({ "jsonrpc": "2.0", "id": mid, "result": {
                            "content": [{ "type": "text", "text": "slow-done" }]
                        }})
                        .to_string();
                        let rest = format!("data: {rpc}\r\n\r\n");
                        let _ = stream
                            .write_all(
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    first.len() + rest.len()
                                )
                                .as_bytes(),
                            )
                            .await;
                        let _ = stream.write_all(first.as_bytes()).await;
                        let _ = stream.flush().await;
                        tokio::time::sleep(std::time::Duration::from_millis(CHUNK_DELAY_MS)).await;
                        rest
                    }
                    _ => sse_response(json!({ "jsonrpc": "2.0", "id": mid, "result": {} })),
                };
                let _ = stream.write_all(result.as_bytes()).await;
            });
        }
    });

    let mut servers = IndexMap::new();
    servers.insert(
        "slow_srv".to_string(),
        McpServerConfig {
            url: Some(format!("http://{addr}/mcp")),
            enabled: true,
            http_idle_timeout_sec: Some(1),
            ..McpServerConfig::default()
        },
    );

    // Override smaller than the stream's inter-chunk gap → idle timeout fires.
    let (gw, report) =
        ConfiguredMcpGateway::connect_all(&servers, &ws, &AgentSettings::default(), false).await;
    eprintln!("WARNINGS {:?}", report.warnings);
    assert_eq!(report.connected_servers, vec!["slow_srv"]);
    let mut ctx = make_ctx(
        ws.clone(),
        tmp.path().join("state"),
        PermissionMode::Auto,
        Vec::new(),
    );
    ctx.mcp_gateway = Some(gw);
    let reg = NativeToolRegistry::new();
    let err = reg
        .execute(
            &ctx,
            "mcp_idle_1",
            "code",
            &json!({ "code": "r = mcp_call_sync('slow_srv', 'slow', {})\nprint(r)" }),
        )
        .await
        .expect_err("1s override must trip before the 1.2s chunk");
    assert!(
        err.to_string().contains("SSE stream idle timeout"),
        "got: {err}"
    );

    // Override larger than the gap → the delayed chunk resolves the call.
    let mut servers_fast = IndexMap::new();
    servers_fast.insert(
        "slow_srv".to_string(),
        McpServerConfig {
            url: Some(format!("http://{addr}/mcp")),
            enabled: true,
            http_idle_timeout_sec: Some(5),
            ..McpServerConfig::default()
        },
    );
    let (gw2, _report) =
        ConfiguredMcpGateway::connect_all(&servers_fast, &ws, &AgentSettings::default(), false)
            .await;
    let mut ctx2 = make_ctx(
        ws,
        tmp.path().join("state2"),
        PermissionMode::Auto,
        Vec::new(),
    );
    ctx2.mcp_gateway = Some(gw2);
    let out = reg
        .execute(
            &ctx2,
            "mcp_idle_2",
            "code",
            &json!({ "code": "r = mcp_call_sync('slow_srv', 'slow', {})\nprint(r['content'][0]['text'])" }),
        )
        .await
        .expect("5s override must survive the 1.2s gap");
    let ToolOutput::CodeExecution { output, .. } = out else {
        panic!("expected CodeExecution");
    };
    assert_eq!(output.trim(), "slow-done");
}

/// One request received by [`spawn_mcp_mock`].
#[derive(Clone, Debug)]
struct MockRequest {
    path: String,
    method: String,
    params: Value,
    id: Value,
}

/// Serves streamable-HTTP JSON-RPC on a loopback port and logs every request. `respond` returns
/// the SSE body for a request (`None`, or a notification, answers with a bare `202`).
async fn spawn_mcp_mock(
    respond: impl Fn(&MockRequest) -> Option<String> + Send + Sync + 'static,
) -> (std::net::SocketAddr, Arc<Mutex<Vec<MockRequest>>>) {
    let log: Arc<Mutex<Vec<MockRequest>>> = Arc::default();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mcp mock");
    let addr = listener.local_addr().expect("addr");
    let respond = Arc::new(respond);
    let log_srv = Arc::clone(&log);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let respond = Arc::clone(&respond);
            let log = Arc::clone(&log_srv);
            tokio::spawn(async move {
                // Read the headers, then the Content-Length body.
                let mut buf = Vec::new();
                let mut chunk = vec![0_u8; 8192];
                let body_start = loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let head = String::from_utf8_lossy(&buf);
                    if let Some(pos) = head.find("\r\n\r\n") {
                        let len: usize = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buf.len() >= pos + 4 + len {
                            break pos;
                        }
                    }
                };
                let head = String::from_utf8_lossy(&buf[..body_start]).into_owned();
                let path = head
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let body = String::from_utf8_lossy(&buf[body_start + 4..]).to_string();
                let Ok(msg) = serde_json::from_str::<Value>(body.trim()) else {
                    return;
                };
                let req = MockRequest {
                    path,
                    method: msg
                        .get("method")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    params: msg.get("params").cloned().unwrap_or_else(|| json!({})),
                    id: msg.get("id").cloned().unwrap_or(Value::Null),
                };
                let is_notification = msg.get("id").is_none();
                log.lock().expect("lock").push(req.clone());
                let http = match (is_notification, respond(&req)) {
                    (true, _) | (false, None) => {
                        "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    }
                    (false, Some(sse)) => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                        sse.len()
                    ),
                };
                let _ = stream.write_all(http.as_bytes()).await;
            });
        }
    });
    (addr, log)
}

/// One SSE `data:` frame carrying `rpc`.
fn sse_frame(rpc: Value) -> String {
    format!("data: {rpc}\r\n\r\n")
}

/// One `notifications/progress` delivery observed by a hook.
#[derive(Debug, Clone, PartialEq)]
struct ProgressSeen {
    server: String,
    tool: String,
    progress: f64,
    total: Option<f64>,
    message: Option<String>,
}

/// Hook recording every progress delivery in arrival order.
#[derive(Default)]
struct ProgressLog {
    seen: Mutex<Vec<ProgressSeen>>,
}

impl tekvra_tools::McpProgressHook for ProgressLog {
    fn on_progress(
        &self,
        server: &str,
        tool: &str,
        progress: f64,
        total: Option<f64>,
        message: Option<&str>,
    ) {
        self.seen.lock().expect("lock").push(ProgressSeen {
            server: server.to_string(),
            tool: tool.to_string(),
            progress,
            total,
            message: message.map(str::to_string),
        });
    }
}

/// Catalog discovery: capabilities recorded at handshake gate the list requests (a server
/// without them receives none, and a `tools/list` failure is only fatal when `tools` is
/// advertised), lists are cursor-paginated then sorted by server and URI/name with duplicates
/// removed, `notifications/resources/list_changed` triggers a refresh that picks up the change,
/// and none of it touches the model-facing tool schemas.
#[tokio::test]
async fn mcp_catalog_discovery_is_sorted_paginated_refreshed_and_capability_gated() {
    use indexmap::IndexMap;
    use std::sync::atomic::AtomicBool;
    use tekvra_config::{AgentSettings, McpServerConfig};
    use tekvra_tools::{
        ConfiguredMcpGateway, McpPromptArgument, McpPromptDescriptor, McpResourceDescriptor,
        McpResourceTemplateDescriptor,
    };

    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("mkdir ws");

    let resources_v2 = Arc::new(AtomicBool::new(false));
    let resources_v2_srv = Arc::clone(&resources_v2);
    let (addr, log) = spawn_mcp_mock(move |req| {
        let result = match (req.path.as_str(), req.method.as_str()) {
            ("/docs", "initialize") => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {}, "resources": { "listChanged": true }, "prompts": {} },
                "serverInfo": { "name": "docs", "version": "1" }
            }),
            ("/plain", "initialize") | ("/zeta", "initialize") => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": if req.path == "/zeta" { json!({ "resources": {} }) } else { json!({}) },
                "serverInfo": { "name": "mock", "version": "1" }
            }),
            // Advertises no `tools`: rejecting `tools/list` must not fail the connection.
            ("/zeta", "tools/list") => {
                return Some(sse_frame(json!({
                    "jsonrpc": "2.0", "id": req.id,
                    "error": { "code": -32601, "message": "Method not found" }
                })));
            }
            ("/docs", "tools/list") => json!({ "tools": [
                { "name": "lookup", "description": "Lookup docs", "inputSchema": { "type": "object" } }
            ] }),
            ("/plain", "tools/list") => json!({ "tools": [] }),
            ("/docs", "resources/list") => match req.params.get("cursor").and_then(Value::as_str) {
                None => json!({ "resources": [
                    { "uri": "file:///c.log", "name": "Changelog", "mimeType": "text/plain", "size": 3 },
                    { "uri": "file:///a.txt", "name": "Readme", "description": "Top readme", "mimeType": "text/markdown", "size": 10 },
                    { "uri": "file:///a.txt", "name": "Readme (duplicate)" }
                ], "nextCursor": "c2" }),
                Some(_) => json!({ "resources": [
                    { "uri": "file:///b.md", "mimeType": "text/markdown",
                      "name": if resources_v2_srv.load(Ordering::Acquire) { "Guide v2" } else { "Guide" } }
                ] }),
            },
            ("/zeta", "resources/list") => {
                json!({ "resources": [ { "uri": "file:///0.txt", "name": "Zero" } ] })
            }
            ("/docs", "resources/templates/list") => json!({ "resourceTemplates": [
                { "uriTemplate": "file:///{+path}", "name": "Any file",
                  "description": "Read any workspace file", "mimeType": "text/plain" }
            ] }),
            ("/docs", "prompts/list") => json!({ "prompts": [
                { "name": "summarize", "description": "Summarize a file" },
                { "name": "review", "description": "Review a diff",
                  "arguments": [ { "name": "path", "description": "File to review", "required": true } ] }
            ] }),
            ("/docs", "tools/call") => {
                resources_v2_srv.store(true, Ordering::Release);
                return Some(format!(
                    "{}{}",
                    sse_frame(json!({ "jsonrpc": "2.0", "method": "notifications/resources/list_changed" })),
                    sse_frame(json!({ "jsonrpc": "2.0", "id": req.id,
                        "result": { "content": [ { "type": "text", "text": "ok" } ] } }))
                ));
            }
            _ => json!({}),
        };
        Some(sse_frame(json!({ "jsonrpc": "2.0", "id": req.id, "result": result })))
    })
    .await;

    let mut servers = IndexMap::new();
    for (name, path) in [("docs", "/docs"), ("zeta", "/zeta"), ("plain", "/plain")] {
        servers.insert(
            name.to_string(),
            McpServerConfig {
                url: Some(format!("http://{addr}{path}")),
                enabled: true,
                ..McpServerConfig::default()
            },
        );
    }

    let codex_caps = ModelCapabilities::resolve("openai", "gpt-5.4-codex", None);
    let schema_before = build_tool_definitions_for_model(&codex_caps);

    let (gw, report) =
        ConfiguredMcpGateway::connect_all(&servers, &ws, &AgentSettings::default(), false).await;
    let mut connected = report.connected_servers.clone();
    connected.sort();
    assert_eq!(connected, ["docs", "plain", "zeta"]);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);

    // Sorted by server then URI/name; the duplicate `file:///a.txt` is listed once.
    let resources = gw.list_resources();
    let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
    assert_eq!(
        uris,
        [
            "file:///a.txt",
            "file:///b.md",
            "file:///c.log",
            "file:///0.txt"
        ]
    );
    assert_eq!(
        resources[0],
        McpResourceDescriptor {
            server: "docs".to_string(),
            uri: "file:///a.txt".to_string(),
            name: "Readme".to_string(),
            description: Some("Top readme".to_string()),
            mime_type: Some("text/markdown".to_string()),
        }
    );
    assert_eq!(resources[3].server, "zeta");
    assert_eq!(
        gw.list_resource_templates(),
        vec![McpResourceTemplateDescriptor {
            server: "docs".to_string(),
            uri_template: "file:///{+path}".to_string(),
            name: "Any file".to_string(),
            description: Some("Read any workspace file".to_string()),
            mime_type: Some("text/plain".to_string()),
        }]
    );
    let prompts = gw.list_prompts();
    let names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["review", "summarize"]);
    assert_eq!(
        prompts[0],
        McpPromptDescriptor {
            server: "docs".to_string(),
            name: "review".to_string(),
            description: Some("Review a diff".to_string()),
            arguments: vec![McpPromptArgument {
                name: "path".to_string(),
                description: Some("File to review".to_string()),
                required: Some(true),
            }],
        }
    );
    assert!(prompts[1].arguments.is_empty());

    // Only servers that advertise a capability receive its list requests; `docs` paginated.
    let requests = log.lock().expect("lock").clone();
    let methods = |path: &str| -> Vec<String> {
        requests
            .iter()
            .filter(|r| r.path == path)
            .map(|r| r.method.clone())
            .collect()
    };
    assert_eq!(
        methods("/plain"),
        ["initialize", "notifications/initialized", "tools/list"]
    );
    assert_eq!(
        methods("/zeta"),
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "resources/list",
            "resources/templates/list"
        ]
    );
    let docs_lists = || {
        methods("/docs")
            .into_iter()
            .filter(|m| m.ends_with("/list"))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        docs_lists(),
        [
            "tools/list",
            "resources/list",
            "resources/list",
            "resources/templates/list",
            "prompts/list"
        ]
    );

    // `notifications/resources/list_changed` (delivered on the `tools/call` stream) refreshes
    // the whole catalog before the call returns.
    let out = gw
        .call_tool("docs", "lookup", json!({}))
        .await
        .expect("call");
    assert_eq!(out["content"][0]["text"], "ok");
    let refreshed_resources = gw.list_resources();
    let names: Vec<&str> = refreshed_resources
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(names, ["Readme", "Guide v2", "Changelog", "Zero"]);

    // An explicit refresh re-queries every advertised list of that server only.
    let before = log.lock().expect("lock").len();
    let tools = gw
        .refresh_server_catalog("docs")
        .await
        .expect("refresh docs");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].tool, "lookup");
    let requests = log.lock().expect("lock").clone();
    let refreshed: Vec<String> = requests[before..]
        .iter()
        .map(|r| format!("{} {}", r.path, r.method))
        .collect();
    assert_eq!(
        refreshed,
        [
            "/docs tools/list",
            "/docs resources/list",
            "/docs resources/list",
            "/docs resources/templates/list",
            "/docs prompts/list"
        ]
    );
    assert_eq!(
        schema_before,
        build_tool_definitions_for_model(&codex_caps),
        "resource/prompt catalogs must never mutate top-level LLM tool schemas"
    );

    // Capability gating for the sandbox functions: unknown server, unadvertised capability.
    let err = gw
        .check_support("plain", "resources/read")
        .expect_err("plain advertises no resources");
    assert!(
        err.to_string().contains(
            "`plain` does not advertise the `resources` capability (needed for `resources/read`)"
        ),
        "{err}"
    );
    assert!(gw.check_support("zeta", "prompts/get").is_err());
    assert!(gw.check_support("plain", "tools/call").is_ok());
    let err = gw
        .check_support("nope", "tools/call")
        .expect_err("unknown server");
    assert!(
        err.to_string()
            .contains("MCP server `nope` is not connected"),
        "{err}"
    );
}

/// Static gateway advertising one resource, one template, and one prompt next to `docs/lookup`.
#[derive(Default)]
struct CatalogMcp {
    reads: AtomicUsize,
    prompt_gets: AtomicUsize,
}

#[async_trait]
impl McpGateway for CatalogMcp {
    fn list_tools(&self) -> Vec<McpToolDescriptor> {
        vec![McpToolDescriptor {
            server: "docs".to_string(),
            tool: "lookup".to_string(),
            description: "Lookup docs".to_string(),
            input_schema: json!({"type": "object"}),
        }]
    }

    fn list_resources(&self) -> Vec<tekvra_tools::McpResourceDescriptor> {
        vec![tekvra_tools::McpResourceDescriptor {
            server: "docs".to_string(),
            uri: "file:///README.md".to_string(),
            name: "README".to_string(),
            description: Some("Project readme".to_string()),
            mime_type: Some("text/markdown".to_string()),
        }]
    }

    fn list_resource_templates(&self) -> Vec<tekvra_tools::McpResourceTemplateDescriptor> {
        vec![tekvra_tools::McpResourceTemplateDescriptor {
            server: "docs".to_string(),
            uri_template: "file:///{+path}".to_string(),
            name: "Any file".to_string(),
            description: None,
            mime_type: None,
        }]
    }

    fn list_prompts(&self) -> Vec<tekvra_tools::McpPromptDescriptor> {
        vec![tekvra_tools::McpPromptDescriptor {
            server: "docs".to_string(),
            name: "review".to_string(),
            description: Some("Review a diff".to_string()),
            arguments: vec![tekvra_tools::McpPromptArgument {
                name: "path".to_string(),
                description: Some("File to review".to_string()),
                required: Some(true),
            }],
        }]
    }

    fn check_support(&self, server: &str, operation: &str) -> Result<(), tekvra_tools::ToolsError> {
        if server != "docs" {
            return Err(tekvra_tools::ToolsError::invalid(format!(
                "MCP server `{server}` is not connected"
            )));
        }
        if !matches!(operation, "tools/call" | "resources/read" | "prompts/get") {
            return Err(tekvra_tools::ToolsError::invalid(format!(
                "MCP server `{server}` does not advertise the `{operation}` capability"
            )));
        }
        Ok(())
    }

    async fn read_resource(
        &self,
        _server: &str,
        uri: &str,
    ) -> Result<Value, tekvra_tools::ToolsError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(json!({ "contents": [
            { "uri": uri, "mimeType": "text/markdown", "text": "x".repeat(5_000) }
        ] }))
    }

    async fn get_prompt(
        &self,
        _server: &str,
        name: &str,
        arguments: Value,
    ) -> Result<Value, tekvra_tools::ToolsError> {
        self.prompt_gets.fetch_add(1, Ordering::SeqCst);
        Ok(json!({
            "description": "Review prompt",
            "messages": [
                { "role": "user", "content": { "type": "text", "text": format!("Review {name}") } }
            ],
            "arguments": arguments
        }))
    }

    async fn call_tool(
        &self,
        _server: &str,
        _tool: &str,
        arguments: Value,
    ) -> Result<Value, tekvra_tools::ToolsError> {
        Ok(json!({ "echo": arguments }))
    }
}

/// `mcp_resources()` / `mcp_prompts()` return the cached catalogs as lists of dicts inside
/// `code` (empty without catalogs), and the static `code` description names them.
#[tokio::test]
async fn mcp_resources_and_mcp_prompts_expose_cached_catalogs_in_sandbox() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&ws).expect("mkdir");
    let reg = NativeToolRegistry::new();

    let mut ctx = make_ctx(ws.clone(), state.clone(), PermissionMode::Auto, Vec::new());
    ctx.mcp_gateway = Some(Arc::new(CatalogMcp::default()));
    let out = reg
        .execute(
            &ctx,
            "cat1",
            "code",
            &json!({ "code": "
resources = mcp_resources()
prompts = mcp_prompts()
for r in resources:
    print(r['server'], r['uri'], r['name'], r['description'], r['mime_type'])
for p in prompts:
    a = p['arguments'][0]
    print(p['server'], p['name'], p['description'], a['name'], a['description'], a['required'])
" }),
        )
        .await
        .expect("catalog functions");
    let ToolOutput::CodeExecution { output, .. } = out else {
        panic!("expected CodeExecution");
    };
    assert_eq!(
        output.trim(),
        "docs file:///README.md README Project readme text/markdown\n\
         docs review Review a diff path File to review True"
    );

    // No catalogs (default methods) and no gateway at all both yield empty lists.
    let gateways: [Option<Arc<dyn McpGateway>>; 2] = [Some(Arc::new(MockMcp::default())), None];
    for gateway in gateways {
        ctx.mcp_gateway = gateway;
        let out = reg
            .execute(
                &ctx,
                "cat2",
                "code",
                &json!({ "code": "print(mcp_resources(), mcp_prompts())" }),
            )
            .await
            .expect("empty catalogs");
        let ToolOutput::CodeExecution { output, .. } = out else {
            panic!("expected CodeExecution");
        };
        assert_eq!(output.trim(), "[] []");
    }

    let code_def = build_tool_definitions_for_model(&ModelCapabilities::resolve(
        "openai",
        "gpt-5.4-codex",
        None,
    ))
    .into_iter()
    .find(|def| def.name == "code")
    .expect("code tool definition");
    for named in [
        "mcp_list",
        "mcp_resources",
        "mcp_prompts",
        "mcp_call",
        "mcp_read",
        "mcp_get_prompt",
    ] {
        assert!(
            code_def.description.contains(named),
            "{}",
            code_def.description
        );
    }
    assert!(
        !code_def.description.contains("docs"),
        "{}",
        code_def.description
    );
}

struct DenyingCallback;

#[async_trait]
impl tekvra_tools::ApprovalCallback for DenyingCallback {
    async fn request_approval(
        &self,
        _prompt: tekvra_protocol::ApprovalPrompt,
    ) -> ApprovalResolution {
        ApprovalResolution::Deny {
            reason: Some("no".to_string()),
        }
    }
}

/// `mcp_read` / `mcp_get_prompt` authorize as `resources/read` / `prompts/get` before any
/// request leaves, bound their results, and fail validation (unknown server, unadvertised
/// capability, bad `max_chars`) without an approval prompt or a gateway call.
#[tokio::test]
async fn mcp_read_and_mcp_get_prompt_authorize_bound_and_validate() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&ws).expect("mkdir");
    let reg = NativeToolRegistry::new();
    let snippet = json!({ "code": "
r = await mcp_read('docs', 'file:///README.md', 2000)
print(len(r['contents'][0]['text']), r.get('truncated'))
full = await mcp_read('docs', 'file:///README.md')
print(len(full['contents'][0]['text']), 'truncated' in full)
p = await mcp_get_prompt('docs', 'review', {'path': 'src/main.rs'})
print(p['messages'][0]['content']['text'], p['arguments']['path'])
try:
    mcp_read_sync('nope', 'file:///x')
    print('unexpected success')
except RuntimeError as e:
    print(e)
" });

    // 1. Build mode without an approval callback fails closed.
    let ctx_no_cb = make_ctx(ws.clone(), state.clone(), PermissionMode::Build, Vec::new());
    let err = reg
        .execute(&ctx_no_cb, "r0", "code", &snippet)
        .await
        .expect_err("prompt without approval callback must deny");
    assert!(err.to_string().contains("approval required"), "{err}");

    // 2. Interactive approval: one prompt per read / get, each under its authorization name.
    let mut ctx = make_ctx(ws.clone(), state.clone(), PermissionMode::Build, Vec::new());
    let catalog = Arc::new(CatalogMcp::default());
    ctx.mcp_gateway = Some(catalog.clone());
    let approving = Arc::new(ApprovingCallback {
        prompts: Mutex::new(Vec::new()),
    });
    ctx.approval_cb = Some(approving.clone());
    let out = reg
        .execute(&ctx, "r1", "code", &snippet)
        .await
        .expect("approved mcp_read / mcp_get_prompt");
    let ToolOutput::CodeExecution { output, mcp_calls } = out else {
        panic!("expected CodeExecution");
    };
    assert_eq!(mcp_calls, 3);
    let lines: Vec<&str> = output.trim().lines().collect();
    assert_eq!(lines.len(), 4, "{output}");
    assert_eq!(lines[0], "2000 True");
    assert_eq!(lines[1], "5000 False");
    assert_eq!(lines[2], "Review review src/main.rs");
    assert!(lines[3].contains("`nope` is not connected"), "{}", lines[3]);
    // One prompt per authorization name: the project-lifetime grant for `docs/resources/read`
    // covers the second read, while `docs/prompts/get` still prompts.
    assert_eq!(
        *approving.prompts.lock().expect("lock"),
        [
            "mcp: docs/resources/read".to_string(),
            "mcp: docs/prompts/get".to_string(),
        ]
    );
    assert_eq!(catalog.reads.load(Ordering::SeqCst), 2);
    assert_eq!(catalog.prompt_gets.load(Ordering::SeqCst), 1);

    // 2b. A bad `max_chars` fails the cell as a validation error before any prompt or call.
    let prompts_before = approving.prompts.lock().expect("lock").len();
    let err = reg
        .execute(
            &ctx,
            "r1b",
            "code",
            &json!({ "code": "await mcp_read_sync('docs', 'file:///README.md', 0)" }),
        )
        .await
        .expect_err("bad max_chars");
    assert!(
        matches!(err, tekvra_tools::ToolsError::InvalidInput { .. })
            && err
                .to_string()
                .contains("requires a positive integer `max_chars`"),
        "{err:?}"
    );
    assert_eq!(
        approving.prompts.lock().expect("lock").len(),
        prompts_before
    );
    assert_eq!(catalog.reads.load(Ordering::SeqCst), 2);

    // 3. Denying the prompt refuses the read before it reaches the gateway; a fresh context
    //    holds none of the grants above.
    let reads_before = catalog.reads.load(Ordering::SeqCst);
    let mut ctx = make_ctx(
        ws.clone(),
        tmp.path().join("state-deny"),
        PermissionMode::Build,
        Vec::new(),
    );
    ctx.mcp_gateway = Some(catalog.clone());
    ctx.approval_cb = Some(Arc::new(DenyingCallback));
    let err = reg
        .execute(
            &ctx,
            "r2",
            "code",
            &json!({ "code": "await mcp_read('docs', 'file:///README.md')" }),
        )
        .await
        .expect_err("denied read");
    assert!(
        matches!(err, tekvra_tools::ToolsError::PermissionDenied { .. }),
        "{err:?}"
    );
    assert_eq!(catalog.reads.load(Ordering::SeqCst), reads_before);

    // 4. A `deny` rule covering `docs` refuses `mcp_get_prompt`; the gateway sees no request.
    let rules = vec![PermissionRule {
        tool: "mcp".to_string(),
        pattern: "docs/*".to_string(),
        action: RuleAction::Deny,
    }];
    let mut ctx = make_ctx(ws, state, PermissionMode::Auto, rules);
    ctx.mcp_gateway = Some(catalog.clone());
    let err = reg
        .execute(
            &ctx,
            "r3",
            "code",
            &json!({ "code": "await mcp_get_prompt_sync('docs', 'review', {})" }),
        )
        .await
        .expect_err("denied prompt");
    assert!(
        err.to_string()
            .contains("MCP call `docs/prompts/get` blocked by explicit `deny` rule"),
        "{err}"
    );
    assert_eq!(catalog.prompt_gets.load(Ordering::SeqCst), 1);
}

/// `notifications/progress` frames carry the `_meta.progressToken` of their `tools/call` and
/// reach the hook (coalesced, in order, with server and tool) while the call still returns its
/// result; unknown tokens are dropped without a reply. Streamable HTTP and stdio both apply.
#[tokio::test]
async fn notifications_progress_reach_the_hook_for_in_flight_calls_only() {
    use indexmap::IndexMap;
    use tekvra_config::{AgentSettings, McpServerConfig};
    use tekvra_tools::{ConfiguredMcpGateway, McpServerHooks};

    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("mkdir ws");
    let progress = Arc::new(ProgressLog::default());
    let (addr, log) = spawn_mcp_mock(move |req| {
        let result = match req.method.as_str() {
            "initialize" => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "prog", "version": "1" }
            }),
            "tools/list" => json!({ "tools": [
                { "name": "long_job", "description": "d", "inputSchema": { "type": "object" } }
            ] }),
            "tools/call" => {
                let token = req.params["_meta"]["progressToken"].clone();
                let frame = |params: Value| {
                    sse_frame(json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": params }))
                };
                return Some(format!(
                    "{}{}{}{}{}",
                    frame(json!({ "progressToken": token, "progress": 1, "total": 3 })),
                    frame(json!({ "progressToken": token, "progress": 2, "total": 3, "message": "indexing" })),
                    // Unknown (never issued) and non-numeric tokens are dropped.
                    frame(json!({ "progressToken": 4_242_424, "progress": 3, "total": 3 })),
                    frame(json!({ "progressToken": "not-ours", "progress": 3, "total": 3 })),
                    sse_frame(json!({ "jsonrpc": "2.0", "id": req.id,
                        "result": { "content": [ { "type": "text", "text": "done" } ] } }))
                ));
            }
            _ => json!({}),
        };
        Some(sse_frame(json!({ "jsonrpc": "2.0", "id": req.id, "result": result })))
    })
    .await;

    let mut servers = IndexMap::new();
    servers.insert(
        "http_srv".to_string(),
        McpServerConfig {
            url: Some(format!("http://{addr}/mcp")),
            enabled: true,
            ..McpServerConfig::default()
        },
    );

    // Stdio mock streaming the same two frames before the result, on the request's token.
    let script = tmp.path().join("progress_stdio_mcp.py");
    std::fs::write(
        &script,
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
        res = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "prog-stdio", "version": "1"}}
    elif method == "tools/list":
        res = {"tools": [{"name": "long_job", "description": "d", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        token = msg["params"]["_meta"]["progressToken"]
        send({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": token, "progress": 1, "total": 3}})
        send({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": token, "progress": 2, "total": 3, "message": "indexing"}})
        res = {"content": [{"type": "text", "text": "done"}]}
    else:
        res = {}
    send({"jsonrpc": "2.0", "id": mid, "result": res})
"#,
    )
    .expect("write progress stdio script");
    servers.insert(
        "stdio_srv".to_string(),
        McpServerConfig {
            command: Some("python3".to_string()),
            args: vec![script.to_string_lossy().into_owned()],
            enabled: true,
            ..McpServerConfig::default()
        },
    );

    let (gw, report) =
        ConfiguredMcpGateway::connect_all(&servers, &ws, &AgentSettings::default(), false).await;
    let mut connected = report.connected_servers.clone();
    connected.sort();
    assert_eq!(connected, ["http_srv", "stdio_srv"]);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    gw.set_server_hooks(Arc::new(McpServerHooks {
        sampling: None,
        elicitation: None,
        progress: Some(Arc::clone(&progress) as Arc<dyn tekvra_tools::McpProgressHook>),
    }));

    for server in ["http_srv", "stdio_srv"] {
        let out = gw
            .call_tool(server, "long_job", json!({}))
            .await
            .expect("progress-bearing call returns its result");
        assert_eq!(out["content"][0]["text"], "done", "{server}");
    }

    // The second frame of each call is coalesced (held >= 100 ms after the first): poll for it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while progress.seen.lock().expect("lock").len() < 4 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let mut seen = progress.seen.lock().expect("lock").clone();
    seen.sort_by(|a, b| a.server.cmp(&b.server));
    assert_eq!(
        seen,
        [
            ProgressSeen {
                server: "http_srv".to_string(),
                tool: "long_job".to_string(),
                progress: 1.0,
                total: Some(3.0),
                message: None
            },
            ProgressSeen {
                server: "http_srv".to_string(),
                tool: "long_job".to_string(),
                progress: 2.0,
                total: Some(3.0),
                message: Some("indexing".to_string())
            },
            ProgressSeen {
                server: "stdio_srv".to_string(),
                tool: "long_job".to_string(),
                progress: 1.0,
                total: Some(3.0),
                message: None
            },
            ProgressSeen {
                server: "stdio_srv".to_string(),
                tool: "long_job".to_string(),
                progress: 2.0,
                total: Some(3.0),
                message: Some("indexing".to_string())
            },
        ]
    );
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        progress.seen.lock().expect("lock").len(),
        4,
        "unknown tokens must not reach the hook"
    );

    // Every `tools/call` carried a numeric progress token; the gateway never answered a
    // progress notification (all outbound frames are requests or client notifications).
    let requests = log.lock().expect("lock").clone();
    let calls: Vec<&MockRequest> = requests
        .iter()
        .filter(|r| r.method == "tools/call")
        .collect();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].params["_meta"]["progressToken"].is_u64(),
        "{}",
        calls[0].params
    );
    assert!(requests.iter().all(|r| !r.method.is_empty()));
}

/// Gateway advertising nothing (no tools, no catalogs).
struct EmptyMcp;

#[async_trait]
impl McpGateway for EmptyMcp {
    fn list_tools(&self) -> Vec<McpToolDescriptor> {
        Vec::new()
    }

    async fn call_tool(
        &self,
        _server: &str,
        _tool: &str,
        _arguments: Value,
    ) -> Result<Value, tekvra_tools::ToolsError> {
        unreachable!("no tools to call")
    }
}

/// `/mcp` text and `tekvra mcp test` catalog lines: tools first, then resources (URIs, then
/// templates), then prompts, per server in name order; servers without catalogs add nothing.
#[test]
fn mcp_status_text_lists_tools_then_catalogs_only_when_present() {
    use tekvra_tools::{format_mcp_status, mcp_catalog_lines};

    let docs = CatalogMcp::default();
    assert_eq!(
        format_mcp_status(&docs),
        "Connected MCP tools (1): docs/lookup\n\
         docs resources: file:///README.md, file:///{+path}\n\
         docs prompts: review"
    );
    assert_eq!(
        mcp_catalog_lines(&docs),
        [
            (
                "docs".to_string(),
                "resources: file:///README.md, file:///{+path}".to_string()
            ),
            ("docs".to_string(), "prompts: review".to_string()),
        ]
    );

    // Tools-only gateways render exactly as before the catalogs existed.
    let tools_only = MockMcp::default();
    assert_eq!(
        format_mcp_status(&tools_only),
        "Connected MCP tools (2): docs/lookup, docs/forbidden"
    );
    assert!(mcp_catalog_lines(&tools_only).is_empty());
    assert_eq!(
        format_mcp_status(&EmptyMcp),
        "MCP gateway connected (0 tools advertised)."
    );
}

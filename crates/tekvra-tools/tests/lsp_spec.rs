//! Specification tests for the `lsp_diagnostics` tool (`phase6-mcp-streaming-lsp-websearch`):
//! framed JSON-RPC round trip, per-session server reuse, bounded startup-crash errors,
//! missing-config errors, and read-ladder authorization.

use std::path::PathBuf;

use serde_json::json;
use tekvra_config::{LspServerConfig, PermissionRule};
use tekvra_protocol::{PermissionMode, RuleAction, ToolOutput};
use tekvra_tools::{LspManager, NativeToolRegistry};
use tempfile::TempDir;

fn make_ctx(
    ws: PathBuf,
    state_dir: PathBuf,
    mode: PermissionMode,
    rules: Vec<PermissionRule>,
    lsp: Option<LspManager>,
) -> tekvra_tools::ToolExecutionContext {
    let index = tekvra_storage::StateIndex::open(&state_dir).expect("open state index");
    let snapshots = tekvra_storage::WorkspaceSnapshotStore::new(&state_dir);
    let evaluator = tekvra_tools::PermissionEvaluator::new(
        mode,
        std::fs::canonicalize(&ws).expect("canon ws"),
        Some(ws.clone()),
        rules,
        Vec::new(),
    );
    tekvra_tools::ToolExecutionContext {
        session_id: "thr_lsp".to_string(),
        turn_id: "turn_lsp".to_string(),
        workspace_root: std::fs::canonicalize(&ws).expect("canon ws"),
        settings: tekvra_config::AgentSettings::default(),
        path_locks: tekvra_tools::WorkspacePathLocks::new(),
        pty_manager: tekvra_tools::PtySessionManager::new(None),
        web_fetcher: tekvra_tools::SafeWebFetcher::new(&[]),
        code_sandbox: tekvra_tools::MontyCodeSandbox,
        permissions: std::sync::Arc::new(std::sync::Mutex::new(evaluator)),
        state_index: Some(std::sync::Arc::new(std::sync::Mutex::new(index))),
        snapshots: Some(snapshots),
        mcp_gateway: None,
        approval_cb: None,
        question_cb: None,
        lsp_manager: lsp.map(std::sync::Arc::new),
        plan_markdown: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
    }
}

/// Minimal LSP server: framed JSON-RPC over stdio, one publishDiagnostics per didOpen,
/// `initialize` count persisted to `state_path` for reuse assertions.
fn write_mock_server(tmp: &TempDir) -> PathBuf {
    let script = tmp.path().join("mock_lsp.py");
    std::fs::write(
        &script,
        r#"import json, sys

state_path = sys.argv[1]
init_count = 0
try:
    init_count = int(open(state_path).read().strip())
except Exception:
    init_count = 0

def read_msg():
    headers = {}
    while True:
        line = sys.stdin.readline()
        if line in ("\n", "\r\n", ""):
            break
        k, _, v = line.partition(":")
        headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", 0))
    return json.loads(sys.stdin.read(n))

def send(obj):
    body = json.dumps(obj).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()

while True:
    try:
        msg = read_msg()
    except Exception:
        break
    m = msg.get("method")
    if m == "initialize":
        init_count += 1
        open(state_path, "w").write(str(init_count))
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"capabilities": {"textDocumentSync": 1}}})
    elif m == "textDocument/didOpen":
        uri = msg["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
              "params": {"uri": uri, "diagnostics": [
                  {"range": {"start": {"line": 2, "character": 4}},
                   "severity": 1, "message": "LSP_MOCK_PROBLEM"}]}})
"#,
    )
    .expect("write mock lsp");
    script
}

fn lsp_config(script: &std::path::Path, state: &std::path::Path) -> LspServerConfig {
    LspServerConfig {
        command: Some("python3".to_string()),
        args: vec![
            script.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        ],
        languages: Vec::new(),
    }
}

#[tokio::test]
async fn lsp_diagnostics_round_trip_reports_severity_and_reuses_server() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&ws).expect("mkdir ws");
    std::fs::create_dir_all(&state).expect("mkdir state");

    let script = write_mock_server(&tmp);
    let state_path = ws.join("lsp_mock_state");
    let config = lsp_config(&script, &state_path);
    let manager = LspManager::new(
        [("mock".to_string(), config)].into_iter().collect(),
        ws.clone(),
    );

    std::fs::write(ws.join("broken.rs"), "fn a() {\n  let x = ;\n}\n").expect("write file");

    // First call: initialize + didOpen + publishDiagnostics.
    let diags = manager
        .diagnostics("mock", &ws.join("broken.rs"))
        .await
        .expect("first diagnostics call");
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].line, 3);
    assert_eq!(diags[0].column, 5);
    assert_eq!(diags[0].severity, "error");
    assert_eq!(diags[0].message, "LSP_MOCK_PROBLEM");

    // Server-side initialize count is exactly 1.
    let count1: u32 = std::fs::read_to_string(&state_path)
        .expect("state file")
        .trim()
        .parse()
        .expect("count");
    assert_eq!(count1, 1);

    // Second call reuses the same server process (no re-initialize).
    let diags2 = manager
        .diagnostics("mock", &ws.join("broken.rs"))
        .await
        .expect("second diagnostics call");
    assert_eq!(diags2.len(), 1);
    let count2: u32 = std::fs::read_to_string(&state_path)
        .expect("state file")
        .trim()
        .parse()
        .expect("count");
    assert_eq!(count2, 1, "server must be reused across calls");

    manager.shutdown_all().await;
}

#[tokio::test]
async fn lsp_tool_registry_arm_authorizes_reads_and_reports_bounded_errors() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir ws/.git");
    std::fs::create_dir_all(&state).expect("mkdir state");

    let script = write_mock_server(&tmp);
    let config = lsp_config(&script, &ws.join("lsp_mock_state"));
    let manager = LspManager::new(
        [("mock".to_string(), config)].into_iter().collect(),
        ws.clone(),
    );

    std::fs::write(ws.join("broken.rs"), "fn a() {}\n").expect("write file");

    // Happy path through the registry (ReadWorkspace ladder is read-only → allowed).
    let ctx = make_ctx(
        ws.clone(),
        state.clone(),
        PermissionMode::Build,
        Vec::new(),
        Some(manager),
    );
    let reg = NativeToolRegistry::new();
    let out = reg
        .execute(
            &ctx,
            "lsp_ok_1",
            "lsp_diagnostics",
            &json!({ "path": "broken.rs" }),
        )
        .await
        .expect("diagnostics through registry");
    let ToolOutput::Text { text } = out else {
        panic!("expected Text");
    };
    assert!(
        text.contains("broken.rs:3:5 [error] LSP_MOCK_PROBLEM"),
        "got: {text}"
    );

    // Startup crash → bounded error.
    let crashing = LspManager::new(
        [(
            "crash".to_string(),
            LspServerConfig {
                command: Some("false".to_string()),
                args: Vec::new(),
                languages: Vec::new(),
            },
        )]
        .into_iter()
        .collect(),
        ws.clone(),
    );
    let ctx_crash = make_ctx(
        ws.clone(),
        state.clone(),
        PermissionMode::Build,
        Vec::new(),
        Some(crashing),
    );
    let err = reg
        .execute(
            &ctx_crash,
            "lsp_crash_1",
            "lsp_diagnostics",
            &json!({ "path": "broken.rs", "server": "crash" }),
        )
        .await
        .expect_err("crashing server must yield a bounded error");
    assert!(
        err.to_string().contains("failed to start server")
            || err.to_string().contains("initialize failed or timed out"),
        "got: {err}"
    );

    // Missing configuration → bounded error.
    let ctx_none = make_ctx(
        ws.clone(),
        state.clone(),
        PermissionMode::Build,
        Vec::new(),
        None,
    );
    let err = reg
        .execute(
            &ctx_none,
            "lsp_none_1",
            "lsp_diagnostics",
            &json!({ "path": "broken.rs" }),
        )
        .await
        .expect_err("missing config must yield a bounded error");
    assert!(
        err.to_string().contains("requires configured"),
        "got: {err}"
    );

    // Extension mismatch (languages filter) → bounded error.
    let filtered = LspManager::new(
        [(
            "py_only".to_string(),
            LspServerConfig {
                command: Some("python3".to_string()),
                args: vec![
                    script.to_string_lossy().into_owned(),
                    ws.join("s2").to_string_lossy().into_owned(),
                ],
                languages: vec!["py".to_string()],
            },
        )]
        .into_iter()
        .collect(),
        ws.clone(),
    );
    let ctx_filtered = make_ctx(ws, state, PermissionMode::Build, Vec::new(), Some(filtered));
    let err = reg
        .execute(
            &ctx_filtered,
            "lsp_mismatch_1",
            "lsp_diagnostics",
            &json!({ "path": "broken.rs", "server": "py_only" }),
        )
        .await
        .expect_err("language mismatch must yield a bounded error");
    assert!(err.to_string().contains("no language server"), "got: {err}");

    // Deny rule on reads → refused by the ladder (before any server interaction).
    let deny_ws = tmp.path().join("deny_ws");
    let deny_state = tmp.path().join("deny_state");
    std::fs::create_dir_all(deny_ws.join(".git")).expect("mkdir deny ws");
    std::fs::create_dir_all(&deny_state).expect("mkdir deny state");
    std::fs::write(deny_ws.join("broken.rs"), "fn a() {}\n").expect("write deny file");
    let deny_target = deny_ws.join("broken.rs").to_string_lossy().into_owned();
    let deny = vec![PermissionRule {
        tool: "lsp_diagnostics".to_string(),
        pattern: deny_target,
        action: RuleAction::Deny,
    }];
    let manager_deny = LspManager::new(std::collections::BTreeMap::default(), deny_ws.clone());
    let ctx_deny = make_ctx(
        deny_ws,
        deny_state,
        PermissionMode::Build,
        deny,
        Some(manager_deny),
    );
    let err = reg
        .execute(
            &ctx_deny,
            "lsp_deny_1",
            "lsp_diagnostics",
            &json!({ "path": "broken.rs" }),
        )
        .await
        .expect_err("deny rule must refuse the call");
    assert!(
        err.to_string().contains("denied") || err.to_string().contains("Denied"),
        "got: {err}"
    );
}

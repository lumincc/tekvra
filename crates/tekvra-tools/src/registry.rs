//! Zero-Lua native built-in tool registry (`NativeToolRegistry`) and model-aligned
//! single-editor tool schema roster (`apply_patch` vs `file_edit` + `file_write`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tekvra_config::AgentSettings;
use tekvra_protocol::{ApprovalPrompt, ApprovalResolution, ToolOutput};
use tekvra_providers::{ModelCapabilities, ToolDefinition};
use tekvra_storage::{StateIndex, WorkspaceSnapshotStore};

use crate::ToolsError;
use crate::fs_tools::{
    FileEditSpec, WorkspacePathLocks, extract_file_outline, file_edit, file_glob, file_grep,
    file_read, file_write, view_image,
};
use crate::lsp_client::LspManager;
use crate::memory::MemoryStore;
use crate::patch::{PatchHunk, apply_patch_to_workspace, parse_patch};
use crate::permissions::{PermissionDecision, PermissionEvaluator, ToolAction};
use crate::pty::{PtyCommandRequest, PtySessionManager};
use crate::web_and_code::{McpCallAuthorizer, McpGateway, MontyCodeSandbox, SafeWebFetcher};
use crate::web_search::{DEFAULT_WEBSEARCH_BASE_URL, WebSearcher};

/// Host callback invoked when a tool call requires interactive user approval (`ApprovalPrompt`).
#[async_trait]
pub trait ApprovalCallback: Send + Sync {
    /// Presents `prompt` to the user/client and returns their [`ApprovalResolution`].
    async fn request_approval(&self, prompt: ApprovalPrompt) -> ApprovalResolution;
}

/// Host callback invoked when the `question` tool asks the user structured questions.
#[async_trait]
pub trait QuestionCallback: Send + Sync {
    /// Presents `questions` and returns `(question, answer)` pairs.
    async fn ask_questions(&self, questions: Vec<String>) -> Vec<(String, String)>;
}

/// Shared execution context passed to [`NativeToolRegistry::execute`].
pub struct ToolExecutionContext {
    /// Active session/conversation id.
    pub session_id: String,
    /// Active turn id.
    pub turn_id: String,
    /// Canonical workspace root.
    pub workspace_root: PathBuf,
    /// Agent runtime settings.
    pub settings: AgentSettings,
    /// Per-path file mutation locks.
    pub path_locks: WorkspacePathLocks,
    /// Interactive PTY session manager.
    pub pty_manager: PtySessionManager,
    /// SSRF-safe web fetcher.
    pub web_fetcher: SafeWebFetcher,
    /// Isolated Monty Python sandbox.
    pub code_sandbox: MontyCodeSandbox,
    /// Permission evaluator shared for the session.
    pub permissions: Arc<Mutex<PermissionEvaluator>>,
    /// Optional SQLite state index (retained raw outputs + persisted permission grants).
    pub state_index: Option<Arc<Mutex<StateIndex>>>,
    /// Optional content-addressed workspace file snapshot store.
    pub snapshots: Option<WorkspaceSnapshotStore>,
    /// Optional connected MCP gateway (invoked via `code`).
    pub mcp_gateway: Option<Arc<dyn McpGateway>>,
    /// Optional interactive approval callback (if `None`, prompts are denied cleanly).
    pub approval_cb: Option<Arc<dyn ApprovalCallback>>,
    /// Optional structured user question callback.
    pub question_cb: Option<Arc<dyn QuestionCallback>>,
    /// Optional session-scoped language-server manager (`lsp_diagnostics`).
    pub lsp_manager: Option<Arc<LspManager>>,
    /// Project memory store (`memory_save` tool; `None` when memory is unavailable).
    pub memory_store: Option<Arc<MemoryStore>>,
    /// Latest session plan Markdown updated via `plan`.
    pub plan_markdown: Arc<Mutex<String>>,
}

/// Builds the prompt-cache-stable LLM [`ToolDefinition`] roster for `model`.
///
/// Key invariants:
/// 1. **Single-editor alignment**: GPT-5+, o-series, and Codex models (`model.uses_apply_patch == true`)
///    receive `apply_patch` and NEVER `file_edit` / `file_write`; other models receive `file_edit` + `file_write`
///    and NEVER `apply_patch`.
/// 2. **Cache stability**: Connecting, disconnecting, or calling MCP servers NEVER alters this top-level tool list;
///    MCP tools are discovered and invoked inside `code`!
#[must_use]
pub fn build_tool_definitions_for_model(model: &ModelCapabilities) -> Vec<ToolDefinition> {
    let mut defs = Vec::with_capacity(16);

    if model.uses_apply_patch {
        defs.push(ToolDefinition {
            name: "apply_patch".to_string(),
            description: "Apply a `*** Begin Patch` ... `*** End Patch` multi-file patch (`Add File`, `Update File`, `Move to`, `Delete File`).".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": { "type": "string", "description": "Patch text using *** Begin Patch / *** End Patch grammar." }
                },
                "required": ["patch"]
            }),
        });
    } else {
        defs.push(ToolDefinition {
            name: "file_edit".to_string(),
            description: "Replace an exact string in an existing workspace file.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" },
                    "replace_all": { "type": "boolean" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        });
        defs.push(ToolDefinition {
            name: "file_write".to_string(),
            description: "Create or overwrite a workspace file with full text content.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        });
    }

    let common = [
        (
            "file_read",
            "Read a bounded line slice from a workspace file.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer" },
                    "max_lines": { "type": "integer" }
                },
                "required": ["path"]
            }),
        ),
        (
            "file_glob",
            "Find workspace files matching a glob pattern (skipping .git and credential files).",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "max_matches": { "type": "integer" }
                },
                "required": ["pattern"]
            }),
        ),
        (
            "file_grep",
            "Search workspace files with a regular expression (skipping .git and credential files).",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" },
                    "max_matches": { "type": "integer" }
                },
                "required": ["pattern"]
            }),
        ),
        (
            "file_index",
            "Extract structural syntax-tree symbol outlines with exact 1-based line ranges.",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        ),
        (
            "exec_command",
            "Execute a shell command in an interactive PTY with output noise reduction.",
            json!({
                "type": "object",
                "properties": {
                    "cmd": { "type": "string" },
                    "cwd": { "type": "string" },
                    "yield_time_ms": { "type": "integer" },
                    "background": { "type": "boolean" }
                },
                "required": ["cmd"]
            }),
        ),
        (
            "write_stdin",
            "Write characters to an active PTY session (`session_id`) and read newly emitted output.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "chars": { "type": "string" },
                    "yield_time_ms": { "type": "integer" }
                },
                "required": ["session_id"]
            }),
        ),
        (
            "tool_output",
            "Page or regex-search a retained unfiltered command output log by `output_id`.",
            json!({
                "type": "object",
                "properties": {
                    "output_id": { "type": "string" },
                    "offset_line": { "type": "integer" },
                    "limit_lines": { "type": "integer" },
                    "regex": { "type": "string" }
                },
                "required": ["output_id"]
            }),
        ),
        (
            "view_image",
            "Inspect an image file inside the workspace.",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        ),
        (
            "webfetch",
            "Fetch an HTTP/HTTPS URL with SSRF protection across DNS and redirects.",
            json!({
                "type": "object",
                "properties": { "url": { "type": "string" } },
                "required": ["url"]
            }),
        ),
        (
            "web_search",
            "Search the web via an HTML endpoint (SSRF-protected); returns numbered results.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "max_results": { "type": "integer", "minimum": 1, "maximum": 10 }
                },
                "required": ["query"]
            }),
        ),
        (
            "lsp_diagnostics",
            "Collect published diagnostics for a workspace file from a configured language server.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "server": { "type": "string", "description": "Optional [lsp.<id>] key; auto-selected when omitted." }
                },
                "required": ["path"]
            }),
        ),
        (
            "memory_save",
            "Append a durable fact to the cross-session project memory (dated entry in MEMORY.md).",
            json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "Single fact worth remembering across sessions." }
                },
                "required": ["content"]
            }),
        ),
        (
            "plan",
            "Create or update the structured session plan Markdown document.",
            json!({
                "type": "object",
                "properties": { "markdown": { "type": "string" } },
                "required": ["markdown"]
            }),
        ),
        (
            "question",
            "Ask the user clarifying questions.",
            json!({
                "type": "object",
                "properties": {
                    "questions": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["questions"]
            }),
        ),
        (
            "code",
            "Execute Python computation and orchestrate connected MCP tools (`mcp_list()`, `await mcp_call(server, tool, params)`) in an isolated sandbox.",
            json!({
                "type": "object",
                "properties": { "code": { "type": "string" } },
                "required": ["code"]
            }),
        ),
        (
            "batch",
            "Execute multiple read/search/index tool calls in a single batch.",
            json!({
                "type": "object",
                "properties": {
                    "calls": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "tool": { "type": "string" },
                                "arguments": { "type": "object" }
                            },
                            "required": ["tool", "arguments"]
                        }
                    }
                },
                "required": ["calls"]
            }),
        ),
        (
            "task",
            "Spawn or steer a durable foreground/background subagent task.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "New task prompt, or steering message when `task_id` is set." },
                    "role": { "type": "string", "description": "Optional subagent role override." },
                    "model": { "type": "string", "description": "Optional model spec override." },
                    "background": { "type": "boolean", "description": "Run without blocking the parent turn." },
                    "worktree": { "type": "boolean", "description": "Run in a dedicated git worktree (branch `tekvra/<task_id>`); merge back later with `merge`." },
                    "task_id": { "type": "string", "description": "Existing task id to steer instead of spawning." },
                    "delivery": { "type": "string", "enum": ["guide", "replace"], "description": "How `prompt` reaches a steered task: queued guidance (default) or replacement." },
                    "merge": { "type": "string", "enum": ["merge", "discard"], "description": "With `task_id`: merge the worktree branch into the parent tree, or discard the worktree and branch." }
                },
                "required": ["prompt"]
            }),
        ),
    ];

    for (name, desc, params) in common {
        defs.push(ToolDefinition {
            name: name.to_string(),
            description: desc.to_string(),
            parameters: params,
        });
    }
    defs
}

/// 100% native Rust built-in tool dispatcher requiring zero Lua VM initialization.
#[derive(Debug, Clone, Default)]
pub struct NativeToolRegistry;

impl NativeToolRegistry {
    /// Creates a [`NativeToolRegistry`].
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Authorizes and executes a built-in native tool call, returning structured [`ToolOutput`].
    ///
    /// # Errors
    /// Returns [`ToolsError`] on invalid arguments, permission denial, path escape, or execution failure.
    pub async fn execute(
        &self,
        ctx: &ToolExecutionContext,
        call_id: &str,
        tool_name: &str,
        args: &Value,
    ) -> Result<ToolOutput, ToolsError> {
        match tool_name {
            "file_read" => {
                let path = req_str(args, "path")?;
                self.authorize(
                    ctx,
                    call_id,
                    ToolAction::ReadWorkspace {
                        tool_name: "file_read".to_string(),
                        path: PathBuf::from(path),
                    },
                )
                .await?;
                let start = args
                    .get("start_line")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize);
                let max = args
                    .get("max_lines")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize);
                file_read(&ctx.workspace_root, Path::new(path), start, max)
            }
            "file_write" => {
                let path = req_str(args, "path")?;
                let content = req_str(args, "content")?;
                self.authorize(
                    ctx,
                    call_id,
                    ToolAction::WriteWorkspace {
                        tool_name: "file_write".to_string(),
                        paths: vec![PathBuf::from(path)],
                    },
                )
                .await?;
                file_write(
                    &ctx.workspace_root,
                    &ctx.session_id,
                    &ctx.turn_id,
                    Path::new(path),
                    content,
                    &ctx.path_locks,
                    ctx.snapshots.as_ref(),
                )
                .await
            }
            "file_edit" => {
                let path = req_str(args, "path")?;
                let old_s = req_str(args, "old_string")?;
                let new_s = req_str(args, "new_string")?;
                let replace_all = args
                    .get("replace_all")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.authorize(
                    ctx,
                    call_id,
                    ToolAction::WriteWorkspace {
                        tool_name: "file_edit".to_string(),
                        paths: vec![PathBuf::from(path)],
                    },
                )
                .await?;
                file_edit(
                    &ctx.workspace_root,
                    &ctx.session_id,
                    &ctx.turn_id,
                    FileEditSpec {
                        target: Path::new(path),
                        old_string: old_s,
                        new_string: new_s,
                        replace_all,
                    },
                    &ctx.path_locks,
                    ctx.snapshots.as_ref(),
                )
                .await
            }
            "apply_patch" => {
                let patch_text = req_str(args, "patch")?;
                let parsed = parse_patch(patch_text)?;
                let mut target_paths = Vec::new();
                for h in parsed {
                    match h {
                        PatchHunk::Add { path, .. } | PatchHunk::Delete { path } => {
                            target_paths.push(PathBuf::from(path));
                        }
                        PatchHunk::Update {
                            path, move_path, ..
                        } => {
                            target_paths.push(PathBuf::from(path));
                            if let Some(m) = move_path {
                                target_paths.push(PathBuf::from(m));
                            }
                        }
                    }
                }
                self.authorize(
                    ctx,
                    call_id,
                    ToolAction::WriteWorkspace {
                        tool_name: "apply_patch".to_string(),
                        paths: target_paths,
                    },
                )
                .await?;
                apply_patch_to_workspace(
                    &ctx.workspace_root,
                    &ctx.session_id,
                    &ctx.turn_id,
                    patch_text,
                    &ctx.path_locks,
                    ctx.snapshots.as_ref(),
                )
                .await
            }
            "file_glob" => {
                let pattern = req_str(args, "pattern")?;
                let max = args
                    .get("max_matches")
                    .and_then(Value::as_u64)
                    .unwrap_or(200) as usize;
                self.authorize(
                    ctx,
                    call_id,
                    ToolAction::ReadWorkspace {
                        tool_name: "file_glob".to_string(),
                        path: ctx.workspace_root.clone(),
                    },
                )
                .await?;
                file_glob(&ctx.workspace_root, pattern, max)
            }
            "file_grep" => {
                let pattern = req_str(args, "pattern")?;
                let subpath = args.get("path").and_then(Value::as_str).map(Path::new);
                let max = args
                    .get("max_matches")
                    .and_then(Value::as_u64)
                    .unwrap_or(200) as usize;
                self.authorize(
                    ctx,
                    call_id,
                    ToolAction::ReadWorkspace {
                        tool_name: "file_grep".to_string(),
                        path: subpath.map_or_else(|| ctx.workspace_root.clone(), Path::to_path_buf),
                    },
                )
                .await?;
                file_grep(&ctx.workspace_root, pattern, subpath, max)
            }
            "file_index" => {
                let path = req_str(args, "path")?;
                self.authorize(
                    ctx,
                    call_id,
                    ToolAction::ReadWorkspace {
                        tool_name: "file_index".to_string(),
                        path: PathBuf::from(path),
                    },
                )
                .await?;
                extract_file_outline(&ctx.workspace_root, Path::new(path))
            }
            "view_image" => {
                let path = req_str(args, "path")?;
                self.authorize(
                    ctx,
                    call_id,
                    ToolAction::ReadWorkspace {
                        tool_name: "view_image".to_string(),
                        path: PathBuf::from(path),
                    },
                )
                .await?;
                view_image(&ctx.workspace_root, Path::new(path))
            }
            "plan" => {
                let markdown = req_str(args, "markdown")?;
                self.authorize(ctx, call_id, ToolAction::UpdatePlan).await?;
                {
                    let mut p = ctx
                        .plan_markdown
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *p = markdown.to_string();
                }
                Ok(ToolOutput::Plan {
                    markdown: markdown.to_string(),
                })
            }
            "question" => {
                let qs: Vec<String> = args
                    .get("questions")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let answered = if let Some(cb) = &ctx.question_cb {
                    cb.ask_questions(qs).await
                } else {
                    qs.into_iter().map(|q| (q, String::new())).collect()
                };
                Ok(ToolOutput::Question {
                    questions: answered,
                })
            }
            "exec_command" => {
                let cmd = args
                    .get("cmd")
                    .or_else(|| args.get("command"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolsError::invalid("`exec_command` requires `cmd`"))?;
                let cwd = args
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(|s| ctx.workspace_root.join(s))
                    .unwrap_or_else(|| ctx.workspace_root.clone());
                self.authorize(
                    ctx,
                    call_id,
                    ToolAction::Shell {
                        command: cmd.to_string(),
                        cwd: cwd.clone(),
                    },
                )
                .await?;
                let yield_ms = args
                    .get("yield_time_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(1_500);
                let background = args
                    .get("background")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                ctx.pty_manager
                    .exec_command(
                        PtyCommandRequest {
                            conversation_id: ctx.session_id.clone(),
                            command: cmd.to_string(),
                            cwd,
                            yield_time_ms: yield_ms,
                            background,
                            rows: 24,
                            cols: 80,
                        },
                        &ctx.settings,
                        ctx.state_index.as_ref(),
                    )
                    .await
            }
            "write_stdin" => {
                let session_id = req_str(args, "session_id")?;
                let chars = args.get("chars").and_then(Value::as_str).unwrap_or("");
                let yield_ms = args
                    .get("yield_time_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(1_000);
                ctx.pty_manager
                    .write_stdin(session_id, chars, yield_ms, ctx.state_index.as_ref())
                    .await
            }
            "tool_output" => {
                let output_id = req_str(args, "output_id")?;
                let offset = args.get("offset_line").and_then(Value::as_u64).unwrap_or(1) as usize;
                let limit = args
                    .get("limit_lines")
                    .and_then(Value::as_u64)
                    .unwrap_or(100) as usize;
                let regex_pat = args.get("regex").and_then(Value::as_str);
                let Some(idx_arc) = &ctx.state_index else {
                    return Err(ToolsError::invalid(
                        "`tool_output` requires an active `StateIndex`",
                    ));
                };
                let page = idx_arc
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .query_retained_output(output_id, offset, limit, regex_pat)?;
                Ok(ToolOutput::Text {
                    text: format!(
                        "[output_id={} total_lines={} showing={}..{}]\n{}",
                        page.output_id,
                        page.total_lines,
                        page.start_line,
                        page.end_line,
                        page.lines.join("\n")
                    ),
                })
            }
            "webfetch" => {
                let url = req_str(args, "url")?;
                self.authorize(
                    ctx,
                    call_id,
                    ToolAction::WebFetch {
                        url: url.to_string(),
                    },
                )
                .await?;
                ctx.web_fetcher.fetch(url).await
            }
            "web_search" => {
                let query = req_str(args, "query")?;
                let max_results = args
                    .get("max_results")
                    .and_then(Value::as_u64)
                    .unwrap_or(8)
                    .clamp(1, 10) as u32;
                let searcher = WebSearcher::new(
                    ctx.web_fetcher.clone(),
                    ctx.settings.websearch_base_url.clone(),
                );
                // Authorize the search endpoint through the webfetch ladder class.
                self.authorize(
                    ctx,
                    call_id,
                    ToolAction::WebFetch {
                        url: ctx
                            .settings
                            .websearch_base_url
                            .clone()
                            .unwrap_or_else(|| DEFAULT_WEBSEARCH_BASE_URL.to_string()),
                    },
                )
                .await?;
                searcher.search(query, max_results).await
            }
            "lsp_diagnostics" => {
                let path = req_str(args, "path")?;
                let server_id = args
                    .get("server")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let file = ctx.workspace_root.join(path);
                self.authorize(
                    ctx,
                    call_id,
                    ToolAction::ReadWorkspace {
                        tool_name: "lsp_diagnostics".to_string(),
                        path: file.clone(),
                    },
                )
                .await?;
                let Some(manager) = &ctx.lsp_manager else {
                    return Err(ToolsError::invalid(
                        "`lsp_diagnostics` requires configured `[lsp.*]` servers",
                    ));
                };
                let diags = manager.diagnostics(&server_id, &file).await?;
                if diags.is_empty() {
                    Ok(ToolOutput::Text {
                        text: format!("No diagnostics for `{path}`."),
                    })
                } else {
                    let body = diags
                        .iter()
                        .map(|d| {
                            format!(
                                "{}:{}:{} [{}] {}",
                                path, d.line, d.column, d.severity, d.message
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    Ok(ToolOutput::Text { text: body })
                }
            }
            "memory_save" => {
                let content = req_str(args, "content")?;
                self.authorize(ctx, call_id, ToolAction::UpdateMemory)
                    .await?;
                let Some(store) = &ctx.memory_store else {
                    return Err(ToolsError::invalid(
                        "`memory_save` requires project memory (state dir unavailable)",
                    ));
                };
                store.append(content)?;
                Ok(ToolOutput::Text {
                    text: format!("saved to {}", store.path().display()),
                })
            }
            "code" => {
                let snippet = req_str(args, "code")?;
                let authorizer = SessionMcpAuthorizer::new(
                    Arc::clone(&ctx.permissions),
                    ctx.approval_cb.clone(),
                    ctx.state_index.clone(),
                    format!("{call_id}_mcp"),
                );
                ctx.code_sandbox
                    .execute(snippet, ctx.mcp_gateway.clone(), Arc::new(authorizer))
                    .await
            }
            "batch" => {
                let arr = args
                    .get("calls")
                    .and_then(Value::as_array)
                    .ok_or_else(|| ToolsError::invalid("`batch` requires `calls` array"))?;
                const BATCH_ALLOWED_TOOLS: [&str; 5] = [
                    "file_read",
                    "file_glob",
                    "file_grep",
                    "file_index",
                    "tool_output",
                ];
                let sub_ids: Vec<String> =
                    (0..arr.len()).map(|i| format!("{call_id}_b{i}")).collect();
                let mut futures = Vec::with_capacity(arr.len());
                for (i, item) in arr.iter().enumerate() {
                    let sub_tool = item
                        .get("tool")
                        .and_then(Value::as_str)
                        .ok_or_else(|| ToolsError::invalid("batch item missing `tool`"))?;
                    if !BATCH_ALLOWED_TOOLS.contains(&sub_tool) {
                        return Err(ToolsError::invalid(format!(
                            "`batch` only accepts read/search/index tools ({}); `{sub_tool}` must run standalone",
                            BATCH_ALLOWED_TOOLS.join(", ")
                        )));
                    }
                    let sub_args = item.get("arguments").unwrap_or(&Value::Null);
                    futures.push(self.execute(ctx, &sub_ids[i], sub_tool, sub_args));
                }
                let results = futures::future::try_join_all(futures).await?;
                let outputs: Vec<String> = results
                    .iter()
                    .zip(arr.iter())
                    .map(|(out, item)| {
                        let sub_tool = item
                            .get("tool")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        format!("=== {sub_tool} ===\n{}", out.to_model_text())
                    })
                    .collect();
                Ok(ToolOutput::Text {
                    text: outputs.join("\n\n"),
                })
            }
            other => Err(ToolsError::UnknownTool {
                name: other.to_string(),
            }),
        }
    }

    async fn authorize(
        &self,
        ctx: &ToolExecutionContext,
        call_id: &str,
        action: ToolAction,
    ) -> Result<(), ToolsError> {
        run_permission_ladder(
            &ctx.permissions,
            ctx.approval_cb.as_ref(),
            ctx.state_index.as_ref(),
            call_id,
            &action,
        )
        .await
    }
}

/// Shared permission evaluation ladder: explicit `allow`/`deny` rules first,
/// then interactive approval prompts (denying cleanly when no approval callback
/// is available). Loops so compound shell commands authorize each subcommand.
/// Also used by session-level flows that mutate the workspace outside the
/// native registry (e.g. `task` worktree merge-back).
pub async fn run_permission_ladder(
    permissions: &Arc<Mutex<PermissionEvaluator>>,
    approval_cb: Option<&Arc<dyn ApprovalCallback>>,
    state_index: Option<&Arc<Mutex<StateIndex>>>,
    call_id: &str,
    action: &ToolAction,
) -> Result<(), ToolsError> {
    loop {
        let decision = {
            let eval = permissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            eval.evaluate(call_id, action)
        };
        match decision {
            PermissionDecision::Allow => return Ok(()),
            PermissionDecision::Deny { reason } => {
                return Err(ToolsError::PermissionDenied { message: reason });
            }
            PermissionDecision::Prompt(prompt) => {
                let Some(cb) = approval_cb else {
                    return Err(ToolsError::PermissionDenied {
                        message: format!(
                            "approval required for `{}` ({})",
                            prompt.tool_name, prompt.summary
                        ),
                    });
                };
                let resolution = cb.request_approval(prompt.clone()).await;
                let is_once_approve = matches!(
                    &resolution,
                    ApprovalResolution::Approve {
                        lifetime: tekvra_protocol::GrantLifetime::Once,
                        ..
                    }
                );
                {
                    let mut eval = permissions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let idx_guard = state_index.map(|arc| {
                        arc.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                    });
                    eval.apply_resolution(&prompt, resolution, idx_guard.as_deref())?;
                }
                if is_once_approve {
                    return Ok(());
                }
            }
        }
    }
}

/// Permission-ladder authorizer for MCP calls issued from inside Monty `code`:
/// prompts interactively through [`ApprovalCallback`] in `Build` mode instead of
/// failing closed.
pub struct SessionMcpAuthorizer {
    permissions: Arc<Mutex<PermissionEvaluator>>,
    approval_cb: Option<Arc<dyn ApprovalCallback>>,
    state_index: Option<Arc<Mutex<StateIndex>>>,
    call_id: String,
}

impl SessionMcpAuthorizer {
    /// Creates an authorizer bound to the session permission evaluator.
    #[must_use]
    pub fn new(
        permissions: Arc<Mutex<PermissionEvaluator>>,
        approval_cb: Option<Arc<dyn ApprovalCallback>>,
        state_index: Option<Arc<Mutex<StateIndex>>>,
        call_id: String,
    ) -> Self {
        Self {
            permissions,
            approval_cb,
            state_index,
            call_id,
        }
    }
}

#[async_trait]
impl McpCallAuthorizer for SessionMcpAuthorizer {
    async fn authorize_mcp(&self, server: &str, tool: &str) -> Result<(), ToolsError> {
        run_permission_ladder(
            &self.permissions,
            self.approval_cb.as_ref(),
            self.state_index.as_ref(),
            &self.call_id,
            &ToolAction::McpCall {
                server: server.to_string(),
                tool: tool.to_string(),
            },
        )
        .await
    }
}

fn req_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, ToolsError> {
    args.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolsError::invalid(format!("missing required string argument `{field}`")))
}

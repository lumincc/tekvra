//! Live `stdio` child-process and Streamable HTTP (`application/json` + `text/event-stream`)
//! JSON-RPC 2.0 MCP client gateway (`ConfiguredMcpGateway`).
//!
//! Connects enabled `[mcp_servers.<name>]` entries from trusted `tekvra.toml` configuration and
//! refreshes cached tool catalogs on `notifications/tools/list_changed` while keeping top-level
//! LLM `ToolDefinition`s completely unchanged across turns.
//!
//! Transport invariants:
//! - **Stdio**: one dedicated reader task owns the child stdout and feeds a pending-call
//!   `PendingMap` (oneshot resolution) plus a server→client request channel — concurrent calls
//!   never serialize behind a session lock.
//! - **HTTP**: `text/event-stream` responses are drained incrementally through
//!   [`SseFrameParser`] with a header-phase timeout and a per-chunk idle timeout;
//!   `application/json` responses keep the buffered path.
//! - **Session affinity**: `Mcp-Session-Id` is captured from server responses, echoed on every
//!   subsequent request, and released via `DELETE` on drop (opt-out: `mcp_servers.<name>.session = false`).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use indexmap::IndexMap;
use serde_json::{Value, json};
use tekvra_config::{AgentSettings, McpServerConfig};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::sync::{Mutex, oneshot};
use tokio::time::Instant;

use crate::ToolsError;
use crate::pty::build_sanitized_shell_env;
use crate::web_and_code::{
    McpGateway, McpPromptArgument, McpPromptDescriptor, McpResourceDescriptor,
    McpResourceTemplateDescriptor, McpServerHooks, McpToolDescriptor,
};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on the HTTP header phase (request sent → response headers received).
const HTTP_HEADER_TIMEOUT: Duration = Duration::from_secs(60);
/// Per-chunk idle bound while draining an SSE body.
const HTTP_IDLE_CHUNK_TIMEOUT: Duration = Duration::from_secs(10);

/// Page cap for cursor-paginated catalog lists (`nextCursor`).
const MAX_CATALOG_PAGES: usize = 16;
/// Item cap per catalog list.
const MAX_CATALOG_ITEMS: usize = 1000;

/// Summary of startup MCP server discovery.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpStartupReport {
    /// Names of MCP servers that completed handshake and tool discovery.
    pub connected_servers: Vec<String>,
    /// Non-fatal connection warnings for servers that could not be reached.
    pub warnings: Vec<String>,
}

type PendingMap = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

/// Shared inbound-frame router for one transport: resolves pending calls, marks the catalog
/// dirty on `*/*_list_changed` notifications, forwards `notifications/progress` for in-flight
/// `tools/call`s, and fans out server→client requests.
struct InboundRouter {
    pending: PendingMap,
    server_requests_tx: mpsc::UnboundedSender<Value>,
    catalog_dirty: Arc<AtomicBool>,
    /// Progress token → tool name of each in-flight `tools/call`.
    tracked_calls: TrackedCalls,
    progress_tx: mpsc::UnboundedSender<ProgressUpdate>,
}

impl InboundRouter {
    fn route(&self, msg: &Value) {
        if is_catalog_changed_notification(msg) {
            self.catalog_dirty.store(true, Ordering::Release);
        }
        if is_progress_notification(msg) {
            // Progress is a notification: never answered, never treated as a server request.
            // Frames whose token is unknown (late, string, or not ours) are dropped.
            if let Some(update) = parse_progress_notification(msg)
                && let Some(tool) = self.tool_for_token(update.token)
            {
                let _ = self.progress_tx.send(ProgressUpdate {
                    tool,
                    progress: update.progress,
                    total: update.total,
                    message: update.message,
                });
            }
            return;
        }
        let has_method = msg.get("method").is_some();
        let is_response = msg.get("result").is_some() || msg.get("error").is_some();
        if has_method && !is_response {
            // Server→client request (sampling / elicitation / ping): fan out to the handler
            // task regardless of id type (string ids are legal).
            let _ = self.server_requests_tx.send(msg.clone());
        } else if is_response && !has_method {
            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                self.resolve(id, msg);
            } else {
                eprintln!(
                    "tekvra-mcp: dropping unmatched non-numeric JSON-RPC response id={}",
                    msg.get("id").map(|v| v.to_string()).unwrap_or_default()
                );
            }
        }
    }

    /// Resolves `id` with `msg` if a caller is parked on it; `false` for late/unknown ids.
    fn resolve(&self, id: u64, msg: &Value) -> bool {
        let tx = {
            let mut guard = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.remove(&id)
        };
        match tx {
            Some(tx) => {
                let _ = tx.send(msg.clone());
                true
            }
            None => {
                eprintln!("tekvra-mcp: dropping unmatched JSON-RPC response id={id}");
                false
            }
        }
    }

    /// Fails every parked call (reader EOF / transport teardown) so callers never hang.
    fn fail_all_pending(&self, reason: &str) {
        let mut guard = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, tx) in guard.drain() {
            let _ = tx.send(json!({
                "error": { "code": -32000_i64, "message": reason }
            }));
        }
        drop(guard);
        self.tracked_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Registers `tool` as the in-flight call identified by progress token `token`; the
    /// returned guard forgets the token when dropped, on every exit path of the call.
    fn track_call(self: &Arc<Self>, token: u64, tool: &str) -> TrackedCall {
        self.tracked_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(token, tool.to_string());
        TrackedCall {
            router: Arc::clone(self),
            token,
        }
    }

    fn forget_call(&self, token: u64) {
        self.tracked_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&token);
    }

    fn tool_for_token(&self, token: u64) -> Option<String> {
        self.tracked_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&token)
            .cloned()
    }
}

/// Progress token → tool name of each in-flight `tools/call`. Tokens are JSON-RPC ids skipped
/// when the call's own id was allocated, so they never collide with a response id.
type TrackedCalls = Arc<std::sync::Mutex<HashMap<u64, String>>>;

/// RAII registration of one in-flight `tools/call` progress token (see `InboundRouter::track_call`).
struct TrackedCall {
    router: Arc<InboundRouter>,
    token: u64,
}

impl Drop for TrackedCall {
    fn drop(&mut self) {
        self.router.forget_call(self.token);
    }
}

/// One progress update, already correlated to an in-flight call's tool name.
struct ProgressUpdate {
    tool: String,
    progress: f64,
    total: Option<f64>,
    message: Option<String>,
}

/// One `notifications/progress` frame as sent by the server.
struct ParsedProgress {
    token: u64,
    progress: f64,
    total: Option<f64>,
    message: Option<String>,
}

/// Minimum interval between two hook deliveries for the same in-flight call.
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(100);

fn is_progress_notification(msg: &Value) -> bool {
    matches!(
        msg.get("method").and_then(Value::as_str),
        Some("notifications/progress" | "progress")
    )
}

/// Parses a `notifications/progress` frame; `None` when the token is not a number we issued or
/// `progress` is missing/not a number.
fn parse_progress_notification(msg: &Value) -> Option<ParsedProgress> {
    let params = msg.get("params")?;
    Some(ParsedProgress {
        token: params.get("progressToken").and_then(Value::as_u64)?,
        progress: params.get("progress").and_then(Value::as_f64)?,
        total: params.get("total").and_then(Value::as_f64),
        message: params
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Per-transport forwarder for one server: coalesces progress updates to at most one hook
/// delivery per call per [`PROGRESS_MIN_INTERVAL`] while always delivering the latest value.
async fn forward_progress(
    mut rx: mpsc::UnboundedReceiver<ProgressUpdate>,
    server_name: String,
    hooks: SharedMcpHooks,
) {
    let mut held: HashMap<String, ProgressUpdate> = HashMap::new();
    let mut last_sent: HashMap<String, Instant> = HashMap::new();
    let mut flush_at: Option<Instant> = None;
    loop {
        tokio::select! {
            update = rx.recv() => {
                let Some(update) = update else { break };
                let now = Instant::now();
                if due(now, last_sent.get(&update.tool)) {
                    deliver_progress(&server_name, &hooks, &update);
                    last_sent.insert(update.tool.clone(), now);
                } else {
                    held.insert(update.tool.clone(), update);
                    flush_at = next_flush(&held, &last_sent);
                }
            }
            _ = tokio::time::sleep_until(flush_at.unwrap_or_else(Instant::now)), if flush_at.is_some() => {
                let now = Instant::now();
                let ready: Vec<String> = held
                    .keys()
                    .filter(|tool| due(now, last_sent.get(*tool)))
                    .cloned()
                    .collect();
                for tool in ready {
                    if let Some(update) = held.remove(&tool) {
                        deliver_progress(&server_name, &hooks, &update);
                        last_sent.insert(tool, now);
                    }
                }
                flush_at = next_flush(&held, &last_sent);
            }
        }
    }
}

/// Whether a call last delivered at `last` (if ever) may be delivered again at `now`.
fn due(now: Instant, last: Option<&Instant>) -> bool {
    last.is_none_or(|last| now.duration_since(*last) >= PROGRESS_MIN_INTERVAL)
}

/// Earliest instant at which some held update becomes deliverable.
fn next_flush(
    held: &HashMap<String, ProgressUpdate>,
    last_sent: &HashMap<String, Instant>,
) -> Option<Instant> {
    held.keys()
        .filter_map(|tool| last_sent.get(tool))
        .map(|last| *last + PROGRESS_MIN_INTERVAL)
        .min()
}

fn deliver_progress(server_name: &str, hooks: &SharedMcpHooks, update: &ProgressUpdate) {
    let hooks = snapshot_hooks(hooks);
    if let Some(hook) = hooks.as_ref().and_then(|hooks| hooks.progress.as_deref()) {
        hook.on_progress(
            server_name,
            &update.tool,
            update.progress,
            update.total,
            update.message.as_deref(),
        );
    }
}

struct StdioSession {
    _child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    _reader: tokio::task::JoinHandle<()>,
}

/// Streamable-HTTP session-affinity state: captured `Mcp-Session-Id`, echoed per request,
/// released with a best-effort `DELETE` on drop.
struct HttpSessionState {
    enabled: bool,
    id: std::sync::Mutex<Option<String>>,
    url: String,
    client: reqwest::Client,
}

impl HttpSessionState {
    fn current(&self) -> Option<String> {
        self.id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn capture(&self, value: Option<&str>) {
        if !self.enabled {
            return;
        }
        let Some(sid) = value.filter(|s| !s.trim().is_empty()) else {
            return;
        };
        let mut guard = self
            .id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_none() {
            *guard = Some(sid.to_string());
        }
    }
}

impl Drop for HttpSessionState {
    fn drop(&mut self) {
        let Some(sid) = self
            .id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        // `Drop` is sync: spawn the best-effort DELETE when a runtime is available.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let client = self.client.clone();
        let url = self.url.clone();
        handle.spawn(async move {
            let _ = client
                .delete(&url)
                .header("Mcp-Session-Id", sid)
                .send()
                .await;
        });
    }
}

enum McpClientTransport {
    Stdio {
        session: Box<StdioSession>,
        router: Arc<InboundRouter>,
        catalog_dirty: Arc<AtomicBool>,
        /// Catalog filled at handshake and replaced on refresh.
        catalog: Arc<std::sync::Mutex<ServerCatalog>>,
        next_id: AtomicU64,
        call_timeout: Duration,
    },
    Http {
        client: reqwest::Client,
        url: String,
        bearer_token: Option<String>,
        router: Arc<InboundRouter>,
        catalog_dirty: Arc<AtomicBool>,
        catalog: Arc<std::sync::Mutex<ServerCatalog>>,
        next_id: AtomicU64,
        call_timeout: Duration,
        /// Per-server overrides of [`HTTP_HEADER_TIMEOUT`].
        header_timeout: Duration,
        /// Per-server overrides of [`HTTP_IDLE_CHUNK_TIMEOUT`].
        idle_timeout: Duration,
        session: Arc<HttpSessionState>,
    },
}

struct StdioConnectParams<'a> {
    server_name: &'a str,
    executable: &'a str,
    args: &'a [String],
    extra_env: &'a BTreeMap<String, String>,
    cwd: &'a Path,
    agent_settings: &'a AgentSettings,
    handshake_timeout: Duration,
    call_timeout: Duration,
    sampling_allowed: bool,
    hooks: SharedMcpHooks,
}

/// Builds the JSON-RPC response for a server→client request, consulting attached hooks.
async fn handle_server_request(
    hooks: Option<Arc<McpServerHooks>>,
    sampling_allowed: bool,
    server_name: &str,
    req: &Value,
) -> Value {
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let rpc_result = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let rpc_error = |code: i64, message: String| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    match method.as_str() {
        "sampling/createMessage" => {
            let Some(hook) = hooks.as_ref().and_then(|h| h.sampling.clone()) else {
                return rpc_error(
                    -32603,
                    "MCP `sampling/createMessage` is not enabled on this client".to_string(),
                );
            };
            if !sampling_allowed {
                return rpc_error(
                    -32603,
                    "MCP server sampling is not enabled for this server (requires \
                     `[experimental] mcp_sampling = true` and per-server `sampling = true`)"
                        .to_string(),
                );
            }
            match hook.sample(server_name, &id, &params).await {
                Ok(result) => rpc_result(result),
                Err(message) => rpc_error(-32603, message),
            }
        }
        "elicitation/create" => {
            let Some(hook) = hooks.as_ref().and_then(|h| h.elicitation.clone()) else {
                // No question host attached (headless): a plain, explicit decline.
                return rpc_result(json!({ "action": "decline" }));
            };
            match hook.elicit(server_name, &params).await {
                Ok(result) => rpc_result(result),
                Err(_) => rpc_result(json!({ "action": "decline" })),
            }
        }
        other => rpc_error(
            -32601,
            format!("method `{other}` is not supported by this client"),
        ),
    }
}

/// Spawns the stdio server-request handler task.
fn spawn_stdio_server_request_handler(
    stdin: Arc<Mutex<ChildStdin>>,
    mut rx: mpsc::UnboundedReceiver<Value>,
    server_name: String,
    sampling_allowed: bool,
    hooks: SharedMcpHooks,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let hooks_now = hooks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let response =
                handle_server_request(hooks_now, sampling_allowed, &server_name, &req).await;
            if write_stdio_frame(&stdin, &response, &server_name, "server-request")
                .await
                .is_err()
            {
                break;
            }
        }
    })
}

/// Shared, late-bound server→client hooks (set by the host session after connect).
type SharedMcpHooks = Arc<std::sync::Mutex<Option<Arc<McpServerHooks>>>>;

/// Reads the current hooks snapshot.
fn snapshot_hooks(hooks: &SharedMcpHooks) -> Option<Arc<McpServerHooks>> {
    hooks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

async fn write_stdio_frame(
    stdin: &Arc<Mutex<ChildStdin>>,
    payload: &Value,
    server_name: &str,
    method: &str,
) -> Result<(), ToolsError> {
    let mut raw = serde_json::to_vec(payload).map_err(|e| ToolsError::CodeSandbox {
        message: format!("failed encoding MCP request `{method}`: {e}"),
    })?;
    raw.push(b'\n');
    let mut guard = stdin.lock().await;
    guard
        .write_all(&raw)
        .await
        .map_err(|e| ToolsError::CodeSandbox {
            message: format!("MCP `{server_name}` stdin write failed on `{method}`: {e}"),
        })?;
    guard.flush().await.map_err(|e| ToolsError::CodeSandbox {
        message: format!("MCP `{server_name}` stdin flush failed on `{method}`: {e}"),
    })?;
    Ok(())
}

impl McpClientTransport {
    const fn call_timeout(&self) -> Duration {
        match self {
            Self::Stdio { call_timeout, .. } | Self::Http { call_timeout, .. } => *call_timeout,
        }
    }

    /// Streaming budget for HTTP servers: the per-server `http_header_timeout_sec` /
    /// `http_idle_timeout_sec` overrides when set, else the built-in defaults.
    const fn http_stream_timeouts(&self) -> (Duration, Duration) {
        match self {
            Self::Stdio { .. } => (HTTP_HEADER_TIMEOUT, HTTP_IDLE_CHUNK_TIMEOUT),
            Self::Http {
                header_timeout,
                idle_timeout,
                ..
            } => (*header_timeout, *idle_timeout),
        }
    }

    fn take_catalog_dirty(&self) -> bool {
        match self {
            Self::Stdio { catalog_dirty, .. } | Self::Http { catalog_dirty, .. } => {
                catalog_dirty.swap(false, Ordering::AcqRel)
            }
        }
    }

    fn router(&self) -> &Arc<InboundRouter> {
        match self {
            Self::Stdio { router, .. } | Self::Http { router, .. } => router,
        }
    }

    /// Allocates a fresh progress token (a skipped JSON-RPC id, never reused for a response).
    fn allocate_progress_token(&self) -> u64 {
        match self {
            Self::Stdio { next_id, .. } | Self::Http { next_id, .. } => {
                next_id.fetch_add(1, Ordering::Relaxed)
            }
        }
    }

    fn capabilities(&self) -> ServerCapabilities {
        self.catalog()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .capabilities
    }

    /// Clones one list out of the shared catalog.
    fn catalog_list<T: Clone>(&self, pick: impl FnOnce(&ServerCatalog) -> &[T]) -> Vec<T> {
        let guard = self
            .catalog()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pick(&guard).to_vec()
    }

    fn catalog(&self) -> &Arc<std::sync::Mutex<ServerCatalog>> {
        match self {
            Self::Stdio { catalog, .. } | Self::Http { catalog, .. } => catalog,
        }
    }

    async fn connect_stdio(params: StdioConnectParams<'_>) -> Result<Self, ToolsError> {
        let StdioConnectParams {
            server_name,
            executable,
            args,
            extra_env,
            cwd,
            agent_settings,
            handshake_timeout,
            call_timeout,
            sampling_allowed,
            hooks,
        } = params;

        let mut base_env = build_sanitized_shell_env(agent_settings);
        for (k, v) in extra_env {
            base_env.insert(k.clone(), v.clone());
        }

        let mut cmd = Command::new(executable);
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(&base_env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| ToolsError::CodeSandbox {
            message: format!(
                "failed spawning stdio MCP server `{server_name}` (`{executable}`): {e}"
            ),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| ToolsError::CodeSandbox {
            message: format!("stdio MCP server `{server_name}` missing stdin pipe"),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| ToolsError::CodeSandbox {
            message: format!("stdio MCP server `{server_name}` missing stdout pipe"),
        })?;

        let stdin = Arc::new(Mutex::new(stdin));
        let (server_requests_tx, server_requests_rx) = mpsc::unbounded_channel();
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        let catalog_dirty = Arc::new(AtomicBool::new(false));
        let router = Arc::new(InboundRouter {
            pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
            server_requests_tx,
            catalog_dirty: Arc::clone(&catalog_dirty),
            tracked_calls: Arc::new(std::sync::Mutex::new(HashMap::new())),
            progress_tx,
        });
        tokio::spawn(forward_progress(
            progress_rx,
            server_name.to_string(),
            Arc::clone(&hooks),
        ));

        // Dedicated reader task: owns stdout, routes every frame, fails parked calls on EOF.
        let reader_router = Arc::clone(&router);
        let reader_name = server_name.to_string();
        let reader = tokio::spawn(async move {
            let mut stdout = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match stdout.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
                    continue;
                };
                let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                    reader_router.route(&msg);
                    continue;
                };
                let has_method = msg.get("method").is_some();
                let is_response = msg.get("result").is_some() || msg.get("error").is_some();
                if has_method && !is_response {
                    reader_router.route(&msg);
                } else if is_response {
                    reader_router.resolve(id, &msg);
                }
            }
            reader_router.fail_all_pending(&format!(
                "MCP server `{reader_name}` closed its stdout stream"
            ));
        });
        spawn_stdio_server_request_handler(
            Arc::clone(&stdin),
            server_requests_rx,
            server_name.to_string(),
            sampling_allowed,
            hooks,
        );

        let transport = Self::Stdio {
            session: Box::new(StdioSession {
                _child: child,
                stdin,
                _reader: reader,
            }),
            router,
            catalog: Arc::new(std::sync::Mutex::new(ServerCatalog::default())),
            catalog_dirty,
            next_id: AtomicU64::new(1),
            call_timeout,
        };

        tokio::time::timeout(handshake_timeout, transport.handshake_and_list(server_name))
            .await
            .map_err(|_| ToolsError::CodeSandbox {
                message: format!("stdio MCP server `{server_name}` timed out during handshake"),
            })??;

        Ok(transport)
    }

    #[allow(clippy::too_many_arguments)] // one-shot internal connector; each param is distinct state
    async fn connect_http(
        server_name: &str,
        raw_url: &str,
        bearer_token: Option<String>,
        session_enabled: bool,
        sampling_allowed: bool,
        hooks: SharedMcpHooks,
        handshake_timeout: Duration,
        call_timeout: Duration,
        header_timeout: Duration,
        idle_timeout: Duration,
    ) -> Result<Self, ToolsError> {
        let parsed = url::Url::parse(raw_url).map_err(|e| ToolsError::CodeSandbox {
            message: format!("invalid HTTP MCP URL `{raw_url}` for `{server_name}`: {e}"),
        })?;
        let is_loopback = parsed
            .host_str()
            .is_some_and(|h| matches!(h, "127.0.0.1" | "localhost" | "[::1]" | "::1"));

        let mut builder = reqwest::Client::builder().timeout(call_timeout);
        if is_loopback {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(|e| ToolsError::CodeSandbox {
            message: format!("failed building HTTP MCP client for `{server_name}`: {e}"),
        })?;

        let (server_requests_tx, mut server_requests_rx) = mpsc::unbounded_channel();
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        let catalog_dirty = Arc::new(AtomicBool::new(false));
        let router = Arc::new(InboundRouter {
            pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
            server_requests_tx,
            catalog_dirty: Arc::clone(&catalog_dirty),
            tracked_calls: Arc::new(std::sync::Mutex::new(HashMap::new())),
            progress_tx,
        });
        tokio::spawn(forward_progress(
            progress_rx,
            server_name.to_string(),
            Arc::clone(&hooks),
        ));

        // HTTP server→client requests are answered over POST to the endpoint.
        let handler_client = client.clone();
        let handler_url = raw_url.to_string();
        let handler_session = Arc::new(HttpSessionState {
            enabled: session_enabled,
            id: std::sync::Mutex::new(None),
            url: raw_url.to_string(),
            client: client.clone(),
        });
        let transport_session = Arc::clone(&handler_session);
        let handler_bearer = bearer_token.clone();
        let handler_name = server_name.to_string();
        let handler_hooks = Arc::clone(&hooks);
        tokio::spawn(async move {
            while let Some(req) = server_requests_rx.recv().await {
                let response = handle_server_request(
                    snapshot_hooks(&handler_hooks),
                    sampling_allowed,
                    &handler_name,
                    &req,
                )
                .await;
                let mut post = handler_client
                    .post(&handler_url)
                    .header(
                        reqwest::header::ACCEPT,
                        "application/json, text/event-stream",
                    )
                    .json(&response);
                if let Some(sid) = handler_session.current() {
                    post = post.header("Mcp-Session-Id", sid);
                }
                if let Some(tok) = handler_bearer.as_deref() {
                    post = post.bearer_auth(tok);
                }
                let _ = post.send().await;
            }
        });

        let transport = Self::Http {
            client,
            url: raw_url.to_string(),
            bearer_token,
            router,
            catalog: Arc::new(std::sync::Mutex::new(ServerCatalog::default())),
            catalog_dirty,
            next_id: AtomicU64::new(1),
            call_timeout,
            header_timeout,
            idle_timeout,
            session: transport_session,
        };

        tokio::time::timeout(handshake_timeout, transport.handshake_and_list(server_name))
            .await
            .map_err(|_| ToolsError::CodeSandbox {
                message: format!("HTTP MCP server `{server_name}` timed out during handshake"),
            })??;

        Ok(transport)
    }

    async fn handshake_and_list(&self, server_name: &str) -> Result<(), ToolsError> {
        let init_res = self
            .request(
                server_name,
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "tekvra",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            )
            .await?;
        let capabilities = ServerCapabilities::from_initialize(&init_res);

        self.notify("notifications/initialized", json!({})).await?;
        self.fill_catalog(server_name, capabilities).await
    }

    /// Re-queries every catalog the server advertised at handshake and replaces the shared one.
    async fn refresh_catalog(&self, server_name: &str) -> Result<(), ToolsError> {
        let capabilities = self.capabilities();
        self.fill_catalog(server_name, capabilities).await
    }

    /// Fetches `tools/list` (fatal only when the server advertises `tools`) plus the
    /// resource/prompt catalogs the server advertises (list failures only warn), then swaps
    /// the shared catalog in one write.
    async fn fill_catalog(
        &self,
        server_name: &str,
        capabilities: ServerCapabilities,
    ) -> Result<(), ToolsError> {
        let mut catalog = ServerCatalog {
            capabilities,
            tools: if capabilities.tools {
                self.list_tools_rpc(server_name).await?
            } else {
                // A server that advertises no tools may reject `tools/list`; that is not fatal.
                self.list_tools_rpc(server_name).await.unwrap_or_default()
            },
            ..ServerCatalog::default()
        };
        if capabilities.resources {
            match self
                .list_pages(server_name, "resources/list", "resources")
                .await
            {
                Ok(items) => catalog.resources = parse_resource_descriptors(server_name, &items),
                Err(err) => eprintln!("tekvra-mcp: `{server_name}` resources/list failed: {err}"),
            }
            match self
                .list_pages(server_name, "resources/templates/list", "resourceTemplates")
                .await
            {
                Ok(items) => {
                    catalog.resource_templates =
                        parse_resource_template_descriptors(server_name, &items);
                }
                Err(err) => {
                    eprintln!("tekvra-mcp: `{server_name}` resources/templates/list failed: {err}");
                }
            }
        }
        if capabilities.prompts {
            match self
                .list_pages(server_name, "prompts/list", "prompts")
                .await
            {
                Ok(items) => catalog.prompts = parse_prompt_descriptors(server_name, &items),
                Err(err) => eprintln!("tekvra-mcp: `{server_name}` prompts/list failed: {err}"),
            }
        }
        *self
            .catalog()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = catalog;
        Ok(())
    }

    async fn list_tools_rpc(
        &self,
        server_name: &str,
    ) -> Result<Vec<McpToolDescriptor>, ToolsError> {
        let list_res = self.request(server_name, "tools/list", json!({})).await?;
        Ok(parse_tool_descriptors(server_name, &list_res))
    }

    /// Collects every page of a list method (`nextCursor` cursor pagination), bounded by
    /// [`MAX_CATALOG_PAGES`] pages and [`MAX_CATALOG_ITEMS`] items.
    async fn list_pages(
        &self,
        server_name: &str,
        method: &str,
        key: &str,
    ) -> Result<Vec<Value>, ToolsError> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_CATALOG_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let page = self.request(server_name, method, params).await?;
            if let Some(page_items) = page.get(key).and_then(Value::as_array) {
                items.extend(page_items.iter().cloned());
            }
            if items.len() >= MAX_CATALOG_ITEMS {
                items.truncate(MAX_CATALOG_ITEMS);
                break;
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(items)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), ToolsError> {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        match self {
            Self::Stdio { session, .. } => {
                write_stdio_frame(&session.stdin, &payload, "stdio server", method).await
            }
            Self::Http {
                client,
                url,
                bearer_token,
                session: session_state,
                ..
            } => {
                // Best-effort notification over HTTP (servers may return 200, 202, or 204).
                let mut req = client
                    .post(url)
                    .header(
                        reqwest::header::ACCEPT,
                        "application/json, text/event-stream",
                    )
                    .json(&payload);
                if let Some(sid) = session_state.current() {
                    req = req.header("Mcp-Session-Id", sid);
                }
                if let Some(tok) = bearer_token.as_deref() {
                    req = req.bearer_auth(tok);
                }
                let _ = req.send().await;
                Ok(())
            }
        }
    }

    async fn request(
        &self,
        server_name: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, ToolsError> {
        match self {
            Self::Stdio {
                session,
                router,
                next_id,
                call_timeout,
                ..
            } => {
                let req_id = next_id.fetch_add(1, Ordering::Relaxed);
                let payload = json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "method": method,
                    "params": params,
                });
                // Park the oneshot BEFORE writing: the reader may resolve within microseconds.
                let (tx, rx) = oneshot::channel();
                {
                    let mut guard = router
                        .pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.insert(req_id, tx);
                }
                if let Err(write_err) =
                    write_stdio_frame(&session.stdin, &payload, server_name, method).await
                {
                    let mut guard = router
                        .pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.remove(&req_id);
                    return Err(write_err);
                }

                let msg = match tokio::time::timeout(*call_timeout, rx).await {
                    Ok(Ok(msg)) => msg,
                    Ok(Err(_recv)) => {
                        // Reader task failed the pending map (server died).
                        return Err(ToolsError::CodeSandbox {
                            message: format!(
                                "MCP `{server_name}` closed stdout while awaiting `{method}`"
                            ),
                        });
                    }
                    Err(_elapsed) => {
                        let mut guard = router
                            .pending
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        guard.remove(&req_id);
                        return Err(ToolsError::CodeSandbox {
                            message: format!(
                                "MCP `{server_name}` timed out awaiting `{method}` response"
                            ),
                        });
                    }
                };
                extract_rpc_result(server_name, method, msg)
            }
            Self::Http {
                client,
                url,
                bearer_token,
                router,
                next_id,
                call_timeout,
                session: session_state,
                ..
            } => {
                let req_id = next_id.fetch_add(1, Ordering::Relaxed);
                let payload = json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "method": method,
                    "params": params,
                });
                let mut req = client
                    .post(url)
                    .header(
                        reqwest::header::ACCEPT,
                        "application/json, text/event-stream",
                    )
                    .json(&payload);
                if let Some(sid) = session_state.current() {
                    req = req.header("Mcp-Session-Id", sid);
                }
                if let Some(tok) = bearer_token.as_deref() {
                    req = req.bearer_auth(tok);
                }

                // Header phase: bounded independently of body streaming.
                let header_deadline = self.http_stream_timeouts().0.min(*call_timeout);
                let resp = tokio::time::timeout(header_deadline, req.send())
                    .await
                    .map_err(|_| ToolsError::CodeSandbox {
                        message: format!(
                            "HTTP MCP `{server_name}` `{method}` header phase timed out"
                        ),
                    })?
                    .map_err(|e| ToolsError::CodeSandbox {
                        message: format!("HTTP MCP `{server_name}` request `{method}` failed: {e}"),
                    })?;

                session_state.capture(
                    resp.headers()
                        .get("mcp-session-id")
                        .and_then(|v| v.to_str().ok()),
                );

                if !resp.status().is_success() {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    return Err(ToolsError::CodeSandbox {
                        message: format!(
                            "HTTP MCP `{server_name}` `{method}` returned HTTP {status}: {body}"
                        ),
                    });
                }
                let is_sse = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|ct| ct.to_ascii_lowercase().contains("text/event-stream"));

                if is_sse {
                    self.drain_sse_response(resp, server_name, method, req_id, router)
                        .await
                } else {
                    let msg: Value = resp.json().await.map_err(|e| ToolsError::CodeSandbox {
                        message: format!(
                            "HTTP MCP `{server_name}` `{method}` returned invalid JSON: {e}"
                        ),
                    })?;
                    extract_rpc_result(server_name, method, msg)
                }
            }
        }
    }

    /// Drains an SSE body incrementally, routing inter-leaved notifications and server→client
    /// requests while resolving the frame matching `req_id`.
    async fn drain_sse_response(
        &self,
        resp: reqwest::Response,
        server_name: &str,
        method: &str,
        req_id: u64,
        router: &InboundRouter,
    ) -> Result<Value, ToolsError> {
        let idle_timeout = self.http_stream_timeouts().1;
        let mut parser = SseFrameParser::default();
        let mut stream = resp.bytes_stream();
        let mut matched: Option<Value> = None;

        loop {
            let received = tokio::time::timeout(idle_timeout, stream.next()).await;
            let mut events: Vec<(Option<String>, String)> = Vec::new();
            let mut stream_ended = false;
            let chunk = match received {
                Err(_idle) => {
                    return Err(ToolsError::CodeSandbox {
                        message: format!(
                            "HTTP MCP `{server_name}` `{method}` SSE stream idle timeout"
                        ),
                    });
                }
                Ok(None) => {
                    parser.finish(&mut events);
                    stream_ended = true;
                    None
                }
                Ok(Some(result)) => Some(result.map_err(|e| ToolsError::CodeSandbox {
                    message: format!(
                        "HTTP MCP `{server_name}` `{method}` SSE stream read failed: {e}"
                    ),
                })?),
            };
            if let Some(bytes) = chunk {
                parser.feed(&bytes, &mut events);
            }

            for (_event, data) in events {
                let trimmed = data.trim();
                if trimmed.is_empty() || trimmed == "[DONE]" {
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
                    return Err(ToolsError::CodeSandbox {
                        message: format!(
                            "HTTP MCP `{server_name}` `{method}` emitted invalid SSE JSON: {trimmed}"
                        ),
                    });
                };
                let is_response_frame = msg.get("method").is_none()
                    && (msg.get("result").is_some() || msg.get("error").is_some())
                    && msg.get("id").and_then(Value::as_u64) == Some(req_id);
                if is_response_frame {
                    matched = Some(msg);
                    break;
                }
                router.route(&msg);
            }
            if matched.is_some() {
                break;
            }
            if stream_ended {
                break;
            }
        }

        let msg = matched.ok_or_else(|| ToolsError::CodeSandbox {
            message: format!(
                "HTTP MCP `{server_name}` `{method}` SSE stream ended without matching JSON-RPC response"
            ),
        })?;
        extract_rpc_result(server_name, method, msg)
    }
}

/// Incremental, CRLF-tolerant `text/event-stream` parser: `feed` accepts arbitrary byte chunks
/// and emits complete `(event, data)` frames on blank-line boundaries.
#[derive(Default)]
struct SseFrameParser {
    buf: Vec<u8>,
    event: Option<String>,
    data_lines: Vec<String>,
}

impl SseFrameParser {
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<(Option<String>, String)>) {
        self.buf.extend_from_slice(chunk);
        while let Some(idx) = self.buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.buf.drain(..=idx).collect();
            let mut line =
                String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]).into_owned();
            if line.ends_with('\r') {
                line.pop();
            }
            self.handle_line(&line, out);
        }
    }

    fn finish(&mut self, out: &mut Vec<(Option<String>, String)>) {
        if !self.buf.is_empty() {
            let rest = String::from_utf8_lossy(&self.buf).into_owned();
            self.buf.clear();
            self.handle_line(&rest, out);
        }
        self.flush_event(out);
    }

    fn handle_line(&mut self, line: &str, out: &mut Vec<(Option<String>, String)>) {
        if line.is_empty() {
            self.flush_event(out);
            return;
        }
        if line.starts_with(':') {
            return; // comment / heartbeat
        }
        if let Some(rest) = line.strip_prefix("event:") {
            self.event = Some(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            self.data_lines
                .push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
        // `id:`/`retry:` and unknown fields are ignored (client never reconnects by id).
    }

    fn flush_event(&mut self, out: &mut Vec<(Option<String>, String)>) {
        if self.data_lines.is_empty() {
            self.event = None;
            return;
        }
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        out.push((self.event.take(), data));
    }
}

/// `notifications/*/list_changed` from a server: its catalog must be re-queried before next use.
fn is_catalog_changed_notification(msg: &Value) -> bool {
    matches!(
        msg.get("method").and_then(Value::as_str),
        Some(
            "notifications/tools/list_changed"
                | "tools/list_changed"
                | "notifications/resources/list_changed"
                | "resources/list_changed"
                | "notifications/resources/templates/list_changed"
                | "resources/templates/list_changed"
                | "notifications/prompts/list_changed"
                | "prompts/list_changed"
        )
    )
}

fn extract_rpc_result(
    server_name: &str,
    method: &str,
    mut msg: Value,
) -> Result<Value, ToolsError> {
    if let Some(err_obj) = msg.get("error")
        && !err_obj.is_null()
    {
        let err_msg = err_obj
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown JSON-RPC error");
        return Err(ToolsError::CodeSandbox {
            message: format!("MCP `{server_name}` `{method}` error: {err_msg}"),
        });
    }
    Ok(msg.get_mut("result").map_or(Value::Null, Value::take))
}

fn parse_tool_descriptors(server_name: &str, result: &Value) -> Vec<McpToolDescriptor> {
    let Some(items) = result.get("tools").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        let description = item
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let input_schema = item
            .get("inputSchema")
            .or_else(|| item.get("input_schema"))
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object" }));
        out.push(McpToolDescriptor {
            server: server_name.to_string(),
            tool: name.to_string(),
            description,
            input_schema,
        });
    }
    out
}

/// Catalogs a server advertised, filled at handshake and replaced on refresh.
#[derive(Debug, Default, Clone)]
struct ServerCatalog {
    capabilities: ServerCapabilities,
    tools: Vec<McpToolDescriptor>,
    resources: Vec<McpResourceDescriptor>,
    resource_templates: Vec<McpResourceTemplateDescriptor>,
    prompts: Vec<McpPromptDescriptor>,
}

/// Capabilities a server announced in its `initialize` result.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ServerCapabilities {
    tools: bool,
    resources: bool,
    prompts: bool,
}

impl ServerCapabilities {
    fn from_initialize(init_result: &Value) -> Self {
        let has = |key: &str| {
            init_result
                .get("capabilities")
                .and_then(|caps| caps.get(key))
                .is_some_and(|cap| !cap.is_null() && cap.as_bool() != Some(false))
        };
        Self {
            tools: has("tools"),
            resources: has("resources"),
            prompts: has("prompts"),
        }
    }

    /// Whether the server advertised the capability `operation` needs.
    fn supports(self, operation: &str) -> bool {
        match operation {
            "tools/call" => true,
            "resources/read" => self.resources,
            "prompts/get" => self.prompts,
            _ => false,
        }
    }
}

fn capability_name(operation: &str) -> &'static str {
    match operation {
        "resources/read" => "resources",
        "prompts/get" => "prompts",
        _ => "tools",
    }
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn parse_resource_descriptors(server_name: &str, items: &[Value]) -> Vec<McpResourceDescriptor> {
    let mut out: Vec<_> = items
        .iter()
        .filter_map(|item| {
            let uri = item.get("uri").and_then(Value::as_str)?;
            Some(McpResourceDescriptor {
                server: server_name.to_string(),
                uri: uri.to_string(),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(uri)
                    .to_string(),
                description: optional_string(item, "description"),
                mime_type: optional_string(item, "mimeType"),
            })
        })
        .collect();
    out.sort_by(|a, b| a.uri.cmp(&b.uri));
    out.dedup_by(|a, b| a.uri == b.uri);
    out
}

fn parse_resource_template_descriptors(
    server_name: &str,
    items: &[Value],
) -> Vec<McpResourceTemplateDescriptor> {
    let mut out: Vec<_> = items
        .iter()
        .filter_map(|item| {
            let uri_template = item.get("uriTemplate").and_then(Value::as_str)?;
            Some(McpResourceTemplateDescriptor {
                server: server_name.to_string(),
                uri_template: uri_template.to_string(),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(uri_template)
                    .to_string(),
                description: optional_string(item, "description"),
                mime_type: optional_string(item, "mimeType"),
            })
        })
        .collect();
    out.sort_by(|a, b| a.uri_template.cmp(&b.uri_template));
    out.dedup_by(|a, b| a.uri_template == b.uri_template);
    out
}

fn parse_prompt_descriptors(server_name: &str, items: &[Value]) -> Vec<McpPromptDescriptor> {
    let mut out: Vec<_> = items
        .iter()
        .filter_map(|item| {
            let name = item.get("name").and_then(Value::as_str)?;
            let arguments = item
                .get("arguments")
                .and_then(Value::as_array)
                .map(|args| {
                    args.iter()
                        .filter_map(|arg| {
                            let name = arg.get("name").and_then(Value::as_str)?;
                            Some(McpPromptArgument {
                                name: name.to_string(),
                                description: optional_string(arg, "description"),
                                required: arg.get("required").and_then(Value::as_bool),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(McpPromptDescriptor {
                server: server_name.to_string(),
                name: name.to_string(),
                description: optional_string(item, "description"),
                arguments,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

/// Multi-server MCP client gateway bridging child `stdio` processes and Streamable HTTP endpoints.
pub struct ConfiguredMcpGateway {
    transports: BTreeMap<String, Arc<McpClientTransport>>,
    /// Late-bound server→client hooks shared by every transport handler task.
    hooks: SharedMcpHooks,
}

impl std::fmt::Debug for ConfiguredMcpGateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let servers: Vec<&String> = self.transports.keys().collect();
        f.debug_struct("ConfiguredMcpGateway")
            .field("servers", &servers)
            .finish_non_exhaustive()
    }
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ConfiguredMcpGateway>();
};

impl ConfiguredMcpGateway {
    /// Connects all enabled `[mcp_servers.<name>]` entries from trusted configuration.
    ///
    /// `experimental_mcp_sampling` gates the `sampling/createMessage` server→client capability
    /// globally (per-server `[mcp_servers.<name>.sampling]` opts in additionally).
    ///
    /// Individual server failures are recorded in [`McpStartupReport::warnings`] without aborting
    /// the remaining servers or crashing CLI startup.
    pub async fn connect_all(
        servers: &IndexMap<String, McpServerConfig>,
        cwd: &Path,
        agent_settings: &AgentSettings,
        experimental_mcp_sampling: bool,
    ) -> (Arc<Self>, McpStartupReport) {
        let mut transports = BTreeMap::new();
        let mut report = McpStartupReport::default();
        let hooks: SharedMcpHooks = Arc::new(std::sync::Mutex::new(None));

        for (name, cfg) in servers {
            if !cfg.enabled {
                continue;
            }
            let handshake_timeout = cfg
                .startup_timeout_sec
                .filter(|&s| s > 0)
                .map_or(DEFAULT_HANDSHAKE_TIMEOUT, Duration::from_secs);
            let call_timeout = cfg
                .tool_timeout_sec
                .filter(|&s| s > 0)
                .map_or(DEFAULT_CALL_TIMEOUT, Duration::from_secs);
            let session_enabled = cfg.session.unwrap_or(true);
            let sampling_allowed = experimental_mcp_sampling && cfg.sampling;

            let outcome = if let Some(cmd) = cfg.command.as_deref().filter(|s| !s.trim().is_empty())
            {
                McpClientTransport::connect_stdio(StdioConnectParams {
                    server_name: name,
                    executable: cmd,
                    args: &cfg.args,
                    extra_env: &cfg.env,
                    cwd,
                    agent_settings,
                    handshake_timeout,
                    call_timeout,
                    sampling_allowed,
                    hooks: Arc::clone(&hooks),
                })
                .await
            } else if let Some(url) = cfg.url.as_deref().filter(|s| !s.trim().is_empty()) {
                let bearer_token = cfg
                    .bearer_token_env_var
                    .as_deref()
                    .filter(|k| !k.trim().is_empty())
                    .and_then(|k| {
                        cfg.env
                            .get(k)
                            .cloned()
                            .or_else(|| std::env::var(k).ok())
                            .filter(|v| !v.trim().is_empty())
                    });
                McpClientTransport::connect_http(
                    name,
                    url,
                    bearer_token,
                    session_enabled,
                    sampling_allowed,
                    Arc::clone(&hooks),
                    handshake_timeout,
                    call_timeout,
                    cfg.http_header_timeout_sec
                        .filter(|&s| s > 0)
                        .map_or(HTTP_HEADER_TIMEOUT, Duration::from_secs),
                    cfg.http_idle_timeout_sec
                        .filter(|&s| s > 0)
                        .map_or(HTTP_IDLE_CHUNK_TIMEOUT, Duration::from_secs),
                )
                .await
            } else {
                Err(ToolsError::invalid(format!(
                    "MCP server `{name}` is enabled but specifies neither `command` nor `url`"
                )))
            };

            match outcome {
                Ok(transport) => {
                    report.connected_servers.push(name.clone());
                    transports.insert(name.clone(), Arc::new(transport));
                }
                Err(err) => {
                    report
                        .warnings
                        .push(format!("MCP `{name}` unavailable: {err}"));
                }
            }
        }

        let gateway = Arc::new(Self { transports, hooks });
        (gateway, report)
    }

    /// Re-queries every catalog of `server_name` (`tools/list` plus the resource/prompt lists it
    /// advertises) and atomically replaces its shared catalog; other servers are untouched.
    ///
    /// # Errors
    /// Returns [`ToolsError`] if `server_name` is disconnected, the refresh times out, or
    /// `tools/list` fails.
    pub async fn refresh_server_catalog(
        &self,
        server_name: &str,
    ) -> Result<Vec<McpToolDescriptor>, ToolsError> {
        let transport = self.transport(server_name)?;
        tokio::time::timeout(
            transport.call_timeout(),
            transport.refresh_catalog(server_name),
        )
        .await
        .map_err(|_| ToolsError::CodeSandbox {
            message: format!("MCP `{server_name}` `tools/list` refresh timed out"),
        })??;
        Ok(self.list_tools())
    }

    fn transport(&self, server: &str) -> Result<Arc<McpClientTransport>, ToolsError> {
        self.transports
            .get(server)
            .cloned()
            .ok_or_else(|| ToolsError::CodeSandbox {
                message: format!("MCP server `{server}` is not connected"),
            })
    }

    /// One bounded request on `transport` (`resources/read` / `prompts/get`).
    async fn bounded_request(
        &self,
        transport: &Arc<McpClientTransport>,
        server: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, ToolsError> {
        let result = tokio::time::timeout(
            transport.call_timeout(),
            transport.request(server, method, params),
        )
        .await
        .map_err(|_| ToolsError::CodeSandbox {
            message: format!("MCP `{server}` `{method}` timed out"),
        })??;
        if transport.take_catalog_dirty() {
            let _ = self.refresh_server_catalog(server).await;
        }
        Ok(result)
    }

    /// Returns `true` when no MCP servers are currently connected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.transports.is_empty()
    }

    /// Returns the number of connected MCP servers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.transports.len()
    }
}

#[async_trait]
impl McpGateway for ConfiguredMcpGateway {
    fn list_tools(&self) -> Vec<McpToolDescriptor> {
        self.transports
            .values()
            .flat_map(|transport| transport.catalog_list(|catalog| &catalog.tools))
            .collect()
    }

    fn list_resources(&self) -> Vec<McpResourceDescriptor> {
        self.transports
            .values()
            .flat_map(|transport| transport.catalog_list(|catalog| &catalog.resources))
            .collect()
    }

    fn list_resource_templates(&self) -> Vec<McpResourceTemplateDescriptor> {
        self.transports
            .values()
            .flat_map(|transport| transport.catalog_list(|catalog| &catalog.resource_templates))
            .collect()
    }

    fn list_prompts(&self) -> Vec<McpPromptDescriptor> {
        self.transports
            .values()
            .flat_map(|transport| transport.catalog_list(|catalog| &catalog.prompts))
            .collect()
    }

    fn check_support(&self, server: &str, operation: &str) -> Result<(), ToolsError> {
        let transport = self.transports.get(server).ok_or_else(|| {
            ToolsError::invalid(format!("MCP server `{server}` is not connected"))
        })?;
        if transport.capabilities().supports(operation) {
            return Ok(());
        }
        Err(ToolsError::invalid(format!(
            "MCP server `{server}` does not advertise the `{}` capability (needed for `{operation}`)",
            capability_name(operation)
        )))
    }

    /// Attaches server→client hooks (`sampling/createMessage`, `elicitation/create`, progress)
    /// that transport handler tasks consult for every subsequent server-initiated request.
    fn set_server_hooks(&self, hooks: Arc<McpServerHooks>) {
        *self
            .hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hooks);
    }

    async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<Value, ToolsError> {
        let transport = self.transport(server)?;
        let token = transport.allocate_progress_token();
        let _tracked = transport.router().track_call(token, tool);
        let result = tokio::time::timeout(
            transport.call_timeout(),
            transport.request(
                server,
                "tools/call",
                json!({
                    "name": tool,
                    "arguments": arguments,
                    "_meta": { "progressToken": token },
                }),
            ),
        )
        .await
        .map_err(|_| ToolsError::CodeSandbox {
            message: format!("MCP tool call `{server}/{tool}` timed out"),
        })??;
        drop(_tracked);

        if transport.take_catalog_dirty() {
            let _ = self.refresh_server_catalog(server).await;
        }

        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(ToolsError::CodeSandbox {
                message: format!("MCP tool `{server}/{tool}` returned error: {result}"),
            });
        }

        Ok(result)
    }

    async fn read_resource(&self, server: &str, uri: &str) -> Result<Value, ToolsError> {
        let transport = self.transport(server)?;
        self.bounded_request(&transport, server, "resources/read", json!({ "uri": uri }))
            .await
    }

    async fn get_prompt(
        &self,
        server: &str,
        name: &str,
        arguments: Value,
    ) -> Result<Value, ToolsError> {
        let transport = self.transport(server)?;
        self.bounded_request(
            &transport,
            server,
            "prompts/get",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }
}

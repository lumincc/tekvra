//! Live `stdio` child-process and Streamable HTTP (`application/json` + `text/event-stream`)
//! JSON-RPC 2.0 MCP client gateway (`ConfiguredMcpGateway`).
//!
//! Connects enabled `[mcp_servers.<name>]` entries from trusted `tekvra.toml` configuration and
//! refreshes cached tool catalogs on `notifications/tools/list_changed` while keeping top-level
//! LLM `ToolDefinition`s completely unchanged across turns.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{Value, json};
use tekvra_config::{AgentSettings, McpServerConfig};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::ToolsError;
use crate::pty::build_sanitized_shell_env;
use crate::web_and_code::{McpGateway, McpToolDescriptor};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Summary of startup MCP server discovery.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpStartupReport {
    /// Names of MCP servers that completed handshake and tool discovery.
    pub connected_servers: Vec<String>,
    /// Non-fatal connection warnings for servers that could not be reached.
    pub warnings: Vec<String>,
}

struct StdioSession {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

enum McpClientTransport {
    Stdio {
        session: Box<Mutex<StdioSession>>,
        next_id: AtomicU64,
        catalog_dirty: AtomicBool,
        call_timeout: Duration,
    },
    Http {
        client: reqwest::Client,
        url: String,
        bearer_token: Option<String>,
        next_id: AtomicU64,
        catalog_dirty: AtomicBool,
        call_timeout: Duration,
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
}

impl McpClientTransport {
    const fn call_timeout(&self) -> Duration {
        match self {
            Self::Stdio { call_timeout, .. } | Self::Http { call_timeout, .. } => *call_timeout,
        }
    }

    fn take_catalog_dirty(&self) -> bool {
        match self {
            Self::Stdio { catalog_dirty, .. } | Self::Http { catalog_dirty, .. } => {
                catalog_dirty.swap(false, Ordering::AcqRel)
            }
        }
    }

    async fn connect_stdio(
        params: StdioConnectParams<'_>,
    ) -> Result<(Self, Vec<McpToolDescriptor>), ToolsError> {
        let StdioConnectParams {
            server_name,
            executable,
            args,
            extra_env,
            cwd,
            agent_settings,
            handshake_timeout,
            call_timeout,
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

        let transport = Self::Stdio {
            session: Box::new(Mutex::new(StdioSession {
                _child: child,
                stdin,
                stdout: BufReader::new(stdout),
            })),
            next_id: AtomicU64::new(1),
            catalog_dirty: AtomicBool::new(false),
            call_timeout,
        };

        let tools =
            tokio::time::timeout(handshake_timeout, transport.handshake_and_list(server_name))
                .await
                .map_err(|_| ToolsError::CodeSandbox {
                    message: format!("stdio MCP server `{server_name}` timed out during handshake"),
                })??;

        Ok((transport, tools))
    }

    async fn connect_http(
        server_name: &str,
        raw_url: &str,
        bearer_token: Option<String>,
        handshake_timeout: Duration,
        call_timeout: Duration,
    ) -> Result<(Self, Vec<McpToolDescriptor>), ToolsError> {
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

        let transport = Self::Http {
            client,
            url: raw_url.to_string(),
            bearer_token,
            next_id: AtomicU64::new(1),
            catalog_dirty: AtomicBool::new(false),
            call_timeout,
        };

        let tools =
            tokio::time::timeout(handshake_timeout, transport.handshake_and_list(server_name))
                .await
                .map_err(|_| ToolsError::CodeSandbox {
                    message: format!("HTTP MCP server `{server_name}` timed out during handshake"),
                })??;

        Ok((transport, tools))
    }

    async fn handshake_and_list(
        &self,
        server_name: &str,
    ) -> Result<Vec<McpToolDescriptor>, ToolsError> {
        let _init_res = self
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

        self.notify("notifications/initialized", json!({})).await?;
        self.list_tools_rpc(server_name).await
    }

    async fn list_tools_rpc(
        &self,
        server_name: &str,
    ) -> Result<Vec<McpToolDescriptor>, ToolsError> {
        let list_res = self.request(server_name, "tools/list", json!({})).await?;
        Ok(parse_tool_descriptors(server_name, &list_res))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), ToolsError> {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        match self {
            Self::Stdio { session, .. } => {
                let mut guard = session.lock().await;
                let mut raw =
                    serde_json::to_vec(&payload).map_err(|e| ToolsError::CodeSandbox {
                        message: format!("failed encoding MCP notification: {e}"),
                    })?;
                raw.push(b'\n');
                guard
                    .stdin
                    .write_all(&raw)
                    .await
                    .map_err(|e| ToolsError::CodeSandbox {
                        message: format!("failed writing MCP notification: {e}"),
                    })?;
                guard
                    .stdin
                    .flush()
                    .await
                    .map_err(|e| ToolsError::CodeSandbox {
                        message: format!("failed flushing MCP notification: {e}"),
                    })?;
                Ok(())
            }
            Self::Http {
                client,
                url,
                bearer_token,
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
                next_id,
                catalog_dirty,
                ..
            } => {
                let req_id = next_id.fetch_add(1, Ordering::Relaxed);
                let payload = json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "method": method,
                    "params": params,
                });
                let mut guard = session.lock().await;
                let mut raw =
                    serde_json::to_vec(&payload).map_err(|e| ToolsError::CodeSandbox {
                        message: format!("failed encoding MCP request `{method}`: {e}"),
                    })?;
                raw.push(b'\n');
                guard
                    .stdin
                    .write_all(&raw)
                    .await
                    .map_err(|e| ToolsError::CodeSandbox {
                        message: format!(
                            "MCP `{server_name}` stdin write failed on `{method}`: {e}"
                        ),
                    })?;
                guard
                    .stdin
                    .flush()
                    .await
                    .map_err(|e| ToolsError::CodeSandbox {
                        message: format!(
                            "MCP `{server_name}` stdin flush failed on `{method}`: {e}"
                        ),
                    })?;

                let mut line = String::new();
                loop {
                    line.clear();
                    let n = guard.stdout.read_line(&mut line).await.map_err(|e| {
                        ToolsError::CodeSandbox {
                            message: format!(
                                "MCP `{server_name}` stdout read failed on `{method}`: {e}"
                            ),
                        }
                    })?;
                    if n == 0 {
                        return Err(ToolsError::CodeSandbox {
                            message: format!(
                                "MCP `{server_name}` closed stdout while awaiting `{method}`"
                            ),
                        });
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let msg: Value =
                        serde_json::from_str(trimmed).map_err(|e| ToolsError::CodeSandbox {
                            message: format!(
                                "MCP `{server_name}` emitted invalid JSON on `{method}`: {e}"
                            ),
                        })?;
                    if is_tools_list_changed_notification(&msg) {
                        catalog_dirty.store(true, Ordering::Release);
                    }
                    if msg.get("id").and_then(Value::as_u64) != Some(req_id) {
                        continue;
                    }
                    return extract_rpc_result(server_name, method, msg);
                }
            }
            Self::Http {
                client,
                url,
                bearer_token,
                next_id,
                catalog_dirty,
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
                if let Some(tok) = bearer_token.as_deref() {
                    req = req.bearer_auth(tok);
                }
                let resp = req.send().await.map_err(|e| ToolsError::CodeSandbox {
                    message: format!("HTTP MCP `{server_name}` request `{method}` failed: {e}"),
                })?;
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
                    let raw_sse = resp.text().await.map_err(|e| ToolsError::CodeSandbox {
                        message: format!(
                            "HTTP MCP `{server_name}` `{method}` SSE stream read failed: {e}"
                        ),
                    })?;
                    parse_sse_rpc_response(server_name, method, req_id, &raw_sse, catalog_dirty)
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
}

fn is_tools_list_changed_notification(msg: &Value) -> bool {
    matches!(
        msg.get("method").and_then(Value::as_str),
        Some("notifications/tools/list_changed" | "tools/list_changed")
    )
}

fn parse_sse_rpc_response(
    server_name: &str,
    method: &str,
    req_id: u64,
    raw_sse: &str,
    catalog_dirty: &AtomicBool,
) -> Result<Value, ToolsError> {
    let mut current_data: Vec<&str> = Vec::new();
    let mut matched_msg: Option<Value> = None;

    for raw_line in raw_sse.lines().chain(std::iter::once("")) {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            if !current_data.is_empty() {
                let joined = current_data.join("\n");
                current_data.clear();
                let trimmed = joined.trim();
                if !trimmed.is_empty() && trimmed != "[DONE]" {
                    let msg: Value =
                        serde_json::from_str(trimmed).map_err(|e| ToolsError::CodeSandbox {
                            message: format!(
                                "HTTP MCP `{server_name}` `{method}` emitted invalid SSE JSON: {e}"
                            ),
                        })?;
                    if is_tools_list_changed_notification(&msg) {
                        catalog_dirty.store(true, Ordering::Release);
                    }
                    if msg.get("id").and_then(Value::as_u64) == Some(req_id)
                        || (matched_msg.is_none()
                            && (msg.get("result").is_some() || msg.get("error").is_some()))
                    {
                        matched_msg = Some(msg);
                    }
                }
            }
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            current_data.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }

    let msg = matched_msg.ok_or_else(|| ToolsError::CodeSandbox {
        message: format!(
            "HTTP MCP `{server_name}` `{method}` SSE stream ended without matching JSON-RPC response"
        ),
    })?;
    extract_rpc_result(server_name, method, msg)
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

/// Multi-server MCP client gateway bridging child `stdio` processes and Streamable HTTP endpoints.
pub struct ConfiguredMcpGateway {
    transports: BTreeMap<String, Arc<McpClientTransport>>,
    cached_tools: RwLock<Vec<McpToolDescriptor>>,
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
    /// Individual server failures are recorded in [`McpStartupReport::warnings`] without aborting
    /// the remaining servers or crashing CLI startup.
    pub async fn connect_all(
        servers: &IndexMap<String, McpServerConfig>,
        cwd: &Path,
        agent_settings: &AgentSettings,
    ) -> (Arc<Self>, McpStartupReport) {
        let mut transports = BTreeMap::new();
        let mut all_tools = Vec::new();
        let mut report = McpStartupReport::default();

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
                    handshake_timeout,
                    call_timeout,
                )
                .await
            } else {
                Err(ToolsError::invalid(format!(
                    "MCP server `{name}` is enabled but specifies neither `command` nor `url`"
                )))
            };

            match outcome {
                Ok((transport, tools)) => {
                    report.connected_servers.push(name.clone());
                    all_tools.extend(tools);
                    transports.insert(name.clone(), Arc::new(transport));
                }
                Err(err) => {
                    report
                        .warnings
                        .push(format!("MCP `{name}` unavailable: {err}"));
                }
            }
        }

        let gateway = Arc::new(Self {
            transports,
            cached_tools: RwLock::new(all_tools),
        });
        (gateway, report)
    }

    /// Re-queries `tools/list` on `server_name` and atomically updates its cached tool descriptors.
    ///
    /// # Errors
    /// Returns [`ToolsError`] if `server_name` is disconnected or `tools/list` fails.
    pub async fn refresh_server_catalog(
        &self,
        server_name: &str,
    ) -> Result<Vec<McpToolDescriptor>, ToolsError> {
        let transport =
            self.transports
                .get(server_name)
                .cloned()
                .ok_or_else(|| ToolsError::CodeSandbox {
                    message: format!("MCP server `{server_name}` is not connected"),
                })?;

        let fresh = tokio::time::timeout(
            transport.call_timeout(),
            transport.list_tools_rpc(server_name),
        )
        .await
        .map_err(|_| ToolsError::CodeSandbox {
            message: format!("MCP `{server_name}` `tools/list` refresh timed out"),
        })??;

        let mut guard = self
            .cached_tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.retain(|d| d.server != server_name);
        guard.extend(fresh);
        guard.sort_by(|a, b| a.server.cmp(&b.server).then_with(|| a.tool.cmp(&b.tool)));
        Ok(guard.clone())
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
        self.cached_tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<Value, ToolsError> {
        let transport =
            self.transports
                .get(server)
                .cloned()
                .ok_or_else(|| ToolsError::CodeSandbox {
                    message: format!("MCP server `{server}` is not connected"),
                })?;

        let result = tokio::time::timeout(
            transport.call_timeout(),
            transport.request(
                server,
                "tools/call",
                json!({
                    "name": tool,
                    "arguments": arguments,
                }),
            ),
        )
        .await
        .map_err(|_| ToolsError::CodeSandbox {
            message: format!("MCP tool call `{server}/{tool}` timed out"),
        })??;

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
}

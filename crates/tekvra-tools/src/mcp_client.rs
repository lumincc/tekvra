//! Live `stdio` child-process and HTTP JSON-RPC 2.0 MCP client gateway (`ConfiguredMcpGateway`).
//!
//! Connects enabled `[mcp_servers.<name>]` entries from trusted `tekvra.toml` configuration while
//! keeping top-level LLM `ToolDefinition`s completely unchanged across turns.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
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
const MCP_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MCP_CALL_TIMEOUT: Duration = Duration::from_secs(30);

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
    },
    Http {
        client: reqwest::Client,
        url: String,
        next_id: AtomicU64,
    },
}

impl McpClientTransport {
    async fn connect_stdio(
        server_name: &str,
        executable: &str,
        args: &[String],
        extra_env: &BTreeMap<String, String>,
        cwd: &Path,
        agent_settings: &AgentSettings,
    ) -> Result<(Self, Vec<McpToolDescriptor>), ToolsError> {
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
        };

        let tools = tokio::time::timeout(
            MCP_HANDSHAKE_TIMEOUT,
            transport.handshake_and_list(server_name),
        )
        .await
        .map_err(|_| ToolsError::CodeSandbox {
            message: format!("stdio MCP server `{server_name}` timed out during handshake"),
        })??;

        Ok((transport, tools))
    }

    async fn connect_http(
        server_name: &str,
        raw_url: &str,
    ) -> Result<(Self, Vec<McpToolDescriptor>), ToolsError> {
        let parsed = url::Url::parse(raw_url).map_err(|e| ToolsError::CodeSandbox {
            message: format!("invalid HTTP MCP URL `{raw_url}` for `{server_name}`: {e}"),
        })?;
        let is_loopback = parsed
            .host_str()
            .is_some_and(|h| matches!(h, "127.0.0.1" | "localhost" | "[::1]" | "::1"));

        let mut builder = reqwest::Client::builder().timeout(MCP_CALL_TIMEOUT);
        if is_loopback {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(|e| ToolsError::CodeSandbox {
            message: format!("failed building HTTP MCP client for `{server_name}`: {e}"),
        })?;

        let transport = Self::Http {
            client,
            url: raw_url.to_string(),
            next_id: AtomicU64::new(1),
        };

        let tools = tokio::time::timeout(
            MCP_HANDSHAKE_TIMEOUT,
            transport.handshake_and_list(server_name),
        )
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
                let mut line =
                    serde_json::to_vec(&payload).map_err(|e| ToolsError::CodeSandbox {
                        message: format!("failed serializing MCP notification: {e}"),
                    })?;
                line.push(b'\n');
                guard
                    .stdin
                    .write_all(&line)
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
            Self::Http { client, url, .. } => {
                // Best-effort notification over HTTP (servers may return 200, 202, or 204).
                let _ = client.post(url).json(&payload).send().await;
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
            Self::Stdio { session, next_id } => {
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
                    if msg.get("id").and_then(Value::as_u64) != Some(req_id) {
                        // Ignore asynchronous server notifications while awaiting our response id.
                        continue;
                    }
                    return extract_rpc_result(server_name, method, msg);
                }
            }
            Self::Http {
                client,
                url,
                next_id,
            } => {
                let req_id = next_id.fetch_add(1, Ordering::Relaxed);
                let payload = json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "method": method,
                    "params": params,
                });
                let resp = client.post(url).json(&payload).send().await.map_err(|e| {
                    ToolsError::CodeSandbox {
                        message: format!("HTTP MCP `{server_name}` request `{method}` failed: {e}"),
                    }
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

/// Multi-server MCP client gateway bridging child `stdio` processes and HTTP JSON-RPC endpoints.
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
            let outcome = if let Some(cmd) = cfg.command.as_deref().filter(|s| !s.trim().is_empty())
            {
                McpClientTransport::connect_stdio(
                    name,
                    cmd,
                    &cfg.args,
                    &cfg.env,
                    cwd,
                    agent_settings,
                )
                .await
            } else if let Some(url) = cfg.url.as_deref().filter(|s| !s.trim().is_empty()) {
                McpClientTransport::connect_http(name, url).await
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
            MCP_CALL_TIMEOUT,
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

        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(ToolsError::CodeSandbox {
                message: format!("MCP tool `{server}/{tool}` returned error: {result}"),
            });
        }

        Ok(result)
    }
}

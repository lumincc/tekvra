//! SSRF-safe outbound `webfetch` client (validating DNS and redirects hop-by-hop) and
//! isolated Monty Python `code` sandbox + prompt-cache-stable `McpGateway`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use ipnet::IpNet;
use monty::{MontyRun, RunProgress};
use monty_types::{
    CompileOptions, ExcType, ExtFunctionResult, MontyException, MontyObject, NameLookupResult,
    ObjectRef, PrintWriter, PrintWriterCallback, ResourceLimits, ResourceTracker,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value, json};
use tekvra_protocol::ToolOutput;
use tokio::sync::{mpsc, oneshot};

use crate::ToolsError;

const MAX_REDIRECT_HOPS: usize = 5;
const MAX_WEBFETCH_BYTES: usize = 512 * 1024;
const MONTY_MAX_MEMORY: usize = 32 * 1024 * 1024;
const MONTY_TIMEOUT: Duration = Duration::from_secs(10);

/// Returns `true` when `ip` belongs to loopback, link-local, cloud metadata (`169.254.0.0/16`),
/// private RFC1918, IPv6 ULA (`fc00::/7`), CGNAT (`100.64.0.0/10`), unspecified, or multicast space.
#[must_use]
pub fn is_blocked_ssrf_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped_v4) = v6.to_ipv4_mapped() {
                return is_blocked_v4(mapped_v4);
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
        }
    }
}

const fn is_blocked_v4(v4: Ipv4Addr) -> bool {
    let octets = v4.octets();
    let is_cgnat = octets[0] == 100 && (octets[1] & 0b1100_0000) == 64;
    let is_benchmark = octets[0] == 198 && (octets[1] == 18 || octets[1] == 19);
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_multicast()
        || is_cgnat
        || is_benchmark
}

/// Outbound HTTP/HTTPS fetcher enforcing DNS + per-redirect SSRF checks and reading proxy env
/// strictly from the host process environment (never from `.env` files).
#[derive(Debug, Clone)]
pub struct SafeWebFetcher {
    allowlisted_hosts: Vec<String>,
    allowlisted_cidrs: Vec<IpNet>,
}

impl SafeWebFetcher {
    /// Constructs a [`SafeWebFetcher`] from `AgentSettings::webfetch_allow_private_hosts`.
    #[must_use]
    pub fn new(allowlist_entries: &[String]) -> Self {
        let mut hosts = Vec::new();
        let mut cidrs = Vec::new();
        for entry in allowlist_entries {
            let trimmed = entry.trim();
            if let Ok(net) = trimmed.parse::<IpNet>() {
                cidrs.push(net);
            } else if let Ok(ip) = trimmed.parse::<IpAddr>() {
                cidrs.push(IpNet::from(ip));
            } else if !trimmed.is_empty() {
                hosts.push(trimmed.to_ascii_lowercase());
            }
        }
        Self {
            allowlisted_hosts: hosts,
            allowlisted_cidrs: cidrs,
        }
    }

    /// Fetches `raw_url` after validating DNS and manually validating every redirect hop.
    ///
    /// # Errors
    /// Returns [`ToolsError::WebFetch`] if any target or redirect resolves to a blocked private/loopback/metadata IP.
    pub async fn fetch(&self, raw_url: &str) -> Result<ToolOutput, ToolsError> {
        let mut current = url::Url::parse(raw_url).map_err(|e| ToolsError::WebFetch {
            message: format!("invalid URL `{raw_url}`: {e}"),
        })?;

        for hop in 0..=MAX_REDIRECT_HOPS {
            let scheme = current.scheme();
            if scheme != "http" && scheme != "https" {
                return Err(ToolsError::WebFetch {
                    message: format!("unsupported URL scheme `{scheme}` (expected http or https)"),
                });
            }
            let host = current
                .host_str()
                .ok_or_else(|| ToolsError::WebFetch {
                    message: format!("URL `{current}` is missing a host"),
                })?
                .to_string();
            let port = current.port_or_known_default().unwrap_or(80);

            let resolved_addrs = self.resolve_and_validate(&host, port).await?;

            // Build hop client with redirects disabled and resolved safe IPs pinned to prevent DNS rebinding!
            let mut builder = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(20))
                .user_agent(format!("tekvra/{}", env!("CARGO_PKG_VERSION")));
            if current
                .host()
                .is_some_and(|h| matches!(h, url::Host::Domain(_)))
            {
                builder = builder.resolve_to_addrs(&host, &resolved_addrs);
            }
            let client = builder.build().map_err(|e| ToolsError::WebFetch {
                message: format!("failed building HTTP client: {e}"),
            })?;

            let resp =
                client
                    .get(current.clone())
                    .send()
                    .await
                    .map_err(|e| ToolsError::WebFetch {
                        message: format!("GET `{current}` failed: {e}"),
                    })?;

            let status = resp.status();
            if status.is_redirection() {
                if hop == MAX_REDIRECT_HOPS {
                    return Err(ToolsError::WebFetch {
                        message: format!("exceeded maximum redirect hops ({MAX_REDIRECT_HOPS})"),
                    });
                }
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| ToolsError::WebFetch {
                        message: format!("redirect {} missing Location header", status.as_u16()),
                    })?;
                current = current.join(loc).map_err(|e| ToolsError::WebFetch {
                    message: format!("invalid redirect Location `{loc}`: {e}"),
                })?;
                continue;
            }

            let bytes = resp.bytes().await.map_err(|e| ToolsError::WebFetch {
                message: format!("failed reading response body: {e}"),
            })?;
            let truncated = bytes.len() > MAX_WEBFETCH_BYTES;
            let slice = &bytes[..bytes.len().min(MAX_WEBFETCH_BYTES)];
            let mut text = String::from_utf8_lossy(slice).into_owned();
            if truncated {
                text.push_str("\n...[webfetch body truncated at 512 KiB]");
            }
            return Ok(ToolOutput::Text {
                text: format!("HTTP {} {current}\n\n{text}", status.as_u16()),
            });
        }

        Err(ToolsError::WebFetch {
            message: "redirect loop aborted".to_string(),
        })
    }

    async fn resolve_and_validate(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, ToolsError> {
        let host_unbracketed = host
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(host);
        let host_allowed = self
            .allowlisted_hosts
            .iter()
            .any(|h| h == &host_unbracketed.to_ascii_lowercase());

        let addrs: Vec<SocketAddr> = if let Ok(ip) = host_unbracketed.parse::<IpAddr>() {
            vec![SocketAddr::new(ip, port)]
        } else {
            tokio::net::lookup_host((host_unbracketed, port))
                .await
                .map_err(|e| ToolsError::WebFetch {
                    message: format!("DNS lookup failed for `{host}`: {e}"),
                })?
                .collect()
        };

        if addrs.is_empty() {
            return Err(ToolsError::WebFetch {
                message: format!("DNS lookup returned no addresses for `{host}`"),
            });
        }

        for sock in &addrs {
            let ip = sock.ip();
            let ip_allowed =
                host_allowed || self.allowlisted_cidrs.iter().any(|net| net.contains(&ip));
            if !ip_allowed && is_blocked_ssrf_ip(ip) {
                return Err(ToolsError::WebFetch {
                    message: format!(
                        "blocked SSRF request to `{host}` resolving to restricted IP `{ip}`"
                    ),
                });
            }
        }
        Ok(addrs)
    }
}

/// Metadata describing an external MCP server tool discoverable inside `code` via `mcp_list()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolDescriptor {
    /// MCP server identifier.
    pub server: String,
    /// Tool name on that server.
    pub tool: String,
    /// Short tool description.
    pub description: String,
    /// JSON Schema for input parameters.
    pub input_schema: Value,
}

/// Prompt-cache-stable MCP gateway: connecting, disconnecting, or calling MCP servers happens behind
/// this interface via the isolated `code` tool so top-level LLM `ToolDefinition`s never mutate!
#[async_trait]
pub trait McpGateway: Send + Sync {
    /// Lists currently connected MCP tools.
    fn list_tools(&self) -> Vec<McpToolDescriptor>;

    /// Invokes `server`/`tool` with `arguments`.
    async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<Value, ToolsError>;

    /// Attaches server→client hooks (`sampling/createMessage`, `elicitation/create`).
    /// No-op by default; live gateways override to route server-initiated requests.
    fn set_server_hooks(&self, hooks: Arc<McpServerHooks>) {
        let _ = hooks;
    }
}

/// Completes one MCP `sampling/createMessage` server→client request.
#[async_trait]
pub trait McpSamplingHook: Send + Sync {
    /// Returns the JSON-RPC `result` object (`{ role, content: { type, text }, model }`)
    /// or an error message surfaced as `-32603`.
    async fn sample(
        &self,
        server: &str,
        request_id: &Value,
        params: &Value,
    ) -> Result<Value, String>;
}

/// Answers one MCP `elicitation/create` server→client request.
#[async_trait]
pub trait McpElicitationHook: Send + Sync {
    /// Returns the JSON-RPC `result` object (`{ action: "accept"|"decline", content? }`);
    /// an `Err` is reported as a plain `decline`.
    async fn elicit(&self, server: &str, params: &Value) -> Result<Value, String>;
}

/// Server→client request hooks attached to a live gateway.
#[derive(Default)]
pub struct McpServerHooks {
    /// Model-sampling handler (session provider driver).
    pub sampling: Option<Arc<dyn McpSamplingHook>>,
    /// User-elicitation handler (session question bridge).
    pub elicitation: Option<Arc<dyn McpElicitationHook>>,
}

/// Isolated Monty Python subset sandbox (`code` tool) with zero direct host OS/FS/network/env access,
/// bridging concurrent outbound `mcp_call(server, tool, params)` invocations with per-call authorization.
#[derive(Debug, Clone, Default)]
pub struct MontyCodeSandbox;

struct MontyStdoutBuffer {
    buf: String,
}

impl PrintWriterCallback for MontyStdoutBuffer {
    fn stdout_write(&mut self, output: Cow<'_, str>) -> Result<(), MontyException> {
        self.buf.push_str(&output);
        Ok(())
    }

    fn stdout_push(&mut self, ch: char) -> Result<(), MontyException> {
        self.buf.push(ch);
        Ok(())
    }
}

struct PendingMcpBatch {
    calls: Vec<(u32, String, String, Value)>,
    reply_tx: oneshot::Sender<Vec<(u32, Result<Value, String>)>>,
}

/// Async authorizer for outbound MCP calls made from inside the Monty `code`
/// sandbox (prompts interactively when no explicit allow rule covers the call).
#[async_trait]
pub trait McpCallAuthorizer: Send + Sync {
    /// Authorizes one `server/tool` call, resolving prompts through the session
    /// approval ladder (or denying cleanly when no approval callback exists).
    async fn authorize_mcp(&self, server: &str, tool: &str) -> Result<(), ToolsError>;
}

#[async_trait]
impl McpCallAuthorizer for Arc<dyn McpCallAuthorizer> {
    async fn authorize_mcp(&self, server: &str, tool: &str) -> Result<(), ToolsError> {
        (**self).authorize_mcp(server, tool).await
    }
}

impl MontyCodeSandbox {
    /// Executes `python_code` inside an isolated Monty VM (`ResourceLimits` applied, all `OsCall`s denied).
    /// Supports both `mcp_list()` and concurrent `await mcp_call(server, tool, args)` (as well as `mcp_call_sync`).
    /// Each outbound MCP call is individually authorized via `authorize_mcp` before invoking `gateway`.
    ///
    /// # Errors
    /// Returns [`ToolsError::CodeSandbox`] on Python syntax/runtime errors or sandbox OS-call violations,
    /// or [`ToolsError::PermissionDenied`] if an MCP call is rejected.
    pub async fn execute(
        &self,
        python_code: &str,
        gateway: Option<Arc<dyn McpGateway>>,
        authorize_mcp: Arc<dyn McpCallAuthorizer>,
    ) -> Result<ToolOutput, ToolsError> {
        let gateway_for_vm = gateway.as_ref().map(Arc::clone);
        let (batch_tx, mut batch_rx) = mpsc::unbounded_channel::<PendingMcpBatch>();
        let code_owned = python_code.to_string();

        let vm_handle =
            std::thread::spawn(move || drive_monty_vm(code_owned, gateway_for_vm, &batch_tx));

        let mcp_counter = Arc::new(AtomicU32::new(0));

        // Service async MCP batches concurrently while the Monty thread awaits `reply_rx`!
        while let Some(batch) = batch_rx.recv().await {
            let mut futs = Vec::with_capacity(batch.calls.len());
            let mut immediate_errors: HashMap<u32, String> = HashMap::new();

            for (call_id, server, tool, args) in batch.calls {
                if let Err(err) = authorize_mcp.authorize_mcp(&server, &tool).await {
                    immediate_errors.insert(call_id, err.to_string());
                    continue;
                }
                let Some(gw) = gateway.as_ref().map(Arc::clone) else {
                    immediate_errors.insert(
                        call_id,
                        format!("no MCP gateway configured for `{server}/{tool}`"),
                    );
                    continue;
                };
                let counter = Arc::clone(&mcp_counter);
                futs.push(async move {
                    let res = gw
                        .call_tool(&server, &tool, args)
                        .await
                        .map_err(|e| e.to_string());
                    if res.is_ok() {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                    (call_id, res)
                });
            }

            let mut resolved = futures::future::join_all(futs).await;
            for (cid, msg) in immediate_errors {
                resolved.push((cid, Err(msg)));
            }
            let _ = batch.reply_tx.send(resolved);
        }

        let (stdout, final_val) = vm_handle.join().map_err(|_| ToolsError::CodeSandbox {
            message: "Monty sandbox worker thread panicked".to_string(),
        })??;

        let mut output = stdout;
        if let Some(val) = final_val
            && !val.is_null()
        {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&serde_json::to_string(&val).unwrap_or_default());
        }

        Ok(ToolOutput::CodeExecution {
            output,
            mcp_calls: mcp_counter.load(Ordering::Relaxed),
        })
    }
}

fn drive_monty_vm(
    script: String,
    gateway: Option<Arc<dyn McpGateway>>,
    batch_tx: &mpsc::UnboundedSender<PendingMcpBatch>,
) -> Result<(String, Option<Value>), ToolsError> {
    let mut writer_cb = MontyStdoutBuffer { buf: String::new() };
    let mut print_writer = PrintWriter::Callback(&mut writer_cb);

    let limits = ResourceLimits::default()
        .max_feed_duration(MONTY_TIMEOUT)
        .max_total_sleep(Duration::from_secs(1))
        .max_memory(MONTY_MAX_MEMORY)
        .max_recursion_depth(80);

    let runner =
        MontyRun::new(script, "code.py", vec![], CompileOptions::default()).map_err(|e| {
            ToolsError::CodeSandbox {
                message: format!("Python parse error: {e}"),
            }
        })?;

    let tracker = ResourceTracker::new(limits);
    let mut progress = runner
        .start(vec![], tracker, print_writer.reborrow())
        .map_err(|e| ToolsError::CodeSandbox {
            message: format!("Python runtime error: {e}"),
        })?;

    let mut pending_calls: HashMap<u32, (String, String, Value)> = HashMap::new();

    loop {
        match progress {
            RunProgress::Complete(obj) => {
                let ret = (obj != MontyObject::none()).then(|| monty_to_json(obj.as_ref()));
                return Ok((writer_cb.buf, ret));
            }
            RunProgress::NameLookup(lookup) => {
                let result = match lookup.name.as_str() {
                    "mcp_list" | "mcp_call" | "mcp_call_sync" => {
                        NameLookupResult::Value(MontyObject::function(lookup.name.clone(), None))
                    }
                    _ => NameLookupResult::Undefined,
                };
                progress = lookup
                    .resume(result, print_writer.reborrow())
                    .map_err(|e| ToolsError::CodeSandbox {
                        message: format!("Python runtime error: {e}"),
                    })?;
            }
            RunProgress::FunctionCall(call) => {
                let fn_name = call.function_name.clone();
                let args: Vec<Value> = call.args.args().map(monty_to_json).collect();

                match fn_name.as_str() {
                    "mcp_list" => {
                        let live_tool_list: Value = gateway
                            .as_ref()
                            .map(|gw| {
                                serde_json::to_value(gw.list_tools()).unwrap_or_else(|_| json!([]))
                            })
                            .unwrap_or_else(|| json!([]));
                        progress = call
                            .resume(
                                ExtFunctionResult::Return(json_to_monty(live_tool_list)),
                                print_writer.reborrow(),
                            )
                            .map_err(|e| ToolsError::CodeSandbox {
                                message: format!("Python runtime error: {e}"),
                            })?;
                    }
                    "mcp_call" => {
                        let (server, tool, payload) = parse_mcp_args(&args)?;
                        pending_calls.insert(call.call_id, (server, tool, payload));
                        progress = call.resume_pending(print_writer.reborrow()).map_err(|e| {
                            ToolsError::CodeSandbox {
                                message: format!("Python runtime error: {e}"),
                            }
                        })?;
                    }
                    "mcp_call_sync" => {
                        let (server, tool, payload) = parse_mcp_args(&args)?;
                        let (reply_tx, reply_rx) = oneshot::channel();
                        batch_tx
                            .send(PendingMcpBatch {
                                calls: vec![(call.call_id, server, tool, payload)],
                                reply_tx,
                            })
                            .map_err(|_| ToolsError::CodeSandbox {
                                message: "MCP bridge channel closed".to_string(),
                            })?;
                        let mut res_vec =
                            reply_rx
                                .blocking_recv()
                                .map_err(|_| ToolsError::CodeSandbox {
                                    message: "MCP bridge dropped reply".to_string(),
                                })?;
                        let ext_res = match res_vec.pop() {
                            Some((_, Ok(val))) => ExtFunctionResult::Return(json_to_monty(val)),
                            Some((_, Err(msg))) => {
                                if msg.contains("permission denied") {
                                    return Err(ToolsError::PermissionDenied { message: msg });
                                }
                                ExtFunctionResult::Error(MontyException::new(
                                    ExcType::RuntimeError,
                                    Some(msg),
                                ))
                            }
                            None => ExtFunctionResult::Return(MontyObject::none()),
                        };
                        progress = call.resume(ext_res, print_writer.reborrow()).map_err(|e| {
                            ToolsError::CodeSandbox {
                                message: format!("Python runtime error: {e}"),
                            }
                        })?;
                    }
                    other => {
                        progress = call
                            .resume(
                                ExtFunctionResult::NotFound(other.to_string()),
                                print_writer.reborrow(),
                            )
                            .map_err(|e| ToolsError::CodeSandbox {
                                message: format!("Python runtime error: {e}"),
                            })?;
                    }
                }
            }
            RunProgress::ResolveFutures(state) => {
                let ids = state.pending_call_ids().to_vec();
                let mut calls = Vec::with_capacity(ids.len());
                for id in ids {
                    if let Some((s, t, p)) = pending_calls.remove(&id) {
                        calls.push((id, s, t, p));
                    }
                }
                let (reply_tx, reply_rx) = oneshot::channel();
                batch_tx
                    .send(PendingMcpBatch { calls, reply_tx })
                    .map_err(|_| ToolsError::CodeSandbox {
                        message: "MCP bridge channel closed".to_string(),
                    })?;
                let resolved = reply_rx
                    .blocking_recv()
                    .map_err(|_| ToolsError::CodeSandbox {
                        message: "MCP bridge dropped reply".to_string(),
                    })?;

                let mut ext_results = Vec::with_capacity(resolved.len());
                for (id, outcome) in resolved {
                    match outcome {
                        Ok(val) => {
                            ext_results.push((id, ExtFunctionResult::Return(json_to_monty(val))))
                        }
                        Err(msg) => {
                            if msg.contains("permission denied") {
                                return Err(ToolsError::PermissionDenied { message: msg });
                            }
                            ext_results.push((
                                id,
                                ExtFunctionResult::Error(MontyException::new(
                                    ExcType::RuntimeError,
                                    Some(msg),
                                )),
                            ));
                        }
                    }
                }
                progress = state
                    .resume(ext_results, print_writer.reborrow())
                    .map_err(|e| ToolsError::CodeSandbox {
                        message: format!("Python runtime error: {e}"),
                    })?;
            }
            RunProgress::OsCall(_) => {
                return Err(ToolsError::CodeSandbox {
                    message: "direct host OS/filesystem/env/network calls are prohibited inside the isolated `code` sandbox".to_string(),
                });
            }
        }
    }
}

fn parse_mcp_args(args: &[Value]) -> Result<(String, String, Value), ToolsError> {
    let server = args
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ToolsError::invalid("`mcp_call(server, tool, params)` requires `server: str`")
        })?
        .to_string();
    let tool = args
        .get(1)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ToolsError::invalid("`mcp_call(server, tool, params)` requires `tool: str`")
        })?
        .to_string();
    let payload = args.get(2).cloned().unwrap_or_else(|| json!({}));
    Ok((server, tool, payload))
}

fn json_to_monty(value: Value) -> MontyObject {
    match value {
        Value::Null => MontyObject::none(),
        Value::Bool(b) => MontyObject::bool(b),
        Value::Number(n) => n.as_i64().map_or_else(
            || MontyObject::float(n.as_f64().unwrap_or(0.0)),
            MontyObject::int,
        ),
        Value::String(s) => MontyObject::string(s),
        Value::Array(arr) => MontyObject::list(arr.into_iter().map(json_to_monty)),
        Value::Object(map) => MontyObject::dict(
            map.into_iter()
                .map(|(k, v)| (MontyObject::string(k), json_to_monty(v))),
        ),
    }
}

fn monty_to_json(obj: ObjectRef<'_>) -> Value {
    match obj.type_name() {
        "NoneType" => Some(Value::Null),
        "bool" => obj.as_bool().map(Value::Bool),
        "int" => Some(obj.as_int().map_or_else(
            || Value::String(obj.to_string()),
            |i| Value::Number(i.into()),
        )),
        "float" => obj
            .as_float()
            .map(|f| Number::from_f64(f).map_or(Value::Null, Value::Number)),
        "str" => obj.as_str().map(|s| Value::String(s.to_owned())),
        "list" | "tuple" => obj
            .items()
            .map(|items| Value::Array(items.into_iter().map(monty_to_json).collect())),
        "dict" => obj.pairs().map(|pairs| {
            Value::Object(
                pairs
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), monty_to_json(v)))
                    .collect::<Map<_, _>>(),
            )
        }),
        _ => None,
    }
    .unwrap_or_else(|| Value::String(obj.py_repr()))
}

//! Read-only `lsp_diagnostics` client: spawns language servers over stdio JSON-RPC with
//! `Content-Length` framing, publishes `didOpen`/`didChange`, collects `publishDiagnostics`
//! inside a bounded quiet window, and shuts down cleanly on drop.
//!
//! Servers run as **read-only guests**: no `didSave`, no formatting/write capability requests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tekvra_config::LspServerConfig;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;
use tokio::sync::oneshot;

use crate::ToolsError;

/// Silence window: diagnostics settle for this long before the collection returns.
const DIAGNOSTICS_QUIET_WINDOW: Duration = Duration::from_millis(500);
/// Hard cap on the whole wait regardless of arriving diagnostics.
const DIAGNOSTICS_HARD_CAP: Duration = Duration::from_secs(5);
/// Startup bound for spawn + `initialize` round trip.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(5);

/// One published diagnostic (subset of the LSP shape we surface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDiagnostic {
    /// Absolute or workspace-relative file URI.
    pub uri: String,
    /// 1-based line (LSP ranges are 0-based; we convert).
    pub line: u32,
    /// 1-based column.
    pub column: u32,
    /// Severity name (`error`, `warning`, `info`, `hint`).
    pub severity: String,
    /// Diagnostic message.
    pub message: String,
}

struct LspServerHandle {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    diagnostics: Arc<Mutex<Vec<LspDiagnostic>>>,
    opened_uris: Mutex<Vec<String>>,
    next_id: AtomicU64,
    _reader: tokio::task::JoinHandle<()>,
}

impl Drop for LspServerHandle {
    fn drop(&mut self) {
        // `shutdown` is best-effort; `start_kill` is synchronous and kill_on_drop reaps.
        // A stale already-exited child makes this return `Err`, which is fine here.
        let _ = self.child.start_kill();
    }
}

/// Session-scoped manager: one server process per `[lsp.<id>]` entry, reused across calls.
pub struct LspManager {
    configs: BTreeMap<String, LspServerConfig>,
    workspace_root: PathBuf,
    servers: Mutex<BTreeMap<String, Arc<LspServerHandle>>>,
}

impl std::fmt::Debug for LspManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspManager")
            .field("servers", &self.configs.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl LspManager {
    /// Constructs a manager from trusted `[lsp.<id>]` configuration.
    #[must_use]
    pub fn new(configs: BTreeMap<String, LspServerConfig>, workspace_root: PathBuf) -> Self {
        Self {
            configs,
            workspace_root,
            servers: Mutex::new(BTreeMap::new()),
        }
    }

    /// Collects diagnostics for `file` from server `id` (empty id = first configured match).
    ///
    /// # Errors
    /// Returns bounded [`ToolsError`]s for missing configuration, extension mismatch,
    /// spawn failure, and initialize timeouts.
    pub async fn diagnostics(
        &self,
        server_id: &str,
        file: &Path,
    ) -> Result<Vec<LspDiagnostic>, ToolsError> {
        let (config_id, config) = self.select_server(server_id, file).ok_or_else(|| {
            ToolsError::invalid(format!(
                "`lsp_diagnostics`: no language server configured for `{}` (checked `{server_id}`)",
                file.display()
            ))
        })?;

        let handle = self.get_or_spawn(&config_id, &config).await?;
        self.push_document(&handle, file).await?;
        let snapshot = {
            let guard = handle.diagnostics.lock().await;
            guard.clone()
        };
        Ok(snapshot)
    }

    /// Shuts down every live server (called on session teardown; `Drop` also kills children).
    pub async fn shutdown_all(&self) {
        let mut guard = self.servers.lock().await;
        guard.clear();
    }

    fn select_server(&self, server_id: &str, file: &Path) -> Option<(String, LspServerConfig)> {
        let extension = file
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        if !server_id.is_empty() {
            let cfg = self.configs.get(server_id)?;
            if !cfg.languages.is_empty() && !cfg.languages.iter().any(|l| l == &extension) {
                return None;
            }
            return Some((server_id.to_string(), cfg.clone()));
        }
        // Auto-select: first entry whose `languages` list covers the extension.
        for (id, cfg) in &self.configs {
            if cfg.languages.is_empty() || cfg.languages.iter().any(|l| l == &extension) {
                return Some((id.clone(), cfg.clone()));
            }
        }
        None
    }

    async fn get_or_spawn(
        &self,
        config_id: &str,
        config: &LspServerConfig,
    ) -> Result<Arc<LspServerHandle>, ToolsError> {
        {
            let guard = self.servers.lock().await;
            if let Some(existing) = guard.get(config_id) {
                return Ok(Arc::clone(existing));
            }
        }
        let executable = config
            .command
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ToolsError::invalid(format!(
                    "`lsp_diagnostics`: server `{config_id}` has no `command` configured"
                ))
            })?;

        let mut cmd = Command::new(executable);
        cmd.args(&config.args)
            .current_dir(&self.workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| ToolsError::CodeSandbox {
            message: format!(
                "`lsp_diagnostics`: failed to start server `{config_id}` (`{executable}`): {e}"
            ),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| ToolsError::CodeSandbox {
            message: format!("`lsp_diagnostics`: server `{config_id}` missing stdin"),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| ToolsError::CodeSandbox {
            message: format!("`lsp_diagnostics`: server `{config_id}` missing stdout"),
        })?;

        let stdin = Arc::new(Mutex::new(stdin));
        let diagnostics = Arc::new(Mutex::new(Vec::<LspDiagnostic>::new()));
        let pending: Arc<std::sync::Mutex<BTreeMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(std::sync::Mutex::new(BTreeMap::new()));

        // Framing reader: Content-Length headers + JSON body.
        let reader_diag = Arc::clone(&diagnostics);
        let reader_pending = Arc::clone(&pending);
        let reader = tokio::spawn(async move {
            let mut out = BufReader::new(stdout);
            loop {
                let mut content_length: Option<usize> = None;
                // Headers end at a blank line.
                loop {
                    let mut line = String::new();
                    match out.read_line(&mut line).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some(v) = trimmed
                        .to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                    {
                        content_length = Some(v);
                    }
                }
                let Some(len) = content_length else { return };
                let mut buf = vec![0_u8; len];
                if out.read_exact(&mut buf).await.is_err() {
                    return;
                }
                let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
                    continue;
                };
                if msg.get("method").and_then(Value::as_str)
                    == Some("textDocument/publishDiagnostics")
                {
                    let uri = msg
                        .pointer("/params/uri")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let items = msg
                        .pointer("/params/diagnostics")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let mut guard = reader_diag.lock().await;
                    guard.retain(|d| d.uri != uri);
                    for item in items {
                        let line = item
                            .pointer("/range/start/line")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let column = item
                            .pointer("/range/start/character")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let severity = match item.get("severity").and_then(Value::as_u64) {
                            Some(1) => "error",
                            Some(2) => "warning",
                            Some(3) => "info",
                            Some(4) => "hint",
                            _ => "info",
                        };
                        guard.push(LspDiagnostic {
                            uri: uri.clone(),
                            line: line.saturating_add(1) as u32,
                            column: column.saturating_add(1) as u32,
                            severity: severity.to_string(),
                            message: item
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        });
                    }
                    continue;
                }
                // Response to one of our requests.
                if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    let tx = {
                        let mut guard = reader_pending
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        guard.remove(&id)
                    };
                    if let Some(tx) = tx {
                        let _ = tx.send(msg);
                    }
                }
                // Server→client requests are ignored: the reader owns stdout and cannot
                // reply; servers time out their own requests (we never send any).
            }
        });

        let handle = Arc::new(LspServerHandle {
            child,
            stdin: Arc::clone(&stdin),
            diagnostics,
            opened_uris: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            _reader: reader,
        });
        Self::initialize_handle(&stdin, &pending, &handle.next_id, &self.workspace_root).await?;

        let mut guard = self.servers.lock().await;
        guard.insert(config_id.to_string(), Arc::clone(&handle));
        Ok(handle)
    }

    /// Sends `initialize` + `initialized` (once per server process).
    async fn initialize_handle(
        stdin: &Arc<Mutex<ChildStdin>>,
        pending: &Arc<std::sync::Mutex<BTreeMap<u64, oneshot::Sender<Value>>>>,
        next_id: &AtomicU64,
        workspace_root: &Path,
    ) -> Result<(), ToolsError> {
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.insert(id, tx);
        }
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": path_to_uri(workspace_root),
                "capabilities": {},
            }
        });
        write_frame(stdin, &msg, "initialize").await?;
        if !matches!(
            tokio::time::timeout(INITIALIZE_TIMEOUT, rx).await.ok(),
            Some(Ok(_))
        ) {
            return Err(ToolsError::CodeSandbox {
                message: "`lsp_diagnostics`: initialize failed or timed out".to_string(),
            });
        }
        write_frame(
            stdin,
            &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
            "initialized",
        )
        .await
    }

    async fn push_document(
        &self,
        handle: &Arc<LspServerHandle>,
        file: &Path,
    ) -> Result<(), ToolsError> {
        let text = tokio::fs::read_to_string(file)
            .await
            .map_err(|e| ToolsError::CodeSandbox {
                message: format!("`lsp_diagnostics`: cannot read `{}`: {e}", file.display()),
            })?;
        let uri = path_to_uri(file);
        let language_id = file
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("text")
            .to_string();
        let mut opened = handle.opened_uris.lock().await;
        if opened.iter().any(|u| u == &uri) {
            write_frame(
                &handle.stdin,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/didChange",
                    "params": {
                        "textDocument": { "uri": uri, "version": 2 },
                        "contentChanges": [ { "text": text } ]
                    }
                }),
                "didChange",
            )
            .await?;
        } else {
            write_frame(
                &handle.stdin,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/didOpen",
                    "params": {
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id,
                            "version": 1,
                            "text": text
                        }
                    }
                }),
                "didOpen",
            )
            .await?;
            opened.push(uri);
        }
        drop(opened);
        // Quiet window: let publishDiagnostics settle, bounded by the hard cap.
        let deadline = tokio::time::Instant::now() + DIAGNOSTICS_HARD_CAP;
        let mut previous = {
            let guard = handle.diagnostics.lock().await;
            guard.len()
        };
        loop {
            tokio::time::sleep(DIAGNOSTICS_QUIET_WINDOW).await;
            let now = {
                let guard = handle.diagnostics.lock().await;
                guard.len()
            };
            if now == previous || tokio::time::Instant::now() >= deadline {
                break;
            }
            previous = now;
        }
        Ok(())
    }
}

fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn frame_message(msg: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(msg).unwrap_or_default();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

async fn write_frame(
    stdin: &Arc<Mutex<ChildStdin>>,
    msg: &Value,
    method: &str,
) -> Result<(), ToolsError> {
    let framed = frame_message(msg);
    let mut guard = stdin.lock().await;
    guard
        .write_all(&framed)
        .await
        .map_err(|e| ToolsError::CodeSandbox {
            message: format!("`lsp_diagnostics`: stdin write failed on `{method}`: {e}"),
        })?;
    guard.flush().await.map_err(|e| ToolsError::CodeSandbox {
        message: format!("`lsp_diagnostics`: stdin flush failed on `{method}`: {e}"),
    })?;
    Ok(())
}

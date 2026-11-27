//! JSON-RPC 2.0 transport listeners for `stdio://`, `unix://<socket>`, and `ws://host:port`
//! (with optional `Authorization: Bearer <token>` capability authentication).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::AppServerError;
use crate::server::{AppServerState, JsonRpcConnection};

/// Parsed `--listen` endpoint for `tekvra app-server`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenEndpoint {
    /// Standard input/output (`stdio://`).
    Stdio,
    /// Unix domain socket (`unix:///path/to/tekvra.sock`).
    Unix(PathBuf),
    /// WebSocket server (`ws://127.0.0.1:4500`).
    WebSocket(SocketAddr),
}

/// Parses a `--listen` URI (`stdio://`, `unix://<path>`, or `ws://<addr>`).
///
/// # Errors
/// Returns [`AppServerError::InvalidListenUri`] if the scheme or socket address is invalid.
pub fn parse_listen_endpoint(raw: &str) -> Result<ListenEndpoint, AppServerError> {
    let trimmed = raw.trim();
    if trimmed == "stdio://" || trimmed == "stdio" {
        return Ok(ListenEndpoint::Stdio);
    }
    if let Some(rest) = trimmed.strip_prefix("unix://") {
        if rest.trim().is_empty() {
            return Err(AppServerError::InvalidListenUri {
                uri: trimmed.to_string(),
                reason: "empty unix socket path".to_string(),
            });
        }
        return Ok(ListenEndpoint::Unix(PathBuf::from(rest)));
    }
    if let Some(rest) = trimmed.strip_prefix("ws://") {
        let host_port = rest.split('/').next().unwrap_or(rest);
        let addr: SocketAddr = host_port
            .parse()
            .map_err(|e| AppServerError::InvalidListenUri {
                uri: trimmed.to_string(),
                reason: format!("invalid socket address `{host_port}`: {e}"),
            })?;
        return Ok(ListenEndpoint::WebSocket(addr));
    }
    Err(AppServerError::InvalidListenUri {
        uri: trimmed.to_string(),
        reason: "expected `stdio://`, `unix://<path>`, or `ws://<ip>:<port>`".to_string(),
    })
}

/// Serves newline-delimited JSON-RPC 2.0 over any async `reader` + `writer` stream.
///
/// # Errors
/// Returns [`AppServerError`] on underlying stream read/write failure.
pub async fn serve_read_write<R, W>(
    reader: R,
    mut writer: W,
    state: AppServerState,
) -> Result<(), AppServerError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if writer.write_all(frame.as_bytes()).await.is_err() {
                break;
            }
            if writer.write_all(b"\n").await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
    });

    let mut conn = JsonRpcConnection::new(state, out_tx);
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        conn.handle_raw_frame(&line).await;
    }

    drop(conn);
    let _ = writer_task.await;
    Ok(())
}

/// Serves `stdio://` JSON-RPC 2.0 on process stdin/stdout.
///
/// # Errors
/// Returns [`AppServerError`] on stdio I/O failure.
pub async fn serve_stdio(state: AppServerState) -> Result<(), AppServerError> {
    serve_read_write(tokio::io::stdin(), tokio::io::stdout(), state).await
}

/// Serves `unix://<socket_path>` JSON-RPC 2.0 over a Unix domain socket.
///
/// # Errors
/// Returns [`AppServerError`] if binding the Unix socket fails.
#[cfg(unix)]
pub async fn serve_unix(socket_path: &Path, state: AppServerState) -> Result<(), AppServerError> {
    if socket_path.exists() {
        let _ = std::fs::remove_file(socket_path);
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = tokio::net::UnixListener::bind(socket_path)?;
    loop {
        let (stream, _) = listener.accept().await?;
        let (read_half, write_half) = stream.into_split();
        let st = state.clone();
        tokio::spawn(async move {
            let _ = serve_read_write(read_half, write_half, st).await;
        });
    }
}

#[derive(Clone)]
struct WsAppState {
    app_state: AppServerState,
    auth_token: Option<Arc<String>>,
}

/// Serves `ws://host:port` JSON-RPC 2.0 over WebSocket, enforcing `Authorization: Bearer <token>`
/// when `auth_token` is configured.
///
/// # Errors
/// Returns [`AppServerError`] if binding the TCP listener fails.
pub async fn serve_websocket(
    listener: tokio::net::TcpListener,
    auth_token: Option<String>,
    state: AppServerState,
) -> Result<(), AppServerError> {
    let ws_state = WsAppState {
        app_state: state,
        auth_token: auth_token.map(Arc::new),
    };
    let app = Router::new()
        .route("/", get(ws_upgrade_handler))
        .route("/ws", get(ws_upgrade_handler))
        .with_state(ws_state);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn ws_upgrade_handler(
    State(ws_state): State<WsAppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if let Some(expected) = &ws_state.auth_token {
        let provided = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(str::trim);
        if provided != Some(expected.as_str()) {
            return (
                StatusCode::UNAUTHORIZED,
                "Missing or invalid Authorization Bearer capability token",
            )
                .into_response();
        }
    }

    ws.on_upgrade(move |socket| handle_ws_client(socket, ws_state.app_state))
}

async fn handle_ws_client(socket: WebSocket, state: AppServerState) {
    let (mut ws_sink, mut ws_stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if ws_sink.send(WsMessage::Text(frame.into())).await.is_err() {
                break;
            }
        }
    });

    let mut conn = JsonRpcConnection::new(state, out_tx);
    while let Some(Ok(msg)) = ws_stream.next().await {
        match msg {
            WsMessage::Text(txt) => {
                conn.handle_raw_frame(txt.as_str()).await;
            }
            WsMessage::Close(_) => break,
            _ => {}
        }
    }

    drop(conn);
    let _ = writer_task.await;
}

/// Dispatches `endpoint` (`stdio://`, `unix://`, or `ws://`) until shutdown.
///
/// # Errors
/// Returns [`AppServerError`] on listener or stream failure.
pub async fn serve_endpoint(
    endpoint: ListenEndpoint,
    auth_token: Option<String>,
    state: AppServerState,
) -> Result<(), AppServerError> {
    match endpoint {
        ListenEndpoint::Stdio => serve_stdio(state).await,
        #[cfg(unix)]
        ListenEndpoint::Unix(path) => serve_unix(&path, state).await,
        #[cfg(not(unix))]
        ListenEndpoint::Unix(path) => Err(AppServerError::InvalidListenUri {
            uri: format!("unix://{}", path.display()),
            reason: "Unix domain sockets are not supported on this platform".to_string(),
        }),
        ListenEndpoint::WebSocket(addr) => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            serve_websocket(listener, auth_token, state).await
        }
    }
}

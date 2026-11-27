//! Codex-compatible `app-server` v2 JSON-RPC 2.0 server over `stdio://`, `unix://`, and `ws://`.

pub mod fuzzy;
pub mod server;
pub mod transport;

use thiserror::Error;

pub use fuzzy::{FuzzyFileMatch, FuzzySearchOutput, fuzzy_file_search};
pub use server::{AppServerState, AppServerStateInit, JsonRpcConnection};
#[cfg(unix)]
pub use transport::serve_unix;
pub use transport::{
    ListenEndpoint, parse_listen_endpoint, serve_endpoint, serve_read_write, serve_stdio,
    serve_websocket,
};

/// Errors produced by `tekvra-app-server`.
#[derive(Debug, Error)]
pub enum AppServerError {
    /// Invalid `--listen` URI.
    #[error("invalid app-server listen URI `{uri}`: {reason}")]
    InvalidListenUri {
        /// Raw URI string.
        uri: String,
        /// Explanation.
        reason: String,
    },
    /// Transport I/O failure.
    #[error("transport I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization/deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// Core session error.
    #[error("session core error: {0}")]
    Core(#[from] tekvra_core::CoreError),
    /// Storage error.
    #[error("storage error: {0}")]
    Storage(#[from] tekvra_storage::StorageError),
    /// Config error.
    #[error("config error: {0}")]
    Config(#[from] tekvra_config::ConfigError),
}

//! Opt-in Luau external plugin host (`tekvra-lua`) aligned with Caudra's architecture:
//! - 100% zero built-in Lua tools (no `maki-pack`)
//! - Zero Lua VM initialized unless `experimental.lua_plugins = true` AND `--no-plugins` is false
//! - Per-plugin `plugin.toml` capability manifest gating `fs_read`, `fs_write`, `run`, `net`, and `env`
//! - Sandboxed `require()`, `--no-jit` interpreter debug mode, live `/reload`, and safe Luau GC drop ordering.

pub mod manifest;
pub mod runtime;

use thiserror::Error;

pub use manifest::{CapabilitySpec, PluginManifest, PluginPermissions};
pub use runtime::{LuaCommandSpec, LuaHostOptions, PluginHost, PluginLoadReport};
pub use tekvra_protocol::KeymapSpec;

/// Errors produced by `tekvra-lua`.
#[derive(Debug, Error)]
pub enum LuaHostError {
    /// Filesystem I/O failure.
    #[error("plugin I/O error on `{path}`: {source}")]
    Io {
        /// File path.
        path: std::path::PathBuf,
        /// OS error.
        #[source]
        source: std::io::Error,
    },
    /// Malformed `plugin.toml` manifest.
    #[error("invalid plugin manifest `{path}`: {message}")]
    Manifest {
        /// Manifest path.
        path: std::path::PathBuf,
        /// Parse error.
        message: String,
    },
    /// Luau VM compilation or runtime error.
    #[error("luau error: {0}")]
    Lua(#[from] mlua::Error),
}

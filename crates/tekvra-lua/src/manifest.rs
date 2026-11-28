//! Per-plugin `plugin.toml` capability schema (`fs_read`, `fs_write`, `run`, `net`, `env`)
//! with default-deny enforcement when `plugin.toml` is absent or omits a capability.

use std::fs;
use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

use crate::LuaHostError;

/// Capability grant in `plugin.toml`: either boolean (`true` = unrestricted within workspace/host,
/// `false` = denied) or an explicit allowlist of patterns/names.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum CapabilitySpec {
    /// `true` (all allowed) or `false` (denied).
    Bool(bool),
    /// Allowlisted globs, binary names, hostnames, or env var names.
    List(Vec<String>),
}

impl Default for CapabilitySpec {
    fn default() -> Self {
        Self::Bool(false)
    }
}

/// Raw `[permissions]` table inside `plugin.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RawPermissions {
    /// File read capability (`tekvra.fs.read_text`, `tekvra.fs.read_bytes`).
    pub fs_read: CapabilitySpec,
    /// File write capability (`tekvra.fs.write_text`, `tekvra.fs.write_bytes`).
    pub fs_write: CapabilitySpec,
    /// Subprocess execution capability (`tekvra.run.exec`).
    pub run: CapabilitySpec,
    /// Outbound HTTP capability (`tekvra.net.get`).
    pub net: CapabilitySpec,
    /// Host environment read capability (`tekvra.env.get`).
    pub env: CapabilitySpec,
}

/// Declarative `plugin.toml` manifest placed alongside a plugin's `init.lua`.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    /// Plugin identifier.
    pub name: String,
    /// Semantic version string.
    #[serde(default = "default_version")]
    pub version: String,
    /// Declared capability grants (all default to `false` when omitted!).
    #[serde(default)]
    pub permissions: RawPermissions,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

/// Compiled runtime capability checker bound to a single loaded Luau plugin.
#[derive(Debug, Clone)]
pub struct PluginPermissions {
    plugin_name: String,
    workspace_root: PathBuf,
    fs_read: CompiledGlobCap,
    fs_write: CompiledGlobCap,
    run: CapabilitySpec,
    net: CapabilitySpec,
    env: CapabilitySpec,
}

#[derive(Debug, Clone)]
enum CompiledGlobCap {
    Denied,
    AllInWorkspace,
    Globs(GlobSet),
}

impl PluginPermissions {
    /// Default-deny capability set used when `plugin.toml` is absent!
    #[must_use]
    pub fn deny_all(plugin_name: impl Into<String>, workspace_root: PathBuf) -> Self {
        Self {
            plugin_name: plugin_name.into(),
            workspace_root,
            fs_read: CompiledGlobCap::Denied,
            fs_write: CompiledGlobCap::Denied,
            run: CapabilitySpec::Bool(false),
            net: CapabilitySpec::Bool(false),
            env: CapabilitySpec::Bool(false),
        }
    }

    /// Loads `<plugin_dir>/plugin.toml` if present, or returns [`Self::deny_all`] if absent.
    ///
    /// # Errors
    /// Returns [`LuaHostError`] if an existing `plugin.toml` is malformed or contains invalid globs.
    pub fn load_for_dir(
        fallback_name: &str,
        plugin_dir: &Path,
        workspace_root: &Path,
    ) -> Result<Self, LuaHostError> {
        let canonical_ws =
            fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
        let manifest_path = plugin_dir.join("plugin.toml");
        if !manifest_path.is_file() {
            return Ok(Self::deny_all(fallback_name, canonical_ws));
        }
        let raw = fs::read_to_string(&manifest_path).map_err(|source| LuaHostError::Io {
            path: manifest_path.clone(),
            source,
        })?;
        let manifest: PluginManifest =
            toml::from_str(&raw).map_err(|e| LuaHostError::Manifest {
                path: manifest_path.clone(),
                message: e.to_string(),
            })?;

        Ok(Self {
            plugin_name: manifest.name,
            workspace_root: canonical_ws,
            fs_read: compile_glob_cap(&manifest.permissions.fs_read, &manifest_path)?,
            fs_write: compile_glob_cap(&manifest.permissions.fs_write, &manifest_path)?,
            run: manifest.permissions.run,
            net: manifest.permissions.net,
            env: manifest.permissions.env,
        })
    }

    /// Verifies `fs_read` permission for `candidate` and returns the resolved path inside `workspace_root`.
    ///
    /// # Errors
    /// Returns an `mlua::Error` if `fs_read` was not granted or `candidate` escapes `workspace_root`.
    pub fn check_fs_read(&self, candidate: &Path) -> mlua::Result<PathBuf> {
        self.check_fs_cap("fs_read", &self.fs_read, candidate)
    }

    /// Verifies `fs_write` permission for `candidate` and returns the resolved path inside `workspace_root`.
    ///
    /// # Errors
    /// Returns an `mlua::Error` if `fs_write` was not granted or `candidate` escapes `workspace_root`.
    pub fn check_fs_write(&self, candidate: &Path) -> mlua::Result<PathBuf> {
        self.check_fs_cap("fs_write", &self.fs_write, candidate)
    }

    /// Verifies `run` permission for `program`.
    ///
    /// # Errors
    /// Returns an `mlua::Error` if `run` was not granted for `program`.
    pub fn check_run(&self, program: &str) -> mlua::Result<()> {
        let base = Path::new(program)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(program);
        let allowed = match &self.run {
            CapabilitySpec::Bool(b) => *b,
            CapabilitySpec::List(list) => list
                .iter()
                .any(|item| item == "*" || item == base || item == program),
        };
        if allowed {
            Ok(())
        } else {
            Err(mlua::Error::RuntimeError(format!(
                "permission denied: plugin `{}` lacks `run` capability for `{program}` in plugin.toml",
                self.plugin_name
            )))
        }
    }

    /// Whether the `net` grant covers `host` (exact, case-insensitive match or `*`).
    #[must_use]
    pub fn net_host_granted(&self, host: &str) -> bool {
        match &self.net {
            CapabilitySpec::Bool(granted) => *granted,
            CapabilitySpec::List(list) => list
                .iter()
                .any(|h| h == "*" || h.eq_ignore_ascii_case(host)),
        }
    }

    /// Verifies `net` permission for `raw_url` (the host of the requested URL).
    ///
    /// # Errors
    /// Returns an `mlua::Error` if `net` was not granted for the target host.
    pub fn check_net(&self, raw_url: &str) -> mlua::Result<()> {
        let parsed = url::Url::parse(raw_url)
            .map_err(|e| mlua::Error::RuntimeError(format!("invalid URL `{raw_url}`: {e}")))?;
        let host = parsed.host_str().unwrap_or_default();
        if self.net_host_granted(host) {
            Ok(())
        } else {
            Err(mlua::Error::RuntimeError(format!(
                "permission denied: {}",
                self.net_denial(host, "host")
            )))
        }
    }

    /// Verifies `net` permission for one redirect hop, so a grant for one host never
    /// authorizes a redirect to another.
    ///
    /// # Errors
    /// Returns the denial message (without a `permission denied:` prefix) if the hop host
    /// was not granted.
    pub fn check_redirect_host(&self, host: &str) -> Result<(), String> {
        if self.net_host_granted(host) {
            Ok(())
        } else {
            Err(self.net_denial(host, "redirect host"))
        }
    }

    fn net_denial(&self, host: &str, kind: &str) -> String {
        format!(
            "plugin `{}` lacks `net` capability for {kind} `{host}` in plugin.toml",
            self.plugin_name
        )
    }

    /// Verifies `env` permission for `var_name`.
    ///
    /// # Errors
    /// Returns an `mlua::Error` if `env` was not granted for `var_name`.
    pub fn check_env(&self, var_name: &str) -> mlua::Result<()> {
        let allowed = match &self.env {
            CapabilitySpec::Bool(b) => *b,
            CapabilitySpec::List(list) => list.iter().any(|v| v == "*" || v == var_name),
        };
        if allowed {
            Ok(())
        } else {
            Err(mlua::Error::RuntimeError(format!(
                "permission denied: plugin `{}` lacks `env` capability for `{var_name}` in plugin.toml",
                self.plugin_name
            )))
        }
    }

    fn check_fs_cap(
        &self,
        cap_label: &str,
        cap: &CompiledGlobCap,
        candidate: &Path,
    ) -> mlua::Result<PathBuf> {
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.workspace_root.join(candidate)
        };
        let mut clean = PathBuf::new();
        for comp in joined.components() {
            match comp {
                Component::Prefix(p) => clean.push(p.as_os_str()),
                Component::RootDir => clean.push(comp.as_os_str()),
                Component::CurDir => {}
                Component::ParentDir => {
                    clean.pop();
                }
                Component::Normal(s) => clean.push(s),
            }
        }
        let rel = clean.strip_prefix(&self.workspace_root).map_err(|_| {
            mlua::Error::RuntimeError(format!(
                "permission denied: plugin `{}` path `{}` escapes workspace root",
                self.plugin_name,
                candidate.display()
            ))
        })?;

        match cap {
            CompiledGlobCap::Denied => Err(mlua::Error::RuntimeError(format!(
                "permission denied: plugin `{}` lacks `{cap_label}` capability in plugin.toml",
                self.plugin_name
            ))),
            CompiledGlobCap::AllInWorkspace => Ok(clean),
            CompiledGlobCap::Globs(gs) => {
                if gs.is_match(rel) {
                    Ok(clean)
                } else {
                    Err(mlua::Error::RuntimeError(format!(
                        "permission denied: plugin `{}` `{cap_label}` does not match `{}` in plugin.toml",
                        self.plugin_name,
                        rel.display()
                    )))
                }
            }
        }
    }
}

fn compile_glob_cap(
    spec: &CapabilitySpec,
    manifest_path: &Path,
) -> Result<CompiledGlobCap, LuaHostError> {
    match spec {
        CapabilitySpec::Bool(false) => Ok(CompiledGlobCap::Denied),
        CapabilitySpec::Bool(true) => Ok(CompiledGlobCap::AllInWorkspace),
        CapabilitySpec::List(patterns) if patterns.is_empty() => Ok(CompiledGlobCap::Denied),
        CapabilitySpec::List(patterns) => {
            let mut builder = GlobSetBuilder::new();
            for pat in patterns {
                let g = Glob::new(pat).map_err(|e| LuaHostError::Manifest {
                    path: manifest_path.to_path_buf(),
                    message: format!("invalid glob `{pat}`: {e}"),
                })?;
                builder.add(g);
            }
            let set = builder.build().map_err(|e| LuaHostError::Manifest {
                path: manifest_path.to_path_buf(),
                message: e.to_string(),
            })?;
            Ok(CompiledGlobCap::Globs(set))
        }
    }
}

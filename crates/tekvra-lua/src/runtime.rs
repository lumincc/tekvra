//! Sandboxed Luau VM lifecycle, `tekvra.*` host bindings, `--no-jit` compiler mode,
//! live `/reload`, and safe `LuaRegistryKey` teardown before `Lua` drop.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use mlua::chunk::Compiler;
use mlua::{Function, Lua, LuaSerdeExt, RegistryKey, Table, Value as LuaValue, VmState};
use serde_json::{Value as JsonValue, json};
use tekvra_config::ResolvedConfig;
use tekvra_core::{HookEvent, HookEventPayload, HookOutcome, PluginToolExecutor};
use tekvra_protocol::{KeymapSpec, ToolOutput};
use tekvra_providers::ToolDefinition;
use tekvra_tools::{SafeWebFetcher, ToolsError};

use crate::LuaHostError;
use crate::manifest::PluginPermissions;

/// Per-dispatch VM instruction budget for hook handlers (and plugin tool/command
/// calls): interrupts fire on Luau instruction boundaries and abort the running
/// call with an error once exhausted, bounding runaway plugins.
const HOOK_INSTRUCTION_BUDGET: u64 = 5_000_000;

/// Wall-clock budget for one `tekvra.net.get` call, including DNS, redirects, and body.
const PLUGIN_NET_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum body size returned by `tekvra.net.get` before truncation.
const PLUGIN_NET_MAX_BODY_BYTES: usize = 64 * 1024;

const PLUGIN_NET_TRUNCATION_MARKER: &str = "\n...[net.get: body truncated at 64 KiB]";

/// VM-wide registries shared by every plugin. Each entry carries the index of the plugin that
/// registered it so a plugin that fails to load can be rolled back without touching the others.
type SharedTools = Arc<Mutex<BTreeMap<String, RegisteredLuaTool>>>;
type SharedCommands = Arc<Mutex<BTreeMap<String, RegisteredLuaCommand>>>;
type SharedKeymaps = Arc<Mutex<Vec<(usize, KeymapSpec)>>>;
type SharedHooks = Arc<Mutex<BTreeMap<&'static str, Vec<(usize, RegistryKey)>>>>;

fn registry_lock<T>(slot: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// CLI/runtime flags controlling [`PluginHost`] initialization.
#[derive(Debug, Clone, Default)]
pub struct LuaHostOptions {
    /// Suppresses all Lua plugin loading for this process (`--no-plugins`), even if `experimental.lua_plugins = true`.
    pub no_plugins: bool,
    /// Disables Luau native JIT codegen and enables full line/stack debug info (`--no-jit`).
    pub no_jit: bool,
    /// Extra hosts/CIDRs/IPs exempt from SSRF filtering for `tekvra.net.get` (same syntax as
    /// `agent.webfetch_allow_private_hosts`). The CLI passes none; tests use it to reach loopback servers.
    pub net_ssrf_allowlist: Vec<String>,
}

/// Custom slash command registered via `tekvra.api.register_command`.
#[derive(Debug, Clone)]
pub struct LuaCommandSpec {
    /// Slash command name (without leading `/`).
    pub name: String,
    /// Description shown in help.
    pub description: String,
}

/// Slash command registered by one plugin through `tekvra.api.register_command`.
struct RegisteredLuaCommand {
    spec: LuaCommandSpec,
    handler_key: RegistryKey,
    /// Index of the registering plugin (see the `Shared*` registry aliases).
    plugin_idx: usize,
}

/// Summary returned on startup or `/reload`.
#[derive(Debug, Clone, Default)]
pub struct PluginLoadReport {
    /// Whether an `mlua::Lua` VM was instantiated.
    pub vm_initialized: bool,
    /// Whether Luau JIT codegen is active on the VM.
    pub jit_enabled: bool,
    /// Loaded plugin names/entrypoints.
    pub loaded_plugins: Vec<String>,
    /// Migration or skip notices emitted when `init.lua` exists while disabled.
    pub notices: Vec<String>,
}

struct RegisteredLuaTool {
    definition: ToolDefinition,
    handler_key: RegistryKey,
    /// Index of the registering plugin (see the `Shared*` registry aliases).
    plugin_idx: usize,
}

struct ActiveVmState {
    /// IMPORTANT DROP INVARIANT: `tool_handlers` and `command_handlers` MUST be cleared via
    /// `lua.remove_registry_value(...)` BEFORE `lua` is dropped so Luau's `lua_close` never sees
    /// dangling registry references or GC-order panics!
    tools: BTreeMap<String, RegisteredLuaTool>,
    commands: BTreeMap<String, RegisteredLuaCommand>,
    keymaps: Vec<(usize, KeymapSpec)>,
    /// Lifecycle hook handler registry keys, keyed by [`HookEvent::as_str`].
    hooks: BTreeMap<&'static str, Vec<(usize, RegistryKey)>>,
    /// Remaining instruction budget consumed by the VM interrupt callback.
    hook_budget: Arc<AtomicU64>,
    jit_enabled: bool,
    lua: Lua,
}

impl Drop for ActiveVmState {
    fn drop(&mut self) {
        for (_, tool) in std::mem::take(&mut self.tools) {
            let _ = self.lua.remove_registry_value(tool.handler_key);
        }
        for (_, cmd) in std::mem::take(&mut self.commands) {
            let _ = self.lua.remove_registry_value(cmd.handler_key);
        }
        for (_, keys) in std::mem::take(&mut self.hooks) {
            for (_, key) in keys {
                let _ = self.lua.remove_registry_value(key);
            }
        }
        let _ = self.lua.gc_collect();
    }
}

/// Thread-safe opt-in Luau plugin host implementing [`PluginToolExecutor`].
#[derive(Clone)]
pub struct PluginHost {
    config: Arc<Mutex<ResolvedConfig>>,
    options: LuaHostOptions,
    /// SSRF-safe fetcher backing `tekvra.net.get` (strict policy unless the options allowlist says otherwise).
    net_fetcher: SafeWebFetcher,
    vm: Arc<Mutex<Option<ActiveVmState>>>,
}

impl PluginHost {
    /// Initializes the plugin host according to `config.lua_plugins_active()` and `options`.
    /// When disabled (the default!), **zero** `mlua::Lua` VM is allocated!
    ///
    /// # Errors
    /// Returns [`LuaHostError`] only when the Luau VM itself cannot be built; individual plugin
    /// failures are reported as notices.
    pub fn initialize(
        config: ResolvedConfig,
        options: LuaHostOptions,
    ) -> Result<(Self, PluginLoadReport), LuaHostError> {
        let net_fetcher = SafeWebFetcher::new(&options.net_ssrf_allowlist);
        let host = Self {
            config: Arc::new(Mutex::new(config)),
            options,
            net_fetcher,
            vm: Arc::new(Mutex::new(None)),
        };
        let report = host.reload()?;
        Ok((host, report))
    }

    /// Returns `true` if a Luau VM is currently instantiated in memory.
    #[must_use]
    pub fn is_vm_initialized(&self) -> bool {
        self.vm
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Returns `true` if an active Luau VM currently has native JIT codegen enabled.
    #[must_use]
    pub fn is_jit_enabled(&self) -> bool {
        self.vm
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|s| s.jit_enabled)
    }

    /// Lists tool definitions registered by active Luau plugins (empty when disabled).
    #[must_use]
    pub fn registered_tools(&self) -> Vec<ToolDefinition> {
        let guard = self
            .vm
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(state) = guard.as_ref() else {
            return Vec::new();
        };
        state.tools.values().map(|t| t.definition.clone()).collect()
    }

    /// Lists slash commands registered by active Luau plugins.
    #[must_use]
    pub fn registered_commands(&self) -> Vec<LuaCommandSpec> {
        let guard = self
            .vm
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(state) = guard.as_ref() else {
            return Vec::new();
        };
        state
            .commands
            .values()
            .map(|cmd| cmd.spec.clone())
            .collect()
    }

    /// Returns `true` if an active Luau plugin registered a slash command named `name`.
    #[must_use]
    pub fn has_command(&self, name: &str) -> bool {
        let guard = self
            .vm
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .as_ref()
            .is_some_and(|state| state.commands.contains_key(name))
    }

    /// Lists keymaps registered by active Luau plugins.
    #[must_use]
    pub fn registered_keymaps(&self) -> Vec<KeymapSpec> {
        let guard = self
            .vm
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.as_ref().map_or_else(Vec::new, |s| {
            s.keymaps.iter().map(|(_, spec)| spec.clone()).collect()
        })
    }

    /// Invokes a plugin-registered slash command by name.
    ///
    /// # Errors
    /// Returns [`LuaHostError`] if the command is unknown or throws a Luau runtime error.
    pub fn invoke_command(&self, name: &str, args: &str) -> Result<String, LuaHostError> {
        let guard = self
            .vm
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(state) = guard.as_ref() else {
            return Err(LuaHostError::Lua(mlua::Error::RuntimeError(
                "lua plugins are disabled".to_string(),
            )));
        };
        let Some(cmd) = state.commands.get(name) else {
            return Err(LuaHostError::Lua(mlua::Error::RuntimeError(format!(
                "unknown plugin command `{name}`"
            ))));
        };
        let func: Function = state.lua.registry_value(&cmd.handler_key)?;
        let out: LuaValue = func.call(args)?;
        Ok(match out {
            LuaValue::String(s) => s.to_str()?.to_string(),
            LuaValue::Nil => String::new(),
            other => format!("{other:?}"),
        })
    }

    /// Rebuilds the Luau VM and re-evaluates all eligible plugins (`/reload`), tearing down any
    /// previous VM deterministically first.
    ///
    /// # Errors
    /// Returns [`LuaHostError`] only when the Luau VM itself cannot be built; individual plugin
    /// failures are reported as notices.
    pub fn reload(&self) -> Result<PluginLoadReport, LuaHostError> {
        let cfg = self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();

        // Tear down existing VM cleanly first
        {
            let mut slot = self
                .vm
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = None;
        }

        let mut report = PluginLoadReport::default();

        if self.options.no_plugins || !cfg.lua_plugins_active() {
            // Zero VM initialization! Emit helpful migration notices if `init.lua` exists.
            let global_init = cfg.paths.global_config_dir.join("init.lua");
            let proj_init = cfg.project_root.join(".tekvra").join("init.lua");
            for candidate in [global_init, proj_init] {
                if candidate.is_file() {
                    report.notices.push(format!(
                        "Ignored `{}` because Lua plugins are disabled (`experimental.lua_plugins = false` or `--no-plugins`). Move static settings to `tekvra.toml` or set `experimental.lua_plugins = true` in global `tekvra.toml`.",
                        candidate.display()
                    ));
                }
            }
            return Ok(report);
        }

        let lua = Lua::new();
        let jit_enabled = !self.options.no_jit;
        lua.enable_jit(jit_enabled);
        let compiler = if self.options.no_jit {
            Compiler::new().set_optimization_level(1).set_debug_level(2)
        } else {
            Compiler::new().set_optimization_level(2).set_debug_level(1)
        };
        lua.set_compiler(compiler.clone());
        lua.sandbox(true)?;

        // Instruction budget enforced via the Luau interrupt callback: the shared
        // counter is re-armed before every plugin call; exhaustion aborts the call.
        let hook_budget = Arc::new(AtomicU64::new(HOOK_INSTRUCTION_BUDGET));
        {
            let budget_for_interrupt = Arc::clone(&hook_budget);
            lua.set_interrupt(move |_| {
                match budget_for_interrupt
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                {
                    Ok(_) => Ok(VmState::Continue),
                    Err(_) => Err(mlua::Error::RuntimeError(
                        "plugin instruction budget exceeded".to_string(),
                    )),
                }
            });
        }

        let shared_tools: SharedTools = Arc::new(Mutex::new(BTreeMap::new()));
        let shared_cmds: SharedCommands = Arc::new(Mutex::new(BTreeMap::new()));
        let shared_keys: SharedKeymaps = Arc::new(Mutex::new(Vec::new()));
        let shared_hooks: SharedHooks = Arc::new(Mutex::new(BTreeMap::new()));

        for (plugin_idx, target) in discover_plugin_targets(&cfg).into_iter().enumerate() {
            // Each plugin loads in isolation: any failure (manifest, host table, `init.lua`)
            // rolls back only that plugin's registrations and surfaces as a notice, so one
            // broken plugin never takes the others down.
            let loaded = (|| -> Result<(), LuaHostError> {
                let perms = PluginPermissions::load_for_dir(
                    &target.name,
                    &target.plugin_dir,
                    &cfg.project_root,
                )?;
                let tekvra_tbl = build_plugin_tekvra_table(
                    &lua,
                    &compiler,
                    &target.plugin_dir,
                    plugin_idx,
                    perms,
                    self.net_fetcher.clone(),
                    Arc::clone(&shared_tools),
                    Arc::clone(&shared_cmds),
                    Arc::clone(&shared_keys),
                    Arc::clone(&shared_hooks),
                )?;
                let env = create_plugin_env(&lua, &compiler, &target.plugin_dir, tekvra_tbl)?;
                let code =
                    fs::read_to_string(&target.entry_file).map_err(|source| LuaHostError::Io {
                        path: target.entry_file.clone(),
                        source,
                    })?;
                lua.load(&code)
                    .set_name(format!("@{}", target.entry_file.display()))
                    .set_compiler(compiler.clone())
                    .set_environment(env)
                    .exec()?;
                Ok(())
            })();
            match loaded {
                Ok(()) => report.loaded_plugins.push(target.name),
                Err(err) => {
                    rollback_plugin_registrations(
                        &lua,
                        plugin_idx,
                        &shared_tools,
                        &shared_cmds,
                        &shared_keys,
                        &shared_hooks,
                    );
                    report.notices.push(format!(
                        "Failed to load plugin `{}`: {err} (its registrations were discarded)",
                        target.name
                    ));
                }
            }
        }

        let tools = std::mem::take(
            &mut *shared_tools
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let commands = std::mem::take(
            &mut *shared_cmds
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let keymaps = std::mem::take(
            &mut *shared_keys
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let hooks = std::mem::take(
            &mut *shared_hooks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );

        report.vm_initialized = true;
        report.jit_enabled = jit_enabled;

        let mut slot = self
            .vm
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(ActiveVmState {
            tools,
            commands,
            keymaps,
            hooks,
            hook_budget,
            jit_enabled,
            lua,
        });

        Ok(report)
    }
}

#[async_trait]
impl PluginToolExecutor for PluginHost {
    async fn execute_plugin_tool(
        &self,
        tool_name: &str,
        arguments: &JsonValue,
    ) -> Result<ToolOutput, String> {
        let guard = self
            .vm
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(state) = guard.as_ref() else {
            return Err("lua plugins are disabled".to_string());
        };
        let Some(reg_tool) = state.tools.get(tool_name) else {
            return Err(format!("unknown plugin tool `{tool_name}`"));
        };
        let func: Function = state
            .lua
            .registry_value(&reg_tool.handler_key)
            .map_err(|e| e.to_string())?;
        let lua_args = state.lua.to_value(arguments).map_err(|e| e.to_string())?;
        state
            .hook_budget
            .store(HOOK_INSTRUCTION_BUDGET, Ordering::Relaxed);
        let res: LuaValue = func.call(lua_args).map_err(|e| e.to_string())?;
        let text = match res {
            LuaValue::String(s) => s.to_str().map_err(|e| e.to_string())?.to_string(),
            other => {
                let j: JsonValue = state.lua.from_value(other).unwrap_or(JsonValue::Null);
                serde_json::to_string_pretty(&j).unwrap_or_default()
            }
        };
        Ok(ToolOutput::Text { text })
    }

    fn dispatch_hook(&self, event: HookEvent, payload: HookEventPayload) -> HookOutcome {
        let mut outcome = HookOutcome::default();
        let guard = self
            .vm
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(state) = guard.as_ref() else {
            return outcome;
        };
        // Registry keys are not `Clone`; iterate in place while holding the VM
        // lock (same discipline as `execute_plugin_tool`).
        let handler_count = match state.hooks.get(event.as_str()) {
            Some(keys) => keys.len(),
            None => return outcome,
        };
        if handler_count == 0 {
            return outcome;
        }

        let payload_json = json!({
            "turn_id": payload.turn_id,
            "status": payload.status,
            "model": payload.model,
            "tool": payload.tool,
            "arguments": payload.arguments,
            "call_id": payload.call_id,
        });
        let payload_value = match state.lua.to_value(&payload_json) {
            Ok(v) => v,
            Err(err) => {
                outcome
                    .failures
                    .push(format!("hook payload encode failed: {err}"));
                return outcome;
            }
        };

        state
            .hook_budget
            .store(HOOK_INSTRUCTION_BUDGET, Ordering::Relaxed);
        for idx in 0..handler_count {
            let func: Function = match state
                .hooks
                .get(event.as_str())
                .and_then(|keys| keys.get(idx))
                .map(|(_, key)| state.lua.registry_value(key))
            {
                Some(Ok(f)) => f,
                Some(Err(err)) => {
                    outcome
                        .failures
                        .push(format!("handler lookup failed: {err}"));
                    continue;
                }
                None => break,
            };
            match func.call::<LuaValue>(payload_value.clone()) {
                Ok(LuaValue::Nil) => {}
                Ok(LuaValue::String(note)) => match note.to_str() {
                    Ok(text) => outcome.notes.push(text.to_string()),
                    Err(err) => outcome.failures.push(format!("note decode failed: {err}")),
                },
                Ok(LuaValue::Table(result)) => {
                    let decision: Option<String> = result.get("decision").ok();
                    if decision.as_deref() == Some("deny") {
                        let reason: String = result.get("reason").unwrap_or_default();
                        if reason.is_empty() {
                            outcome
                                .failures
                                .push("deny hook returned an empty `reason`".to_string());
                        } else {
                            outcome.denied = Some(reason);
                        }
                        break;
                    }
                }
                Ok(other) => {
                    let json_val: JsonValue =
                        state.lua.from_value(other).unwrap_or(JsonValue::Null);
                    outcome
                        .notes
                        .push(serde_json::to_string(&json_val).unwrap_or_default());
                }
                Err(err) => outcome.failures.push(err.to_string()),
            }
        }
        outcome
    }
}

struct DiscoveredPlugin {
    name: String,
    plugin_dir: PathBuf,
    entry_file: PathBuf,
}

fn discover_plugin_targets(cfg: &ResolvedConfig) -> Vec<DiscoveredPlugin> {
    let mut out = Vec::new();

    // 1. Subdirectory plugins `<global_config_dir>/lua/<name>/init.lua`
    let global_lua_root = cfg.paths.global_config_dir.join("lua");
    collect_lua_subdirs(&global_lua_root, &mut out);

    // 2. Global `init.lua`
    let global_init = cfg.paths.global_config_dir.join("init.lua");
    if global_init.is_file() {
        out.push(DiscoveredPlugin {
            name: "global-init".to_string(),
            plugin_dir: cfg.paths.global_config_dir.clone(),
            entry_file: global_init,
        });
    }

    // 3. Trusted project `.tekvra/lua/<name>/init.lua` and `.tekvra/init.lua`
    if cfg.project_trusted {
        let proj_cfg = cfg.project_root.join(".tekvra");
        collect_lua_subdirs(&proj_cfg.join("lua"), &mut out);
        let proj_init = proj_cfg.join("init.lua");
        if proj_init.is_file() {
            out.push(DiscoveredPlugin {
                name: "project-init".to_string(),
                plugin_dir: proj_cfg,
                entry_file: proj_init,
            });
        }
    }

    out
}

fn collect_lua_subdirs(lua_root: &Path, out: &mut Vec<DiscoveredPlugin>) {
    let Ok(entries) = fs::read_dir(lua_root) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let init = dir.join("init.lua");
        if init.is_file() {
            let name = dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("plugin")
                .to_string();
            out.push(DiscoveredPlugin {
                name,
                plugin_dir: dir,
                entry_file: init,
            });
        }
    }
}

fn create_plugin_env(
    lua: &Lua,
    compiler: &Compiler,
    plugin_dir: &Path,
    tekvra_tbl: Table,
) -> mlua::Result<Table> {
    let env = lua.create_table()?;
    let globals = lua.globals();
    let meta = lua.create_table()?;
    meta.set("__index", globals)?;
    env.set_metatable(Some(meta))?;

    env.set("tekvra", tekvra_tbl)?;

    // Sandboxed `require(modname)` confined strictly inside `plugin_dir`!
    let root = fs::canonicalize(plugin_dir).unwrap_or_else(|_| plugin_dir.to_path_buf());
    let comp = compiler.clone();
    let env_weak = env.clone();
    let require_fn = lua.create_function(move |lua_ctx, mod_name: String| {
        if mod_name.contains("..") {
            return Err(mlua::Error::RuntimeError(format!(
                "sandboxed `require` rejects parent traversal in `{mod_name}`"
            )));
        }
        let rel_str = mod_name.replace('.', "/");
        let candidate = root.join(format!("{rel_str}.lua"));
        let mut clean = PathBuf::new();
        for c in candidate.components() {
            match c {
                Component::ParentDir => {
                    return Err(mlua::Error::RuntimeError(
                        "sandboxed `require` blocked `..` segment".to_string(),
                    ));
                }
                Component::CurDir => {}
                other => clean.push(other.as_os_str()),
            }
        }
        if !clean.starts_with(&root) || !clean.is_file() {
            return Err(mlua::Error::RuntimeError(format!(
                "module `{mod_name}` not found inside `{}`",
                root.display()
            )));
        }
        let src = fs::read_to_string(&clean).map_err(|e| {
            mlua::Error::RuntimeError(format!("failed reading `{}`: {e}", clean.display()))
        })?;
        lua_ctx
            .load(&src)
            .set_name(format!("@{}", clean.display()))
            .set_compiler(comp.clone())
            .set_environment(env_weak.clone())
            .eval::<LuaValue>()
    })?;
    env.set("require", require_fn)?;

    Ok(env)
}

/// Runs one `tekvra.net.get` fetch on a dedicated thread with a private current-thread runtime:
/// plugin callbacks are synchronous and may run on a Tokio worker thread, where `block_on` is
/// forbidden. Returns the fetcher's text form with the body capped, or the error message.
fn net_get_blocking(
    fetcher: &SafeWebFetcher,
    url: &str,
    hop_check: &(dyn Fn(&str) -> Result<(), ToolsError> + Send + Sync),
) -> Result<String, String> {
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                    .map_err(|e| format!("net.get failed to start its runtime: {e}"))?;
                let fetch = fetcher.fetch_with_hop_check(url, hop_check);
                // The timer needs a runtime clock: build it inside `block_on`.
                match rt.block_on(async { tokio::time::timeout(PLUGIN_NET_TIMEOUT, fetch).await }) {
                    Err(_elapsed) => Err(format!(
                        "net.get timed out after {}s",
                        PLUGIN_NET_TIMEOUT.as_secs()
                    )),
                    Ok(Err(err)) => Err(err.to_string()),
                    Ok(Ok(output)) => Ok(cap_net_body(output.to_model_text())),
                }
            })
            .join()
            .map_err(|_| "net.get worker thread panicked".to_string())?
    })
}

/// Caps the `<body>` half of the fetcher's `"HTTP <status> <url>\n\n<body>"` text at
/// [`PLUGIN_NET_MAX_BODY_BYTES`], leaving the status line intact.
fn cap_net_body(text: String) -> String {
    if text.len() <= PLUGIN_NET_MAX_BODY_BYTES {
        return text;
    }
    match text.split_once("\n\n") {
        Some((head, body)) => format!("{head}\n\n{}", truncate_net_text(body)),
        None => truncate_net_text(&text),
    }
}

fn truncate_net_text(text: &str) -> String {
    if text.len() <= PLUGIN_NET_MAX_BODY_BYTES {
        return text.to_string();
    }
    let mut cut = PLUGIN_NET_MAX_BODY_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{PLUGIN_NET_TRUNCATION_MARKER}", &text[..cut])
}

/// Removes every registration made by plugin `plugin_idx` from the shared registries and
/// releases its `RegistryKey`s, so a plugin that failed to load leaves nothing behind.
fn rollback_plugin_registrations(
    lua: &Lua,
    plugin_idx: usize,
    tools: &SharedTools,
    commands: &SharedCommands,
    keymaps: &SharedKeymaps,
    hooks: &SharedHooks,
) {
    {
        let mut tools = registry_lock(tools);
        let names: Vec<String> = tools
            .iter()
            .filter(|(_, tool)| tool.plugin_idx == plugin_idx)
            .map(|(name, _)| name.clone())
            .collect();
        for name in names {
            if let Some(tool) = tools.remove(&name) {
                let _ = lua.remove_registry_value(tool.handler_key);
            }
        }
    }
    {
        let mut commands = registry_lock(commands);
        let names: Vec<String> = commands
            .iter()
            .filter(|(_, cmd)| cmd.plugin_idx == plugin_idx)
            .map(|(name, _)| name.clone())
            .collect();
        for name in names {
            if let Some(cmd) = commands.remove(&name) {
                let _ = lua.remove_registry_value(cmd.handler_key);
            }
        }
    }
    registry_lock(keymaps).retain(|(idx, _)| *idx != plugin_idx);
    let mut hooks = registry_lock(hooks);
    for keys in hooks.values_mut() {
        let mut i = 0;
        while i < keys.len() {
            if keys[i].0 == plugin_idx {
                let (_, key) = keys.swap_remove(i);
                let _ = lua.remove_registry_value(key);
            } else {
                i += 1;
            }
        }
    }
    hooks.retain(|_, keys| !keys.is_empty());
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn build_plugin_tekvra_table(
    lua: &Lua,
    _compiler: &Compiler,
    _plugin_dir: &Path,
    plugin_idx: usize,
    perms: PluginPermissions,
    net_fetcher: SafeWebFetcher,
    tools: SharedTools,
    commands: SharedCommands,
    keymaps: SharedKeymaps,
    hooks: SharedHooks,
) -> mlua::Result<Table> {
    let tekvra = lua.create_table()?;

    // 1. `tekvra.api.register_tool` & `tekvra.api.register_command`
    let api = lua.create_table()?;
    {
        let tools_ref = Arc::clone(&tools);
        api.set(
            "register_tool",
            lua.create_function(move |lua_ctx, spec: Table| {
                let name: String = spec.get("name")?;
                let description: String = spec.get("description").unwrap_or_default();
                let params_val: LuaValue = spec.get("parameters").unwrap_or(LuaValue::Nil);
                let parameters: JsonValue = match params_val {
                    LuaValue::Nil => json!({ "type": "object", "properties": {} }),
                    other => lua_ctx.from_value(other)?,
                };
                let handler: Function = spec.get("handler")?;
                let key = lua_ctx.create_registry_value(handler)?;
                let mut map = tools_ref
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(prev) = map.remove(&name) {
                    let _ = lua_ctx.remove_registry_value(prev.handler_key);
                }
                map.insert(
                    name.clone(),
                    RegisteredLuaTool {
                        definition: ToolDefinition {
                            name,
                            description,
                            parameters,
                        },
                        handler_key: key,
                        plugin_idx,
                    },
                );
                Ok(())
            })?,
        )?;
    }
    {
        let cmds_ref = Arc::clone(&commands);
        api.set(
            "register_command",
            lua.create_function(move |lua_ctx, spec: Table| {
                let name: String = spec.get("name")?;
                let description: String = spec.get("description").unwrap_or_default();
                let handler: Function = spec.get("handler")?;
                let key = lua_ctx.create_registry_value(handler)?;
                let mut map = cmds_ref
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(prev) = map.remove(&name) {
                    let _ = lua_ctx.remove_registry_value(prev.handler_key);
                }
                map.insert(
                    name.clone(),
                    RegisteredLuaCommand {
                        spec: LuaCommandSpec { name, description },
                        handler_key: key,
                        plugin_idx,
                    },
                );
                Ok(())
            })?,
        )?;
    }
    {
        let hooks_ref = Arc::clone(&hooks);
        api.set(
            "register_hook",
            lua.create_function(move |lua_ctx, (event, handler): (String, Function)| {
                let Some(hook_event) = HookEvent::parse(&event) else {
                    return Err(mlua::Error::RuntimeError(format!(
                        "unknown hook event `{event}`; expected one of \
                             session_start, turn_started, turn_completed, \
                             pre_tool_use, post_tool_use"
                    )));
                };
                let key = lua_ctx.create_registry_value(handler)?;
                hooks_ref
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .entry(hook_event.as_str())
                    .or_default()
                    .push((plugin_idx, key));
                Ok(())
            })?,
        )?;
    }
    tekvra.set("api", api)?;

    // 2. `tekvra.keymap.set(mode, lhs, command)` — validated here so a bad binding fails that
    //    plugin's load instead of silently registering something the TUI can never match.
    let keymap = lua.create_table()?;
    {
        let keys_ref = Arc::clone(&keymaps);
        keymap.set(
            "set",
            lua.create_function(move |_, (mode, lhs, command): (String, String, String)| {
                let spec =
                    KeymapSpec::parse(&mode, &lhs, &command).map_err(mlua::Error::RuntimeError)?;
                keys_ref
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((plugin_idx, spec));
                Ok(())
            })?,
        )?;
    }
    tekvra.set("keymap", keymap)?;

    // 3. `tekvra.fs.{read_text, read_bytes, write_text}` gated by `plugin.toml` `fs_read` / `fs_write`!
    let fs_tbl = lua.create_table()?;
    {
        let p = perms.clone();
        fs_tbl.set(
            "read_text",
            lua.create_function(move |_, path: String| {
                let target = p.check_fs_read(Path::new(&path))?;
                fs::read_to_string(&target).map_err(|e| {
                    mlua::Error::RuntimeError(format!(
                        "read_text `{}` failed: {e}",
                        target.display()
                    ))
                })
            })?,
        )?;
    }
    {
        let p = perms.clone();
        fs_tbl.set(
            "read_bytes",
            lua.create_function(move |lua_ctx, path: String| {
                let target = p.check_fs_read(Path::new(&path))?;
                let bytes = fs::read(&target).map_err(|e| {
                    mlua::Error::RuntimeError(format!(
                        "read_bytes `{}` failed: {e}",
                        target.display()
                    ))
                })?;
                lua_ctx.create_string(&bytes)
            })?,
        )?;
    }
    {
        let p = perms.clone();
        fs_tbl.set(
            "write_text",
            lua.create_function(move |_, (path, content): (String, String)| {
                let target = p.check_fs_write(Path::new(&path))?;
                if let Some(parent) = target.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                fs::write(&target, content.as_bytes()).map_err(|e| {
                    mlua::Error::RuntimeError(format!(
                        "write_text `{}` failed: {e}",
                        target.display()
                    ))
                })
            })?,
        )?;
    }
    tekvra.set("fs", fs_tbl)?;

    // 4. `tekvra.env.get(var)` gated by `plugin.toml` `env`!
    let env_tbl = lua.create_table()?;
    {
        let p = perms.clone();
        env_tbl.set(
            "get",
            lua.create_function(move |_, var_name: String| {
                p.check_env(&var_name)?;
                Ok(std::env::var(&var_name).ok())
            })?,
        )?;
    }
    tekvra.set("env", env_tbl)?;

    // 5. `tekvra.run.exec(program, args)` gated by `plugin.toml` `run`!
    let run_tbl = lua.create_table()?;
    {
        let p = perms.clone();
        run_tbl.set(
            "exec",
            lua.create_function(move |_, (program, args): (String, Option<Vec<String>>)| {
                p.check_run(&program)?;
                let out = Command::new(&program)
                    .args(args.unwrap_or_default())
                    .output()
                    .map_err(|e| {
                        mlua::Error::RuntimeError(format!("exec `{program}` failed: {e}"))
                    })?;
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            })?,
        )?;
    }
    tekvra.set("run", run_tbl)?;

    // 6. `tekvra.net.get(url)` gated by `plugin.toml` `net`: a real HTTP(S) GET through the
    //    SSRF-safe fetcher, with the grant re-checked on every redirect hop.
    let net_tbl = lua.create_table()?;
    {
        let p = perms;
        net_tbl.set(
            "get",
            lua.create_function(move |_, url_str: String| {
                p.check_net(&url_str)?;
                net_get_blocking(&net_fetcher, &url_str, &|host| {
                    p.check_redirect_host(host)
                        .map_err(|message| ToolsError::PermissionDenied { message })
                })
                .map_err(mlua::Error::RuntimeError)
            })?,
        )?;
    }
    tekvra.set("net", net_tbl)?;

    Ok(tekvra)
}

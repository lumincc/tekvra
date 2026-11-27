//! Sandboxed Luau VM lifecycle, `tekvra.*` host bindings, `--no-jit` compiler mode,
//! live `/reload`, and safe `LuaRegistryKey` teardown before `Lua` drop.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mlua::chunk::Compiler;
use mlua::{Function, Lua, LuaSerdeExt, RegistryKey, Table, Value as LuaValue};
use serde_json::{Value as JsonValue, json};
use tekvra_config::ResolvedConfig;
use tekvra_core::PluginToolExecutor;
use tekvra_protocol::ToolOutput;
use tekvra_providers::ToolDefinition;

use crate::LuaHostError;
use crate::manifest::PluginPermissions;

/// CLI/runtime flags controlling [`PluginHost`] initialization.
#[derive(Debug, Clone, Copy, Default)]
pub struct LuaHostOptions {
    /// Suppresses all Lua plugin loading for this process (`--no-plugins`), even if `experimental.lua_plugins = true`.
    pub no_plugins: bool,
    /// Disables Luau native JIT codegen and enables full line/stack debug info (`--no-jit`).
    pub no_jit: bool,
}

/// Custom slash command registered via `tekvra.api.register_command`.
#[derive(Debug, Clone)]
pub struct LuaCommandSpec {
    /// Slash command name (without leading `/`).
    pub name: String,
    /// Description shown in help.
    pub description: String,
}

/// Custom keybinding registered via `tekvra.keymap.set`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaKeymapSpec {
    /// Mode (`"n"`, `"i"`, `"*"`).
    pub mode: String,
    /// Key chord string (e.g., `"<C-g>"`).
    pub lhs: String,
    /// Action description or command string.
    pub description: String,
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
}

struct ActiveVmState {
    /// IMPORTANT DROP INVARIANT: `tool_handlers` and `command_handlers` MUST be cleared via
    /// `lua.remove_registry_value(...)` BEFORE `lua` is dropped so Luau's `lua_close` never sees
    /// dangling registry references or GC-order panics!
    tools: BTreeMap<String, RegisteredLuaTool>,
    commands: BTreeMap<String, (LuaCommandSpec, RegistryKey)>,
    keymaps: Vec<LuaKeymapSpec>,
    jit_enabled: bool,
    lua: Lua,
}

impl Drop for ActiveVmState {
    fn drop(&mut self) {
        for (_, tool) in std::mem::take(&mut self.tools) {
            let _ = self.lua.remove_registry_value(tool.handler_key);
        }
        for (_, (_, key)) in std::mem::take(&mut self.commands) {
            let _ = self.lua.remove_registry_value(key);
        }
        let _ = self.lua.gc_collect();
    }
}

/// Thread-safe opt-in Luau plugin host implementing [`PluginToolExecutor`].
#[derive(Clone)]
pub struct PluginHost {
    config: Arc<Mutex<ResolvedConfig>>,
    options: LuaHostOptions,
    vm: Arc<Mutex<Option<ActiveVmState>>>,
}

impl PluginHost {
    /// Initializes the plugin host according to `config.lua_plugins_active()` and `options`.
    /// When disabled (the default!), **zero** `mlua::Lua` VM is allocated!
    ///
    /// # Errors
    /// Returns [`LuaHostError`] if `experimental.lua_plugins` is active and a plugin fails to load.
    pub fn initialize(
        config: ResolvedConfig,
        options: LuaHostOptions,
    ) -> Result<(Self, PluginLoadReport), LuaHostError> {
        let host = Self {
            config: Arc::new(Mutex::new(config)),
            options,
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
        state.commands.values().map(|(s, _)| s.clone()).collect()
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
    pub fn registered_keymaps(&self) -> Vec<LuaKeymapSpec> {
        let guard = self
            .vm
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.as_ref().map_or_else(Vec::new, |s| s.keymaps.clone())
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
        let Some((_, key)) = state.commands.get(name) else {
            return Err(LuaHostError::Lua(mlua::Error::RuntimeError(format!(
                "unknown plugin command `{name}`"
            ))));
        };
        let func: Function = state.lua.registry_value(key)?;
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
    /// Returns [`LuaHostError`] if plugin manifest parsing or Luau execution fails.
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

        let shared_tools: Arc<Mutex<BTreeMap<String, RegisteredLuaTool>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let shared_cmds: Arc<Mutex<BTreeMap<String, (LuaCommandSpec, RegistryKey)>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let shared_keys: Arc<Mutex<Vec<LuaKeymapSpec>>> = Arc::new(Mutex::new(Vec::new()));

        let discovered = discover_plugin_targets(&cfg);
        for target in discovered {
            let perms = PluginPermissions::load_for_dir(
                &target.name,
                &target.plugin_dir,
                &cfg.project_root,
            )?;
            let tekvra_tbl = build_plugin_tekvra_table(
                &lua,
                &compiler,
                &target.plugin_dir,
                perms,
                Arc::clone(&shared_tools),
                Arc::clone(&shared_cmds),
                Arc::clone(&shared_keys),
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

            report.loaded_plugins.push(target.name);
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

#[allow(clippy::too_many_lines)]
fn build_plugin_tekvra_table(
    lua: &Lua,
    _compiler: &Compiler,
    _plugin_dir: &Path,
    perms: PluginPermissions,
    tools: Arc<Mutex<BTreeMap<String, RegisteredLuaTool>>>,
    commands: Arc<Mutex<BTreeMap<String, (LuaCommandSpec, RegistryKey)>>>,
    keymaps: Arc<Mutex<Vec<LuaKeymapSpec>>>,
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
                if let Some((_, prev_key)) = map.remove(&name) {
                    let _ = lua_ctx.remove_registry_value(prev_key);
                }
                map.insert(name.clone(), (LuaCommandSpec { name, description }, key));
                Ok(())
            })?,
        )?;
    }
    tekvra.set("api", api)?;

    // 2. `tekvra.keymap.set(mode, lhs, desc)`
    let keymap = lua.create_table()?;
    {
        let keys_ref = Arc::clone(&keymaps);
        keymap.set(
            "set",
            lua.create_function(move |_, (mode, lhs, desc): (String, String, String)| {
                keys_ref
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(LuaKeymapSpec {
                        mode,
                        lhs,
                        description: desc,
                    });
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

    // 6. `tekvra.net.check_url(url)` / `tekvra.net.get(url)` gated by `plugin.toml` `net`!
    let net_tbl = lua.create_table()?;
    {
        let p = perms;
        net_tbl.set(
            "get",
            lua.create_function(move |_, url_str: String| {
                p.check_net(&url_str)?;
                Ok(format!("authorized:{url_str}"))
            })?,
        )?;
    }
    tekvra.set("net", net_tbl)?;

    Ok(tekvra)
}

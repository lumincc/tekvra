# Tekvra Opt-In Luau Plugin System (`tekvra-lua`)

Tekvra executes **100% of its built-in tools in native Rust** (`crates/tekvra-tools`). By default, **zero Lua VM is initialized** at startup (`experimental.lua_plugins = false`), guaranteeing fast cold starts and zero accidental script execution.

## Enabling External Luau Plugins

External plugins are an explicit global opt-in in `~/.config/tekvra/tekvra.toml`:

```toml
version = 2

[experimental]
lua_plugins = true
```

> Project-level `.tekvra/tekvra.toml` files **cannot** enable `experimental.lua_plugins`, and untrusted project `.tekvra/lua/` scripts are withheld until folder trust is granted.

## Plugin Discovery & Sandboxed `require()`

When `experimental.lua_plugins = true` (and `--no-plugins` is not passed):
1. Global plugins in `~/.config/tekvra/lua/<plugin>/init.lua` and `~/.config/tekvra/init.lua` are loaded.
2. Trusted project plugins in `<repo>/.tekvra/lua/<plugin>/init.lua` and `<repo>/.tekvra/init.lua` are loaded.
3. `require("submodule")` is sandboxed strictly inside the plugin's own directory, rejecting `..` parent traversal.

## Per-Plugin `plugin.toml` Capability Grants

Every plugin runs under **default-deny** host capabilities unless `<plugin_dir>/plugin.toml` declares explicit grants:

```toml
name = "release-helper"
version = "0.1.0"

[permissions]
fs_read = ["Cargo.toml", "crates/**/Cargo.toml"]
fs_write = false
run = ["git"]
net = ["crates.io"]
env = ["CI"]
```

| Capability | Host APIs Gated | Default |
|---|---|---|
| `fs_read` | `tekvra.fs.read_text(path)`, `tekvra.fs.read_bytes(path)` | `false` |
| `fs_write` | `tekvra.fs.write_text(path, text)` | `false` |
| `run` | `tekvra.run.exec(program, args)` | `false` |
| `net` | `tekvra.net.get(url)` | `false` |
| `env` | `tekvra.env.get(var)` | `false` |

## Host Registration Surface (`tekvra.*`)

- `tekvra.api.register_tool({ name, description, parameters, handler })`
- `tekvra.api.register_command({ name, description, handler })`
- `tekvra.api.register_hook(event, handler)` — lifecycle hooks; unknown event names are rejected at registration.
- `tekvra.keymap.set(mode, lhs, description)`

## Lifecycle Hooks

`register_hook` accepts the events `session_start`, `turn_started`, `turn_completed`, `pre_tool_use`, and `post_tool_use` (registration order preserved per event). Handlers receive a payload table (`turn_id`, `status`, `model`; or `tool`, `arguments`, `call_id` for the tool events) and may return:

- `nil` (no-op), a **string** (surfaced in the transcript as a `plugin` steering note), or
- for `pre_tool_use` only, a table `{ decision = "deny", reason = "..." }` — the tool call is then blocked before any permission prompt or side effect, and the model receives `denied by plugin: <reason>` as its tool result.

Semantics:

- **Failure isolation**: a handler that errors, panics, or exceeds its per-dispatch instruction budget (5M VM instructions) is detached for that invocation; the turn continues and the transcript shows a single `plugin_error` warning naming the failure.
- **Frame safety**: hook output only ever lands in rollout steering notes and `SteeringInjected` transcript events — never inside the append-only prompt-cache frame.
- Example — deny destructive shells and annotate writes:

```lua
tekvra.api.register_hook("pre_tool_use", function(payload)
    if payload.tool == "exec_command" and payload.arguments:find("rm %-rf") then
        return { decision = "deny", reason = "rm -rf is not allowed by plugin policy" }
    end
    return nil
end)

tekvra.api.register_hook("post_tool_use", function(payload)
    if payload.tool == "file_write" then
        return "wrote " .. payload.arguments
    end
    return nil
end)
```

## CLI Flags & Live Reload

- `--no-plugins`: Disables all Luau plugin loading for the current invocation even when `experimental.lua_plugins = true`.
- `--no-jit`: Switches Luau from native JIT compilation to the bytecode interpreter with full debug symbols (`debug_level = 2`).
- `/reload`: Safely releases all `RegistryKey` handles, runs Luau GC cleanup, drops the previous VM, and reloads all plugins in place.

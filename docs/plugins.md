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
- `tekvra.keymap.set(mode, lhs, description)`

## CLI Flags & Live Reload

- `--no-plugins`: Disables all Luau plugin loading for the current invocation even when `experimental.lua_plugins = true`.
- `--no-jit`: Switches Luau from native JIT compilation to the bytecode interpreter with full debug symbols (`debug_level = 2`).
- `/reload`: Safely releases all `RegistryKey` handles, runs Luau GC cleanup, drops the previous VM, and reloads all plugins in place.

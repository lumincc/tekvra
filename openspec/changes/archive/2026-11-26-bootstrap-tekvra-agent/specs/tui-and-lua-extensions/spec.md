# Full-Screen TUI and Opt-In Lua Extensions Specification

## Purpose

Defines Tekvra's full-screen alternate-screen terminal interface and its Caudra-aligned opt-in external Lua (Luau) plugin runtime, capability manifest permissions, and hot-reload lifecycle.

## ADDED Requirements

### Requirement: Full-screen Alt-Screen TUI with native tool cards
The system SHALL provide a 60 FPS full-screen alternate-screen terminal UI displaying real-time token/cost/cache-hit status, streaming Markdown and diffs, collapsible native tool cards (with Raw/Filtered output toggle), interactive permission scope ladders, subagent task chat switching, session search/picker, and file-revert previews.

#### Scenario: Toggling filtered shell output to raw view in TUI
- **WHEN** a completed shell tool card displays filtered output in the TUI and the user clicks or activates the raw/filtered toggle
- **THEN** the card switches immediately between the reduced model-facing text and the retained unfiltered terminal output without re-running the command or invoking Lua

#### Scenario: Inspecting and steering a running subagent from TUI
- **WHEN** one or more subagents are running and the user opens `/tasks` (`Ctrl-X`)
- **THEN** the user can switch into a subagent's live transcript view, queue `Guide` input or `Replace` its current turn, and return to the main chat

### Requirement: Opt-in Luau plugin runtime with capability manifest (`plugin.toml`)
The system SHALL support external Luau plugins under `<config>/init.lua` + `<config>/lua/*.lua` and trusted `<project>/.tekvra/init.lua` + `<project>/.tekvra/lua/*.lua` ONLY when `experimental.lua_plugins = true` is set in global `tekvra.toml` and `--no-plugins` is not passed, gating all sensitive host APIs (`fs_read`, `fs_write`, `run`, `net`, `env`) behind an explicit `plugin.toml` permission manifest per plugin directory.

#### Scenario: `init.lua` present while `experimental.lua_plugins` is off
- **WHEN** `~/.config/tekvra/init.lua` exists on disk while `experimental.lua_plugins` is `false` (default)
- **THEN** `tekvra` does not start the Lua VM, skips `init.lua`, and emits a non-fatal notice explaining how to migrate settings to `tekvra.toml` or enable `experimental.lua_plugins = true`

#### Scenario: Enabled Lua plugin registers a custom tool without `plugin.toml`
- **WHEN** `experimental.lua_plugins = true` and a plugin in `~/.config/tekvra/lua/custom.lua` registers a tool via `tekvra.api.register_tool` and attempts `tekvra.fs.read_bytes` without `fs_read = true` in `plugin.toml`
- **THEN** the tool registers cleanly, `require()` stays sandboxed to the `lua/` root, and the unauthorized `fs.read_bytes` call returns a permission error without crashing the host

#### Scenario: Hot-reloading Lua plugins and debugging without JIT
- **WHEN** a user edits a Lua plugin and runs `/reload` (or starts `tekvra --no-jit`)
- **THEN** `/reload` rebuilds plugin registrations and keymaps in place without dropping the active conversation, and `--no-jit` executes plugins on the Luau interpreter with full backtrace line information

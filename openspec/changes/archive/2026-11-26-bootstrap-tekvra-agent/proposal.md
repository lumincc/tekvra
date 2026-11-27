# Proposal

## Why

Tekvra aims to deliver a native Rust AI coding agent that combines Codex-grade developer workflows and `app-server` protocol compatibility with minimal memory footprint, 60 FPS full-screen terminal ergonomics, and multi-provider flexibility. Starting from `.tmp/maki` (`v0.7.3`) gives Tekvra a clean, modern pure-`tokio` + `reqwest` + `rustls` foundation (avoiding `.tmp/caudra`'s 1.22M-LOC monolith and dual `smol`/`isahc` + `tokio`/`reqwest` runtime split), while selectively adopting Caudra's proven advances (native Rust tools, `apply_patch`, encrypted Responses reasoning, RTK output filtering, automatic steering, `/goal`, file snapshot revert, declarative TOML config, and opt-in Lua extension boundaries) under Apache-2.0.

## What Changes

- Initialize Tekvra as an Apache-2.0 pure virtual Cargo workspace (`crates/tekvra-*`) with `edition = "2024"`, `resolver = "3"`, and workspace-wide `unsafe_code = "forbid"` (except audited PTY/termios wrappers where required).
- Establish declarative, versioned TOML configuration (`tekvra.toml`, `permissions.toml`, `providers.toml`) with hierarchical `AGENTS.md` instructions, `.agents/skills` discovery, project folder trust, and global-only `[experimental]` feature flags—replacing Maki's mandatory `init.lua` startup configuration.
- Port and unify the multi-provider LLM client on Maki's `tokio` + `reqwest` stack, adding Caudra's OpenAI `/v1/responses` stateless `reasoning.encrypted_content` replay, explicit `DeveloperMessage` prompt-cache breakpoints, `prompt_cache_key` session affinity, ChatGPT/Codex OAuth, multi-account quota rotation, and role-based model routing (`main`, `plan`, `subagent`, `compact`, `review`, `goal`, `fast`).
- Replace Maki's built-in Lua tool plugins with 100% native Rust tools: model-routed `apply_patch` (`*** Begin Patch` for GPT-5+/o-series/Codex) vs `file_edit` (for other models), Codex-style interactive PTY execution (`exec_command` + `write_stdin`), built-in RTK shell output filtering with terminal progress-bar decoding, `tool_output` pagination, tree-sitter `file_index`, isolated Monty Python `code` execution (with MCP gateway), `view_image`, `plan`, `question`, `batch`, and `task`.
- Implement consent-based execution safety without local OS kernel sandboxing (matching Maki and Caudra): tree-sitter Bash AST command decomposition, `Plan` / `Build` / `Auto` / `YOLO` modes, Caudra-style scoped approval ladders (`Once` / `Conversation` / `Project` / `Global`), SSRF-safe web fetching, and folder trust.
- Upgrade the append-only agent runtime and storage layer with Caudra-inspired automatic steering/nudges, `Next` / `Guide` / `Replace` turn-boundary queues, durable steerable subagents, evidence-gated `/goal`, Codex-style `/review` (`tekvra review`), and hybrid session storage (JSONL rollout + SQLite/FTS5 search index + per-call workspace file snapshots for atomic chat+file revert and fork).
- Retain Maki/Caudra's full-screen Alt-Screen `ratatui` TUI architecture (rendering structured Rust `ToolOutput` with zero Lua round-trips) alongside Caudra's opt-in `tekvra-lua` host (`experimental.lua_plugins = true` in global `tekvra.toml` only, `plugin.toml` capability grants, `--no-plugins`, `--no-jit`, `/reload`).
- Add a wire-compatible Codex `app-server` JSON-RPC 2.0 server (`stdio://`, `unix://`, `ws://`) supporting `thread/*`, `turn/*`, `review/start`, `command/exec*`, `model/*`, `config/*`, `account/*`, streaming `item/*` notifications, and server-initiated approval/input requests, alongside `tekvra exec` and `tekvra acp`.

## Capabilities

### New Capabilities
- `workspace-and-config`: Pure virtual Cargo workspace layout, Apache-2.0 licensing/attribution, declarative self-migrating TOML configuration, `AGENTS.md` & skill discovery, project folder trust, and global `[experimental]` feature gates.
- `providers-and-model-routing`: Tokio-native multi-provider LLM client, OpenAI Responses API (`reasoning.encrypted_content`, cache breakpoints, ChatGPT/Codex OAuth), multi-account quota failover, and per-job model routing.
- `native-tools-and-pty`: Native Rust built-in tools including model-routed `apply_patch` vs `file_edit`, interactive PTY `exec_command`/`write_stdin`, built-in RTK output filtering, `tool_output` retention, tree-sitter `file_index`, and Monty `code` MCP mode.
- `permissions-and-safety`: Non-sandboxed consent & authority model using tree-sitter Bash AST parsing, `Plan`/`Build`/`Auto`/`YOLO` modes, scoped resource/command approval ladders, and SSRF protection.
- `agent-core-and-sessions`: Append-only cache-preserving agent loop, automatic steering nudges, `Next`/`Guide`/`Replace` queues, durable steerable subagents, `/goal` evaluation, `/review` workflows, and JSONL + SQLite/FTS5 + file-snapshot reversible sessions.
- `tui-and-lua-extensions`: Full-screen Alt-Screen `ratatui` TUI with native `ToolOutput` cards, approval ladders, subagent windows, and revert previews, plus Caudra-style opt-in `tekvra-lua` external plugin runtime.
- `app-server-and-headless`: Codex-compatible `app-server` v2 JSON-RPC 2.0 protocol (`stdio://`, `unix://`, `ws://`) for desktop/IDE integration, plus `tekvra exec` headless CI runner and `tekvra acp` editor server.

### Modified Capabilities

## Impact

- **Repository & Build**: Introduces root virtual `Cargo.toml`, `NOTICE.md`, `LICENSE` (Apache-2.0), `THIRD_PARTY_LICENSES/` (preserving Maki MIT and Caudra/Workcell/RTK Apache-2.0 notices), and 11 workspace crates under `crates/tekvra-*`.
- **Runtime Dependencies**: Standardizes on `tokio`, `reqwest` (`rustls` + `aws-lc-rs`), `ratatui 0.30`, `crossterm`, `rusqlite` (bundled + FTS5), `tree-sitter` grammars, `portable-pty` / `rustix` PTY primitives, `mlua` (Luau), and `monty`—explicitly excluding Caudra's `smol`, `isahc`, `rhai`, `tla-checker`, `caudra-workbench`, and `e2b-libvirt` VM manager.
- **External Integration**: Enables drop-in compatibility with Codex `app-server` clients (multi-pane desktop hosts and IDE extensions), Zed via `tekvra acp`, and headless CI pipelines via `tekvra exec` / `--print`.

# Technical Design: Phase 3 TUI Autocomplete, Session Picker, Streamable HTTP MCP, and CI/CD

## Context
Phase 1 (`bootstrap-tekvra-agent`) and Phase 2 (`harden-runtime-mcp-oauth-and-tui`) delivered a complete workspace, CLI, Alt-Screen TUI, multi-provider LLM router with OAuth login/refresh, native tools, PTY sessions, Monty `code`, and stdio/HTTP JSON-RPC MCP client gateway. Phase 3 completes interactive TUI discoverability, Streamable HTTP MCP event-stream support, and automated multi-platform CI/CD.

## Goals / Non-Goals
- **Goals**:
  - Provide instant fuzzy completion popups when typing `/` (slash commands + Luau plugin commands) or `@` (workspace relative file paths respecting `.gitignore` and sensitive-file filters) in the TUI composer.
  - Provide an interactive `/sessions` modal listing recent sessions from `StateIndex` (`id`, `title`, `preview`, `updated_at`) to resume or fork in place, alongside `tekvra resume` and `tekvra fork` CLI subcommands.
  - Support MCP Streamable HTTP (`Content-Type: text/event-stream` replies to JSON-RPC `POST`, bearer auth headers, and `notifications/tools/list_changed` catalog refresh) in `ConfiguredMcpGateway`.
  - Establish reproducible GitHub Actions CI (`fmt`, `clippy`, Linux/macOS `test`, Windows `check`, `cargo-deny`, packaging audit, unified `CI` gate) and tagged `v*` Release CD (5 OS/arch targets + `SHA256SUMS.txt`).
- **Non-Goals**:
  - Mutating top-level LLM `ToolDefinition` schemas when MCP servers emit `tools/list_changed` (tool schemas remain strictly static for prompt-cache stability).

## Decisions
1. **Cached Workspace File Index for `@` Mentions**: Scan non-sensitive workspace relative paths via `ignore::WalkBuilder` + `is_sensitive_credential_path` on TUI startup so composer keystrokes never block on disk I/O and never expose `.git` or `.env*` secret paths.
2. **Unified SSE + JSON Response Decoder in `ConfiguredMcpGateway`**: Send `Accept: application/json, text/event-stream` on HTTP MCP requests; inspect `Content-Type` to parse `application/json` directly or decode SSE `data:` frames, flagging `notifications/tools/list_changed` to refresh `cached_tools` asynchronously.
3. **Reusable CI Gate Calling into Tagged Release CD**: Structure `.github/workflows/ci.yml` with `workflow_call` so `.github/workflows/release.yml` enforces full CI verification and `scripts/ci_verify_packaging.py` tag/license validation before building LTO release archives across Linux, macOS, and Windows.

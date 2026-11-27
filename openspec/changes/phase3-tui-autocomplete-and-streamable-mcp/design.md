# Technical Design: Phase 3 TUI Autocomplete, Session Picker, and Streamable HTTP MCP

## Context
Phase 1 (`bootstrap-tekvra-agent`) and Phase 2 (`harden-runtime-mcp-oauth-and-tui`) delivered a complete workspace, CLI, Alt-Screen TUI, multi-provider LLM router with OAuth login/refresh, native tools, PTY sessions, Monty `code`, and stdio/HTTP JSON-RPC MCP client gateway. Phase 3 targets interactive discoverability in the TUI and Streamable HTTP MCP event-stream support.

## Goals / Non-Goals
- **Goals**:
  - Provide instant fuzzy completion popups when typing `/` (slash commands + Luau plugin commands) or `@` (workspace relative file paths respecting `.gitignore` and sensitive-file filters) in the TUI composer.
  - Provide an interactive `/sessions` modal listing recent sessions from `StateIndex` (`id`, `title`, `cwd`, `updated_at`) to resume or fork in place.
  - Support MCP Streamable HTTP (`Content-Type: text/event-stream` replies to JSON-RPC `POST`) in `ConfiguredMcpGateway`.
- **Non-Goals**:
  - Mutating top-level LLM `ToolDefinition` schemas when MCP servers emit `tools/list_changed` (tool schemas remain strictly static for prompt-cache stability).

## Decisions
1. **Cached Workspace File Index for `@` Mentions**: Build a bounded workspace path list asynchronously via `ignore::WalkBuilder` on TUI startup and refresh on file-mutation tool completion so composer keystrokes never block on disk I/O.
2. **Unified SSE + JSON Response Decoder in `HttpMcpTransport`**: Inspect `Content-Type` on HTTP MCP responses; parse `application/json` directly or extract the matching JSON-RPC `id` event from `text/event-stream` frames.

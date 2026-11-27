# Proposal: Phase 3 Interactive TUI Autocomplete, Session Picker, and MCP SSE/Streamable HTTP

## Why
With Tekvra's core runtime, native tool suite, consent-driven permission model, end-to-end PKCE/device-code OAuth + refresh, live stdio/HTTP JSON-RPC MCP gateway, and Codex v2 `app-server` / `acp` complete, the next ergonomic enhancements focus on rich interactive TUI productivity (`@file` / `/command` fuzzy autocomplete popups and an interactive `/resume` session picker) plus Streamable HTTP SSE notification support for remote MCP servers.

## What Changes
- **TUI Fuzzy Autocomplete & Interactive Session Picker**: Add `/` command + `@path` workspace file completion popups and an interactive `/sessions` / `/resume` session browser backed by `StateIndex::list_sessions`.
- **MCP Streamable HTTP SSE Notifications**: Extend `ConfiguredMcpGateway` to parse `text/event-stream` responses on HTTP MCP servers (`tools/list_changed` refresh notifications) while preserving prompt-cache stability.

## Capabilities

### Modified Capabilities
- `tui-and-lua-extensions`: Add fuzzy `/` slash-command and `@file` path completion popups and an interactive `/sessions` picker modal in the Alt-Screen TUI.
- `native-tools-and-pty`: Add `text/event-stream` (Streamable HTTP MCP) response framing and dynamic `notifications/tools/list_changed` catalog refresh in `ConfiguredMcpGateway`.

## Impact
- **Affected Crates**: `crates/tekvra-tui`, `crates/tekvra-tools`, `crates/tekvra-cli`.
- **Compatibility**: Additive UX and transport improvements; zero breaking config or wire-protocol changes.

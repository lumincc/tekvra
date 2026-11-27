# Tasks

## 1. TUI Fuzzy Autocomplete & Session Browser

- [ ] 1.1 Implement `/` slash-command and `@file` non-sensitive workspace path fuzzy completion popup in `crates/tekvra-tui` with keyboard navigation (`Up`/`Down`/`Tab`/`Esc`) and unit tests in `crates/tekvra-tui/tests/tui_spec.rs`.
- [ ] 1.2 Add `/sessions` interactive session-picker modal in `crates/tekvra-tui` wired to `StateIndex::list_sessions` in `crates/tekvra-cli` for in-place `resume` and `fork`.

## 2. Streamable HTTP MCP Support

- [ ] 2.1 Extend `HttpMcpClient` in `crates/tekvra-tools/src/mcp_client.rs` to decode `Content-Type: text/event-stream` JSON-RPC responses and refresh cached tool catalogs on `notifications/tools/list_changed`, verified by integration tests in `crates/tekvra-tools/tests/tools_spec.rs`.

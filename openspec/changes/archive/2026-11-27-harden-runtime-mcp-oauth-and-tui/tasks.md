# Tasks

## 1. Live Stdio and HTTP JSON-RPC MCP Client Gateway

- [x] 1.1 Implement `crates/tekvra-tools/src/mcp_client.rs` (`ConfiguredMcpGateway` implementing `McpGateway` over child-process `stdio` JSON-RPC 2.0 and HTTP JSON-RPC 2.0 `POST` with `initialize`, `notifications/initialized`, `tools/list`, and `tools/call`, bounded timeouts, and non-fatal startup diagnostics) and export it from `tekvra-tools`; verify with integration tests in `crates/tekvra-tools/tests/tools_spec.rs` covering both a live stdio Python/sh MCP server and an in-process HTTP JSON-RPC MCP server invoked from Monty `code` (`mcp_list` + `await mcp_call`).
- [x] 1.2 Wire `ConfiguredMcpGateway` into `crates/tekvra-cli/src/main.rs` (`tekvra`, `tekvra exec`, `tekvra app-server`, `tekvra acp`) and update `crates/tekvra-app-server/src/server.rs` (`mcpServerStatus/list`) to report live connected tools and server status; verify with `cargo test -p tekvra-app-server` asserting `mcpServerStatus/list` exposes live MCP tool descriptors.

## 2. End-to-End OAuth Loopback, Device-Code Polling, and Token Refresh

- [x] 2.1 Extend `crates/tekvra-providers/src/auth.rs` with `OAuthProviderEndpoints`, RFC 8628 `request_device_code` + `poll_device_code_token` (`authorization_pending` and `slow_down` handling), and `refresh_oauth_account` (`grant_type=refresh_token`), and wire `tekvra auth login` (loopback PKCE + `--device-code`) and startup token refresh in `crates/tekvra-cli/src/main.rs`; verify with mock-HTTP integration tests in `crates/tekvra-providers/tests/providers_spec.rs` and CLI process tests in `crates/tekvra-cli/tests/cli_spec.rs`.

## 3. Interactive TUI Slash Commands, UTF-8 Composer, and Luau Command Dispatch

- [x] 3.1 Add `PluginHost::execute_command` and `registered_commands` in `crates/tekvra-lua/src/runtime.rs`, and upgrade `crates/tekvra-tui` with UTF-8 cursor-aware composer editing (`Left`/`Right`/`Home`/`End`/`Delete`/`Ctrl-J`), `/help`, `/model`, `/mode`, `/mcp`, `/review`, and custom Luau slash-command execution wired in `crates/tekvra-cli/src/main.rs`; verify with `cargo test -p tekvra-lua` and `cargo test -p tekvra-tui` covering CJK cursor editing, `/model` + `/mode` + `/mcp` commands, and `/hello` Luau command execution.

## 4. Project Documentation, Release Build, and Verification Gates

- [x] 4.1 Update `.gitignore` (ignoring `.tmp/`), write comprehensive root `README.md` and update `docs/{configuration,providers,tools-and-permissions}.md`, compile `cargo build --release`, smoke-test `./target/release/tekvra`, and pass `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo deny --offline check`, and `openspec validate --strict`.

# Design

## Context

See `proposal.md` for motivation. `tekvra-tools` already defines `McpGateway` and `McpToolDescriptor` consumed by `MontyCodeSandbox`, `tekvra-providers` already provides `LoopbackOAuthListener`, `DeviceCodeSession`, `exchange_oauth_token`, and `CredentialStore`, and `tekvra-lua` already records `commands: BTreeMap<String, (LuaCommandSpec, RegistryKey)>`. This change connects those building blocks to live transports and user-facing CLI/TUI entrypoints without adding third-party crate dependencies.

## Goals / Non-Goals

**Goals:**
- Provide a real async `ConfiguredMcpGateway` supporting both newline-delimited JSON-RPC 2.0 over child `stdio` (`tokio::process::Command` with `kill_on_drop(true)`) and HTTP JSON-RPC 2.0 (`reqwest::Client`), respecting folder trust (already enforced by `tekvra-config` withholding untrusted project `mcp_servers`).
- Complete end-to-end OAuth login and refresh in `tekvra-providers` + `tekvra-cli` with configurable endpoint overrides (`TEKVRA_OAUTH_*` or method args) so both production OpenAI/Copilot endpoints and deterministic local mock HTTP servers work identically.
- Upgrade `TuiApp` composer editing, built-in slash commands (`/help`, `/model`, `/mode`, `/mcp`, `/review`), and opt-in Luau custom command execution.

**Non-Goals:**
- Top-level LLM tool schema injection for MCP servers: MCP tools MUST remain accessed exclusively via `code` (`mcp_list` / `mcp_call`) so prompt cache prefixes never invalidate when MCP servers connect or reload.
- External browser binary bundling: `tekvra auth login` prints the clickable authorization URL and optionally attempts non-fatal `xdg-open` / `open` when `--no-browser` is not set.

## Decisions

1. **Dual-Transport `ConfiguredMcpGateway` (`crates/tekvra-tools/src/mcp_client.rs`)**
   - *Choice*: Represent each enabled `[mcp_servers.<name>]` entry as either a `StdioMcpTransport` (spawning `tokio::process::Command` with piped `stdin`/`stdout`, serialized behind an async `tokio::sync::Mutex` for request/response correlation, `kill_on_drop(true)`, and sanitized env + configured `env` overrides) or an `HttpMcpTransport` (`reqwest::Client` sending JSON-RPC 2.0 `POST` requests to `url`).
   - *Initialization*: `ConfiguredMcpGateway::connect_all(&IndexMap<String, McpServerConfig>, &Path)` connects enabled servers, sends MCP `initialize` (`protocolVersion: "2024-11-05"`) + `notifications/initialized` + `tools/list`, caches `Vec<McpToolDescriptor>` in an `RwLock`, and records per-server connection errors as non-fatal diagnostics rather than crashing startup if one optional MCP server binary is absent.
   - *Alternatives considered*: Adding an external `rmcp` dependency. Rejected because MCP's `initialize` / `tools/list` / `tools/call` JSON-RPC 2.0 envelope is ~200 lines of clean `tokio` + `serde_json` + `reqwest` code and avoids pulling new transitive dependencies into `deny.toml`.

2. **Configurable OAuth Endpoints, RFC 8628 Polling, and Auto-Refresh (`crates/tekvra-providers/src/auth.rs`)**
   - *Choice*: Introduce `OAuthProviderEndpoints` (`authorize_url`, `device_code_url`, `token_url`, `client_id`, `scopes`) with built-in defaults for `openai` and `github-copilot` plus optional CLI/env overrides (`--authorize-url`, `--device-url`, `--token-url`, `--client-id` or `TEKVRA_OAUTH_*`).
   - Add `request_device_code(...) -> Result<DeviceCodeSession, ProviderError>`, `poll_device_code_token(...) -> Result<StoredOAuthAccount, ProviderError>` (handling RFC 8628 `authorization_pending`, `slow_down` +5s backoff, `expired_token`, and `access_denied`), and `refresh_oauth_account(...) -> Result<StoredOAuthAccount, ProviderError>`.
   - During `build_account_pool` in `tekvra-cli`, any stored account whose `expires_at_epoch_secs <= now + 60` with a `Some(refresh_token)` attempts automatic refresh and persists the updated `StoredOAuthAccount` back to `CredentialStore`.

3. **Cursor-Aware Composer & Slash-Command Dispatch in `tekvra-tui` + `tekvra-lua`**
   - *Choice*: Add `cursor_byte_pos: usize` to `TuiApp` (always maintained on UTF-8 char boundaries) supporting `Left`, `Right`, `Home`, `End`, `Backspace`, `Delete`, and `Ctrl-J` newline insertion.
   - Add `TuiCommand` variants `RunReview { target: ReviewTarget }`, `ShowMcpStatus`, and `InvokePluginCommand { name: String, args: String }`, plus `PluginHost::execute_command(&self, name: &str, args: &str) -> Result<Option<String>, LuaHostError>` in `tekvra-lua`.

## Risks / Trade-offs

- **[Risk] Unresponsive stdio MCP child hangs startup or `code` execution** → **Mitigation**: Wrap MCP `initialize`, `tools/list`, and `tools/call` operations in `tokio::time::timeout` (default 10s on startup discovery, 30s on tool calls) and enable `kill_on_drop(true)` on child processes.
- **[Risk] UTF-8 multi-byte panics when editing CJK text in TUI composer** → **Mitigation**: Step `cursor_byte_pos` strictly using `str::floor_char_boundary` / `char_indices` helpers and cover CJK cursor insertion/deletion in `tui_spec.rs`.

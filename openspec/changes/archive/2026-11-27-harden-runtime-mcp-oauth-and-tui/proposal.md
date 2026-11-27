# Proposal

## Why

Phase 1 (`bootstrap-tekvra-agent`) established Tekvra's 11-crate architecture, protocol contracts, native tools, storage, TUI, opt-in Luau host, and Codex `app-server` v2, while leaving the CLI entrypoint's live MCP client transport (`mcp_gateway: None`), end-to-end `tekvra auth login` OAuth callback/device-code polling loop, and richer interactive TUI slash-command/composer workflows ready for production wiring. Completing these integrations and release documentation turns Tekvra into a turnkey daily-driver binary out of the box.

## What Changes

- Implement a production `ConfiguredMcpGateway` in `tekvra-tools` supporting both child-process `stdio` JSON-RPC 2.0 MCP servers (`initialize`, `notifications/initialized`, `tools/list`, `tools/call`) and remote HTTP JSON-RPC MCP endpoints configured in `tekvra.toml` (`[mcp_servers.<name>]`), and wire it into `tekvra` TUI, `tekvra exec`, `tekvra app-server` (`mcpServerStatus/list`), and `tekvra acp`.
- Complete end-to-end OAuth login and refresh workflows in `tekvra-providers` and `tekvra-cli`: drive `LoopbackOAuthListener` + PKCE token exchange and RFC 8628 device-code initiation + interval/`slow_down` polling in `tekvra auth login`, and automatically refresh expired stored OAuth credentials when a `refresh_token` is present.
- Expand `tekvra-tui` interactive UX with `/help`, `/model <provider/model>`, `/mode <plan|build|auto|yolo>`, `/mcp` status inspection, `/review` in-session diff review dispatch, composer cursor movement (`Left`/`Right`/`Home`/`End` + multiline newline insertion), and execution of custom Luau slash commands registered via `tekvra.api.register_command`.
- Finalize repository delivery assets: author comprehensive root `README.md`, ignore local reference checkouts (`.tmp/`) in `.gitignore`, verify release-profile compilation (`cargo build --release`), and run end-to-end CLI smoke checks.

## Capabilities

### New Capabilities

### Modified Capabilities
- `native-tools-and-pty`: Add live stdio child-process and HTTP JSON-RPC `ConfiguredMcpGateway` runtime lifecycle wired across CLI, TUI, `app-server`, and `acp` while preserving top-level LLM tool schema stability.
- `providers-and-model-routing`: Add end-to-end `tekvra auth login` PKCE loopback callback exchange, RFC 8628 device-code polling (`authorization_pending` / `slow_down`), and automatic OAuth `refresh_token` renewal.
- `tui-and-lua-extensions`: Add interactive TUI slash commands (`/help`, `/model`, `/mode`, `/mcp`, `/review`), richer composer cursor/multiline editing, and opt-in Luau custom slash-command execution.

## Impact

- **Crates modified**: `tekvra-tools` (new `mcp_client.rs` + exports), `tekvra-providers` (`auth.rs` device-code flow + refresh), `tekvra-lua` (custom command execution helper), `tekvra-tui` (`app.rs` composer/slash commands/plugin command callback), `tekvra-app-server` (live MCP tool/status reporting), and `tekvra-cli` (`main.rs` MCP bootstrap, async OAuth CLI flows, TUI wiring).
- **Documentation & repo root**: Root `README.md`, `.gitignore`, `docs/configuration.md`, `docs/providers.md`, and `docs/tools-and-permissions.md`.
- **Dependencies**: Uses existing pure-Rust workspace crates (`tokio`, `reqwest` + `rustls`, `serde_json`, `ratatui`, `crossterm`) with zero new third-party dependencies.

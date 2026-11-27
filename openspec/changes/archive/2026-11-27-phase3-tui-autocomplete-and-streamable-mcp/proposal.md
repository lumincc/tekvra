# Proposal: Phase 3 Interactive TUI Autocomplete, Session Picker, Streamable HTTP MCP, and CI/CD Pipeline

## Why
With Tekvra's core runtime, native tool suite, consent-driven permission model, end-to-end PKCE/device-code OAuth + refresh, live stdio/HTTP JSON-RPC MCP gateway, and Codex v2 `app-server` / `acp` complete, Phase 3 delivers interactive TUI discoverability (`@file` / `/command` fuzzy autocomplete popups and an interactive `/sessions` resume/fork picker), Streamable HTTP SSE MCP framing + catalog refresh, and automated multi-platform CI/CD release pipelines.

## What Changes
- **TUI Fuzzy Autocomplete & Interactive Session Picker**: Add `/` command + `@path` workspace file completion popups and an interactive `/sessions` (`/resume`) session browser backed by `StateIndex::list_sessions` for in-place resume and fork, plus top-level `tekvra resume` and `tekvra fork` CLI subcommands.
- **MCP Streamable HTTP SSE & Catalog Refresh**: Extend `ConfiguredMcpGateway` to support `bearer_token_env_var`, decode `Content-Type: text/event-stream` responses on HTTP MCP servers, and refresh cached tool catalogs on `notifications/tools/list_changed` while preserving prompt-cache stability.
- **Automated Multi-Platform CI/CD & Packaging Verification**: Add GitHub Actions CI (`.github/workflows/ci.yml`), tagged multi-target Release CD (`.github/workflows/release.yml` across Linux x64/arm64, macOS x64/arm64, and Windows x64 with `SHA256SUMS.txt`), `.github/dependabot.yml`, and deterministic packaging/workflow verification (`scripts/ci_verify_packaging.py`).

## Capabilities

### Modified Capabilities
- `tui-and-lua-extensions`: Add fuzzy `/` slash-command and `@file` path completion popups and an interactive `/sessions` picker modal in the Alt-Screen TUI.
- `native-tools-and-pty`: Add `text/event-stream` (Streamable HTTP MCP) response framing, bearer token header injection, and dynamic `notifications/tools/list_changed` catalog refresh in `ConfiguredMcpGateway`.
- `workspace-and-config`: Add automated multi-platform CI quality gates, Apache-2.0 attribution/packaging verification, and multi-target tagged release CD workflows.

## Impact
- **Affected Crates & Infrastructure**: `crates/tekvra-tui`, `crates/tekvra-tools`, `crates/tekvra-cli`, `.github/workflows/`, `scripts/ci_verify_packaging.py`.
- **Compatibility**: Additive UX, transport, CLI, and release automation improvements; zero breaking config or wire-protocol changes.

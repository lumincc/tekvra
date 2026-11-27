# Tasks

## 1. Streamable HTTP MCP & Dynamic Catalog Refresh

- [x] 1.1 Extend `ConfiguredMcpGateway` in `crates/tekvra-tools/src/mcp_client.rs` to support `bearer_token_env_var`, decode `Content-Type: text/event-stream` JSON-RPC responses, and refresh cached tool catalogs on `notifications/tools/list_changed` without mutating top-level LLM tool schemas; verify with SSE integration tests in `crates/tekvra-tools/tests/tools_spec.rs`.

## 2. TUI Fuzzy Autocomplete, Session Browser & CLI Resume/Fork

- [x] 2.1 Implement `/` slash-command and `@file` non-sensitive workspace path fuzzy completion popup plus `/sessions` (`/resume`) interactive session-picker modal in `crates/tekvra-tui`, with unit tests in `crates/tekvra-tui/tests/tui_spec.rs`.
- [x] 2.2 Wire workspace non-sensitive file scanning, in-place TUI session switching (`Resume` / `Fork`), `tekvra resume [--last | <ID>]`, `tekvra fork <ID>`, and `tekvra exec -o <FILE>` in `crates/tekvra-cli`, verified by process tests in `crates/tekvra-cli/tests/cli_spec.rs`.

## 3. Multi-Platform CI/CD Workflows & Packaging Verification

- [x] 3.1 Create `.github/workflows/ci.yml`, `.github/workflows/release.yml`, `.github/dependabot.yml`, `scripts/ci_verify_packaging.py`, and CI/CD documentation in `README.md`, and verify with `python3 scripts/ci_verify_packaging.py --check-workflows` and `openspec validate --all --strict`.

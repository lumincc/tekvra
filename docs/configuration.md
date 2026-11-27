# Tekvra Configuration Guide

Tekvra uses declarative, versioned TOML configuration files and starts **zero Lua virtual machines** by default.

## Configuration Hierarchy

Settings merge from lowest to highest precedence:

1. Built-in defaults (`model = "openai/gpt-5.4-codex"`, `default_mode = "build"`, `experimental.lua_plugins = false`)
2. Global user files under `~/.config/tekvra/` (or `~/.config/tekvra-<TEKVRA_NAMESPACE>/`):
   - `tekvra.toml`
   - `permissions.toml`
   - `providers.toml`
3. Trusted project files under `<repo>/.tekvra/` (gated by canonical folder trust, except `deny` rules in `.tekvra/permissions.toml`, which apply immediately even when untrusted)
4. Selected CLI `--profile <name>`
5. Process-local CLI `-c key=value` dotted overrides and `--enable` / `--no-plugins` flags

## Automatic Schema Migration

Global TOML files carry `version = 2`. When Tekvra encounters an older supported global version (such as `version = 1`), it writes a verbatim backup under `~/.local/share/tekvra/backups/` and atomically upgrades the global config file before parsing.

## Folder Trust Boundary

Opening an untrusted repository withholds `.tekvra/tekvra.toml` (including project MCP servers), `.tekvra/providers.toml`, `.env` variables, `.tekvra/init.lua`, and project `allow`/`ask` permission rules until the user explicitly grants trust for that canonical directory. Project `deny` rules in `.tekvra/permissions.toml` are enforced immediately.

## Global-Only Experimental Flags

`[experimental]` flags—including `experimental.lua_plugins = true`—are honored **only** from global `~/.config/tekvra/tekvra.toml` (or CLI `--enable`), never from a project `.tekvra/tekvra.toml`.

## Instruction and Skill Discovery

- `AGENTS.md` and `AGENTS.override.md` are discovered from `~/.config/tekvra/` first, then in deterministic root-to-leaf order from the Git repository root down to the current working directory.
- `SKILL.md` packages are discovered from `~/.config/tekvra/skills/`, `~/.agents/skills/`, `~/.claude/skills/`, `<repo>/.agents/skills/`, and `<repo>/.claude/skills/`.

## External MCP Servers (`[mcp_servers.<name>]`)

Trusted `tekvra.toml` files can configure child-process `stdio` or remote HTTP JSON-RPC 2.0 MCP servers:

```toml
version = 2

[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
startup_timeout_sec = 10
tool_timeout_sec = 60

[mcp_servers.remote_docs]
url = "https://mcp.example.com/rpc"
bearer_token_env_var = "DOCS_MCP_TOKEN"
startup_timeout_sec = 10
tool_timeout_sec = 30
```

Enabled servers are connected at startup via `ConfiguredMcpGateway` (`initialize` -> `notifications/initialized` -> `tools/list`) and exposed inside Monty `code` (`mcp_list()` / `await mcp_call(...)`) as well as `/mcp` and `mcpServerStatus/list` without mutating top-level LLM tool schemas.

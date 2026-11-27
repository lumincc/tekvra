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

Trust state lives in `~/.local/share/tekvra/trusted_folders.toml` and is managed with the `tekvra trust` subcommand (no session, plugin, or MCP startup):

```bash
tekvra trust status   # print whether the workspace root is trusted
tekvra trust allow    # grant trust for the canonical workspace root
tekvra trust revoke   # revoke trust for the canonical workspace root
```

Inside the TUI, `/trust <status|allow|revoke>` dispatches the same operations against the same store.

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

### Managing MCP Servers from the CLI (`tekvra mcp`)

The `tekvra mcp` subcommand family manages `[mcp_servers.*]` entries **without starting a session, Lua VM, or gateway**:

```bash
tekvra mcp list                              # "name  kind(stdio|http)  state(enabled|disabled)"
tekvra mcp add filesystem --command npx -- -y @modelcontextprotocol/server-filesystem .
tekvra mcp add remote_docs --url https://mcp.example.com/rpc --bearer-env DOCS_MCP_TOKEN
tekvra mcp add fs2 --command uvx --scope user  # write to ~/.config/tekvra/tekvra.toml
tekvra mcp remove filesystem                 # removes from the scope that defines it
tekvra mcp test remote_docs                  # one-shot handshake; prints discovered tool names
```

`add`/`remove` edit `<repo>/.tekvra/tekvra.toml` (project scope, the default) or the user-global file via `toml_edit::DocumentMut`, so unrelated comments and formatting survive the mutation. `--command` (stdio) and `--url` (HTTP) are mutually exclusive; trailing arguments after `--` become the stdio argv. `test` performs a one-shot `initialize` + `tools/list` handshake against a single server and prints `name/tool` pairs (non-zero exit when the handshake fails).

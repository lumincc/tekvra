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
session = true        # Streamable-HTTP `Mcp-Session-Id` affinity (default true)
http_header_timeout_sec = 120   # response-header budget (default 60 s)
http_idle_timeout_sec = 300     # SSE inter-chunk budget (default 10 s)

[mcp_servers.local_sampler]
command = "python3"
args = ["sampler_mcp.py"]
sampling = true       # per-server opt-in for sampling/createMessage

[experimental]
mcp_sampling = true   # global gate; sampling without it answers -32603
```

Server→client requests are routed: `sampling/createMessage` uses the session's `fast`-role model binding after an interactive approval prompt (usage lands in the ledger under `<rpc_id>_mcp_sampling`); `elicitation/create` becomes interactive questions; unknown methods answer `-32601`.

### HTTP Streaming Timeouts (`http_header_timeout_sec` / `http_idle_timeout_sec`)

Per-server (HTTP `url` servers only) overrides of the streaming budget: `http_header_timeout_sec`
bounds waiting for the response headers (default 60 s), `http_idle_timeout_sec` bounds the gap
between SSE body chunks (default 10 s). Slow backends that stream long-running tool results raise
the idle budget instead of tripping the built-in one; the effective timeout is still clamped by
`tool_timeout_sec`/`reqwest`'s overall call budget. `0` is rejected at load time with an
`invalid_mcp_http_timeout` notice and the built-in default applies.

### Language Servers (`[lsp.<id>]`)

Trusted `tekvra.toml` files configure read-only language servers for the `lsp_diagnostics` tool:

```toml
[lsp.rust-analyzer]
command = "rust-analyzer"
languages = ["rs"]
```

- Project `.tekvra/tekvra.toml` `[lsp]` tables inherit the existing trust gate (withheld from untrusted projects together with the rest of the project config).
- Servers run as read-only guests over stdio (`initialize` → `didOpen`/`didChange` → `publishDiagnostics`), are reused across calls in a session, and are shut down on drop. Diagnostics settle inside a 500 ms quiet window bounded by 5 s.

### Web Search Endpoint (`agent.websearch_base_url`)

`web_search` posts one query to an HTML endpoint — default `https://html.duckduckgo.com/html/`. Override for self-hosted SearXNG/Whoogle-style HTML pages:

```toml
[agent]
websearch_base_url = "https://search.internal.corp/search"
```

The endpoint is authorized through the `webfetch` ladder class; results are capped at 1..=10 (default 8) with duplicate-host collapsing.

Enabled servers are connected at startup via `ConfiguredMcpGateway` (`initialize` -> `notifications/initialized` -> `tools/list`, then `resources/list` / `resources/templates/list` / `prompts/list` for servers that advertise them) and exposed inside Monty `code` (`mcp_list()` / `await mcp_call(...)`, plus `mcp_resources()` / `mcp_prompts()` and `await mcp_read(...)` / `await mcp_get_prompt(...)`) as well as `/mcp` and `mcpServerStatus/list` without mutating top-level LLM tool schemas.

### Managing MCP Servers from the CLI (`tekvra mcp`)

The `tekvra mcp` subcommand family manages `[mcp_servers.*]` entries **without starting a session, Lua VM, or gateway**:

```bash
tekvra mcp list                              # "name  kind(stdio|http)  state(enabled|disabled)"
tekvra mcp add filesystem --command npx -- -y @modelcontextprotocol/server-filesystem .
tekvra mcp add remote_docs --url https://mcp.example.com/rpc --bearer-env DOCS_MCP_TOKEN
tekvra mcp add fs2 --command uvx --scope user  # write to ~/.config/tekvra/tekvra.toml
tekvra mcp remove filesystem                 # removes from the scope that defines it
tekvra mcp test remote_docs                  # one-shot handshake; prints tools, then resources / prompts
```

`add`/`remove` edit `<repo>/.tekvra/tekvra.toml` (project scope, the default) or the user-global file via `toml_edit::DocumentMut`, so unrelated comments and formatting survive the mutation. `--command` (stdio) and `--url` (HTTP) are mutually exclusive; trailing arguments after `--` become the stdio argv. `test` performs a one-shot `initialize` + `tools/list` handshake (plus the resource / prompt lists when advertised) against a single server and prints `name/tool` pairs, followed by `resources:` and `prompts:` lines when the server has any (non-zero exit when the handshake fails).

## Spend Cap (`agent.spend_limit_usd`)

Set a hard ceiling on cumulative USD spend; the next turn after crossing it is refused with
`TurnRefused` (the in-flight turn that crosses the cap still completes):

```toml
[agent]
spend_limit_usd = 5.0                 # omit the key entirely to disable (default)
spend_limit_window = "session"        # "session" (default) or "day"
```

- `session`: spend accumulates across the whole session (all turns, all models).
- `day`: spend accumulates per UTC calendar day (the state ledger records UTC timestamps;
  the day boundary is UTC, not local time — documented deviation for zero new dependencies).
- Raising or removing `spend_limit_usd` unblocks a refused session immediately (checked at
  turn start, no restart needed). Surfaces: the TUI/app-server `thread/tokenUsage/updated`
  payload carries `remainingBudget { limitUsd, spentUsd, remainingUsd }`, and
  `account/rateLimits` reports `spendControlReached`.

## Task Auto-Merge Gate (`agent.allow_task_auto_merge`)

```toml
[agent]
allow_task_auto_merge = true   # default: false (merge-back stays explicit)
```

Opt-in gate for `task { task_id, merge: "auto" }`: when enabled, `"auto"` runs a real merge
probe (immediately aborted, tree restored), then merges for real if clean — identical to
`merge: "merge"` — and otherwise reports the conflict (worktree and branch preserved,
conflicting paths named) without prompting or mutating the parent tree. `"auto"` never
resolves to `"discard"`; explicit `"merge"` / `"discard"` keep their exact semantics with
the flag on or off. Permission authorization runs for the resolved verb only (the conflict
path authorizes nothing), and explicit `deny` rules still win.

## Model Fallback Chains (`models.fallback`)

```toml
[models]
fallback = ["anthropic/claude-sonnet-4-5", "gemini/gemini-3-pro"]
```

When the primary `Main` model fails HARD — provider accounts exhausted, transport down, or a
non-retryable HTTP status (anything outside 429/502/503/504/529) — the turn loop advances down
the chain with a steering-note announcement (`model_fallback`) naming the failed and taken
specs. Retryable statuses keep using the same-model retry path. The chain is depth-capped at 3
(incl. primary), deduped against the primary, and `Main`-role only: every other job role
(`plan`, `compact`, …) fails loudly instead of silently degrading. Usage is attributed to the
model that actually served the turn; exhausting the whole chain fails the turn with every
attempt named.

## Project Memory (state_dir memory)

Tekvra keeps a per-project scratch file at `<state_dir>/memory/<project-slug>/MEMORY.md`
(dated `- YYYY-MM-DD:` lines, 8 KiB per entry / 256 KiB per file). It is injected as a
`## Project memory` section after the workspace instructions (stable position → prompt-cache
friendly). The model appends entries through the `memory_save` tool (Build/Auto/Yolo prompt,
ReadOnly/Plan deny, `allow`/`deny` rules honored — see
[tools-and-permissions](./tools-and-permissions.md)). Interactive surfaces: `/memory` prints
path + content, `/memory edit` prints the path for manual editing, and `tekvra memory`
(`--append TEXT`) reads/appends statelessly outside any session.

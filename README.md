# Tekvra

**Tekvra** is a pure-Rust, Codex-compatible coding agent CLI, fullscreen Alt-Screen TUI, and protocol server (`app-server` JSON-RPC 2.0 + `acp` Agent Client Protocol) licensed under **Apache-2.0**.

Built on a unified `tokio` + `reqwest` (`rustls` + `aws-lc-rs`) async foundation with `#![forbid(unsafe_code)]` across every workspace crate, Tekvra combines Maki's ergonomic terminal workflow and RTK-style tool output compression with Caudra's declarative TOML configuration, zero-Lua-by-default native tool suite, consent-driven permission boundaries, and Codex-grade multi-turn execution.

---

## Key Capabilities

- **Zero-Lua-by-Default Native Tool Suite (`tekvra-tools`)**:
  - Model-family-aligned editing: GPT-5+, o-series, and Codex models receive `apply_patch` (`*** Begin Patch` ... `*** End Patch`); Claude/Gemini/other models receive `file_edit` + `file_write` with deterministic per-path locks and automatic pre-mutation file snapshots (`tekvra-storage`).
  - Fast workspace discovery (`file_read`, `file_glob`, `file_grep`) that automatically skips `.git` and sensitive secret files (`.env*`, `*.pem`, `*.key`, `id_rsa`, `id_ed25519`), plus `tree-sitter` structural symbol outlines (`file_index` for Rust, Python, Go, JS, TS/TSX, Bash).
  - Persistent interactive PTY sessions (`exec_command` + `write_stdin`) via `portable-pty` with sanitized environments and background completion notifications.
  - RTK-derived terminal `\r` decoding, ANSI stripping, and build/test output compression—with raw logs persisted on disk and searchable via `tool_output`.
  - Isolated Monty Python `code` interpreter (`monty`) paired with a **prompt-cache-stable MCP gateway** (`ConfiguredMcpGateway` over child-process `stdio` JSON-RPC 2.0 and HTTP JSON-RPC 2.0 `POST`) exposed via `mcp_list()` and `await mcp_call(...)` without mutating top-level LLM tool schemas.
- **Consent-Driven Safety Without Kernel Sandboxes (`tekvra-tools::permissions`)**:
  - `tree-sitter-bash` AST decomposition evaluates every subcommand across `&&`, `||`, `;`, `|`, subshells, `$()`, and redirections independently (`Once`-only fallback on dynamic `eval` / `bash -c "$VAR"`); native Windows PowerShell invocations (`pwsh` / `powershell -Command`) get the same per-statement treatment via a hand-written lexical decomposer (`tekvra_tools::powershell_syntax`) with case-insensitive read-only allowlisting (`Get-ChildItem`, `Select-String`, `git status`, …) and destructive script-block detection.
  - Explicit modes (`Plan`, `ReadOnly`, `Build`, `Auto`, `Yolo`) where `deny` rules in `permissions.toml` are **never** bypassed—even in `Yolo` mode or untrusted repositories.
  - Canonical folder trust boundary withholding `.tekvra/tekvra.toml`, `.tekvra/providers.toml`, `.env`, `.tekvra/init.lua`, and project `allow`/`ask` rules until trusted.
  - SSRF-safe `webfetch` blocking loopback, link-local/cloud metadata (`169.254.169.254`), and RFC1918/ULA addresses across DNS resolution and every redirect hop.
- **Multi-Provider LLM Routing & End-to-End OAuth (`tekvra-providers`)**:
  - Stateless OpenAI `/v1/responses` (`store: false`, `include: ["reasoning.encrypted_content"]`, `prompt_cache_key`, `session-id`, `prompt_cache_breakpoint`), OpenAI Chat Completions, Anthropic `/v1/messages` (`cache_control: {"type": "ephemeral"}`), and Google Gemini `streamGenerateContent`.
  - Built-in routing across `openai`, `anthropic`, `gemini`, `github-copilot`, `xai`, `deepseek`, `mistral`, `zai`, `openrouter`, `ollama`, and `llamacpp`, with role bindings (`main`, `plan`, `subagent`, `compact`, `review`, `goal`, `title`, `fast`) and multi-account quota failover (`AccountPool`).
  - End-to-end `tekvra auth login` supporting browser loopback PKCE callback exchange, headless RFC 8628 `--device-code` polling (`authorization_pending` / `slow_down`), and automatic `refresh_token` renewal (`0600` `credentials.json`).
- **Session Durability, Context Compaction, Subagents & Code Review (`tekvra-core`, `tekvra-storage`)**:
  - Append-only JSONL rollouts (`~/.local/share/tekvra/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`) + SQLite WAL metadata index (`state.db`), supporting `resume`, `fork`, and reversible `/undo` across file mutations.
  - Reverse token-budget compaction preserving recent turns and active `plan` state while summarizing older history, plus pruned large tool outputs recoverable via `tool_output`.
  - Depth-bounded (`agent.subagent_max_depth`, default `1`) subagent orchestration via the `task` tool and `SubagentManager` (`spawn`/`steer`/`list`, foreground or background, optional worktree isolation with explicit merge-back) and dedicated read-only `/review` (`tekvra review`).
- **Fullscreen Alt-Screen TUI, Headless `exec`, Codex v2 `app-server` & `acp`**:
  - `ratatui` + `crossterm` Alt-Screen TUI (`tekvra-tui`) with live streaming Markdown, `syntect` syntax highlighting, inline diff hunks, Raw/Filtered tool output toggle (`Ctrl-O`), interactive permission approval modal, UTF-8/CJK cursor-aware composer, `/queue` vs `/steer` mid-turn submission, slash commands (`/help`, `/model`, `/mode`, `/mcp`, `/review`, `/undo`, `/compact`), and opt-in Luau (`mlua` 0.12) custom commands (`experimental.lua_plugins = true`).
  - Non-interactive `tekvra exec` (`text`, `--json` JSONL events, `--output-schema`, `-o`).
  - Codex v2 bidirectional `tekvra app-server` (`stdio://` JSONL and `ws://IP:PORT` with loopback/bearer-token enforcement and bounded outbound backpressure) plus `tekvra acp`.

---

## Workspace Architecture

| Crate | Responsibility |
| --- | --- |
| `crates/tekvra-protocol` | Canonical message, tool, rollout, permission, and Codex v2 JSON-RPC wire types |
| `crates/tekvra-config` | Versioned TOML loader, automatic v1->v2 backup & migration, folder trust, `AGENTS.md` & `SKILL.md` discovery |
| `crates/tekvra-storage` | Append-only JSONL rollout writer/reader, SQLite WAL state index, raw tool logs, atomic file snapshots |
| `crates/tekvra-providers` | OpenAI Responses/Chat, Anthropic, Gemini streaming adapters, PKCE/device-code OAuth + refresh, `AccountPool` |
| `crates/tekvra-tools` | Native file/patch/search/`tree-sitter` index tools, `tree-sitter-bash` permission & SSRF engine, `portable-pty`, RTK filter, Monty `code`, stdio/HTTP MCP client |
| `crates/tekvra-core` | Turn loop, mid-turn `steer`/`queue`, reverse compaction, `/undo`, depth-bounded subagents, read-only code review |
| `crates/tekvra-lua` | Strictly opt-in Luau 0.12 (`mlua`) sandboxed plugin host with memory & instruction quotas (`--no-plugins` bypass) |
| `crates/tekvra-tui` | Fullscreen Alt-Screen `ratatui` interface, composer cursor editing, permission modal, slash commands |
| `crates/tekvra-app-server` | Codex v2 JSON-RPC 2.0 server (`stdio://` & `ws://`) with bearer-token auth and backpressure |
| `crates/tekvra-acp` | Agent Client Protocol (`acp`) stdio bridge |
| `crates/tekvra-cli` | Unified `tekvra` binary (`exec`, `resume`, `fork`, `review`, `auth`, `mcp`, `app-server`, `acp`, `completion`) |

---

## Quick Start

### Build from Source

```bash
cargo build --release
./target/release/tekvra --version
```

### Interactive TUI & Session Lifecycle

```bash
# Launch fullscreen Alt-Screen TUI in the current repository
tekvra

# Launch with a specific model and permission mode
tekvra -m anthropic/claude-sonnet-4-5 --mode plan "Analyze the workspace architecture"

# Resume the most recent session or fork from an existing session ID
tekvra resume --last
tekvra fork <SESSION_UUID>
```

### Headless Automation, Code Review & Protocol Servers

```bash
# Headless execution with structured JSONL event output
tekvra exec --json -o summary.txt "Summarize recent changes"

# Dedicated read-only code review against uncommitted changes or a base branch
tekvra review
tekvra review --base main

# Authenticate via OAuth PKCE loopback or headless RFC 8628 device code
tekvra auth login openai
tekvra auth login openai --device-code
tekvra auth status

# Manage MCP servers, workspace trust, and shell completions without a session
tekvra mcp list
tekvra mcp add filesystem --command npx -- -y @modelcontextprotocol/server-filesystem .
tekvra mcp add remote_docs --url https://mcp.example.com/rpc --bearer-env DOCS_MCP_TOKEN
tekvra mcp remove filesystem
tekvra mcp test remote_docs
tekvra trust status && tekvra trust allow && tekvra trust revoke
tekvra completion bash | source /dev/stdin   # zsh | fish | powershell also supported

# Isolated Git worktrees (`tekvra/<name>` branches under <state_dir>/worktrees/)
tekvra worktree list
tekvra worktree create feat-refactor
tekvra worktree remove feat-refactor
tekvra --worktree feat-refactor exec "refactor the parser"

# Multimodal prompts: attach images (repeatable) and inject SKILL.md instructions
tekvra exec -i screenshot.png -i diagram.jpg "Find the misaligned button"
tekvra exec --skill review-api "Audit the HTTP handlers"

# Start Codex v2 app-server (stdio or authenticated WebSocket) or ACP bridge
tekvra app-server --listen stdio://
tekvra app-server --listen ws://127.0.0.1:8765
tekvra acp
```

### TUI Keyboard Shortcuts & Slash Commands

- **Composer & Autocomplete**: Type `/` for fuzzy slash-command completion, `@` for non-sensitive workspace file completion, or a `/skill <prefix>` argument for fuzzy SKILL.md completion (`Up`/`Down` navigate, `Tab`/`Enter` accept, `Esc` dismiss); `Enter` submit (`Guide` while streaming), `Alt-Enter` steer (`Replace`) while streaming (or insert newline when idle), `Ctrl-J` insert newline, `Left`/`Right`/`Home`/`End`/`Backspace`/`Delete` UTF-8 cursor editing, `Shift-Tab` cycle permission mode, `r` toggle Filtered/Raw tool output, mouse wheel scrolls the transcript, `Ctrl-C` interrupt active turn (or exit when idle).
- **Built-In Slash Commands**: `/help`, `/model [<provider/model>]` (bare `/model` opens the fuzzy model picker modal), `/mode <plan|build|auto|yolo>`, `/sessions` (`/resume [id]`), `/fork <id>`, `/mcp`, `/review [uncommitted|base <branch>|commit <sha>]`, `/tasks` (`Ctrl-X`), `/search <query>`, `/revert <turn_id>`, `/undo` (revert the latest turn), `/compact` (manual context compaction with a token-delta banner), `/skill <name> [prompt]`, `/trust <status|allow|revoke>`, `/copy [code|all]` (OSC-52 clipboard export), `/goal <cond|clear>`, `/reload`, `/quit`.
- **Prompt Attachments**: `@path/to/file` inlines a bounded file block, `@path#L10-L25` limits the range, sensitive credential paths are withheld with a warning, and `@@` escapes a literal `@`.

---

## Documentation

- [Configuration & Folder Trust Guide](docs/configuration.md)
- [Providers, OAuth & Role Routing](docs/providers.md)
- [Native Tools, PTY, MCP & Permission Model](docs/tools-and-permissions.md)
- [Codex v2 `app-server` & `acp` Protocol Guide](docs/app-server.md)
- [Opt-In Sandboxed Luau Plugins](docs/plugins.md)
- [CI/CD & Multi-Platform Release Pipeline](docs/ci-cd.md)

---

## Verification & Quality Gates

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny --offline check licenses bans sources
python3 scripts/ci_verify_packaging.py --check-workflows
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE) (`Apache-2.0`).

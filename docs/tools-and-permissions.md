# Native Tools, PTY Execution, Output Filtering, and Permissions

## 1. Zero-Lua Native Built-In Tool Suite

All built-in tools live in `crates/tekvra-tools` and operate with zero Lua runtime initialization (`experimental.lua_plugins = false` by default and under `--no-plugins`):

- **Single-Editor Model Alignment**:
  - OpenAI GPT-5+, o-series, and Codex models receive `apply_patch` (`*** Begin Patch` / `*** Add File:` / `*** Update File:` / `*** Move to:` / `*** Delete File:` / `*** End Patch`) with whitespace-tolerant context seeking, deterministic per-path locks, and pre-mutation `tekvra-storage` snapshots.
  - Other model families receive `file_edit` + `file_write`.
- **Workspace Discovery & Code Outline**:
  - `file_read`, `file_glob`, and `file_grep` enforce output caps and skip `.git` as well as sensitive credential files (`.env`, `.env.*`, `*.pem`, `*.key`, `id_rsa`, `id_ed25519`, `.npmrc`, `.pypirc`, `.netrc`) during scans, reporting `truncated` and `skipped_sensitive_files` explicitly.
  - `file_index` extracts structural symbol outlines with exact 1-based line ranges via native `tree-sitter` grammars (Rust, Python, Go, JavaScript, TypeScript/TSX, Bash).
- **Interactive PTY Engine (`exec_command` & `write_stdin`)**:
  - Commands run inside a real pseudo-terminal (`portable-pty`) with a sanitized environment (`PATH`, `HOME`, `USER`, `LANG`, `TERM`, `TMPDIR` plus `AgentSettings` passthroughs/overrides).
  - Commands awaiting stdin or exceeding `yield_time_ms` return a live `session_id` receipt without restarting, allowing `write_stdin`, `resize`, `terminate`, and background completion notifications.
- **RTK-Derived Output Filter & `tool_output`**:
  - Terminal `\r` progress bars decode into single rows (`render_terminal`), ANSI escapes strip cleanly, and command-aware rules reduce build/test noise while **never** masking non-zero exit codes as success.
  - Full raw outputs are retained on disk and indexed in `state.db` for regex/line retrieval via `tool_output` and Raw/Filtered toggling in the TUI.
- **Isolated Monty Python `code` & Prompt-Cache-Stable MCP Gateway**:
  - `code` executes inside `monty` with zero direct host OS, filesystem, environment, or network access.
  - `ConfiguredMcpGateway` connects enabled `[mcp_servers.<name>]` entries over child-process `stdio` JSON-RPC 2.0 (`initialize`, `notifications/initialized`, `tools/list`, `tools/call`) and remote HTTP JSON-RPC 2.0 `POST` endpoints with bounded startup/call timeouts.
  - Connected MCP tools are listed (`mcp_list()`) and called (`await mcp_call(...)` / `mcp_call_sync(...)`) from `code`—and surfaced in `/mcp` and `mcpServerStatus/list`—without mutating top-level LLM `ToolDefinition` schemas when MCP servers connect or change.

## 2. Consent-Driven Execution & Safety Model (No Kernel Sandbox)

Tekvra intentionally omits local OS kernel sandboxes (Landlock/seccomp/Seatbelt) and instead enforces transparent, inspectable consent boundaries:

1. **Platform Shell Subcommand Decomposition**: Every `exec_command` invocation is decomposed into constituent subcommands and evaluated independently — compound lines like `git status && rm -rf build/` never authorize as a single blob. Unix (`/bin/bash` or `sh`) payloads parse via `tree-sitter-bash` across `&&`, `||`, `;`, `|`, subshells, `$()`, and redirections. Windows (`pwsh.exe`/`powershell.exe -Command`) payloads parse with a native PowerShell lexical decomposition (`tekvra_tools::powershell_syntax`): statements split on `;`, newlines, `|`, `&&`, `||` (including inside `(...)`); `{...}` script blocks stay opaque and turn destructive if they contain cmdlets like `Remove-Item`/`Set-Content`/`Invoke-Expression`; call operators on literal paths decompose while `& $var`, `iex`, `$()`, splatting, and nested `-Command` payloads are dynamic; `>`, `>>`, `2>`, `*>` redirect targets surface as write targets (`2>&1` merges are inert). Dynamic or unparseable constructs on either shell allow `Once`-only approval (fail-safe); PS read-only allowlisting (e.g. `Get-ChildItem`, `Select-String`, `git status`, numeric ranges) is case-insensitive.
2. **Execution Modes**:
   - `Plan` / `ReadOnly`: Read-only workspace inspection and (`Plan` only) `plan` updates; file writes and non-read-only commands are rejected before touching disk.
   - `Build`: Workspace file mutations allowed; unapproved shell commands, external paths, `webfetch`, and MCP calls prompt.
   - `Auto`: Skips routine unmatched prompts within workspace boundaries while enforcing `deny`/`ask` rules, external path prompts, and `Once` prompts for dynamic scripts.
   - `Yolo`: Skips interactive prompts while **unconditionally enforcing explicit `deny` rules**.
3. **Scoped Approval Ladders**: Prompts present narrow-to-broad scopes (`ExactCommand` -> `CommandPrefix` rungs, `PathPrefix`, `UrlPrefix`, `McpTool`) across `Once`, `Conversation`, `Project`, and `Global` lifetimes. Broad grants (such as 1-token `cargo *` or paths outside `$HOME`) require secondary confirmation (`confirmed_broad = true`).
4. **SSRF-Safe `webfetch`**: Resolves DNS and validates every redirect hop to block loopback (`127.0.0.0/8`, `::1`), link-local/cloud metadata (`169.254.0.0/16`, `fe80::/10`), and private RFC1918/ULA ranges unless allowlisted in global config.

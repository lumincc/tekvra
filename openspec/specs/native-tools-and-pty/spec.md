# native-tools-and-pty Specification

## Purpose

Defines Tekvra's 100% native built-in tool suite, model-specific editor selection (`apply_patch` vs `file_edit`), interactive PTY command execution with output filtering and pagination, structural code indexing, and isolated Python code mode for computation and MCP orchestration.

## Requirements

### Requirement: Zero-Lua native built-in tool registry
The system SHALL provide all built-in tools natively without requiring the Lua runtime, emitting structured tool outputs that render, fold, and restore from session storage without executing plugin scripts.

#### Scenario: Built-in tools operate with `--no-plugins`
- **WHEN** `tekvra` runs with `experimental.lua_plugins = false` or `--no-plugins`
- **THEN** all built-in file, patch, PTY shell, search, index, web, plan, question, image, code, batch, and subagent tools remain available and fully functional

### Requirement: Model-aligned single code editor (`apply_patch` vs `file_edit`)
The system SHALL expose exactly one primary code-editing tool per model: OpenAI GPT-5+, o-series, and Codex model families MUST receive `apply_patch` (`*** Begin Patch` / `*** Add File:` / `*** Delete File:` / `*** Update File:` / `*** Move to:` / `*** End Patch`), while other model families receive `file_edit` + `file_write`.

#### Scenario: Multi-file patch with file move on a Codex model
- **WHEN** a Codex-family model invokes `apply_patch` containing an `Add File`, an `Update File` with `Move to`, and a `Delete File` section inside the workspace
- **THEN** the system validates all target paths before mutating disk, applies hunks with whitespace-tolerant context seeking, records pre-mutation snapshots, and reports structured per-file diffs

### Requirement: Interactive PTY execution (`exec_command` and `write_stdin`) with background promotion
The system SHALL execute shell commands inside a pseudo-terminal (PTY) with a sanitized environment, returning completed output directly for short-lived commands and returning an active `session_id` with partial output when a command remains running or requires interactive stdin so the model or user can send input via `write_stdin`, resize, or terminate it.

#### Scenario: Interactive process prompt handling
- **WHEN** `exec_command` launches a process that prompts on stdout/stderr without exiting within its initial yield window
- **THEN** the tool returns a running receipt with `session_id` and captured terminal output, and a subsequent `write_stdin` call with that `session_id` delivers characters to the PTY and returns newly emitted output

#### Scenario: Long foreground build transitions to background
- **WHEN** a command exceeds the foreground wait threshold or is started in background mode
- **THEN** the command continues running without restart, returns a receipt to the agent, and delivers a completion notification at a safe turn boundary when it exits

### Requirement: Built-in command-aware output filtering and retained output pagination
The system SHALL decode terminal carriage-return progress bars into single rows and apply command-aware RTK-style noise reduction to model-facing shell outputs while preserving full raw output on disk for inspection in the UI and on-demand regex/line retrieval via `tool_output`.

#### Scenario: Failing test suite output reduction
- **WHEN** a test or build command exits with a non-zero code and produces thousands of lines of output
- **THEN** the filtered result never reports failure as success, retains failure diagnostics and head/tail context, appends a `[filtered: ...]` summary with an `output_id`, and allows `tool_output` to search or page the retained raw log

### Requirement: Structural code outline and bounded workspace search
The system SHALL provide `file_index` to extract syntax-tree symbol outlines with exact line ranges across supported languages, alongside `file_read`, `file_glob`, and `file_grep` that skip `.git` and credential files (`.env*`, private keys, `.npmrc`, `.netrc`) during directory scans while reporting explicit truncation metadata when scan limits are reached.

#### Scenario: Directory grep encounters credential files and scan cap
- **WHEN** `file_grep` searches a workspace directory containing `.env` and exceeding the match cap
- **THEN** `.env` is skipped during directory traversal and the result returns collected matches together with an explicit indicator of withheld scan scope

### Requirement: Isolated Python `code` mode and prompt-cache-stable MCP gateway
The system SHALL provide an isolated `code` tool backed by a sandboxed Python subset interpreter (no direct host filesystem, network, env, or subprocess access) through which the model can perform computation and invoke connected MCP server tools asynchronously without mutating top-level LLM tool definitions when MCP servers connect or change.

#### Scenario: Calling multiple MCP tools from one `code` snippet
- **WHEN** an MCP server is connected and the model executes a Python snippet calling multiple MCP tools concurrently
- **THEN** top-level LLM tool definitions remain unchanged across turns, each outbound MCP call is authorized individually against permission rules, and only printed/returned summary values enter the conversation context

### Requirement: Configured stdio and HTTP JSON-RPC MCP client gateway
The system SHALL connect enabled `[mcp_servers.<name>]` entries from trusted `tekvra.toml` configuration over child-process `stdio` JSON-RPC 2.0 (`initialize`, `notifications/initialized`, `tools/list`, `tools/call`) and remote HTTP JSON-RPC 2.0 `POST` endpoints, exposing discovered tools through `McpGateway` to `code` (`mcp_list` / `mcp_call`) and `app-server` (`mcpServerStatus/list`) without mutating top-level LLM tool schemas.

#### Scenario: Stdio MCP server discovered and invoked through `code`
- **WHEN** trusted `tekvra.toml` enables a stdio MCP server with `command` and `args` and a session runs Python `code` calling `mcp_list()` and `await mcp_call(server, tool, params)`
- **THEN** the gateway completes the MCP `initialize` handshake, caches `tools/list` descriptors, authorizes and dispatches `tools/call` over stdio JSON-RPC, and leaves top-level LLM `ToolDefinition`s unchanged

#### Scenario: HTTP JSON-RPC MCP server invoked and reported in `app-server`
- **WHEN** trusted `tekvra.toml` enables an HTTP MCP server with `url` and a client calls `mcpServerStatus/list` or invokes `mcp_call`
- **THEN** the gateway discovers tools via JSON-RPC `POST` to `url`, executes `tools/call` over HTTP, and reports the server's live tool map in `mcpServerStatus/list`

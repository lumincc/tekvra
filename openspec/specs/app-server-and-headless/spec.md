# app-server-and-headless Specification

## Purpose

Defines Tekvra's wire-compatible Codex `app-server` JSON-RPC 2.0 daemon/stdio protocol for desktop and IDE hosts, alongside headless `tekvra exec` (`--print` / `--output-schema` / `stream-json`) and `tekvra acp` editor integration.

## Requirements

### Requirement: Codex-compatible `app-server` JSON-RPC 2.0 transport and handshake
The system SHALL provide `tekvra app-server` supporting `--listen stdio://` (default `--stdio`), `unix://[PATH]`, and `ws://IP:PORT` (with capability/bearer token auth options), implementing the Codex `app-server` v2 JSON-RPC 2.0 `initialize` request and `initialized` notification handshake.

#### Scenario: Desktop or IDE client connects over stdio or WebSocket
- **WHEN** a Codex-compatible client starts `tekvra app-server --listen stdio://` and sends `initialize` followed by `initialized`
- **THEN** `tekvra` responds with server capabilities and version metadata matching the Codex v2 `InitializeResponse` schema and begins accepting thread, turn, config, and command requests

### Requirement: Core v2 thread, turn, review, command, and host query methods
The system SHALL implement Codex `app-server` v2 client requests covering thread lifecycle and history management (`thread/*`), turn execution and steering (`turn/start`, `turn/steer`, `turn/interrupt`), code review (`review/start`), interactive PTY sessions (`command/exec*`), and workspace/host inspection (`model/list`, `modelProvider/list`, `skills/list`, `mcpServerStatus/list`, `config/*`, `account/*`, `fuzzyFileSearch`).

#### Scenario: Client resumes and steers a stored thread
- **WHEN** an `app-server` client calls `thread/list`, resumes a session with `thread/resume`, starts a turn via `turn/start`, and sends mid-turn guidance via `turn/steer`
- **THEN** the server restores the thread history, injects the steering message at the next safe turn boundary, and completes the turn without dropping state

### Requirement: Streaming v2 item lifecycle, delta, diff, and plan notifications
The system SHALL emit Codex `app-server` v2 JSON-RPC server notifications for thread and turn transitions, typed item lifecycles (`item/started`, `item/completed`), incremental deltas (`item/agentMessage/delta`, `item/reasoning/summaryTextDelta`, `item/reasoning/textDelta`, `item/commandExecution/outputDelta`, `item/fileChange/patchUpdated`), and turn-level `turn/diff/updated` and `turn/plan/updated` events.

#### Scenario: Full turn lifecycle with streaming items and diff updates over `app-server`
- **WHEN** an `app-server` client calls `thread/start` and `turn/start` with user input
- **THEN** the server emits `thread/started`, `turn/started`, streaming `item/*` lifecycle and delta notifications (`userMessage`, `reasoning`, `commandExecution`, `fileChange`, `agentMessage`), `turn/diff/updated` when files change, and `turn/completed` with token usage updates

### Requirement: Sandbox policy mapping and bidirectional server approval requests
The system SHALL map Codex `sandboxPolicy` and `approvalPolicy` inputs onto Tekvra's `Plan`/`ReadOnly`, `Build`, `Auto`, and `YOLO` modes (emitting a `configWarning` notification when a restrictive OS sandbox is requested) and SHALL issue bidirectional JSON-RPC server requests (`item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, `item/tool/requestUserInput`, `mcpServer/elicitation/request`) whenever interactive consent or user answers are needed.

#### Scenario: Client approves a shell command over `app-server`
- **WHEN** a `Build`-mode turn on `app-server` invokes an unapproved shell command under `approvalPolicy = "untrusted"` or `"on-request"`
- **THEN** the server pauses that tool call, sends `item/commandExecution/requestApproval` to the client, and executes or denies the command according to the client's JSON-RPC response

#### Scenario: Client requests `read-only` sandbox policy
- **WHEN** an `app-server` client starts a thread or turn with `sandbox = "read-only"`
- **THEN** Tekvra enforces read-only tool restrictions (`Plan`/`ReadOnly` policy blocking file writes and unverified shell commands) and emits a `configWarning` stating that isolation is enforced via tool/AST consent rather than an OS kernel sandbox

### Requirement: Non-interactive `tekvra exec` / `--print` and `tekvra acp` modes
The system SHALL provide non-interactive execution via `tekvra exec` and `tekvra --print` (supporting plain text, JSONL events, `--output-schema <PATH>` structured final output validation, and Claude Code-compatible `--input-format stream-json` / `--output-format stream-json` SDK streams) as well as `tekvra acp` for Agent Client Protocol editors.

#### Scenario: Structured JSON schema output in CI via `tekvra exec`
- **WHEN** a CI script runs `tekvra exec --output-schema schema.json "summarize failing modules"`
- **THEN** `tekvra` runs non-interactively, validates the final assistant response against `schema.json`, writes the conforming JSON to stdout, and exits zero (or non-zero on failure)

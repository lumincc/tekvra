# Design

## Context

`tekvra` starts from an empty Git repository (`README.md` only), with two reference codebases checked into `.tmp/` for local architectural study and Apache-2.0-compatible porting, plus `codex-cli 0.172.0` available as the reference wire specification for `app-server`:

1. **`.tmp/maki` (`v0.7.3`, MIT, ~344K LOC)**: A fast Rust TUI coding agent that completed a 1,117-commit modernization after Caudra's fork point (`d2d6e75`), standardizing on **`tokio` + `reqwest` (`rustls` + `aws-lc-rs`)**, append-only prompt caching (`Frame` + `assert_append_only`), and OpenAI `/responses` + ChatGPT/Codex OAuth. However, Maki implements all 16+ built-in tools in Luau (`plugins/`), requires `init.lua` at startup, lacks `apply_patch`, lacks interactive PTY tools, lacks file-revert snapshots, and has no Codex `app-server` protocol.
2. **`.tmp/caudra` (`v0.2.0-preview.17`, Apache-2.0 + MIT, ~1.22M LOC)**: Forked from Maki at `d2d6e75`. Caudra proved out native Rust built-in tools (`workcell/*` + `caudra-agent/src/tools/native`), `apply_patch` (`workcell/crates/mcp-files/src/patch.rs`), OpenAI `reasoning.encrypted_content` stateless replay, RTK shell output filtering (`workcell/crates/output-filter`), automatic steering (`steering.rs`), `/goal` (`goal.rs`), atomic workspace snapshots (`workcell/crates/workspace-snapshot`), declarative self-migrating TOML config, and opt-in `experimental.lua_plugins`. However, Caudra retained pre-Tokio **`smol` + `isahc` (`static-curl`)** alongside a second `tokio` + `reqwest` runtime for Workcell, grew single files past 400–560 KB (`run.rs`, `headless.rs`), and bundled heavy non-goals (`rhai` workflows/automations, `tla-checker`, full-screen IDE `caudra-workbench`, `e2b-libvirt` VM manager, MPL-2.0 syntax assets).

Five explicit product/architectural constraints are locked:
1. **Codex `app-server` wire compatibility** (`stdio://`, `unix://`, `ws://`, Codex v2 JSON-RPC schema).
2. **Caudra-aligned Lua policy**: keep Luau extensibility (`tekvra-lua`), but ship **zero Lua built-ins**, start **no Lua VM by default**, gate external plugins behind global `experimental.lua_plugins = true`, and enforce `plugin.toml` capability manifests.
3. **No local OS kernel process sandbox** (no Landlock/seccomp/Seatbelt; rely on Bash AST permission decomposition + modes + folder trust, matching Maki and Caudra).
4. **Full-screen Alt-Screen `ratatui` TUI** (matching Maki/Caudra ergonomics).
5. **Apache-2.0 license**.

## Goals / Non-Goals

**Goals:**
- Keep total workspace scale lean (~120K–180K LOC target vs Caudra's 1.22M LOC), compiling cleanly on a **single `tokio` + `reqwest` (`rustls`) runtime** with fast incremental builds.
- Decouple the core agent/session runtime (`tekvra-core` + `tekvra-protocol`) from frontends so the **exact same engine** drives `tekvra-tui`, `tekvra-app-server` (Codex v2 JSON-RPC), `tekvra exec` (`--print`), and `tekvra-acp`.
- Preserve >94% prompt-cache hit rates on OpenAI and Anthropic by enforcing Maki's append-only frame invariant, Caudra's `DeveloperMessage` explicit cache breakpoints, and MCP-via-`code` / `tool_search` schema stability.
- Enforce `unsafe_code = "forbid"` at the workspace level (isolating any POSIX PTY/signal handling to safe crates such as `rustix` / `portable-pty`).

**Non-Goals:**
- Local OS kernel sandboxing (`Landlock`, `seccomp`, `bubblewrap`, `sandbox-exec`, Windows AppContainer) or remote `e2b-libvirt` VM orchestration (`caudra-sandbox`).
- In-terminal IDE workbench (`caudra-workbench`), Rhai scripting (`caudra-workflow`, `caudra-automation`, `caudra-script`), TLA+ model checking, external HTTP decision-engine screening (`caudra-decision`), or bundled third-party skill packs (`pstack`, `mattpocock`, `emil`).
- Inline non-alt-screen TUI viewport mode in v0.1.

## Decisions

### 1. Pure Virtual Cargo Workspace (`crates/tekvra-*`) vs Mixed Root Package
- **Decision**: Use a pure `[workspace]` virtual manifest (`resolver = "3"`, `edition = "2024"`) at the repository root with 11 focused crates under `crates/`, ordered in a strict 5-layer DAG:
  - **L1 Foundation**: `tekvra-protocol` (internal & Codex v2 wire types, events, item schemas, error codes).
  - **L2 Infrastructure**: `tekvra-config` (declarative self-migrating TOML, profiles, `AGENTS.md` & skill discovery, `FeatureFlags`), `tekvra-storage` (JSONL rollout + `rusqlite` FTS5 index + workspace file snapshot store), `tekvra-providers` (Tokio/reqwest LLM providers, OAuth, multi-account rotation).
  - **L3 Execution & Extensions**: `tekvra-tools` (native Rust tools, `apply_patch`, PTY manager, RTK `output-filter`, tree-sitter `file_index`, Monty `code` bridge, Bash AST permission analyzer), `tekvra-lua` (Tokio-native `mlua` Luau host adapted from `maki-lua` with Caudra's slimmed boundary).
  - **L4 Domain Engine**: `tekvra-core` (append-only session loop, `Next`/`Guide`/`Replace` mailbox, automatic steering, `/goal`, `/review`, durable subagent supervisor, MCP client manager).
  - **L5 Frontends & Binary**: `tekvra-tui` (full-screen `ratatui` UI including syntax highlighting and Markdown rendering modules adapted from `maki-highlight` and `maki-markdown`, excluding Caudra's MPL-2.0 `rhai.sublime-syntax`), `tekvra-app-server` (Codex v2 JSON-RPC server over stdio/unix/ws), `tekvra-acp` (ACP stdio server), and `tekvra-cli` (`tekvra` binary).
- **Rationale**: Eliminates the mixed-root-package anti-pattern present in both Maki and Caudra (`src/` polluting root, `cargo check` building only root, coupled feature unification) and enforces compile-time boundary discipline (`tekvra-core` never depends on `ratatui` or `clap`).
- **Alternatives considered**: Keeping Maki's root-flat `maki-*` mixed root layout (rejected: poor Cargo workspace ergonomics and root clutter); importing Caudra's 51-crate `workcell/*` split verbatim (rejected: `workcell` exists in Caudra primarily to serve a standalone remote MCP server and `e2b-libvirt` VMs, which are non-goals here—collapsing local tool contracts into `tekvra-tools` and snapshot storage into `tekvra-storage` cuts ~20 crates of boilerplate).

### 2. Pure `tokio` + `reqwest` (`rustls`) Stack vs Caudra's `smol` + `isahc`
- **Decision**: Base all async execution, timers, child processes, channels, and HTTP/SSE/WebSocket streams on Maki's `tokio` + `reqwest` (`rustls` + `aws-lc-rs`) + `tokio-tungstenite` stack. Any code ported from Caudra (`responses.rs` extensions, `steering`, `goal`, `workspace-snapshot`) MUST be adapted to `tokio` primitives and `reqwest`.
- **Rationale**: Eliminates C build dependencies on `libcurl` and OpenSSL (`isahc`), halves binary bloat and thread-pool duplication, and integrates natively with `rmcp` (MCP), `axum`/`tokio-tungstenite` (`app-server` WebSocket transport), and `maki-lua`'s async Luau bridge.
- **Alternatives considered**: Forking Caudra directly and ripping out `smol`/`isahc` (rejected: Caudra's 1.22M LOC touches `smol`/`flume`/`isahc` across hundreds of files).

### 3. Caudra-Aligned Native Tools + Opt-In `tekvra-lua` Boundary
- **Decision**:
  1. Register all built-in tools from native Rust (`tekvra-tools` + `tekvra-core`), emitting typed `ToolOutput` enums (`Diff`, `Terminal`, `FileSlice`, `Outline`, `Search`, `SubagentCard`, `Plan`, `Question`) so `tekvra-tui` and `tekvra-app-server` render/serialize them directly in Rust.
  2. Port `maki-lua` (already Tokio-based) as `tekvra-lua`, stripping Maki's `maki-pack` git package manager and builtin-only shims (matching `caudra-lua`'s ~37K LOC surface), renaming the global Lua table to `tekvra`, requiring `experimental.lua_plugins = true` in global `tekvra.toml` before instantiating `PluginHost`, and requiring `plugin.toml` capability flags per plugin directory.
- **Rationale**: Gives default users instant zero-VM startup, typed crash-free session replay, and Codex-grade native tools while preserving full Luau extensibility for power users who want custom tools, slash commands, or keymaps.
- **Alternatives considered**: Keeping Maki's Lua built-in plugins (rejected: slower startup, untyped session restore, awkward PTY/snapshot integration); dropping Lua completely (rejected per user requirement #2).

### 4. Unified PTY Process Engine (`exec_command` + `write_stdin`) + Built-In RTK Output Filter
- **Decision**: Implement a Tokio-managed PTY session table in `tekvra-tools`:
  - `exec_command` spawns commands in a PTY (falling back to pipes on non-TTY-capable platforms if needed) with a sanitized environment (`TMPDIR`/`PATH`/`HOME`/`LANG` + configured `[agent.shell_env]` pass/set rules).
  - Commands completing within `yield_time_ms` return final output in one turn; longer or interactive commands stay registered under `session_id` (up to `agent.shell_foreground_limit_secs` before auto-backgrounding) and accept `write_stdin` / resize / terminate calls from both the LLM (`write_stdin` tool) and `app-server` (`command/exec/write`, `command/exec/resize`, `command/exec/terminate`).
  - Port Workcell's Apache-2.0 `output-filter` pipeline (ANSI/CR progress-bar virtual row decoder + RTK command-aware filter rules) directly into `tekvra-tools`, storing raw logs via `tekvra-storage` for `tool_output` pagination and TUI Raw/Filtered toggling.
- **Rationale**: Matches Codex's signature interactive shell capability (REPLs, test watchers, interactive prompts) while retaining Caudra's best-in-class token savings (~35–50% shell token reduction without external `rtk` binary).

### 5. Model-Routed `apply_patch` vs `file_edit` with Pre-Mutation File Snapshots
- **Decision**: Port Workcell's Apache-2.0 `patch.rs` (`*** Begin Patch` grammar + whitespace-tolerant `seek`) into `tekvra-tools`. The tool roster offers `apply_patch` exclusively to GPT-5+, o-series, and Codex models, and `file_edit` + `file_write` to all other models. Before applying any file mutation (`apply_patch`, `file_edit`, `file_write`), `tekvra-tools` acquires per-path locks and records content-addressed pre-images in `tekvra-storage`'s snapshot store.
- **Rationale**: GPT/Codex models are post-trained specifically on `*** Begin Patch` and degrade when forced into JSON-escaped `oldString`/`newString`; Claude models excel at `file_edit`. Offering one editor per model avoids schema bloat and tool confusion.

### 6. Event-Driven Session Actor Bridging TUI and Codex `app-server` v2
- **Decision**: Model each live conversation in `tekvra-core` as an async `SessionHandle` actor accepting `SessionCommand` (`StartTurn`, `SteerTurn`, `InterruptTurn`, `ResolveApproval`, `ResolveUserInput`, `RevertTo`, `SetGoal`, `SetMode`, `SwitchModel`) and broadcasting `SessionEvent` (`TurnStarted`, `ItemStarted`, `ItemDelta`, `ItemCompleted`, `ApprovalRequested`, `DiffUpdated`, `PlanUpdated`, `TokenUsageUpdated`, `TurnCompleted`).
  - `tekvra-tui` subscribes to `SessionEvent` directly in-process for zero-serialization 60 FPS rendering.
  - `tekvra-app-server` hosts a registry of `ThreadId -> SessionHandle` and translates 1:1 between `SessionCommand`/`SessionEvent` and Codex `app-server` v2 JSON-RPC (`ClientRequest`, `ServerNotification`, `ServerRequest`).
  - When a Codex `app-server` client passes `sandbox` / `sandboxPolicy` + `approvalPolicy`:
    - `read-only` -> `PermissionMode::Plan` (or `ReadOnly` for subagents/reviews)
    - `workspace-write` + `untrusted`/`on-request` -> `PermissionMode::Build` (routing shell/external prompts to `item/commandExecution/requestApproval` and `item/fileChange/requestApproval`)
    - `workspace-write` + `auto_review` -> `PermissionMode::Auto`
    - `danger-full-access` + `never` -> `PermissionMode::Yolo`
    - Non-`danger-full-access` requests emit a one-time `configWarning` notification informing the client that Tekvra enforces Bash AST + tool consent rather than OS kernel sandboxing.
- **Rationale**: Guarantees behavioral parity across TUI and external Codex desktop/IDE clients without duplicating agent logic, and validates protocol conformance directly against `codex app-server generate-json-schema` schemas in integration tests.
- **Alternatives considered**: Making `tekvra-tui` speak JSON-RPC over a local socket to `tekvra-app-server` for every frame (viable later for `--remote` TUI attach, which our `SessionCommand`/`SessionEvent` boundary leaves open, while keeping default local TUI startup instantaneous and daemon-free).

### 7. Hybrid Storage: Append-Only JSONL Rollouts + SQLite (`rusqlite` FTS5) Index
- **Decision**: Store each session's canonical message/event log as an append-only versioned JSONL file (like Maki and Codex rollouts—easy to inspect, export, stream, and recover after crashes) while maintaining a lightweight `state.db` SQLite database (`rusqlite` bundled + WAL + FTS5) for fast thread listing/pagination (`thread/list`), full-text history search (`/search` and `code` `history` module), usage ledger, remembered permission grants, and snapshot references.
- **Rationale**: Combines Maki's simple portable session files and Codex's rollout model with Caudra's sub-second search and metadata queries across 10k+ sessions, avoiding Caudra's 83K-LOC monolithic SQLite-for-everything migration burden.

## Risks / Trade-offs

- **[No OS kernel sandbox leaves approved shell commands unrestricted]** → Mitigate with strict tree-sitter Bash AST decomposition (splitting pipelines, `&&`, `;`, `$()`, subshells, and redirections), refusing wildcard grants on dynamic/unparseable scripts, enforcing folder trust before loading `.tekvra/`, and emitting explicit `configWarning` notices to `app-server` clients expecting kernel sandboxing.
- **[Codex `app-server` protocol surface is large (110 client methods in `0.172.0`) and evolving]** → Mitigate by implementing the core v2 lifecycle, thread/turn/item, approval, command/exec, review, model, config, account, and fuzzy-search methods required by desktop/IDE hosts as Tier-1 (validated against generated JSON Schemas in CI), while returning clean JSON-RPC capability flags / structured `UNSUPPORTED_CAPABILITY` errors for cloud-only OpenAI internal endpoints (`marketplace/*`, `windowsSandbox/*`, `thread/realtime/*` audio SDP).
- **[Porting modules from both Maki (`tokio`) and Caudra (`smol` + `Apache-2.0`) risks license or async-idiom drift]** → Mitigate by forbidding `smol`/`isahc`/`flume` in `deny.toml`, keeping `THIRD_PARTY_LICENSES/` + `NOTICE.md` audited from commit 1, and excluding Caudra's MPL-2.0 `rhai.sublime-syntax`.
- **[Luau GC safety invariants (`Drop` inside Luau GC sweep aborts)]** → Mitigate by preserving Maki's battle-tested `maki-lua/src/runtime.rs` `ScopeEnd` / `JobReaper` invariants and `dropped_by_collector_releases_its_hold` contract test unchanged.

## Migration Plan

Because `tekvra` is a greenfield repository bootstrapping from `.tmp/maki` and `.tmp/caudra`, implementation proceeds in **5 incremental, always-compiling phases** (detailed in `tasks.md`):
1. **Phase 1 — Workspace, Licensing & Declarative Config (`L1` + `tekvra-config` + `tekvra-storage` skeleton)**: Set up virtual `Cargo.toml`, `LICENSE` (Apache-2.0), `NOTICE.md`, `deny.toml`, `tekvra-protocol`, self-migrating TOML config, folder trust, `AGENTS.md` & skill loader.
2. **Phase 2 — Providers & Append-Only Core (`tekvra-providers` + `tekvra-core` base)**: Port `maki-providers` (Tokio/reqwest) + Caudra's OpenAI `/responses` `encrypted_content`, cache breakpoints, multi-account failover, and role-based model jobs; port append-only `Frame` + `History` + compaction.
3. **Phase 3 — Native Rust Tools, PTY, Snapshots & Permissions (`tekvra-tools` + `tekvra-storage`)**: Implement native file tools, `apply_patch` (`patch.rs`), interactive PTY (`exec_command` + `write_stdin`), RTK `output-filter`, `tool_output`, `file_index`, Monty `code` + MCP gateway, Bash AST permission engine, and workspace file snapshots/revert.
4. **Phase 4 — Steering, Subagents, `/goal`, `/review`, Full-Screen TUI & Opt-In Lua (`tekvra-core` + `tekvra-tui` + `tekvra-lua`)**: Port `maki-ui` full-screen Alt-Screen TUI onto native `ToolOutput` cards + Caudra scope-ladder prompts; add `Next`/`Guide`/`Replace`, automatic steering, durable subagents, `/goal`, `/review`, and opt-in `tekvra-lua`.
5. **Phase 5 — Codex `app-server` v2, `exec`, and `acp` (`tekvra-app-server` + `tekvra-acp` + `tekvra-cli`)**: Build stdio/unix/ws JSON-RPC v2 `app-server` (verified against Codex JSON Schemas), `tekvra exec` (`--output-schema` + `stream-json`), and `tekvra acp`.

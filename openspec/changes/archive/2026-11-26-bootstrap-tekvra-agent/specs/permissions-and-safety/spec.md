# Permissions and Execution Safety Specification

## Purpose

Defines Tekvra's consent-driven execution model without OS kernel sandboxing, covering shell AST command decomposition, session execution modes (`Plan`, `Build`, `Auto`, `YOLO`), scoped approval ladders, and outbound network safety.

## ADDED Requirements

### Requirement: Explicit non-sandboxed consent model with Bash AST decomposition
The system SHALL NOT apply OS kernel process sandboxing (such as Landlock, seccomp, or Seatbelt) to local shell commands; instead, it MUST parse every shell invocation into constituent subcommands using a Bash syntax tree prior to execution and evaluate every subcommand, redirection, and working directory independently against permission policy.

#### Scenario: Compound shell command containing a destructive subcommand
- **WHEN** the agent invokes `git status && rm -rf build/`
- **THEN** the permission evaluator decomposes the command into both `git status` and `rm -rf build/`, marks `git status` as read-only/covered if applicable, and requires explicit authorization for `rm -rf build/` rather than treating the line as `git *`

#### Scenario: Unparseable or dynamic inline shell script
- **WHEN** the agent invokes a shell construct whose subcommands cannot be statically proven (such as `eval` or `bash -c "$DYNAMIC"`)
- **THEN** the system refuses reusable pattern grants and offers only single-invocation (`Once`) approval or denial

### Requirement: Session execution modes (`Plan`, `Build`, `Auto`, `YOLO`)
The system SHALL enforce four session execution modes—`Plan` (read-only exploration and session plan updates only), `Build` (workspace file edits allowed; unapproved shell, external paths, web fetches, and MCP side effects prompt), `Auto` (routine unmatched prompts skipped while enforcing `deny`/`ask` rules and path boundaries), and `YOLO` (interactive prompts skipped while enforcing explicit `deny` rules).

#### Scenario: File mutation attempted in Plan mode
- **WHEN** a session is in `Plan` mode and the model calls `apply_patch` or `file_write` on a repository source file
- **THEN** the call is rejected before touching disk with a message instructing the model to record proposed changes in the session `plan` instead

#### Scenario: Explicit deny rule enforced even in Auto and YOLO modes
- **WHEN** a session is running in `Auto` or `YOLO` mode and a tool call matches an explicit `deny` rule in global or project `permissions.toml`
- **THEN** the system rejects the call immediately without prompting or executing it

### Requirement: Scoped approval ladders and lifetimes
The system SHALL bind remembered permission grants to validated tool contracts and typed scopes (exact command, token-prefix ladder, filesystem directory ladder, or URL prefix ladder) across four selectable lifetimes (`Once`, `Conversation`, `Project`, `Global`), requiring explicit secondary confirmation for broad grants (such as whole-program `cargo *`, any shell command, or paths outside `$HOME`).

#### Scenario: Narrowing or broadening a shell scope at prompt time
- **WHEN** a permission prompt is displayed for `cargo test -p tekvra-core`
- **THEN** the user can narrow or broaden the scope between exact command, `cargo test -p tekvra-core *`, `cargo test *`, and `cargo *` and choose `Once`, `Conversation`, or `Project` lifetime, with `cargo *` requiring broad-grant confirmation before persistence

### Requirement: Outbound web SSRF and proxy safety
The system SHALL block `webfetch` requests targeting loopback, link-local, cloud metadata, and private RFC1918/ULA IP ranges (after DNS resolution and across redirects) unless explicitly allowlisted in global configuration, and SHALL read outbound HTTP/HTTPS proxy environment variables strictly from the host environment rather than project `.env` files.

#### Scenario: Web fetch redirects to localhost or metadata IP
- **WHEN** `webfetch` requests a public URL that responds with an HTTP redirect to `http://169.254.169.254/` or `http://127.0.0.1:8080/`
- **THEN** the client aborts the redirect before connecting and returns a blocked-address error

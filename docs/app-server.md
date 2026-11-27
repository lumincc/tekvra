# Tekvra App-Server (`codex app-server` v2 Wire Compatibility)

`tekvra app-server` exposes a bidirectional JSON-RPC 2.0 daemon wire-compatible with OpenAI Codex `app-server` v2 so IDEs, desktop shells, and automation harnesses can drive Tekvra threads without custom forks.

## Transports & Capability Authentication

```bash
# Newline-delimited JSON-RPC over stdin/stdout (default)
tekvra app-server --listen stdio://

# Unix domain socket
tekvra app-server --listen unix:///tmp/tekvra.sock

# WebSocket server with mandatory Bearer capability token
tekvra app-server --listen ws://127.0.0.1:4500 --auth-token "$TEKVRA_WS_TOKEN"
```

When `--auth-token` is set on `ws://`, HTTP upgrade requests lacking `Authorization: Bearer <token>` are rejected with `401 Unauthorized`.

## Handshake & Methods

Clients MUST send `initialize` (followed by `initialized`) before invoking domain methods:

- **Lifecycle**: `thread/start`, `thread/resume`, `thread/fork`, `thread/list`, `thread/loaded/list`, `thread/read`, `thread/archive`, `thread/unarchive`, `thread/rollback`, `thread/compact/start`, `thread/name/set`
- **Turns & Review**: `turn/start`, `turn/steer` (mapped onto non-destructive `Guide` delivery), `turn/interrupt`, `review/start`
- **Interactive PTY**: `command/exec`, `command/exec/write`, `command/exec/resize`, `command/exec/terminate`
- **Discovery & Config**: `model/list`, `modelProvider/list`, `skills/list`, `mcpServerStatus/list`, `config/read`, `config/value/write`, `config/batchWrite`, `account/read`, `account/login/start`, `account/logout`, `account/rateLimits/read`, `fuzzyFileSearch`

## Policy Mapping & `configWarning` Disclosure

Tekvra intentionally omits local OS kernel process sandboxing (Landlock/seccomp/Seatbelt) and instead enforces safety via tree-sitter Bash AST command decomposition, permission modes (`Plan`, `Build`, `Auto`, `Yolo`), and folder trust:

| Codex `sandbox` / `sandboxPolicy` | Codex `approvalPolicy` / `approvalsReviewer` | Tekvra `PermissionMode` | Emits `configWarning` |
|---|---|---|---|
| `read-only` | Any | `Plan` | Yes |
| `workspace-write` | `untrusted` / `on-request` | `Build` | Yes |
| `workspace-write` | `never` or `auto_review` | `Auto` | Yes |
| `danger-full-access` | `never` | `Yolo` | No |

## Bidirectional Server Requests

During active turns, the server sends correlated JSON-RPC requests to the client and suspends tool execution until resolved:

- `item/commandExecution/requestApproval` (`accept` | `acceptForSession` | `decline` | `cancel`)
- `item/fileChange/requestApproval`
- `item/tool/requestUserInput`
- `mcpServer/elicitation/request`

### ACP `session/request_permission`

On every `SessionEvent::ApprovalRequested`, the ACP bridge sends a correlated `session/request_permission` request offering `allow_once`, `allow_always`, and `reject_once` options. Pending requests are tracked per JSON-RPC id; inbound client *response* frames (`{"id": ..., "result": ...}` with no `method`) resolve them:

- `{"outcome": {"outcome": "selected", "optionId": "allow_once"}}` approves once.
- `optionId: "allow_always"` approves with a project-lifetime grant.
- Any other outcome, a malformed result, `session/cancel`, or EOF **fails closed** with `Deny` (the approval task observes a dropped oneshot channel).

### Structured `diff` blocks

`ItemCompleted` events carrying `ToolOutput::Diff` emit `tool_call_update` notifications with `content: [{ "type": "diff", "path", "oldText", "newText" }]` (one per file, `kind: "edit"|"delete"`), driven by the `old_text`/`new_text` fields retained on `FileDiffEntry`.

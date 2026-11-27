# Usage, Cost, and Health

## `tekvra usage` — local spend ledger

Every turn's token/cost accounting is recorded in `<state_dir>/state.db` (table
`usage_ledger`, one row per turn per serving model; MCP-sampling calls attribute under
`<rpc_id>_mcp_sampling`). The `tekvra usage` subcommand reads this ledger statelessly —
no session, plugins, or network:

```bash
tekvra usage                       # grouped by model, whole ledger, human table
tekvra usage --days 7              # trailing 7-day window
tekvra usage --by day              # group by UTC day ('localtime' of the recording host)
tekvra usage --by session          # group by session id
tekvra usage --json                # machine-readable: { by, days, rows: [...] }
tekvra usage --csv out.csv         # full per-turn export (RFC 4180 quoting)
```

Table columns: `KEY / INPUT / CACHED / OUTPUT / REASON / COST` (4-decimal USD). An empty
ledger prints `no usage recorded.` Grouping rows sum tokens and cost; cost ordering is
descending.

## `tekvra memory` — project memory panel

```bash
tekvra memory                      # prints memory file path + current content
tekvra memory --append "note"      # appends one dated entry (- YYYY-MM-DD: note)
```

## `tekvra doctor` — read-only health checks

`tekvra doctor` diagnoses config/trust/provider/MCP/state problems without healing:

```bash
tekvra doctor            # human table: [ OK ] / [WARN] / [FAIL] per check
tekvra doctor --json     # {"checks": [{name, status, detail}...], "healthy": bool}
tekvra doctor --net      # additionally probes builtin provider endpoints (bounded TCP)
```

Checks: global config load (notices → warn), workspace trust, per-provider key-env
resolution + stored account names (never values), MCP server command PATH resolution,
`state.db` open + SQLite `integrity_check`, state-dir writability, and `git`/`cargo`
presence (warn-only). Exit code 1 when any check reports `FAIL`; a broken global config is
exactly what it diagnoses, so doctor runs before the strict config load.

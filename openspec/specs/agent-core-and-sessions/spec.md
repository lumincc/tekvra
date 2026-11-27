# agent-core-and-sessions Specification

## Purpose

Defines Tekvra's append-only agent execution loop, automatic model steering, user input delivery queues, durable steerable subagents, evidence-gated completion goals, dedicated code review workflows, and reversible session persistence with file snapshots.

## Requirements

### Requirement: Append-only prompt frame invariant
The system SHALL maintain strictly append-only message prefixes and immutable system/tool declarations within an active prompt frame during a session, injecting dynamic runtime updates (mode transitions, cwd changes, background task receipts, steering nudges) as trailing context entries rather than mutating earlier messages.

#### Scenario: Switching from Plan mode to Build mode mid-session
- **WHEN** a user approves a plan and switches the session from `Plan` to `Build`
- **THEN** all previously sent system, tool, user, and assistant messages remain byte-identical in the request prefix and the mode transition is appended at the tail of the transcript

### Requirement: Automatic steering nudges for stalled or looping turns
The system SHALL detect stalled or looping model replies—including empty responses after tool calls, statements of intent without tool invocation (`abandoned_turn`), and three consecutive identical failing tool calls—and SHALL inject bounded steering messages (or refuse the repeated call) while recording each nudge visibly in the session transcript.

#### Scenario: Model announces intent to run tests without calling a tool
- **WHEN** an assistant turn ends with prose stating it will now execute a command or edit a file without emitting any tool call
- **THEN** the runtime appends a visible steering nudge instructing the model to perform the announced action and continues the run within configured nudge budgets

### Requirement: `Next`, `Guide`, and `Replace` input delivery modes
The system SHALL allow users (in TUI and `app-server`) to submit messages while a turn is running under three explicit delivery intents: `Next` (queued until the current run completes), `Guide` (injected at the next safe turn boundary without cancelling in-flight tool execution), and `Replace` (cancels the in-flight turn, preserves partial tool output, and starts a new turn immediately).

#### Scenario: Guiding an active run mid-execution
- **WHEN** a user submits a message in `Guide` (`turn/steer`) mode while a long tool call is executing
- **THEN** the running tool finishes normally and the guidance message is delivered to the model immediately before its next provider request

### Requirement: Durable steerable subagents
The system SHALL allow spawning foreground and background subagents (`task`) with stable task IDs, isolated transcripts, model/role selection (`weak`/`fast`, `medium`/inherit, `strong`/`best`, or explicit model spec), live user or parent steering, and persistent histories that can be resumed across session reloads.

#### Scenario: Background subagent finishes after main agent goes idle
- **WHEN** a background subagent completes its task after the main agent's turn has ended
- **THEN** its summary report wakes the main session cleanly for a follow-up turn without polling loops

### Requirement: Evidence-gated `/goal` and first-class `/review` workflows
The system SHALL support `/goal <condition>` (evaluating transcript evidence via a separate tool-less evaluator pass until met or budget-capped) and `/review` / `tekvra review` (running a dedicated read-only code review pass over uncommitted changes, a base branch diff, or a specific commit).

#### Scenario: Unmet completion goal triggers continuation
- **WHEN** a session has an active `/goal "cargo test passes with zero warnings"` and the assistant claims completion before running `cargo test`
- **THEN** the goal evaluator reports insufficient transcript evidence and starts another work turn explaining what remains unverified

#### Scenario: Non-interactive branch code review
- **WHEN** a user runs `tekvra review --base main` (or invokes `review/start` / `/review`)
- **THEN** a read-only review pass inspects the diff against `main` and emits structured review findings with file paths, line spans, and severity

### Requirement: Hybrid session persistence with parent-linked forks and atomic file revert
The system SHALL persist sessions using append-only rollout logs indexed in SQLite (with FTS5 full-text history search and single-writer session leases) and SHALL capture workspace file snapshots before mutating tool calls so users can fork from any message or revert conversation history, workspace files, or both to any prior turn.

#### Scenario: Reverting conversation and workspace files to an earlier message
- **WHEN** a session modifies three files across two turns and the user triggers revert ("conversation + files") to the prompt before those turns
- **THEN** all three workspace files are atomically restored to their pre-turn contents (including removing newly created files) and the abandoned branch remains recoverable in parent-linked storage

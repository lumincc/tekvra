# Spec Delta

## ADDED Requirements

### Requirement: Interactive TUI slash commands, composer cursor editing, and opt-in Luau command dispatch
The system SHALL support interactive TUI slash commands for `/help`, `/model <provider/model>`, `/mode <plan|build|auto|yolo>`, `/mcp`, and `/review [uncommitted|base <branch>|commit <sha>]`, cursor-aware composer editing (`Left`, `Right`, `Home`, `End`, `Delete`, and newline insertion via `Ctrl-J` or `Alt-Enter` when idle), and execution of custom Luau slash commands registered via `tekvra.api.register_command`.

#### Scenario: Switching model and permission mode via TUI slash commands
- **WHEN** a user submits `/model anthropic/claude-sonnet-4-5` and `/mode plan` in the TUI composer
- **THEN** the TUI updates its status bar immediately and dispatches `SwitchModel` and `SetMode` commands to the active session

#### Scenario: Running custom Luau slash command when plugins are enabled
- **WHEN** `experimental.lua_plugins = true` and a loaded plugin registered `hello` via `tekvra.api.register_command`, and the user submits `/hello world` in the TUI
- **THEN** the TUI invokes the registered Luau command handler with `"world"` and renders the returned message as a visible `[plugin]` banner in the transcript

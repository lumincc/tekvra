# Spec Delta

## ADDED Requirements

### Requirement: Interactive TUI slash-command and `@file` fuzzy completion plus `/sessions` picker
The system SHALL display keyboard-navigable completion popups in the Alt-Screen TUI when the composer token at the cursor starts with `/` (matching built-in and active Luau slash commands) or `@` (matching workspace-relative non-sensitive files), and SHALL provide a `/sessions` modal to browse and switch/fork indexed sessions.

#### Scenario: Completing `@file` mention in the TUI composer
- **WHEN** a user types `@crates/tekvra-core` in the TUI composer and presses `Tab` or `Enter` on a highlighted popup candidate
- **THEN** the candidate workspace path is inserted at the UTF-8 cursor boundary without blocking terminal rendering and without surfacing `.git` or `.env*` secret paths

#### Scenario: Browsing recent sessions from `/sessions`
- **WHEN** a user submits `/sessions` in the TUI
- **THEN** an overlay modal lists recent indexed sessions ordered by `updated_at` and allows resuming or forking the selected session

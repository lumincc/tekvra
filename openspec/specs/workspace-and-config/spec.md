# workspace-and-config Specification

## Purpose

Defines Tekvra's licensing, configuration hierarchy, declarative TOML schema and self-migration behavior, project instruction and skill discovery, folder trust boundaries, and global experimental feature gates.

## Requirements

### Requirement: Apache-2.0 distribution and third-party attribution
The system SHALL be distributed under the Apache License 2.0 and MUST preserve upstream copyright and license notices for all adapted MIT and Apache-2.0 components, while excluding copyleft or MPL-licensed assets from the repository and release artifacts.

#### Scenario: Release attribution completeness
- **WHEN** the project is packaged or inspected at the repository root
- **THEN** root `LICENSE` contains the Apache License 2.0 text, `NOTICE.md` documents provenance for adapted upstream sources, and `THIRD_PARTY_LICENSES/` retains verbatim upstream license texts

### Requirement: Declarative versioned TOML configuration without Lua startup
The system SHALL load user and project settings from declarative TOML files (`tekvra.toml`, `permissions.toml`, and `providers.toml`) under the user config directory (`~/.config/tekvra/` by default, overridable via `TEKVRA_NAMESPACE` or `-c key=value` CLI overrides) and project `.tekvra/` directory without starting a Lua runtime.

#### Scenario: Fresh startup with default configuration
- **WHEN** a user starts `tekvra` without any `init.lua` or custom config file
- **THEN** the process initializes using built-in declarative defaults without instantiating a Lua virtual machine

#### Scenario: Dotted CLI config override
- **WHEN** a user passes `-c model="openai/gpt-5.4-codex"` or `--profile fast` on the command line
- **THEN** the specified profile and key-value overrides take precedence over project and global `tekvra.toml` values for that process only

#### Scenario: Automatic global config version migration with backup
- **WHEN** `tekvra` reads a global TOML config file declaring an older supported `version`
- **THEN** the system saves a backup copy into the state backup directory and atomically upgrades the global config to the current schema version before parsing

### Requirement: Project folder trust boundary
The system SHALL treat any newly opened project directory's `.tekvra/` configuration, `.env` files, project MCP definitions, and project Lua plugins as untrusted until the user explicitly grants trust for that canonical directory, except that project `deny` permission rules MUST apply immediately even when untrusted.

#### Scenario: Untrusted repository with project config and deny rules
- **WHEN** `tekvra` opens an untrusted repository containing `.tekvra/tekvra.toml`, `.tekvra/permissions.toml`, and `.tekvra/init.lua`
- **THEN** project `allow` rules, MCP servers, `.env` variables, and `init.lua` are withheld until folder trust is approved, while project `deny` rules in `.tekvra/permissions.toml` are enforced immediately

### Requirement: Hierarchical instruction and skill discovery
The system SHALL discover and merge `AGENTS.md` (and `AGENTS.override.md`) instruction files from global config and from the Git repository root down to the active working directory, and SHALL discover `SKILL.md` skill packages from global and project `.agents/skills/` and `.claude/skills/` directories.

#### Scenario: Nested working directory inside a repository
- **WHEN** a session starts in `<repo>/services/api` where both `<repo>/AGENTS.md` and `<repo>/services/api/AGENTS.md` exist
- **THEN** both instruction files are included in deterministic root-to-leaf order in the initial session instructions

### Requirement: Global-only experimental feature gates
The system SHALL keep experimental capabilities (including `lua_plugins`) disabled by default and SHALL allow enabling them only through the `[experimental]` table in the user's global `tekvra.toml` (or explicit CLI `--enable` flags), never from a project `.tekvra/tekvra.toml`.

#### Scenario: Project config attempts to enable Lua plugins
- **WHEN** a project `.tekvra/tekvra.toml` sets `experimental.lua_plugins = true` while global `tekvra.toml` leaves it unset or `false`
- **THEN** the system rejects or ignores the project-level experimental opt-in and keeps `lua_plugins` disabled

### Requirement: Automated multi-platform CI quality gates and tagged release CD pipeline
The repository SHALL include automated CI workflows enforcing formatting (`cargo fmt`), lint cleanliness (`cargo clippy -D warnings`), workspace test execution, dependency/license policy (`cargo deny`), and Apache-2.0 attribution completeness on pushes and pull requests, alongside a tagged release CD workflow (`v*`) that validates version/tag parity and packages checksummed binaries across Linux, macOS, and Windows.

#### Scenario: Pull request and main branch CI gate verification
- **WHEN** changes are pushed to `main` or opened in a pull request
- **THEN** the CI workflow checks formatting, Clippy warnings across all targets, cross-platform workspace builds/tests, `cargo-deny` license/ban rules, and packaging attribution metadata before passing the required `CI` gate

#### Scenario: Tagged release packaging validation
- **WHEN** a release tag `vX.Y.Z` is validated by the release pipeline
- **THEN** packaging verification confirms workspace `Cargo.toml` version parity, Apache-2.0 `LICENSE`/`NOTICE.md`/`THIRD_PARTY_LICENSES/` completeness, and produces SHA-256 checksummed release archives per target platform

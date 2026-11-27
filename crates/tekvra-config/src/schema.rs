//! Declarative TOML schema types for `tekvra.toml`, `permissions.toml`, and `providers.toml`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tekvra_protocol::{PermissionMode, RuleAction};

/// Current declarative TOML configuration schema version.
pub const CURRENT_CONFIG_VERSION: u32 = 2;

/// Global-only experimental feature flags (`[experimental]`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FeatureFlags {
    /// Enables loading external Luau plugins from `<config>/lua/` and trusted `<project>/.tekvra/lua/`.
    pub lua_plugins: bool,
    /// Global gate for MCP `sampling/createMessage` server→client requests (per-server
    /// `sampling = true` opts in additionally). Default off.
    pub mcp_sampling: bool,
}

/// Role-based model routing table (`[models]`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelJobRouting {
    /// Primary build/coding model (`provider/model`).
    pub main: Option<String>,
    /// Dedicated model used while in `Plan` mode.
    pub plan: Option<String>,
    /// Default subagent `task` model when no explicit spec is passed.
    pub subagent: Option<String>,
    /// Context compaction summarizer model.
    pub compact: Option<String>,
    /// Dedicated `/review` code review model.
    pub review: Option<String>,
    /// Tool-less `/goal` evaluator model.
    pub goal: Option<String>,
    /// Conversation title generator model.
    pub title: Option<String>,
    /// Lightweight/fast helper model.
    pub fast: Option<String>,
    /// Ordered fallback model specs for the `Main` role (consulted when the primary's
    /// provider is exhausted or dies with a non-retryable error; chain depth cap 3).
    pub fallback: Option<Vec<String>>,
}

/// Shell environment sanitization and foreground timeout configuration (`[agent]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    /// Default session permission mode on startup.
    pub default_mode: PermissionMode,
    /// Maximum automatic steering nudges per user turn.
    pub max_steering_nudges: u32,
    /// Maximum iterations for `/goal` evaluation loops.
    pub max_goal_iterations: u32,
    /// Seconds before a foreground shell command is promoted to background.
    pub shell_foreground_limit_secs: u64,
    /// Reasoning effort preference (`"low"`, `"medium"`, `"high"`, `"xhigh"`).
    pub reasoning_effort: Option<String>,
    /// Additional host environment variable names passed into PTY shells.
    pub shell_env_passthrough: Vec<String>,
    /// Explicit static environment variables injected into PTY shells.
    pub shell_env_set: BTreeMap<String, String>,
    /// Optional allowlisted CIDR/host entries permitted by `webfetch` even on private networks.
    pub webfetch_allow_private_hosts: Vec<String>,
    /// Opt-in background session titling (`JobRole::Title`) after the first completed turn.
    pub auto_title: bool,
    /// Maximum nesting depth for `task` subagents (`1` = children cannot spawn subagents).
    pub subagent_max_depth: u32,
    /// Optional `web_search` HTML endpoint override (default: DuckDuckGo HTML Lite).
    pub websearch_base_url: Option<String>,
    /// Optional cumulative spend cap in USD. Enforcement is at turn start; in-flight turns
    /// always complete.
    pub spend_limit_usd: Option<f64>,
    /// Aggregation window for the spend cap (`session` default, `day`).
    pub spend_limit_window: SpendWindow,
    /// Opt-in gate for `task { merge: "auto" }` worktree steering (default disabled:
    /// merge-back stays explicit). When enabled, `"auto"` resolves to a clean merge or a
    /// conflict report — never to `"discard"`.
    pub allow_task_auto_merge: bool,
}

/// Window over which the spend cap accumulates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpendWindow {
    /// Cumulative cost of the current session.
    #[default]
    Session,
    /// Cumulative cost since the start of the local calendar day.
    Day,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            default_mode: PermissionMode::Build,
            max_steering_nudges: 3,
            max_goal_iterations: 5,
            shell_foreground_limit_secs: 120,
            reasoning_effort: None,
            shell_env_passthrough: Vec::new(),
            shell_env_set: BTreeMap::new(),
            webfetch_allow_private_hosts: Vec::new(),
            auto_title: false,
            subagent_max_depth: 1,
            websearch_base_url: None,
            spend_limit_usd: None,
            spend_limit_window: SpendWindow::Session,
            allow_task_auto_merge: false,
        }
    }
}

/// Language-server registration (`[lsp.<id>]`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LspServerConfig {
    /// Server executable (empty/missing entries are ignored with a loader notice).
    pub command: Option<String>,
    /// Command-line arguments for the server process.
    pub args: Vec<String>,
    /// File extensions this server handles (empty = all files routed to it).
    pub languages: Vec<String>,
}

/// External MCP server configuration (`[mcp_servers.<name>]`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    /// Stdio command executable.
    pub command: Option<String>,
    /// Command-line arguments for stdio MCP server.
    pub args: Vec<String>,
    /// HTTP/SSE endpoint URL for remote MCP server.
    pub url: Option<String>,
    /// Optional environment variable holding a Bearer token for HTTP MCP requests.
    pub bearer_token_env_var: Option<String>,
    /// Optional startup/handshake timeout override in seconds.
    pub startup_timeout_sec: Option<u64>,
    /// Optional per-tool call timeout override in seconds.
    pub tool_timeout_sec: Option<u64>,
    /// Optional streaming HTTP header-phase budget override in seconds (default 60).
    pub http_header_timeout_sec: Option<u64>,
    /// Optional streaming HTTP per-chunk idle budget override in seconds (default 10).
    pub http_idle_timeout_sec: Option<u64>,
    /// Extra environment variables passed to stdio MCP server.
    pub env: BTreeMap<String, String>,
    /// Whether this server is enabled.
    pub enabled: bool,
    /// Streamable-HTTP session affinity: capture/echo/release `Mcp-Session-Id` (default on).
    pub session: Option<bool>,
    /// Per-server opt-in for `sampling/createMessage` server→client requests
    /// (also requires `[experimental] mcp_sampling = true`).
    pub sampling: bool,
}

/// Named profile overrides (`[profiles.<name>]` in `tekvra.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileOverride {
    /// Primary model override.
    pub model: Option<String>,
    /// Default mode override.
    pub default_mode: Option<PermissionMode>,
    /// Reasoning effort override.
    pub reasoning_effort: Option<String>,
    /// Role-based model routing overrides.
    pub models: ModelJobRouting,
}

/// Declarative `tekvra.toml` document structure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TekvraToml {
    /// Schema version (`CURRENT_CONFIG_VERSION`).
    pub version: u32,
    /// Default `provider/model` specification (defaults to `"openai/gpt-5.4-codex"`).
    pub model: String,
    /// Role-based model routing assignments.
    pub models: ModelJobRouting,
    /// Agent runtime settings.
    pub agent: AgentSettings,
    /// Configured MCP servers.
    pub mcp_servers: IndexMap<String, McpServerConfig>,
    /// Language servers (`[lsp.<id>]`) — trusted configuration only.
    pub lsp_servers: IndexMap<String, LspServerConfig>,
    /// Named configuration profiles selectable via `--profile <name>`.
    pub profiles: IndexMap<String, ProfileOverride>,
    /// Global-only experimental feature flags.
    pub experimental: FeatureFlags,
}

impl Default for TekvraToml {
    fn default() -> Self {
        Self {
            version: CURRENT_CONFIG_VERSION,
            model: "openai/gpt-5.4-codex".to_string(),
            models: ModelJobRouting::default(),
            agent: AgentSettings::default(),
            mcp_servers: IndexMap::new(),
            lsp_servers: IndexMap::new(),
            profiles: IndexMap::new(),
            experimental: FeatureFlags::default(),
        }
    }
}

/// Declarative permission rule entry in `permissions.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    /// Target tool or category (`"exec_command"`, `"file_write"`, `"webfetch"`, `"mcp"`, or `"*"`).
    pub tool: String,
    /// Glob/prefix pattern matched against command tokens, canonical path, URL, or `server/tool`.
    pub pattern: String,
    /// Effect (`allow`, `ask`, `deny`).
    pub action: RuleAction,
}

/// Declarative `permissions.toml` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsToml {
    /// Schema version.
    pub version: u32,
    /// Ordered permission rules (`deny` rules always win over `allow`/`ask`).
    pub rules: Vec<PermissionRule>,
}

impl Default for PermissionsToml {
    fn default() -> Self {
        Self {
            version: CURRENT_CONFIG_VERSION,
            rules: Vec::new(),
        }
    }
}

/// Named account entry under a provider (`[[providers.<id>.accounts]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAccountConfig {
    /// Account alias (e.g., `"primary"`, `"backup"`).
    pub name: String,
    /// Environment variable holding the API key for this account.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Inline or stored API key (redacted in `Debug` via wrapper in `tekvra-providers`).
    #[serde(default)]
    pub api_key: Option<String>,
    /// OAuth token file reference if backed by subscription login.
    #[serde(default)]
    pub oauth_profile: Option<String>,
}

/// Custom or built-in provider configuration entry in `providers.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderEntryConfig {
    /// Wire compatibility family (`"openai_responses"`, `"openai_chat"`, `"anthropic"`, `"gemini"`).
    pub kind: Option<String>,
    /// Base URL override.
    pub base_url: Option<String>,
    /// Default environment variable name for API key lookup.
    pub api_key_env: Option<String>,
    /// Optional override forcing vision (`MessagePart::Image`) support for this provider's models.
    pub supports_vision: Option<bool>,
    /// Configured accounts in failover priority order.
    pub accounts: Vec<ProviderAccountConfig>,
    /// Extra static HTTP headers.
    pub headers: BTreeMap<String, String>,
}

/// Declarative `providers.toml` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvidersToml {
    /// Schema version.
    pub version: u32,
    /// Provider id -> configuration.
    pub providers: IndexMap<String, ProviderEntryConfig>,
}

impl Default for ProvidersToml {
    fn default() -> Self {
        Self {
            version: CURRENT_CONFIG_VERSION,
            providers: IndexMap::new(),
        }
    }
}

/// Non-fatal diagnostic emitted during config resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigNotice {
    /// Short machine-readable code.
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
    /// Related file path if applicable.
    pub path: Option<PathBuf>,
}

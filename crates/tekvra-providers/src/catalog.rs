//! Built-in and custom provider catalog, model capabilities, and role-based model routing.

use std::collections::BTreeMap;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tekvra_config::{ProviderEntryConfig, ResolvedConfig};
use tekvra_protocol::PermissionMode;

use crate::ProviderError;

/// Wire protocol spoken by a provider endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireProtocol {
    /// OpenAI `/responses` endpoint (`store: false`, `reasoning.encrypted_content`).
    OpenAiResponses,
    /// OpenAI-compatible `/chat/completions` endpoint.
    OpenAiChatCompletions,
    /// Anthropic `/v1/messages` endpoint with explicit `cache_control` breakpoints.
    AnthropicMessages,
    /// Google Gemini `generateContent` / `streamGenerateContent` endpoint.
    GeminiGenerateContent,
}

/// Capabilities and editor/reasoning traits of a specific model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    /// Canonical `provider/model` identifier.
    pub spec: String,
    /// Provider slug (`"openai"`, `"anthropic"`, etc.).
    pub provider: String,
    /// Raw model id sent on the wire.
    pub model_id: String,
    /// Preferred wire protocol.
    pub protocol: WireProtocol,
    /// Whether this model uses `apply_patch` (`*** Begin Patch`) instead of `file_edit`.
    pub uses_apply_patch: bool,
    /// Whether this model supports stateless `reasoning.encrypted_content` replay.
    pub supports_encrypted_reasoning: bool,
    /// Supported reasoning effort tiers in ascending order.
    pub supported_efforts: Vec<String>,
    /// Whether explicit prompt-cache breakpoints are supported.
    pub supports_cache_breakpoints: bool,
    /// Whether this model accepts multimodal image (`MessagePart::Image`) user blocks.
    pub supports_vision: bool,
    /// Maximum context window in tokens.
    pub context_window: u64,
    /// USD per 1M uncached input tokens.
    pub input_cost_per_million: f64,
    /// USD per 1M cached input tokens.
    pub cached_input_cost_per_million: f64,
    /// USD per 1M output tokens.
    pub output_cost_per_million: f64,
}

impl ModelCapabilities {
    /// Resolves capabilities for any `provider/model` string.
    #[must_use]
    pub fn resolve(
        provider: &str,
        model_id: &str,
        protocol_override: Option<WireProtocol>,
    ) -> Self {
        let lower_model = model_id.to_ascii_lowercase();
        let is_openai_reasoning_or_codex = matches!(provider, "openai" | "github-copilot")
            && (lower_model.starts_with("gpt-5")
                || lower_model.starts_with("o1")
                || lower_model.starts_with("o3")
                || lower_model.starts_with("o4")
                || lower_model.contains("codex"));

        let default_protocol = match provider {
            "openai" | "github-copilot" if is_openai_reasoning_or_codex => {
                WireProtocol::OpenAiResponses
            }
            "anthropic" => WireProtocol::AnthropicMessages,
            "gemini" => WireProtocol::GeminiGenerateContent,
            _ => WireProtocol::OpenAiChatCompletions,
        };
        let protocol = protocol_override.unwrap_or(default_protocol);

        let supported_efforts = if is_openai_reasoning_or_codex {
            if lower_model.starts_with("gpt-5") || lower_model.contains("codex") {
                vec![
                    "low".to_string(),
                    "medium".to_string(),
                    "high".to_string(),
                    "xhigh".to_string(),
                ]
            } else {
                vec!["low".to_string(), "medium".to_string(), "high".to_string()]
            }
        } else if provider == "anthropic"
            && (lower_model.contains("claude-3-7")
                || lower_model.contains("claude-4")
                || lower_model.contains("sonnet-4")
                || lower_model.contains("opus-4"))
        {
            vec!["low".to_string(), "medium".to_string(), "high".to_string()]
        } else {
            Vec::new()
        };

        // Vision heuristics: GPT-4o/GPT-5/o-series/Codex, Claude 3.5+/4+, Gemini 1.5+/2.x+.
        let supports_vision = match provider {
            "openai" | "github-copilot" => {
                lower_model.starts_with("gpt-4o")
                    || lower_model.starts_with("gpt-5")
                    || lower_model.starts_with("o1")
                    || lower_model.starts_with("o3")
                    || lower_model.starts_with("o4")
                    || lower_model.contains("codex")
            }
            "anthropic" => {
                lower_model.contains("claude-3-5")
                    || lower_model.contains("claude-3.5")
                    || lower_model.contains("claude-4")
                    || lower_model.contains("sonnet-4")
                    || lower_model.contains("opus-4")
                    || lower_model.contains("haiku-4")
            }
            "gemini" => {
                lower_model.contains("gemini-1.5")
                    || lower_model.contains("gemini-2")
                    || lower_model.contains("gemini-3")
            }
            _ => false,
        };

        Self {
            spec: format!("{provider}/{model_id}"),
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            protocol,
            uses_apply_patch: is_openai_reasoning_or_codex,
            supports_encrypted_reasoning: protocol == WireProtocol::OpenAiResponses
                && is_openai_reasoning_or_codex,
            supported_efforts,
            supports_cache_breakpoints: matches!(
                protocol,
                WireProtocol::OpenAiResponses | WireProtocol::AnthropicMessages
            ),
            supports_vision,
            context_window: if provider == "gemini" {
                1_000_000
            } else {
                200_000
            },
            input_cost_per_million: 2.50,
            cached_input_cost_per_million: 0.25,
            output_cost_per_million: 10.00,
        }
    }

    /// Validates or clamps a requested reasoning effort against this model's capabilities.
    ///
    /// # Errors
    /// Returns [`ProviderError::UnsupportedReasoningEffort`] if the model supports reasoning
    /// efforts and the requested tier is unrecognized for that model.
    pub fn align_reasoning_effort(
        &self,
        requested: Option<&str>,
    ) -> Result<Option<String>, ProviderError> {
        let Some(req) = requested else {
            return Ok(self
                .supported_efforts
                .iter()
                .find(|e| e.as_str() == "medium")
                .cloned()
                .or_else(|| self.supported_efforts.first().cloned()));
        };
        if self.supported_efforts.is_empty() {
            return Ok(None);
        }
        let normalized = req.trim().to_ascii_lowercase();
        if self.supported_efforts.iter().any(|e| e == &normalized) {
            Ok(Some(normalized))
        } else {
            Err(ProviderError::UnsupportedReasoningEffort {
                model: self.spec.clone(),
                requested: req.to_string(),
                supported: self.supported_efforts.clone(),
            })
        }
    }
}

/// Definition of a built-in or user-configured LLM provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDefinition {
    /// Provider slug (`"openai"`, `"anthropic"`, `"gemini"`, etc.).
    pub id: String,
    /// Display name.
    pub display_name: String,
    /// Base HTTP URL without trailing slash.
    pub base_url: String,
    /// Default wire protocol.
    pub default_protocol: WireProtocol,
    /// Default environment variable holding an API key.
    pub default_api_key_env: Option<String>,
    /// Optional `providers.toml` override forcing `ModelCapabilities::supports_vision` for
    /// every model on this provider (`Some(true)` for local vision servers, `Some(false)` to deny).
    pub supports_vision_override: Option<bool>,
    /// Extra static HTTP headers.
    pub headers: BTreeMap<String, String>,
    /// Featured model identifiers for discovery (`model/list`).
    pub featured_models: Vec<String>,
}

/// Registry of all built-in and custom providers.
#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    providers: IndexMap<String, ProviderDefinition>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRegistry {
    /// Builds the built-in catalog covering OpenAI, Anthropic, Gemini, GitHub Copilot,
    /// xAI, DeepSeek, Mistral, Z.AI, OpenRouter, Ollama, and `llama.cpp`.
    #[must_use]
    pub fn new() -> Self {
        let mut providers = IndexMap::new();
        let builtins = [
            (
                "openai",
                "OpenAI",
                "https://api.openai.com/v1",
                WireProtocol::OpenAiResponses,
                Some("OPENAI_API_KEY"),
                vec!["gpt-5.4-codex", "gpt-5.4-mini", "o4-mini", "o3"],
            ),
            (
                "anthropic",
                "Anthropic",
                "https://api.anthropic.com/v1",
                WireProtocol::AnthropicMessages,
                Some("ANTHROPIC_API_KEY"),
                vec!["claude-sonnet-4-5", "claude-opus-4-5", "claude-haiku-4-5"],
            ),
            (
                "gemini",
                "Google Gemini",
                "https://generativelanguage.googleapis.com/v1beta",
                WireProtocol::GeminiGenerateContent,
                Some("GEMINI_API_KEY"),
                vec!["gemini-2.5-pro", "gemini-2.5-flash"],
            ),
            (
                "github-copilot",
                "GitHub Copilot",
                "https://api.githubcopilot.com",
                WireProtocol::OpenAiResponses,
                Some("GITHUB_COPILOT_TOKEN"),
                vec!["gpt-5.4-codex", "claude-sonnet-4-5"],
            ),
            (
                "xai",
                "xAI",
                "https://api.x.ai/v1",
                WireProtocol::OpenAiChatCompletions,
                Some("XAI_API_KEY"),
                vec!["grok-4", "grok-code-fast-1"],
            ),
            (
                "deepseek",
                "DeepSeek",
                "https://api.deepseek.com/v1",
                WireProtocol::OpenAiChatCompletions,
                Some("DEEPSEEK_API_KEY"),
                vec!["deepseek-chat", "deepseek-reasoner"],
            ),
            (
                "mistral",
                "Mistral",
                "https://api.mistral.ai/v1",
                WireProtocol::OpenAiChatCompletions,
                Some("MISTRAL_API_KEY"),
                vec!["codestral-latest", "mistral-large-latest"],
            ),
            (
                "zai",
                "Z.AI",
                "https://api.z.ai/api/paas/v4",
                WireProtocol::OpenAiChatCompletions,
                Some("ZAI_API_KEY"),
                vec!["glm-4.6"],
            ),
            (
                "openrouter",
                "OpenRouter",
                "https://openrouter.ai/api/v1",
                WireProtocol::OpenAiChatCompletions,
                Some("OPENROUTER_API_KEY"),
                vec!["openai/gpt-5.4-codex", "anthropic/claude-sonnet-4.5"],
            ),
            (
                "ollama",
                "Ollama",
                "http://127.0.0.1:11434/v1",
                WireProtocol::OpenAiChatCompletions,
                None,
                vec!["qwen3-coder:30b"],
            ),
            (
                "llamacpp",
                "llama.cpp",
                "http://127.0.0.1:8080/v1",
                WireProtocol::OpenAiChatCompletions,
                None,
                vec!["local-model"],
            ),
        ];

        for (id, name, url, proto, env_key, models) in builtins {
            providers.insert(
                id.to_string(),
                ProviderDefinition {
                    id: id.to_string(),
                    display_name: name.to_string(),
                    base_url: url.to_string(),
                    default_protocol: proto,
                    default_api_key_env: env_key.map(str::to_string),
                    supports_vision_override: None,
                    headers: BTreeMap::new(),
                    featured_models: models.into_iter().map(str::to_string).collect(),
                },
            );
        }
        Self { providers }
    }

    /// Merges user/project `providers.toml` overrides and custom endpoints into the registry.
    pub fn apply_custom_configs(&mut self, custom: &IndexMap<String, ProviderEntryConfig>) {
        for (id, entry) in custom {
            let proto = match entry.kind.as_deref() {
                Some("openai_responses") => Some(WireProtocol::OpenAiResponses),
                Some("openai_chat") => Some(WireProtocol::OpenAiChatCompletions),
                Some("anthropic") => Some(WireProtocol::AnthropicMessages),
                Some("gemini") => Some(WireProtocol::GeminiGenerateContent),
                _ => None,
            };

            if let Some(existing) = self.providers.get_mut(id) {
                if let Some(url) = &entry.base_url {
                    existing.base_url = url.trim_end_matches('/').to_string();
                }
                if let Some(p) = proto {
                    existing.default_protocol = p;
                }
                if let Some(env_name) = &entry.api_key_env {
                    existing.default_api_key_env = Some(env_name.clone());
                }
                existing.supports_vision_override = entry.supports_vision;
                for (hk, hv) in &entry.headers {
                    existing.headers.insert(hk.clone(), hv.clone());
                }
            } else {
                self.providers.insert(
                    id.clone(),
                    ProviderDefinition {
                        id: id.clone(),
                        display_name: id.clone(),
                        base_url: entry
                            .base_url
                            .as_deref()
                            .unwrap_or("http://127.0.0.1:8000/v1")
                            .trim_end_matches('/')
                            .to_string(),
                        default_protocol: proto.unwrap_or(WireProtocol::OpenAiChatCompletions),
                        default_api_key_env: entry.api_key_env.clone(),
                        supports_vision_override: entry.supports_vision,
                        headers: entry.headers.clone(),
                        featured_models: Vec::new(),
                    },
                );
            }
        }
    }

    /// Looks up a provider definition by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ProviderDefinition> {
        self.providers.get(id)
    }

    /// Iterates over all registered provider definitions.
    pub fn iter(&self) -> impl Iterator<Item = &ProviderDefinition> {
        self.providers.values()
    }

    /// Splits a `"provider/model"` spec (defaulting bare model ids to `"openai"`), validates
    /// the provider exists, and resolves [`ModelCapabilities`].
    ///
    /// # Errors
    /// Returns [`ProviderError::UnknownProvider`] if the provider prefix is unregistered.
    pub fn resolve_model_spec(
        &self,
        spec: &str,
    ) -> Result<(&ProviderDefinition, ModelCapabilities), ProviderError> {
        let (provider_id, model_id) = match spec.split_once('/') {
            Some((p, m)) => (p.trim(), m.trim()),
            None => ("openai", spec.trim()),
        };
        let def =
            self.providers
                .get(provider_id)
                .ok_or_else(|| ProviderError::UnknownProvider {
                    provider: provider_id.to_string(),
                })?;
        let protocol_override =
            (def.id != "openai" && def.id != "github-copilot").then_some(def.default_protocol);
        let mut caps = ModelCapabilities::resolve(&def.id, model_id, protocol_override);
        if let Some(vision) = def.supports_vision_override {
            caps.supports_vision = vision;
        }
        Ok((def, caps))
    }
}

/// Runtime job roles that can be independently bound to different models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobRole {
    /// Primary coding/build agent turns.
    Main,
    /// `Plan` mode turns.
    Plan,
    /// Subagent `task` turns when not overridden per-call.
    Subagent,
    /// Context compaction pass.
    Compact,
    /// Dedicated `/review` code review pass.
    Review,
    /// Tool-less `/goal` completion evaluator pass.
    Goal,
    /// Short conversation title generation.
    Title,
    /// Fast lightweight helper tasks.
    Fast,
}

impl JobRole {
    /// Configuration table key name.
    #[must_use]
    pub const fn as_key(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Plan => "plan",
            Self::Subagent => "subagent",
            Self::Compact => "compact",
            Self::Review => "review",
            Self::Goal => "goal",
            Self::Title => "title",
            Self::Fast => "fast",
        }
    }

    /// Selects [`JobRole::Plan`] when in `PermissionMode::Plan`, otherwise [`JobRole::Main`].
    #[must_use]
    pub const fn for_permission_mode(mode: PermissionMode) -> Self {
        match mode {
            PermissionMode::Plan => Self::Plan,
            PermissionMode::ReadOnly => Self::Review,
            PermissionMode::Build | PermissionMode::Auto | PermissionMode::Yolo => Self::Main,
        }
    }
}

/// Maximum depth of a configured `Main` fallback chain (primary + up to 2 fallbacks).
pub const FALLBACK_CHAIN_CAP: usize = 3;

/// Resolves job-specific models without mutating prior conversation history.
#[derive(Debug, Clone)]
pub struct ModelRouter {
    registry: ProviderRegistry,
    default_model: String,
    job_bindings: BTreeMap<JobRole, String>,
    main_fallbacks: Vec<String>,
}

impl ModelRouter {
    /// Builds a [`ModelRouter`] from a [`ResolvedConfig`] and [`ProviderRegistry`].
    #[must_use]
    pub fn from_config(config: &ResolvedConfig, registry: ProviderRegistry) -> Self {
        let mut job_bindings = BTreeMap::new();
        let r = &config.settings.models;
        let pairs = [
            (JobRole::Main, &r.main),
            (JobRole::Plan, &r.plan),
            (JobRole::Subagent, &r.subagent),
            (JobRole::Compact, &r.compact),
            (JobRole::Review, &r.review),
            (JobRole::Goal, &r.goal),
            (JobRole::Title, &r.title),
            (JobRole::Fast, &r.fast),
        ];
        for (role, opt) in pairs {
            if let Some(spec) = opt {
                job_bindings.insert(role, spec.clone());
            }
        }
        let mut main_fallbacks = r.fallback.clone().unwrap_or_default();
        main_fallbacks.truncate(FALLBACK_CHAIN_CAP);
        Self {
            registry,
            default_model: config.settings.model.clone(),
            job_bindings,
            main_fallbacks,
        }
    }

    /// Ordered fallback chain for `role` (primary first): only the `Main` role carries a
    /// configured chain; every other role fails loudly rather than silently degrading.
    #[must_use]
    pub fn fallback_chain_for_job(&self, role: JobRole) -> Vec<String> {
        if role != JobRole::Main {
            return Vec::new();
        }
        let primary = self
            .job_bindings
            .get(&JobRole::Main)
            .unwrap_or(&self.default_model)
            .clone();
        let mut chain = vec![primary];
        for spec in &self.main_fallbacks {
            if !chain.contains(spec) {
                chain.push(spec.clone());
            }
        }
        chain
    }

    /// Resolves an arbitrary model spec (fallback chains use this to hop models).
    ///
    /// # Errors
    /// Returns [`ProviderError`] if the spec references an unknown provider.
    pub fn route_spec(
        &self,
        spec: &str,
    ) -> Result<(&ProviderDefinition, ModelCapabilities), ProviderError> {
        self.registry.resolve_model_spec(spec)
    }

    /// Updates the session's active `Main` model spec in memory.
    pub fn set_active_main_model(&mut self, model_spec: impl Into<String>) {
        let spec = model_spec.into();
        self.default_model.clone_from(&spec);
        self.job_bindings.insert(JobRole::Main, spec);
    }

    /// Resolves the model capabilities and provider definition for `role`.
    ///
    /// # Errors
    /// Returns [`ProviderError`] if the configured model spec references an unknown provider.
    pub fn route(
        &self,
        role: JobRole,
    ) -> Result<(&ProviderDefinition, ModelCapabilities), ProviderError> {
        let spec = self
            .job_bindings
            .get(&role)
            .or_else(|| self.job_bindings.get(&JobRole::Main))
            .unwrap_or(&self.default_model);
        self.registry.resolve_model_spec(spec)
    }

    /// Resolves the model for an interactive turn under `mode` (`Plan` vs `Build`/`Auto`/`Yolo`).
    ///
    /// # Errors
    /// Returns [`ProviderError`] if the configured model spec references an unknown provider.
    pub fn route_for_mode(
        &self,
        mode: PermissionMode,
    ) -> Result<(&ProviderDefinition, ModelCapabilities), ProviderError> {
        let role = match mode {
            PermissionMode::Plan => JobRole::Plan,
            PermissionMode::ReadOnly
            | PermissionMode::Build
            | PermissionMode::Auto
            | PermissionMode::Yolo => JobRole::Main,
        };
        self.route(role)
    }

    /// Borrows the underlying [`ProviderRegistry`].
    #[must_use]
    pub const fn registry(&self) -> &ProviderRegistry {
        &self.registry
    }
}

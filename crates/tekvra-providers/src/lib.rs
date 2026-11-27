//! Tokio + Reqwest (`rustls`) multi-provider LLM client, stateless OpenAI `/responses` driver,
//! ChatGPT/Codex OAuth, multi-account quota failover, and role-based model routing.

pub mod auth;
pub mod catalog;
pub mod driver;

use thiserror::Error;

pub use auth::{
    AccountCandidate, AccountPin, AccountPool, CredentialStore, DeviceCodeSession,
    LoopbackOAuthListener, OAuthProviderEndpoints, SecretToken, StoredOAuthAccount,
    build_oauth_http_client, exchange_oauth_token, extract_jwt_account_metadata,
    generate_pkce_pair, poll_device_code_token, refresh_oauth_account, request_device_code,
};
pub use catalog::{
    JobRole, ModelCapabilities, ModelRouter, ProviderDefinition, ProviderRegistry, WireProtocol,
};
pub use driver::{
    CompletionRequest, CompletionResponse, HttpProviderDriver, MessagePart, MessageRole,
    ProviderDriver, ProviderMessage, StreamDelta, ToolDefinition, build_anthropic_messages_payload,
    build_chat_completions_payload, build_openai_responses_payload, parse_openai_responses_body,
};

/// Errors emitted by `tekvra-providers`.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// Referenced provider slug is not registered.
    #[error("unknown LLM provider `{provider}`")]
    UnknownProvider {
        /// Unrecognized provider id.
        provider: String,
    },
    /// Requested reasoning effort is unsupported by the target model.
    #[error(
        "model `{model}` does not support reasoning effort `{requested}` (supported: {supported:?})"
    )]
    UnsupportedReasoningEffort {
        /// Target model spec.
        model: String,
        /// Requested effort string.
        requested: String,
        /// Allowed effort strings.
        supported: Vec<String>,
    },
    /// Malformed `--account PROVIDER:NAME` pin string.
    #[error("invalid account pin `{raw}`; expected `PROVIDER:NAME`")]
    InvalidAccountPin {
        /// Raw input string.
        raw: String,
    },
    /// No healthy or pinned account is eligible for the provider.
    #[error("no eligible account for provider `{provider}`: {reason}")]
    NoEligibleAccount {
        /// Provider slug.
        provider: String,
        /// Explanation.
        reason: String,
    },
    /// OAuth flow or token refresh error.
    #[error("OAuth authentication failed: {message}")]
    OAuth {
        /// Error detail.
        message: String,
    },
    /// Credential storage I/O failure.
    #[error("credential store error: {message}")]
    CredentialIo {
        /// Error detail.
        message: String,
    },
    /// Network or TLS transport failure.
    #[error("transport error talking to `{provider}`: {message}")]
    Transport {
        /// Provider slug.
        provider: String,
        /// Error detail.
        message: String,
    },
    /// Non-2xx HTTP status returned by provider.
    #[error("provider `{provider}` returned HTTP {status}: {body}")]
    HttpStatus {
        /// Provider slug.
        provider: String,
        /// HTTP status code.
        status: u16,
        /// Raw error body.
        body: String,
    },
    /// Response JSON or SSE decoding failure.
    #[error("failed to decode `{provider}` response: {message}")]
    Decode {
        /// Provider slug.
        provider: String,
        /// Error detail.
        message: String,
    },
}

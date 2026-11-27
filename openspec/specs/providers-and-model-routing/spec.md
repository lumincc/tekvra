# providers-and-model-routing Specification

## Purpose

Defines Tekvra's multi-provider LLM connectivity, stateless OpenAI Responses API integration with encrypted reasoning and prompt-cache affinity, subscription OAuth and multi-account quota failover, and role-based model routing.

## Requirements

### Requirement: Stateless OpenAI Responses API with encrypted reasoning and cache affinity
The system SHALL communicate with OpenAI GPT-5+, o-series, and Codex models through the `/responses` endpoint with `store: false`, requesting `reasoning.encrypted_content` and replaying encrypted reasoning items across multi-turn tool loops while sending stable per-conversation `prompt_cache_key` and session-affinity headers and placing cacheable system instructions in the model-appropriate developer/instructions channel with explicit cache breakpoints where supported.

#### Scenario: Multi-step tool turn on a Codex or GPT reasoning model
- **WHEN** a session executes tool calls across multiple steps within or across turns on an OpenAI Responses model
- **THEN** each outbound request includes `include: ["reasoning.encrypted_content"]`, carries the conversation's stable `prompt_cache_key` and `session-id` header, and replays prior assistant encrypted reasoning items verbatim so reasoning continuity and prefix cache hits are preserved

#### Scenario: Reasoning effort and summary alignment
- **WHEN** a user selects a model and configures or cycles reasoning effort
- **THEN** the system validates the requested effort level against the model's declared capabilities and requests `reasoning.summary = "auto"` when reasoning is active

### Requirement: Multi-provider catalog and authentication methods
The system SHALL support built-in and custom LLM providers (including OpenAI, Anthropic, Google Gemini, GitHub Copilot, xAI, DeepSeek, Mistral, Z.AI, OpenRouter, Ollama, `llama.cpp`, and custom OpenAI-compatible or Anthropic-compatible endpoints) via environment API keys, stored credentials, and browser loopback or device-code OAuth for supported subscriptions (including ChatGPT/Codex OAuth).

#### Scenario: ChatGPT / Codex OAuth device or loopback sign-in
- **WHEN** a user runs `tekvra auth login openai` (or triggers account login via `app-server` or `/login`)
- **THEN** the system completes OAuth authorization, extracts the account identifier, persists refreshable tokens securely, and lists plan-eligible Codex/GPT models and rate-limit windows

### Requirement: Multi-account quota tracking and automatic failover
The system SHALL allow configuring multiple named accounts or keys per provider and SHALL automatically switch a session's next request to another eligible account when the active account hits a quota window or rate-limit exhaustion, unless pinned by `--account PROVIDER:NAME`.

#### Scenario: Subscription quota exhausted mid-session
- **WHEN** an active provider account returns a quota-exhausted error or crosses its remaining quota threshold and a secondary account for that provider is configured
- **THEN** the next turn or retry routes to the secondary account without corrupting conversation history

### Requirement: Role-based model job routing
The system SHALL support independent model assignment for distinct runtime jobs (`main`, `plan`, `subagent`, `compact`, `review`, `goal`, `title`, and `fast`), defaulting unbound jobs to inherit the active session model or provider-matched small model where appropriate.

#### Scenario: Separate Plan and Build models
- **WHEN** a user configures a dedicated `plan` model and `main` model
- **THEN** turns executed while the session is in `Plan` mode route to the `plan` model, and switching to `Build` mode routes subsequent turns to the `main` model without rewriting earlier history

### Requirement: End-to-end CLI OAuth loopback exchange, RFC 8628 device-code polling, and token refresh
The system SHALL execute complete OAuth sign-in flows in `tekvra auth login`—awaiting the local PKCE HTTP callback and exchanging the authorization code, or requesting a device code and polling the token endpoint across `authorization_pending` and `slow_down` responses until completion—and SHALL automatically refresh expired stored OAuth credentials when a `refresh_token` is available.

#### Scenario: Loopback PKCE OAuth login completes end to end
- **WHEN** a user runs `tekvra auth login` in browser loopback mode and the OAuth authorization server redirects to the local callback with matching `state` and `code`
- **THEN** `tekvra` exchanges the `code` and PKCE `code_verifier` at the token endpoint, extracts JWT account metadata, and persists the `0600` credential record

#### Scenario: Device-code OAuth login polls through `authorization_pending`
- **WHEN** a user runs `tekvra auth login --device-code` and the token endpoint returns `authorization_pending` before issuing tokens
- **THEN** `tekvra` displays the `verification_uri` and `user_code`, polls at the server-specified interval (widening on `slow_down`), and stores the issued token upon approval

#### Scenario: Expired stored OAuth token refreshes automatically
- **WHEN** `tekvra` loads a stored OAuth credential whose `expires_at_epoch_secs` has elapsed and a `refresh_token` is present
- **THEN** the client exchanges `grant_type=refresh_token` at the provider token endpoint, updates `credentials.json`, and supplies the refreshed access token to the `AccountPool`

# Tekvra Providers and Model Routing

`tekvra-providers` runs exclusively on `tokio` + `reqwest` (`rustls`) with zero `smol` or `isahc` dependencies.

## Supported Providers

Built-in providers include `openai`, `anthropic`, `gemini`, `github-copilot`, `xai`, `deepseek`, `mistral`, `zai`, `openrouter`, `ollama`, and `llamacpp`, plus custom `openai_responses`, `openai_chat`, `anthropic`, and `gemini` endpoints declared in `providers.toml`.

## Stateless OpenAI Responses API & Prompt Caching

For GPT-5+, o-series, and Codex models, Tekvra uses `POST /v1/responses` with:
- `store: false` and `include: ["reasoning.encrypted_content"]`, replaying `encrypted_content` items verbatim across multi-turn tool calls.
- Stable per-conversation `prompt_cache_key` and `session-id` affinity headers.
- Explicit `prompt_cache_breakpoint` on `developer` instruction blocks (and `cache_control: {"type": "ephemeral"}` on Anthropic `/v1/messages`).
- Capability-checked reasoning effort (`low`, `medium`, `high`, `xhigh`) paired with `reasoning.summary = "auto"`.

## Authentication & Multi-Account Failover

- Supports environment API keys, `providers.toml` accounts, and end-to-end OAuth (`~/.local/share/tekvra/credentials.json`, mode `0600`, `SecretToken` redacted in `Debug`):
  - `tekvra auth login [provider]`: starts an ephemeral `127.0.0.1` loopback HTTP callback listener with S256 PKCE + CSRF `state`, opens the authorization URL (or prints it with `--no-browser`), exchanges the authorization code for tokens, extracts JWT account claims, and saves the `0600` credential entry.
  - `tekvra auth login [provider] --device-code`: runs RFC 8628 device authorization, prints `verification_uri` and `user_code`, and polls the token endpoint across `authorization_pending` and `slow_down` responses until completion.
  - Stored OAuth credentials with an elapsed `expires_at_epoch_secs` and valid `refresh_token` automatically renew via `grant_type=refresh_token` during runtime initialization (`TEKVRA_OAUTH_*` environment overrides are supported for custom/enterprise OAuth endpoints).
- `AccountPool` tracks quota windows and HTTP 429 / `insufficient_quota` responses, automatically rotating to the next healthy account unless pinned via `--account PROVIDER:NAME`.

## Role-Based Model Routing

`[models]` in `tekvra.toml` allows independent model bindings for `main`, `plan`, `subagent`, `compact`, `review`, `goal`, `title`, and `fast`. Switching between `Plan` and `Build` modes routes new turns to the bound model without rewriting earlier transcript messages.

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

## True Incremental SSE Streaming

`ProviderDriver::complete_streaming(provider, model, request, on_delta)` decodes
SSE bodies incrementally off `reqwest`'s `bytes_stream()` instead of buffering
the whole HTTP response, forwarding `StreamDelta::TextDelta` /
`ReasoningSummaryDelta` events to the live sink as frames arrive. The turn loop
emits `SessionEvent::ItemStarted` before dispatch and forwards deltas live
(`SessionEvent::ItemDelta`), so the TUI, `app-server`, and `acp` hosts render
tokens at first-byte latency; interrupting a turn drops the in-flight body
stream immediately and ends the turn `interrupted`.

| Provider / protocol | Streaming request shape | Decoded deltas |
| --- | --- | --- |
| OpenAI `/responses` | `"stream": true` | `response.output_text.delta`, `response.reasoning_summary_text.delta`; final assembly from `response.completed` (keeps `encrypted_content`) |
| OpenAI Chat Completions | `"stream": true` + `"stream_options": {"include_usage": true}` | `choices[].delta.content` / `reasoning_content` fragments, `tool_calls[i].function.arguments` concatenation by index, trailing `usage` chunk |
| Anthropic `/v1/messages` | `"stream": true` | `content_block_delta` `text_delta` / `thinking_delta` / `input_json_delta` / `signature_delta`; usage from `message_start` + `message_delta` |
| Gemini `streamGenerateContent` | `?alt=sse` | one chunk per `data:` frame; `usageMetadata` merged across chunks |

A server that ignores `stream: true` and answers `Content-Type: application/json`
falls back transparently to the buffered parsers, so local/custom endpoints keep
working unchanged.

## Token Usage, Cost & Anthropic Cache Breakpoints

- Usage is parsed for every protocol: OpenAI Responses (`usage.input_tokens` /
  `input_tokens_details.cached_tokens` / `output_tokens_details.reasoning_tokens`),
  Chat Completions (`prompt_tokens`, `prompt_tokens_details.cached_tokens`,
  `completion_tokens`, `completion_tokens_details.reasoning_tokens`), Anthropic
  (`input_tokens` + `cache_creation_input_tokens` + `cache_read_input_tokens`,
  with `cache_read_input_tokens` reported as cached), and Gemini
  (`usageMetadata`). `cost_usd` is estimated from the model's per-million
  input/cached-input/output rates (`compute_cost_usd`).
- Anthropic payloads carry a **rolling ephemeral cache breakpoint**: the final
  content block of the trailing user/tool turn gets
  `cache_control: {"type": "ephemeral"}` alongside the cached developer system
  block (2 of Anthropic's 4 allowed breakpoints), so multi-turn tool loops reuse
  the stable prefix.
- Anthropic extended-thinking blocks are preserved as
  `MessagePart::Reasoning { signature: Some(...) }` and replayed verbatim
  (`{"type": "thinking", "thinking": ..., "signature": ...}`) on later turns.

## Transient HTTP Retry Backoff

Outbound requests retry up to **3 attempts** on transport resets, HTTP
`502`/`503`/`504`/`529`, and *temporary* `429` rate limits (bodies without
`insufficient_quota` / `quota_exceeded` / `usage_limit_reached` markers). A
`Retry-After` seconds hint (<= 10s) wins over exponential backoff
(`250ms` doubling, capped at `4s`). Permanently quota-exhausted `429`s skip
retries and trigger `AccountPool` failover immediately. Cancelling the turn
drops the in-flight future, aborting any pending retry sleep.

## Authentication & Multi-Account Failover

- Supports environment API keys, `providers.toml` accounts, and end-to-end OAuth (`~/.local/share/tekvra/credentials.json`, mode `0600`, `SecretToken` redacted in `Debug`):
  - `tekvra auth login [provider]`: starts an ephemeral `127.0.0.1` loopback HTTP callback listener with S256 PKCE + CSRF `state`, opens the authorization URL (or prints it with `--no-browser`), exchanges the authorization code for tokens, extracts JWT account claims, and saves the `0600` credential entry.
  - `tekvra auth login [provider] --device-code`: runs RFC 8628 device authorization, prints `verification_uri` and `user_code`, and polls the token endpoint across `authorization_pending` and `slow_down` responses until completion.
  - Stored OAuth credentials with an elapsed `expires_at_epoch_secs` and valid `refresh_token` automatically renew via `grant_type=refresh_token` during runtime initialization (`TEKVRA_OAUTH_*` environment overrides are supported for custom/enterprise OAuth endpoints).
- `AccountPool` tracks quota windows and HTTP 429 / `insufficient_quota` responses, automatically rotating to the next healthy account unless pinned via `--account PROVIDER:NAME`.

## Multimodal (Vision) Input

Image attachments (`MessagePart::Image { media_type, data_base64, source_path }`) are encoded natively per wire protocol and gated by `ModelCapabilities::supports_vision` **before any network I/O**:

| Provider / protocol | Image encoding |
| --- | --- |
| OpenAI `/responses` | `input_image` content blocks with `data:image/<mt>;base64,...` URLs (`detail: "auto"`) |
| OpenAI `/chat/completions` | `image_url` content parts alongside `text` parts |
| Anthropic `/v1/messages` | base64 `image` source blocks |
| Gemini `streamGenerateContent` | native `inlineData { mimeType, data }` parts |

- Built-in heuristics flag vision-capable models (`gpt-4o`/`gpt-5`/o-series/Codex, Claude 3.5+/4+, Gemini 1.5+/2.x/3.x).
- Any provider or model can be forced on/off with `supports_vision = true|false` on the `[providers.<id>]` entry in `providers.toml` (`ProviderDefinition::supports_vision_override`).
- Requests carrying images against a non-vision model fail fast with `ProviderError::UnsupportedModality` and never reach the network.
- The CLI attaches images with repeatable `-i/--image <PATH>` (png/jpg/jpeg/webp/gif, max 10 MiB each) on `tekvra`, `tekvra exec`, `tekvra resume`, and `tekvra fork`; in the TUI/protocol, `SessionCommand::SubmitInput { images: Vec<InputImage> }` carries them.

## Role-Based Model Routing

`[models]` in `tekvra.toml` allows independent model bindings for `main`, `plan`, `subagent`, `compact`, `review`, `goal`, `title`, and `fast`. Switching between `Plan` and `Build` modes routes new turns to the bound model without rewriting earlier transcript messages.

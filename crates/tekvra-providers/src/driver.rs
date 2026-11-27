//! Provider-neutral conversation messages, tool schemas, and OpenAI `/responses` wire driver
//! with stateless `reasoning.encrypted_content` replay and explicit cache breakpoints.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tekvra_protocol::TokenUsage;

use crate::ProviderError;
use crate::auth::AccountPool;
use crate::catalog::{ModelCapabilities, ProviderDefinition, WireProtocol};

/// Role of a message in the append-only conversation frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// Developer / system instruction channel.
    Developer,
    /// End-user prompt or steering nudge.
    User,
    /// Model response turn.
    Assistant,
    /// Native or MCP tool execution output.
    Tool,
}

/// Content block inside a [`ProviderMessage`], preserving encrypted reasoning items verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePart {
    /// Plain text content, with optional explicit prompt-cache breakpoint marker.
    Text {
        /// Text body.
        text: String,
        /// Marks the end of a cacheable prefix block (`DeveloperMessage` / Anthropic `ephemeral`).
        #[serde(default)]
        cache_breakpoint: bool,
    },
    /// Model reasoning item including stateless `encrypted_content` for `/responses` replay.
    Reasoning {
        /// Provider item id (`rs_...`).
        id: String,
        /// Human-readable reasoning summary paragraphs.
        #[serde(default)]
        summary: Vec<String>,
        /// Opaque encrypted reasoning blob returned by OpenAI `/responses` (`store: false`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
    /// Assistant tool call invocation.
    ToolCall {
        /// Call identifier (`call_...`).
        call_id: String,
        /// Tool function name.
        name: String,
        /// Raw JSON arguments string.
        arguments_json: String,
    },
    /// Tool execution output paired with `call_id`.
    ToolResult {
        /// Matching `call_id`.
        call_id: String,
        /// Model-facing output text.
        output: String,
        /// Whether the tool returned an error.
        #[serde(default)]
        is_error: bool,
    },
}

/// Single entry in the append-only prompt frame sent to an LLM provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderMessage {
    /// Message role.
    pub role: MessageRole,
    /// Ordered content blocks.
    pub parts: Vec<MessagePart>,
}

impl ProviderMessage {
    /// Constructs a cache-breakpointed developer/system instruction message.
    #[must_use]
    pub fn developer_cached(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Developer,
            parts: vec![MessagePart::Text {
                text: text.into(),
                cache_breakpoint: true,
            }],
        }
    }

    /// Constructs a plain user text message.
    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                text: text.into(),
                cache_breakpoint: false,
            }],
        }
    }
}

/// JSON Schema tool declaration passed to providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name (`apply_patch`, `exec_command`, `file_read`, etc.).
    pub name: String,
    /// Concise model-facing tool description.
    pub description: String,
    /// JSON Schema object describing arguments.
    pub parameters: Value,
}

/// Outbound generation request executed by a [`ProviderDriver`].
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// Stable conversation/session id used for `prompt_cache_key` and `session-id` affinity.
    pub conversation_id: String,
    /// Append-only message frame.
    pub messages: Vec<ProviderMessage>,
    /// Stable tool declarations.
    pub tools: Vec<ToolDefinition>,
    /// Optional requested reasoning effort (`"low"`, `"medium"`, `"high"`, `"xhigh"`).
    pub reasoning_effort: Option<String>,
    /// Optional JSON Schema enforcing structured final output (`tekvra exec --output-schema`).
    pub output_schema: Option<Value>,
}

/// Incremental streaming event yielded during a model response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamDelta {
    /// Assistant prose delta.
    TextDelta(String),
    /// Reasoning summary delta.
    ReasoningSummaryDelta(String),
}

/// Completed model response turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionResponse {
    /// Assistant message parts (reasoning w/ `encrypted_content`, text, tool calls) ready to append verbatim.
    pub assistant_message: ProviderMessage,
    /// Incremental stream events captured during generation.
    pub stream_deltas: Vec<StreamDelta>,
    /// Token and cache usage metrics.
    pub usage: TokenUsage,
    /// Account alias that served the request.
    pub served_by_account: String,
}

/// Object-safe async LLM driver abstraction (implemented by [`HttpProviderDriver`] and test mocks).
#[async_trait]
pub trait ProviderDriver: Send + Sync {
    /// Sends `request` to `provider` / `model` and returns the completed response.
    async fn complete(
        &self,
        provider: &ProviderDefinition,
        model: &ModelCapabilities,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError>;
}

/// Builds the stateless OpenAI `/responses` JSON body (`store: false`, `reasoning.encrypted_content`,
/// `prompt_cache_key`, explicit `DeveloperMessage` cache breakpoint, and verbatim encrypted reasoning replay).
///
/// # Errors
/// Returns [`ProviderError`] if `request.reasoning_effort` is incompatible with `model`.
pub fn build_openai_responses_payload(
    model: &ModelCapabilities,
    request: &CompletionRequest,
) -> Result<Value, ProviderError> {
    let mut input_items = Vec::new();

    for msg in &request.messages {
        match msg.role {
            MessageRole::Developer => {
                for part in &msg.parts {
                    if let MessagePart::Text {
                        text,
                        cache_breakpoint,
                    } = part
                    {
                        let mut content_block = json!({
                            "type": "input_text",
                            "text": text,
                        });
                        if *cache_breakpoint && model.supports_cache_breakpoints {
                            content_block["prompt_cache_breakpoint"] = Value::Bool(true);
                        }
                        input_items.push(json!({
                            "type": "message",
                            "role": "developer",
                            "content": [content_block],
                        }));
                    }
                }
            }
            MessageRole::User => {
                let mut content = Vec::new();
                for part in &msg.parts {
                    if let MessagePart::Text { text, .. } = part {
                        content.push(json!({
                            "type": "input_text",
                            "text": text,
                        }));
                    }
                }
                if !content.is_empty() {
                    input_items.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": content,
                    }));
                }
            }
            MessageRole::Assistant => {
                for part in &msg.parts {
                    match part {
                        MessagePart::Reasoning {
                            id,
                            summary,
                            encrypted_content,
                        } => {
                            let summary_blocks: Vec<Value> = summary
                                .iter()
                                .map(|s| json!({ "type": "summary_text", "text": s }))
                                .collect();
                            let mut item = json!({
                                "type": "reasoning",
                                "id": id,
                                "summary": summary_blocks,
                            });
                            if let Some(enc) = encrypted_content {
                                item["encrypted_content"] = Value::String(enc.clone());
                            }
                            input_items.push(item);
                        }
                        MessagePart::Text { text, .. } => {
                            input_items.push(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{
                                    "type": "output_text",
                                    "text": text,
                                }],
                            }));
                        }
                        MessagePart::ToolCall {
                            call_id,
                            name,
                            arguments_json,
                        } => {
                            input_items.push(json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": name,
                                "arguments": arguments_json,
                            }));
                        }
                        MessagePart::ToolResult { .. } => {}
                    }
                }
            }
            MessageRole::Tool => {
                for part in &msg.parts {
                    if let MessagePart::ToolResult {
                        call_id, output, ..
                    } = part
                    {
                        input_items.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": output,
                        }));
                    }
                }
            }
        }
    }

    let tools_json: Vec<Value> = request
        .tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        })
        .collect();

    let mut body = json!({
        "model": model.model_id,
        "store": false,
        "prompt_cache_key": request.conversation_id,
        "input": input_items,
    });

    if !tools_json.is_empty() {
        body["tools"] = Value::Array(tools_json);
    }

    if model.supports_encrypted_reasoning {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }

    if let Some(effort) = model.align_reasoning_effort(request.reasoning_effort.as_deref())? {
        body["reasoning"] = json!({
            "effort": effort,
            "summary": "auto",
        });
    }

    if let Some(schema) = &request.output_schema {
        body["text"] = json!({
            "format": {
                "type": "json_schema",
                "name": "tekvra_output",
                "schema": schema,
                "strict": true,
            }
        });
    }

    Ok(body)
}

/// Builds an Anthropic `/v1/messages` payload with `cache_control: {"type": "ephemeral"}` on breakpointed blocks.
#[must_use]
pub fn build_anthropic_messages_payload(
    model: &ModelCapabilities,
    request: &CompletionRequest,
) -> Value {
    let mut system_blocks = Vec::new();
    let mut messages = Vec::new();

    for msg in &request.messages {
        match msg.role {
            MessageRole::Developer => {
                for part in &msg.parts {
                    if let MessagePart::Text {
                        text,
                        cache_breakpoint,
                    } = part
                    {
                        let mut blk = json!({ "type": "text", "text": text });
                        if *cache_breakpoint {
                            blk["cache_control"] = json!({ "type": "ephemeral" });
                        }
                        system_blocks.push(blk);
                    }
                }
            }
            MessageRole::User => {
                let content: Vec<Value> = msg
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        MessagePart::Text { text, .. } => {
                            Some(json!({ "type": "text", "text": text }))
                        }
                        _ => None,
                    })
                    .collect();
                messages.push(json!({ "role": "user", "content": content }));
            }
            MessageRole::Assistant => {
                let mut content = Vec::new();
                for p in &msg.parts {
                    match p {
                        MessagePart::Text { text, .. } => {
                            content.push(json!({ "type": "text", "text": text }));
                        }
                        MessagePart::ToolCall {
                            call_id,
                            name,
                            arguments_json,
                        } => {
                            let input_val: Value =
                                serde_json::from_str(arguments_json).unwrap_or_else(|_| json!({}));
                            content.push(json!({
                                "type": "tool_use",
                                "id": call_id,
                                "name": name,
                                "input": input_val,
                            }));
                        }
                        _ => {}
                    }
                }
                messages.push(json!({ "role": "assistant", "content": content }));
            }
            MessageRole::Tool => {
                let content: Vec<Value> = msg
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        MessagePart::ToolResult {
                            call_id,
                            output,
                            is_error,
                        } => Some(json!({
                            "type": "tool_result",
                            "tool_use_id": call_id,
                            "content": output,
                            "is_error": is_error,
                        })),
                        _ => None,
                    })
                    .collect();
                messages.push(json!({ "role": "user", "content": content }));
            }
        }
    }

    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        })
        .collect();

    let mut body = json!({
        "model": model.model_id,
        "max_tokens": 8192,
        "messages": messages,
    });
    if !system_blocks.is_empty() {
        body["system"] = Value::Array(system_blocks);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    body
}

/// Builds an OpenAI-compatible `/chat/completions` payload.
#[must_use]
pub fn build_chat_completions_payload(
    model: &ModelCapabilities,
    request: &CompletionRequest,
) -> Value {
    let mut messages = Vec::new();
    for msg in &request.messages {
        match msg.role {
            MessageRole::Developer => {
                for p in &msg.parts {
                    if let MessagePart::Text { text, .. } = p {
                        messages.push(json!({ "role": "system", "content": text }));
                    }
                }
            }
            MessageRole::User => {
                for p in &msg.parts {
                    if let MessagePart::Text { text, .. } = p {
                        messages.push(json!({ "role": "user", "content": text }));
                    }
                }
            }
            MessageRole::Assistant => {
                let mut text_buf = String::new();
                let mut tool_calls = Vec::new();
                for p in &msg.parts {
                    match p {
                        MessagePart::Text { text, .. } => text_buf.push_str(text),
                        MessagePart::ToolCall {
                            call_id,
                            name,
                            arguments_json,
                        } => tool_calls.push(json!({
                            "id": call_id,
                            "type": "function",
                            "function": { "name": name, "arguments": arguments_json }
                        })),
                        _ => {}
                    }
                }
                let mut m = json!({ "role": "assistant", "content": text_buf });
                if !tool_calls.is_empty() {
                    m["tool_calls"] = Value::Array(tool_calls);
                }
                messages.push(m);
            }
            MessageRole::Tool => {
                for p in &msg.parts {
                    if let MessagePart::ToolResult {
                        call_id, output, ..
                    } = p
                    {
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": output,
                        }));
                    }
                }
            }
        }
    }

    let mut body = json!({
        "model": model.model_id,
        "messages": messages,
    });
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect(),
        );
    }
    body
}

/// Parses either a JSON `/responses` payload or an SSE event stream (`event: ...` / `data: ...`)
/// into a [`CompletionResponse`], preserving `encrypted_content` and cache token metrics.
///
/// # Errors
/// Returns [`ProviderError`] if neither SSE nor JSON response structure can be decoded.
pub fn parse_openai_responses_body(
    raw_body: &str,
    model: &ModelCapabilities,
    served_by_account: &str,
) -> Result<CompletionResponse, ProviderError> {
    let trimmed = raw_body.trim();
    if trimmed.starts_with('{') {
        let obj: Value = serde_json::from_str(trimmed).map_err(|e| ProviderError::Decode {
            provider: model.provider.clone(),
            message: e.to_string(),
        })?;
        return extract_responses_object(&obj, model, served_by_account, Vec::new());
    }

    // Parse SSE stream (`data: {...}` lines ending at `response.completed` or `[DONE]`)
    let mut deltas = Vec::new();
    let mut final_response: Option<Value> = None;

    for line in trimmed.lines() {
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match ev.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                if let Some(d) = ev.get("delta").and_then(Value::as_str) {
                    deltas.push(StreamDelta::TextDelta(d.to_string()));
                }
            }
            Some("response.reasoning_summary_text.delta") => {
                if let Some(d) = ev.get("delta").and_then(Value::as_str) {
                    deltas.push(StreamDelta::ReasoningSummaryDelta(d.to_string()));
                }
            }
            Some("response.completed") => {
                if let Some(resp_obj) = ev.get("response") {
                    final_response = Some(resp_obj.clone());
                }
            }
            _ => {}
        }
    }

    let resp_obj = final_response.ok_or_else(|| ProviderError::Decode {
        provider: model.provider.clone(),
        message: "missing `response.completed` event in `/responses` SSE stream".to_string(),
    })?;
    extract_responses_object(&resp_obj, model, served_by_account, deltas)
}

fn extract_responses_object(
    obj: &Value,
    model: &ModelCapabilities,
    served_by_account: &str,
    mut stream_deltas: Vec<StreamDelta>,
) -> Result<CompletionResponse, ProviderError> {
    let mut parts = Vec::new();
    if let Some(items) = obj.get("output").and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("reasoning") => {
                    let id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("rs_0")
                        .to_string();
                    let summary: Vec<String> = item
                        .get("summary")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|s| s.get("text").and_then(Value::as_str))
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    if stream_deltas.is_empty() {
                        for s in &summary {
                            stream_deltas.push(StreamDelta::ReasoningSummaryDelta(s.clone()));
                        }
                    }
                    let encrypted_content = item
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    parts.push(MessagePart::Reasoning {
                        id,
                        summary,
                        encrypted_content,
                    });
                }
                Some("message") => {
                    if let Some(content_arr) = item.get("content").and_then(Value::as_array) {
                        for c in content_arr {
                            if let Some(txt) = c.get("text").and_then(Value::as_str) {
                                if !stream_deltas
                                    .iter()
                                    .any(|d| matches!(d, StreamDelta::TextDelta(_)))
                                {
                                    stream_deltas.push(StreamDelta::TextDelta(txt.to_string()));
                                }
                                parts.push(MessagePart::Text {
                                    text: txt.to_string(),
                                    cache_breakpoint: false,
                                });
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let call_id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("call_0")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let arguments_json = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}")
                        .to_string();
                    parts.push(MessagePart::ToolCall {
                        call_id,
                        name,
                        arguments_json,
                    });
                }
                _ => {}
            }
        }
    }

    let usage_obj = obj.get("usage");
    let input_tokens = usage_obj
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached_input_tokens = usage_obj
        .and_then(|u| u.get("input_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage_obj
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_tokens = usage_obj
        .and_then(|u| u.get("output_tokens_details"))
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let uncached_input = input_tokens.saturating_sub(cached_input_tokens);
    let cost_usd = (uncached_input as f64 / 1_000_000.0) * model.input_cost_per_million
        + (cached_input_tokens as f64 / 1_000_000.0) * model.cached_input_cost_per_million
        + (output_tokens as f64 / 1_000_000.0) * model.output_cost_per_million;

    Ok(CompletionResponse {
        assistant_message: ProviderMessage {
            role: MessageRole::Assistant,
            parts,
        },
        stream_deltas,
        usage: TokenUsage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
            reasoning_tokens,
            cost_usd,
        },
        served_by_account: served_by_account.to_string(),
    })
}

/// Pure `tokio` + `reqwest` (`rustls`) HTTP provider client with automatic multi-account quota failover.
#[derive(Debug, Clone)]
pub struct HttpProviderDriver {
    http: reqwest::Client,
    accounts: Arc<Mutex<AccountPool>>,
}

impl HttpProviderDriver {
    /// Constructs a new [`HttpProviderDriver`] backed by `reqwest` (`rustls`) and a shared [`AccountPool`].
    #[must_use]
    pub fn new(accounts: AccountPool) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(format!("tekvra/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest rustls client builds cleanly");
        Self {
            http,
            accounts: Arc::new(Mutex::new(accounts)),
        }
    }

    /// Borrows the internal `reqwest::Client`.
    #[must_use]
    pub const fn http_client(&self) -> &reqwest::Client {
        &self.http
    }
}

#[async_trait]
impl ProviderDriver for HttpProviderDriver {
    async fn complete(
        &self,
        provider: &ProviderDefinition,
        model: &ModelCapabilities,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        loop {
            let candidate = {
                let pool = self.accounts.lock().expect("account pool lock poisoned");
                pool.select(&provider.id)?.clone()
            };

            let (endpoint, payload) = match model.protocol {
                WireProtocol::OpenAiResponses => (
                    format!("{}/responses", provider.base_url),
                    build_openai_responses_payload(model, request)?,
                ),
                WireProtocol::AnthropicMessages => (
                    format!("{}/messages", provider.base_url),
                    build_anthropic_messages_payload(model, request),
                ),
                WireProtocol::OpenAiChatCompletions | WireProtocol::GeminiGenerateContent => (
                    format!("{}/chat/completions", provider.base_url),
                    build_chat_completions_payload(model, request),
                ),
            };

            let mut req_builder = self
                .http
                .post(&endpoint)
                .header("session-id", &request.conversation_id)
                .header("x-session-affinity", &request.conversation_id);

            if model.protocol == WireProtocol::AnthropicMessages {
                req_builder = req_builder
                    .header("x-api-key", candidate.token.expose())
                    .header("anthropic-version", "2023-06-01");
            } else {
                req_builder = req_builder.header(
                    "authorization",
                    format!("Bearer {}", candidate.token.expose()),
                );
            }
            if let Some(acct_id) = &candidate.account_id {
                req_builder = req_builder.header("chatgpt-account-id", acct_id);
            }
            for (hk, hv) in &provider.headers {
                req_builder = req_builder.header(hk, hv);
            }

            let resp =
                req_builder
                    .json(&payload)
                    .send()
                    .await
                    .map_err(|e| ProviderError::Transport {
                        provider: provider.id.clone(),
                        message: e.to_string(),
                    })?;

            let status = resp.status();
            let remaining_header = resp
                .headers()
                .get("x-ratelimit-remaining-ratio")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<f64>().ok());
            let raw_body = resp.text().await.map_err(|e| ProviderError::Transport {
                provider: provider.id.clone(),
                message: e.to_string(),
            })?;

            let is_quota_error = status.as_u16() == 429
                || raw_body.contains("insufficient_quota")
                || raw_body.contains("quota_exceeded")
                || raw_body.contains("usage_limit_reached");

            if is_quota_error {
                {
                    let mut pool = self.accounts.lock().expect("account pool lock poisoned");
                    pool.record_quota_state(&provider.id, &candidate.name, Some(0.0), true);
                }
                // Loop immediately attempts failover to the next eligible account (or returns `NoEligibleAccount` if pinned/all exhausted).
                continue;
            }

            if !status.is_success() {
                return Err(ProviderError::HttpStatus {
                    provider: provider.id.clone(),
                    status: status.as_u16(),
                    body: raw_body,
                });
            }

            if let Some(ratio) = remaining_header {
                let mut pool = self.accounts.lock().expect("account pool lock poisoned");
                pool.record_quota_state(&provider.id, &candidate.name, Some(ratio), false);
            }

            return match model.protocol {
                WireProtocol::OpenAiResponses => {
                    parse_openai_responses_body(&raw_body, model, &candidate.name)
                }
                WireProtocol::AnthropicMessages
                | WireProtocol::OpenAiChatCompletions
                | WireProtocol::GeminiGenerateContent => {
                    parse_generic_completion(&raw_body, model, &candidate.name)
                }
            };
        }
    }
}

fn parse_generic_completion(
    raw_body: &str,
    model: &ModelCapabilities,
    served_by_account: &str,
) -> Result<CompletionResponse, ProviderError> {
    let val: Value = serde_json::from_str(raw_body).map_err(|e| ProviderError::Decode {
        provider: model.provider.clone(),
        message: e.to_string(),
    })?;
    let mut parts = Vec::new();
    let mut deltas = Vec::new();

    // OpenAI Chat Completions shape
    if let Some(choice) = val
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        && let Some(msg) = choice.get("message")
    {
        if let Some(txt) = msg.get("content").and_then(Value::as_str)
            && !txt.is_empty()
        {
            deltas.push(StreamDelta::TextDelta(txt.to_string()));
            parts.push(MessagePart::Text {
                text: txt.to_string(),
                cache_breakpoint: false,
            });
        }
        if let Some(tcs) = msg.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                let call_id = tc
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("call_0")
                    .to_string();
                let func = tc.get("function");
                let name = func
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let arguments_json = func
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or("{}")
                    .to_string();
                parts.push(MessagePart::ToolCall {
                    call_id,
                    name,
                    arguments_json,
                });
            }
        }
    } else if let Some(content_arr) = val.get("content").and_then(Value::as_array) {
        // Anthropic Messages shape
        for blk in content_arr {
            match blk.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(txt) = blk.get("text").and_then(Value::as_str) {
                        deltas.push(StreamDelta::TextDelta(txt.to_string()));
                        parts.push(MessagePart::Text {
                            text: txt.to_string(),
                            cache_breakpoint: false,
                        });
                    }
                }
                Some("tool_use") => {
                    let call_id = blk
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("toolu_0")
                        .to_string();
                    let name = blk
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let arguments_json = blk
                        .get("input")
                        .map(Value::to_string)
                        .unwrap_or_else(|| "{}".to_string());
                    parts.push(MessagePart::ToolCall {
                        call_id,
                        name,
                        arguments_json,
                    });
                }
                _ => {}
            }
        }
    }

    Ok(CompletionResponse {
        assistant_message: ProviderMessage {
            role: MessageRole::Assistant,
            parts,
        },
        stream_deltas: deltas,
        usage: TokenUsage::default(),
        served_by_account: served_by_account.to_string(),
    })
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<HttpProviderDriver>();
};

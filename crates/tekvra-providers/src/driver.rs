//! Provider-neutral conversation messages, tool schemas, and OpenAI `/responses` wire driver
//! with stateless `reasoning.encrypted_content` replay and explicit cache breakpoints.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
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
        /// Anthropic extended-thinking signature block (required for verbatim replay).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
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
    /// Multimodal user image block (`PNG`, `JPEG`, `WEBP`, `GIF`) encoded as base64.
    Image {
        /// IANA media type (`image/png`, `image/jpeg`, `image/webp`, `image/gif`).
        media_type: String,
        /// Base64-encoded image bytes (without the `data:` URL prefix).
        data_base64: String,
        /// Optional workspace-relative source path for diagnostics.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_path: Option<String>,
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

    /// Returns `true` when any part of this message is an [`MessagePart::Image`] block.
    #[must_use]
    pub fn has_image_parts(&self) -> bool {
        self.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Image { .. }))
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

/// Callback sink receiving incremental [`StreamDelta`] events as they decode.
pub type StreamDeltaSink = Arc<dyn Fn(StreamDelta) + Send + Sync>;

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

    /// Streaming variant: forwards decoded [`StreamDelta`] events to `on_delta` live while
    /// the HTTP body streams in, then returns the fully assembled response.
    ///
    /// The default implementation delegates to [`ProviderDriver::complete`] and replays
    /// `resp.stream_deltas` into the sink, so deterministic test mocks keep compiling
    /// unchanged.
    async fn complete_streaming(
        &self,
        provider: &ProviderDefinition,
        model: &ModelCapabilities,
        request: &CompletionRequest,
        on_delta: Option<StreamDeltaSink>,
    ) -> Result<CompletionResponse, ProviderError> {
        let resp = self.complete(provider, model, request).await?;
        if let Some(sink) = &on_delta {
            for delta in &resp.stream_deltas {
                sink(delta.clone());
            }
        }
        Ok(resp)
    }

    /// Snapshot of the multi-account quota pool (remaining ratios + exhaustion flags)
    /// for diagnostics surfaces such as the app-server `account/rateLimits/read` RPC.
    /// Deterministic test drivers return an empty pool.
    fn account_pool_snapshot(&self) -> AccountPool {
        AccountPool::default()
    }
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
                    match part {
                        MessagePart::Text { text, .. } => {
                            content.push(json!({
                                "type": "input_text",
                                "text": text,
                            }));
                        }
                        MessagePart::Image {
                            media_type,
                            data_base64,
                            ..
                        } => {
                            content.push(json!({
                                "type": "input_image",
                                "image_url": format!("data:{media_type};base64,{data_base64}"),
                                "detail": "auto",
                            }));
                        }
                        _ => {}
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
                            ..
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
                        MessagePart::Image { .. } => {}
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
                        MessagePart::Image {
                            media_type,
                            data_base64,
                            ..
                        } => Some(json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": media_type,
                                "data": data_base64,
                            }
                        })),
                        _ => None,
                    })
                    .collect();
                if !content.is_empty() {
                    messages.push(json!({ "role": "user", "content": content }));
                }
            }
            MessageRole::Assistant => {
                let mut content = Vec::new();
                for p in &msg.parts {
                    match p {
                        MessagePart::Text { text, .. } => {
                            content.push(json!({ "type": "text", "text": text }));
                        }
                        MessagePart::Reasoning {
                            summary, signature, ..
                        } if signature.is_some() => {
                            // Anthropic extended-thinking blocks must replay verbatim
                            // (thinking text + signature) on multi-step tool turns.
                            content.push(json!({
                                "type": "thinking",
                                "thinking": summary.join("\n"),
                                "signature": signature,
                            }));
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
    // Rolling ephemeral cache breakpoint: mark the final content block of the
    // trailing user/tool turn so multi-turn tool loops reuse the stable prefix.
    // Together with the developer system block this stays within Anthropic's
    // 4-breakpoint limit (2 used).
    if let Some(last_msg) = body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .and_then(|msgs| msgs.last_mut())
        && let Some(last_block) = last_msg
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .and_then(|blocks| blocks.last_mut())
    {
        last_block["cache_control"] = json!({ "type": "ephemeral" });
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
                let has_images = msg.has_image_parts();
                if !has_images {
                    // Preserve plain string `content` for maximum local-server compatibility.
                    for p in &msg.parts {
                        if let MessagePart::Text { text, .. } = p {
                            messages.push(json!({ "role": "user", "content": text }));
                        }
                    }
                } else {
                    let mut content = Vec::new();
                    for p in &msg.parts {
                        match p {
                            MessagePart::Text { text, .. } => {
                                content.push(json!({ "type": "text", "text": text }));
                            }
                            MessagePart::Image {
                                media_type,
                                data_base64,
                                ..
                            } => {
                                content.push(json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": format!("data:{media_type};base64,{data_base64}"),
                                    }
                                }));
                            }
                            _ => {}
                        }
                    }
                    messages.push(json!({ "role": "user", "content": content }));
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

/// Builds a native Google Gemini `:streamGenerateContent` payload with `inlineData` image blocks,
/// `systemInstruction`, and `functionDeclaration` / `functionCall` / `functionResponse` tool mapping.
#[must_use]
pub fn build_gemini_payload(_model: &ModelCapabilities, request: &CompletionRequest) -> Value {
    let mut system_parts: Vec<Value> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();

    // Pre-index assistant `ToolCall` ids so tool results can map `call_id` -> function name.
    let mut call_id_to_name: BTreeMap<String, String> = BTreeMap::new();
    for msg in &request.messages {
        for part in &msg.parts {
            if let MessagePart::ToolCall { call_id, name, .. } = part {
                call_id_to_name.insert(call_id.clone(), name.clone());
            }
        }
    }

    for msg in &request.messages {
        match msg.role {
            MessageRole::Developer => {
                for p in &msg.parts {
                    if let MessagePart::Text { text, .. } = p {
                        system_parts.push(json!({ "text": text }));
                    }
                }
            }
            MessageRole::User => {
                let mut parts = Vec::new();
                for p in &msg.parts {
                    match p {
                        MessagePart::Text { text, .. } => {
                            parts.push(json!({ "text": text }));
                        }
                        MessagePart::Image {
                            media_type,
                            data_base64,
                            ..
                        } => {
                            parts.push(json!({
                                "inlineData": {
                                    "mimeType": media_type,
                                    "data": data_base64,
                                }
                            }));
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
            MessageRole::Assistant => {
                let mut parts = Vec::new();
                for p in &msg.parts {
                    match p {
                        MessagePart::Text { text, .. } => {
                            parts.push(json!({ "text": text }));
                        }
                        MessagePart::ToolCall {
                            name,
                            arguments_json,
                            ..
                        } => {
                            let args: Value =
                                serde_json::from_str(arguments_json).unwrap_or_else(|_| json!({}));
                            parts.push(json!({ "functionCall": { "name": name, "args": args } }));
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            MessageRole::Tool => {
                let mut parts = Vec::new();
                for p in &msg.parts {
                    if let MessagePart::ToolResult {
                        call_id,
                        output,
                        is_error,
                    } = p
                    {
                        let function_name = call_id_to_name
                            .get(call_id)
                            .cloned()
                            .unwrap_or_else(|| call_id.clone());
                        let response = if *is_error {
                            json!({ "error": { "message": output } })
                        } else {
                            json!({ "result": { "content": output } })
                        };
                        parts.push(json!({
                            "functionResponse": {
                                "name": function_name,
                                "response": response,
                            }
                        }));
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
        }
    }

    let mut body = json!({ "contents": contents });
    if !system_parts.is_empty() {
        body["systemInstruction"] = json!({ "parts": system_parts });
    }
    if !request.tools.is_empty() {
        body["tools"] = json!([{
            "functionDeclarations": request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    })
                })
                .collect::<Vec<Value>>(),
        }]);
    }
    body["generationConfig"] = json!({ "maxOutputTokens": 65_536 });
    body
}

/// Parses a native Gemini `:streamGenerateContent` response (chunked JSON array or single object)
/// into a [`CompletionResponse`], synthesizing deterministic `call_<name>_<n>` ids for `functionCall` parts.
///
/// # Errors
/// Returns [`ProviderError`] if the body is not valid JSON.
pub fn parse_gemini_body(
    raw_body: &str,
    model: &ModelCapabilities,
    served_by_account: &str,
) -> Result<CompletionResponse, ProviderError> {
    let val: Value = serde_json::from_str(raw_body.trim()).map_err(|e| ProviderError::Decode {
        provider: model.provider.clone(),
        message: e.to_string(),
    })?;
    let chunks: Vec<&Value> = match &val {
        Value::Array(items) => items.iter().collect(),
        _ => vec![&val],
    };

    let mut acc = GeminiAccumulator::default();
    for chunk in chunks {
        acc.push_chunk(chunk, None);
    }
    Ok(acc.into_response(model, served_by_account))
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
                        signature: None,
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

    let cost_usd = compute_cost_usd(model, input_tokens, cached_input_tokens, output_tokens);

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

/// Returns `true` when `request` carries any [`MessagePart::Image`] block.
#[must_use]
pub fn request_has_image(request: &CompletionRequest) -> bool {
    request
        .messages
        .iter()
        .any(ProviderMessage::has_image_parts)
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

    /// Returns a snapshot of the current [`AccountPool`] state (quota ratios and
    /// exhaustion flags) for diagnostics and `account/rateLimits/read` queries.
    #[must_use]
    pub fn account_pool_snapshot(&self) -> AccountPool {
        self.accounts
            .lock()
            .expect("account pool lock poisoned")
            .clone()
    }
}

/// Bounded retry budget for transient outbound HTTP failures.
const MAX_HTTP_ATTEMPTS: u32 = 3;
/// Exponential backoff base delay for attempt 1 (`250ms`, doubling per attempt).
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
/// Upper bound for a single backoff wait.
const MAX_BACKOFF: Duration = Duration::from_secs(4);
/// Upper bound for honoring a server-provided `Retry-After` seconds hint.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);

/// Outcome of a retried provider HTTP exchange.
struct PreparedReply {
    status: u16,
    content_type: Option<String>,
    remaining_ratio: Option<f64>,
    /// Fully read body for buffered replies (`None` while the live SSE
    /// [`PreparedReply::response`] is still unconsumed).
    body_text: Option<String>,
    /// Live streaming response when the caller requested SSE streaming.
    response: Option<reqwest::Response>,
}

/// Bundled outbound HTTP request parameters shared by the retry and
/// request-composition helpers.
struct OutboundHttpRequest<'a> {
    endpoint: &'a str,
    payload: &'a Value,
    conversation_id: &'a str,
    streaming: bool,
}

/// Returns the delay before retry `attempt` (1-based): `250ms`, `500ms`, `1s`, capped at 4s.
fn backoff_delay(attempt: u32) -> Duration {
    INITIAL_BACKOFF
        .mul_f64(2.0_f64.powi(i32::try_from(attempt).unwrap_or(i32::MAX).saturating_sub(1)))
        .min(MAX_BACKOFF)
}

/// Resolves the retry delay: a bounded `Retry-After` hint wins over exponential backoff.
fn retry_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    match retry_after {
        Some(delay) if delay <= MAX_RETRY_AFTER => delay,
        _ => backoff_delay(attempt),
    }
}

fn parse_retry_after_seconds(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

pub(crate) fn is_retryable_status(status: u16) -> bool {
    matches!(status, 502 | 503 | 504 | 529 | 429)
}

fn body_indicates_permanent_quota_exhaustion(body: &str) -> bool {
    body.contains("insufficient_quota")
        || body.contains("quota_exceeded")
        || body.contains("usage_limit_reached")
}

/// Shared USD cost estimation over uncached/cached input and output rates.
#[must_use]
pub fn compute_cost_usd(
    model: &ModelCapabilities,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
) -> f64 {
    let uncached_input = input_tokens.saturating_sub(cached_input_tokens);
    (uncached_input as f64 / 1_000_000.0) * model.input_cost_per_million
        + (cached_input_tokens as f64 / 1_000_000.0) * model.cached_input_cost_per_million
        + (output_tokens as f64 / 1_000_000.0) * model.output_cost_per_million
}

fn extract_chat_completions_usage(usage: &Value) -> TokenUsage {
    TokenUsage {
        input_tokens: usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_input_tokens: usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning_tokens: usage
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cost_usd: 0.0,
    }
}

fn extract_anthropic_usage(usage: &Value) -> TokenUsage {
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    TokenUsage {
        input_tokens: input_tokens
            .saturating_add(cache_creation)
            .saturating_add(cache_read),
        cached_input_tokens: cache_read,
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning_tokens: 0,
        cost_usd: 0.0,
    }
}

/// Forwards `delta` to the live sink (when present) and records it on the response.
fn emit_delta(sink: Option<&StreamDeltaSink>, deltas: &mut Vec<StreamDelta>, delta: StreamDelta) {
    if let Some(sink) = sink {
        sink(delta.clone());
    }
    deltas.push(delta);
}

/// Reads an SSE body incrementally, dispatching one callback per complete frame
/// (`event:` name + joined `data:` payload). Handles `\r\n` line endings and a
/// trailing frame at EOF; drops the HTTP stream as soon as the callback returns.
async fn drain_sse_frames(
    response: reqwest::Response,
    provider_id: &str,
    mut on_frame: impl FnMut(Option<String>, String),
) -> Result<(), ProviderError> {
    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut event: Option<String> = None;
    let mut data_lines: Vec<String> = Vec::new();

    let flush = |event: &mut Option<String>,
                 data_lines: &mut Vec<String>,
                 on_frame: &mut dyn FnMut(Option<String>, String)| {
        if data_lines.is_empty() {
            *event = None;
            return;
        }
        let payload = data_lines.join("\n");
        data_lines.clear();
        let ev = event.take();
        on_frame(ev, payload);
    };

    let transport_err = |message: String| ProviderError::Transport {
        provider: provider_id.to_string(),
        message,
    };

    while let Some(chunk) = stream
        .next()
        .await
        .map(|c| c.map_err(|e| transport_err(e.to_string())))
    {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = buf.drain(..=pos).collect();
            line.pop(); // drop '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if let Some(rest) = line.strip_prefix("event:") {
                event = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            } else if line.is_empty() {
                flush(&mut event, &mut data_lines, &mut on_frame);
            }
        }
    }
    flush(&mut event, &mut data_lines, &mut on_frame);
    Ok(())
}

/// Streaming accumulator shared by `parse_gemini_body` (JSON array/object) and the
/// live `streamGenerateContent?alt=sse` path. One `MessagePart::Text` per chunk text,
/// deterministic `call_<name>_<n>` ids per candidate, and `usageMetadata` merging.
#[derive(Default)]
struct GeminiAccumulator {
    parts: Vec<MessagePart>,
    deltas: Vec<StreamDelta>,
    usage: TokenUsage,
}

impl GeminiAccumulator {
    fn push_chunk(&mut self, chunk: &Value, sink: Option<&StreamDeltaSink>) {
        if let Some(u) = chunk.get("usageMetadata") {
            self.usage.input_tokens = u
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(self.usage.input_tokens);
            self.usage.cached_input_tokens = u
                .get("cachedContentTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(self.usage.cached_input_tokens);
            self.usage.output_tokens = u
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(self.usage.output_tokens);
            self.usage.reasoning_tokens = u
                .get("thoughtsTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(self.usage.reasoning_tokens);
        }
        let Some(candidates) = chunk.get("candidates").and_then(Value::as_array) else {
            return;
        };
        for candidate in candidates {
            let Some(content_parts) = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            let mut function_counter = 0usize;
            for p in content_parts {
                if let Some(txt) = p.get("text").and_then(Value::as_str)
                    && !txt.is_empty()
                {
                    emit_delta(
                        sink,
                        &mut self.deltas,
                        StreamDelta::TextDelta(txt.to_string()),
                    );
                    self.parts.push(MessagePart::Text {
                        text: txt.to_string(),
                        cache_breakpoint: false,
                    });
                } else if let Some(fc) = p.get("functionCall") {
                    let name = fc
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                    function_counter += 1;
                    self.parts.push(MessagePart::ToolCall {
                        call_id: format!("call_{name}_{function_counter}"),
                        name,
                        arguments_json: args.to_string(),
                    });
                }
            }
        }
    }

    fn into_response(
        self,
        model: &ModelCapabilities,
        served_by_account: &str,
    ) -> CompletionResponse {
        let mut usage = self.usage;
        usage.cost_usd = compute_cost_usd(
            model,
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens,
        );
        CompletionResponse {
            assistant_message: ProviderMessage {
                role: MessageRole::Assistant,
                parts: self.parts,
            },
            stream_deltas: self.deltas,
            usage,
            served_by_account: served_by_account.to_string(),
        }
    }
}

async fn stream_openai_responses_sse(
    response: reqwest::Response,
    provider: &ProviderDefinition,
    model: &ModelCapabilities,
    served_by_account: &str,
    sink: Option<&StreamDeltaSink>,
) -> Result<CompletionResponse, ProviderError> {
    let mut deltas: Vec<StreamDelta> = Vec::new();
    let mut completed: Option<Value> = None;
    let provider_id = provider.id.clone();
    drain_sse_frames(response, &provider_id, |_event, data| {
        if data.trim() == "[DONE]" {
            return;
        }
        let Ok(ev) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        match ev.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                if let Some(d) = ev.get("delta").and_then(Value::as_str) {
                    emit_delta(sink, &mut deltas, StreamDelta::TextDelta(d.to_string()));
                }
            }
            Some("response.reasoning_summary_text.delta") => {
                if let Some(d) = ev.get("delta").and_then(Value::as_str) {
                    emit_delta(
                        sink,
                        &mut deltas,
                        StreamDelta::ReasoningSummaryDelta(d.to_string()),
                    );
                }
            }
            Some("response.completed") | Some("response.incomplete") => {
                if let Some(resp_obj) = ev.get("response") {
                    completed = Some(resp_obj.clone());
                }
            }
            _ => {}
        }
    })
    .await?;

    let resp_obj = completed.ok_or_else(|| ProviderError::Decode {
        provider: model.provider.clone(),
        message: "missing `response.completed` event in `/responses` SSE stream".to_string(),
    })?;
    extract_responses_object(&resp_obj, model, served_by_account, deltas)
}

async fn stream_chat_completions_sse(
    response: reqwest::Response,
    provider: &ProviderDefinition,
    model: &ModelCapabilities,
    served_by_account: &str,
    sink: Option<&StreamDeltaSink>,
) -> Result<CompletionResponse, ProviderError> {
    let mut deltas: Vec<StreamDelta> = Vec::new();
    let mut text = String::new();
    // (call id, function name, concatenated argument fragments) keyed by chunk index.
    let mut tools: BTreeMap<u64, (Option<String>, String, String)> = BTreeMap::new();
    let mut usage_val: Option<Value> = None;
    let provider_id = provider.id.clone();
    drain_sse_frames(response, &provider_id, |_event, data| {
        let trimmed = data.trim();
        if trimmed == "[DONE]" {
            return;
        }
        let Ok(ev) = serde_json::from_str::<Value>(trimmed) else {
            return;
        };
        if let Some(u) = ev.get("usage").filter(|u| !u.is_null()) {
            usage_val = Some(u.clone());
        }
        let Some(delta) = ev
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("delta"))
        else {
            return;
        };
        if let Some(d) = delta.get("content").and_then(Value::as_str)
            && !d.is_empty()
        {
            text.push_str(d);
            emit_delta(sink, &mut deltas, StreamDelta::TextDelta(d.to_string()));
        }
        if let Some(d) = delta.get("reasoning_content").and_then(Value::as_str)
            && !d.is_empty()
        {
            emit_delta(
                sink,
                &mut deltas,
                StreamDelta::ReasoningSummaryDelta(d.to_string()),
            );
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
                let entry = tools.entry(idx).or_default();
                if let Some(id) = tc.get("id").and_then(Value::as_str) {
                    entry.0 = Some(id.to_string());
                }
                if let Some(func) = tc.get("function") {
                    if let Some(n) = func.get("name").and_then(Value::as_str)
                        && entry.1.is_empty()
                    {
                        entry.1 = n.to_string();
                    }
                    if let Some(a) = func.get("arguments").and_then(Value::as_str) {
                        entry.2.push_str(a);
                    }
                }
            }
        }
    })
    .await?;

    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(MessagePart::Text {
            text,
            cache_breakpoint: false,
        });
    }
    for (idx, (call_id, name, arguments_json)) in tools {
        parts.push(MessagePart::ToolCall {
            call_id: call_id.unwrap_or_else(|| format!("call_{idx}")),
            name,
            arguments_json,
        });
    }
    let mut usage = usage_val
        .as_ref()
        .map(extract_chat_completions_usage)
        .unwrap_or_default();
    usage.cost_usd = compute_cost_usd(
        model,
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.output_tokens,
    );
    Ok(CompletionResponse {
        assistant_message: ProviderMessage {
            role: MessageRole::Assistant,
            parts,
        },
        stream_deltas: deltas,
        usage,
        served_by_account: served_by_account.to_string(),
    })
}

#[derive(Default)]
struct AnthropicBlockAcc {
    block_type: String,
    id: String,
    name: String,
    text: String,
    input_json: String,
    signature: String,
}

async fn stream_anthropic_messages_sse(
    response: reqwest::Response,
    provider: &ProviderDefinition,
    model: &ModelCapabilities,
    served_by_account: &str,
    sink: Option<&StreamDeltaSink>,
) -> Result<CompletionResponse, ProviderError> {
    let mut blocks: BTreeMap<u64, AnthropicBlockAcc> = BTreeMap::new();
    let mut deltas: Vec<StreamDelta> = Vec::new();
    let mut input_tokens = 0u64;
    let mut cache_creation_tokens = 0u64;
    let mut cache_read_tokens = 0u64;
    let mut output_tokens = 0u64;
    let provider_id = provider.id.clone();
    drain_sse_frames(response, &provider_id, |_event, data| {
        let Ok(ev) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        match ev.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(u) = ev.get("message").and_then(|m| m.get("usage")) {
                    input_tokens = u
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(input_tokens);
                    cache_creation_tokens = u
                        .get("cache_creation_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(cache_creation_tokens);
                    cache_read_tokens = u
                        .get("cache_read_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(cache_read_tokens);
                }
            }
            Some("content_block_start") => {
                let idx = ev.get("index").and_then(Value::as_u64).unwrap_or(0);
                let blk = ev.get("content_block");
                let entry = blocks.entry(idx).or_default();
                entry.block_type = blk
                    .and_then(|b| b.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("text")
                    .to_string();
                entry.id = blk
                    .and_then(|b| b.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                entry.name = blk
                    .and_then(|b| b.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
            }
            Some("content_block_delta") => {
                let idx = ev.get("index").and_then(Value::as_u64).unwrap_or(0);
                let delta = ev.get("delta");
                let Some(entry) = blocks.get_mut(&idx) else {
                    return;
                };
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = delta.and_then(|d| d.get("text")).and_then(Value::as_str) {
                            entry.text.push_str(t);
                            emit_delta(sink, &mut deltas, StreamDelta::TextDelta(t.to_string()));
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(t) = delta
                            .and_then(|d| d.get("thinking"))
                            .and_then(Value::as_str)
                        {
                            entry.text.push_str(t);
                            emit_delta(
                                sink,
                                &mut deltas,
                                StreamDelta::ReasoningSummaryDelta(t.to_string()),
                            );
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(p) = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(Value::as_str)
                        {
                            entry.input_json.push_str(p);
                        }
                    }
                    Some("signature_delta") => {
                        if let Some(s) = delta
                            .and_then(|d| d.get("signature"))
                            .and_then(Value::as_str)
                        {
                            entry.signature.push_str(s);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(u) = ev.get("usage") {
                    output_tokens = u
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(output_tokens);
                }
            }
            _ => {}
        }
    })
    .await?;

    let mut parts = Vec::new();
    for (idx, blk) in blocks {
        match blk.block_type.as_str() {
            "text" => {
                if !blk.text.is_empty() {
                    parts.push(MessagePart::Text {
                        text: blk.text,
                        cache_breakpoint: false,
                    });
                }
            }
            "thinking" => {
                let synthetic_id = format!("thinking_{idx}");
                parts.push(MessagePart::Reasoning {
                    id: if blk.id.is_empty() {
                        synthetic_id
                    } else {
                        blk.id
                    },
                    summary: vec![blk.text],
                    encrypted_content: None,
                    signature: if blk.signature.is_empty() {
                        None
                    } else {
                        Some(blk.signature)
                    },
                });
            }
            "tool_use" => {
                parts.push(MessagePart::ToolCall {
                    call_id: if blk.id.is_empty() {
                        format!("toolu_{idx}")
                    } else {
                        blk.id
                    },
                    name: blk.name,
                    arguments_json: if blk.input_json.is_empty() {
                        "{}".to_string()
                    } else {
                        blk.input_json
                    },
                });
            }
            _ => {}
        }
    }

    let mut usage = TokenUsage {
        input_tokens: input_tokens
            .saturating_add(cache_creation_tokens)
            .saturating_add(cache_read_tokens),
        cached_input_tokens: cache_read_tokens,
        output_tokens,
        reasoning_tokens: 0,
        cost_usd: 0.0,
    };
    usage.cost_usd = compute_cost_usd(
        model,
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.output_tokens,
    );
    Ok(CompletionResponse {
        assistant_message: ProviderMessage {
            role: MessageRole::Assistant,
            parts,
        },
        stream_deltas: deltas,
        usage,
        served_by_account: served_by_account.to_string(),
    })
}

async fn stream_gemini_sse(
    response: reqwest::Response,
    provider: &ProviderDefinition,
    model: &ModelCapabilities,
    served_by_account: &str,
    sink: Option<&StreamDeltaSink>,
) -> Result<CompletionResponse, ProviderError> {
    let mut acc = GeminiAccumulator::default();
    let provider_id = provider.id.clone();
    drain_sse_frames(response, &provider_id, |_event, data| {
        let trimmed = data.trim();
        if trimmed.is_empty() {
            return;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            return;
        };
        acc.push_chunk(&val, sink);
    })
    .await?;
    Ok(acc.into_response(model, served_by_account))
}

#[async_trait]
impl ProviderDriver for HttpProviderDriver {
    async fn complete(
        &self,
        provider: &ProviderDefinition,
        model: &ModelCapabilities,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.send_and_parse(provider, model, request, false, None)
            .await
    }

    fn account_pool_snapshot(&self) -> AccountPool {
        HttpProviderDriver::account_pool_snapshot(self)
    }

    async fn complete_streaming(
        &self,
        provider: &ProviderDefinition,
        model: &ModelCapabilities,
        request: &CompletionRequest,
        on_delta: Option<StreamDeltaSink>,
    ) -> Result<CompletionResponse, ProviderError> {
        self.send_and_parse(provider, model, request, true, on_delta)
            .await
    }
}

impl HttpProviderDriver {
    /// Shared account-selection loop with bounded transient-failure retry.
    ///
    /// In streaming mode the request gains `stream: true` (plus
    /// `stream_options.include_usage` on Chat Completions and `?alt=sse` on
    /// Gemini); the live SSE response is handed to the per-protocol incremental
    /// decoders, with a transparent fallback to the buffered JSON parsers when a
    /// server ignores the streaming hint.
    async fn send_and_parse(
        &self,
        provider: &ProviderDefinition,
        model: &ModelCapabilities,
        request: &CompletionRequest,
        streaming: bool,
        on_delta: Option<StreamDeltaSink>,
    ) -> Result<CompletionResponse, ProviderError> {
        // Reject unsupported multimodal requests BEFORE any network I/O or account selection.
        if request_has_image(request) && !model.supports_vision {
            return Err(ProviderError::UnsupportedModality {
                model: model.spec.clone(),
                modality: "image".to_string(),
            });
        }

        loop {
            let candidate = {
                let pool = self.accounts.lock().expect("account pool lock poisoned");
                pool.select(&provider.id)?.clone()
            };

            let (mut endpoint, mut payload) = match model.protocol {
                WireProtocol::OpenAiResponses => (
                    format!("{}/responses", provider.base_url),
                    build_openai_responses_payload(model, request)?,
                ),
                WireProtocol::AnthropicMessages => (
                    format!("{}/messages", provider.base_url),
                    build_anthropic_messages_payload(model, request),
                ),
                WireProtocol::OpenAiChatCompletions => (
                    format!("{}/chat/completions", provider.base_url),
                    build_chat_completions_payload(model, request),
                ),
                WireProtocol::GeminiGenerateContent => (
                    format!(
                        "{}/models/{}:streamGenerateContent",
                        provider.base_url, model.model_id
                    ),
                    build_gemini_payload(model, request),
                ),
            };
            if streaming {
                match model.protocol {
                    WireProtocol::OpenAiResponses | WireProtocol::AnthropicMessages => {
                        payload["stream"] = json!(true);
                    }
                    WireProtocol::OpenAiChatCompletions => {
                        payload["stream"] = json!(true);
                        payload["stream_options"] = json!({ "include_usage": true });
                    }
                    WireProtocol::GeminiGenerateContent => {
                        endpoint = format!("{endpoint}?alt=sse");
                    }
                }
            }

            let outbound = OutboundHttpRequest {
                endpoint: &endpoint,
                payload: &payload,
                conversation_id: &request.conversation_id,
                streaming,
            };
            let reply = self
                .execute_request(provider, model.protocol, &candidate, &outbound)
                .await?;

            if reply.status == 429 {
                // Permanent quota exhaustion for this account (soft rate limits are
                // retried inside `execute_request`): mark and fail over to the next
                // eligible account, or surface `NoEligibleAccount` when none remain.
                {
                    let mut pool = self.accounts.lock().expect("account pool lock poisoned");
                    pool.record_quota_state(&provider.id, &candidate.name, Some(0.0), true);
                }
                continue;
            }

            if let Some(ratio) = reply.remaining_ratio {
                let mut pool = self.accounts.lock().expect("account pool lock poisoned");
                pool.record_quota_state(&provider.id, &candidate.name, Some(ratio), false);
            }

            let Some(body_text) = reply.body_text else {
                let resp = reply
                    .response
                    .expect("streaming reply carries live response");
                // A local/custom server may ignore `stream: true` and answer a
                // single JSON body: transparently fall back to the buffered parsers.
                let is_sse = reply
                    .content_type
                    .as_deref()
                    .map(|ct| ct.to_ascii_lowercase().contains("text/event-stream"))
                    .unwrap_or(false);
                if !is_sse {
                    let body_text = resp.text().await.map_err(|e| ProviderError::Transport {
                        provider: provider.id.clone(),
                        message: e.to_string(),
                    })?;
                    return match model.protocol {
                        WireProtocol::OpenAiResponses => {
                            parse_openai_responses_body(&body_text, model, &candidate.name)
                        }
                        WireProtocol::GeminiGenerateContent => {
                            parse_gemini_body(&body_text, model, &candidate.name)
                        }
                        WireProtocol::AnthropicMessages | WireProtocol::OpenAiChatCompletions => {
                            parse_generic_completion(&body_text, model, &candidate.name)
                        }
                    };
                }
                return match model.protocol {
                    WireProtocol::OpenAiResponses => {
                        stream_openai_responses_sse(
                            resp,
                            provider,
                            model,
                            &candidate.name,
                            on_delta.as_ref(),
                        )
                        .await
                    }
                    WireProtocol::OpenAiChatCompletions => {
                        stream_chat_completions_sse(
                            resp,
                            provider,
                            model,
                            &candidate.name,
                            on_delta.as_ref(),
                        )
                        .await
                    }
                    WireProtocol::AnthropicMessages => {
                        stream_anthropic_messages_sse(
                            resp,
                            provider,
                            model,
                            &candidate.name,
                            on_delta.as_ref(),
                        )
                        .await
                    }
                    WireProtocol::GeminiGenerateContent => {
                        stream_gemini_sse(resp, provider, model, &candidate.name, on_delta.as_ref())
                            .await
                    }
                };
            };

            return match model.protocol {
                WireProtocol::OpenAiResponses => {
                    parse_openai_responses_body(&body_text, model, &candidate.name)
                }
                WireProtocol::GeminiGenerateContent => {
                    parse_gemini_body(&body_text, model, &candidate.name)
                }
                WireProtocol::AnthropicMessages | WireProtocol::OpenAiChatCompletions => {
                    parse_generic_completion(&body_text, model, &candidate.name)
                }
            };
        }
    }

    /// Sends one HTTP exchange with up to [`MAX_HTTP_ATTEMPTS`] attempts over
    /// transport errors, transient `502`/`503`/`504`/`529` statuses, and
    /// temporary `429` rate limits (soft limits retry on the same account;
    /// permanent billing exhaustion is returned to the caller for failover).
    async fn execute_request(
        &self,
        provider: &ProviderDefinition,
        protocol: WireProtocol,
        candidate: &crate::auth::AccountCandidate,
        outbound: &OutboundHttpRequest<'_>,
    ) -> Result<PreparedReply, ProviderError> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let req_builder = self.compose_request(provider, protocol, candidate, outbound);
            let sent = req_builder.send().await;
            let resp = match sent {
                Ok(resp) => resp,
                Err(err) => {
                    if attempt >= MAX_HTTP_ATTEMPTS {
                        return Err(ProviderError::Transport {
                            provider: provider.id.clone(),
                            message: err.to_string(),
                        });
                    }
                    tokio::time::sleep(backoff_delay(attempt)).await;
                    continue;
                }
            };

            let status = resp.status();
            let remaining_ratio = resp
                .headers()
                .get("x-ratelimit-remaining-ratio")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<f64>().ok());
            let retry_after = parse_retry_after_seconds(resp.headers());
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);

            let retryable_status = is_retryable_status(status.as_u16());
            if retryable_status && attempt < MAX_HTTP_ATTEMPTS {
                let body_text = resp.text().await.map_err(|e| ProviderError::Transport {
                    provider: provider.id.clone(),
                    message: e.to_string(),
                })?;
                let permanent_exhaustion =
                    status.as_u16() == 429 && body_indicates_permanent_quota_exhaustion(&body_text);
                if !permanent_exhaustion {
                    tokio::time::sleep(retry_delay(attempt, retry_after)).await;
                    continue;
                }
                // Permanent quota exhaustion: let the caller mark the account and fail over.
                return Ok(PreparedReply {
                    status: status.as_u16(),
                    content_type,
                    remaining_ratio: None,
                    body_text: Some(body_text),
                    response: None,
                });
            }

            if outbound.streaming && status.is_success() {
                return Ok(PreparedReply {
                    status: status.as_u16(),
                    content_type,
                    remaining_ratio,
                    body_text: None,
                    response: Some(resp),
                });
            }

            let body_text = resp.text().await.map_err(|e| ProviderError::Transport {
                provider: provider.id.clone(),
                message: e.to_string(),
            })?;
            if !status.is_success() {
                return Err(ProviderError::HttpStatus {
                    provider: provider.id.clone(),
                    status: status.as_u16(),
                    body: body_text,
                });
            }
            return Ok(PreparedReply {
                status: status.as_u16(),
                content_type,
                remaining_ratio,
                body_text: Some(body_text),
                response: None,
            });
        }
    }

    /// Assembles an authenticated POST request for `candidate` against `endpoint`.
    fn compose_request(
        &self,
        provider: &ProviderDefinition,
        protocol: WireProtocol,
        candidate: &crate::auth::AccountCandidate,
        outbound: &OutboundHttpRequest<'_>,
    ) -> reqwest::RequestBuilder {
        let mut req_builder = self
            .http
            .post(outbound.endpoint)
            .header("session-id", outbound.conversation_id)
            .header("x-session-affinity", outbound.conversation_id);

        if protocol == WireProtocol::AnthropicMessages {
            req_builder = req_builder
                .header("x-api-key", candidate.token.expose())
                .header("anthropic-version", "2023-06-01");
        } else if protocol == WireProtocol::GeminiGenerateContent {
            req_builder = req_builder.header("x-goog-api-key", candidate.token.expose());
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
        req_builder.json(outbound.payload)
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
    let mut usage = TokenUsage::default();

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
        if let Some(u) = val.get("usage").filter(|u| !u.is_null()) {
            usage = extract_chat_completions_usage(u);
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
                Some("thinking") => {
                    let thinking_text = blk
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if !thinking_text.is_empty() {
                        deltas.push(StreamDelta::ReasoningSummaryDelta(thinking_text.clone()));
                    }
                    parts.push(MessagePart::Reasoning {
                        id: blk
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("thinking_0")
                            .to_string(),
                        summary: vec![thinking_text],
                        encrypted_content: None,
                        signature: blk
                            .get("signature")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    });
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
        if let Some(u) = val.get("usage").filter(|u| !u.is_null()) {
            usage = extract_anthropic_usage(u);
        }
    }

    usage.cost_usd = compute_cost_usd(
        model,
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.output_tokens,
    );
    Ok(CompletionResponse {
        assistant_message: ProviderMessage {
            role: MessageRole::Assistant,
            parts,
        },
        stream_deltas: deltas,
        usage,
        served_by_account: served_by_account.to_string(),
    })
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<HttpProviderDriver>();
};

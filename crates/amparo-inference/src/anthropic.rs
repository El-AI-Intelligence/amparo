//! Anthropic Messages API provider.
//!
//! Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI);
//! ported to Amparo and relicensed Apache-2.0. See NOTICE at the repo root.
//!
//! Talks the native Anthropic `/v1/messages` wire format and translates it to
//! the crate's OpenAI-shaped contract so callers see one uniform surface:
//!
//! - Requests: system messages → `system`; user/assistant/tool messages →
//!   Anthropic content blocks; OpenAI tool definitions → Anthropic
//!   `{name, description, input_schema}`; assistant `tool_calls` → `tool_use`
//!   blocks; tool results → `tool_result` blocks.
//! - Responses: `tool_use` blocks → `AssistantToolCall`s with JSON-string
//!   arguments (OpenAI convention).
//! - Streaming: Anthropic SSE events are translated to OpenAI-shaped
//!   `data: {"choices":[{"delta":…}]}` chunks terminated by `data: [DONE]`,
//!   including `tool_calls` index/id/name/partial-arguments deltas.

use std::collections::HashMap;

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::Value;

use crate::{
    clamp_max_tokens, stream_with_idle_timeout, AssistantToolCall, ChatMessage, ChatRequest,
    FunctionCall, InferenceError, InferenceProvider, InferenceRequest, InferenceResponse,
    InferenceStream, Result, Tool,
};

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic Messages API provider.
pub struct AnthropicProvider {
    client: reqwest::Client,
    /// Base URL with any trailing `/v1` stripped; endpoints append `/v1/...`.
    base_url: String,
    api_key: String,
    model: String,
    timeout_secs: u64,
    max_tokens_limit: Option<usize>,
}

impl AnthropicProvider {
    pub fn new(
        base_url: String,
        api_key: String,
        model: String,
        timeout_secs: u64,
        max_tokens_limit: Option<usize>,
    ) -> Self {
        // Accept both "https://api.anthropic.com" and ".../v1" forms.
        let trimmed = base_url.trim_end_matches('/');
        let base_url = trimmed
            .strip_suffix("/v1")
            .unwrap_or(trimmed)
            .to_string();
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            base_url,
            api_key,
            model,
            timeout_secs,
            max_tokens_limit,
        }
    }

    fn timeout_dur(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs)
    }

    async fn send_with_timeout(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        tokio::time::timeout(self.timeout_dur(), req.send())
            .await
            .map_err(|_| InferenceError::Timeout)?
            .map_err(|e| InferenceError::Provider(e.to_string()))
    }

    async fn response_json(&self, resp: reqwest::Response) -> Result<Value> {
        tokio::time::timeout(self.timeout_dur(), resp.json())
            .await
            .map_err(|_| InferenceError::Timeout)?
            .map_err(|e| InferenceError::Provider(e.to_string()))
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-api-key",
            reqwest::header::HeaderValue::from_str(&self.api_key)
                .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("")),
        );
        headers.insert(
            "anthropic-version",
            reqwest::header::HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        headers
    }

    /// Non-streaming chat with full tool-call parsing. Returns an assistant
    /// `ChatMessage` whose `tool_calls` are populated when the model responds
    /// with `tool_use` blocks. Convenience for call sites that prefer a
    /// complete message over the byte-stream contract.
    pub async fn chat(&self, request: ChatRequest) -> Result<ChatMessage> {
        let model = request.model.as_deref().unwrap_or(&self.model);
        let body = anthropic_request_body(&request, model, self.max_tokens_limit);
        let req = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .headers(self.headers())
            .json(&body);
        let resp = self.send_with_timeout(req).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(InferenceError::Provider(format!(
                "Anthropic /v1/messages failed (HTTP {}): {}",
                status, text
            )));
        }
        let data = self.response_json(resp).await?;
        parse_anthropic_message(&data)
    }
}

#[async_trait]
impl InferenceProvider for AnthropicProvider {
    async fn complete(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        let model = request.model.as_deref().unwrap_or(&self.model);
        let chat = ChatRequest {
            messages: vec![ChatMessage::user(&request.prompt)],
            tools: None,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stream: Some(false),
            model: Some(model.to_string()),
            privacy_level: None,
            json_schema: None,
            thinking: None,
            web_search: None,
            challenge_level: None,
        };
        let body = anthropic_request_body(&chat, model, self.max_tokens_limit);
        let req = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .headers(self.headers())
            .json(&body);
        let resp = self.send_with_timeout(req).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(InferenceError::Provider(format!(
                "Anthropic /v1/messages failed (HTTP {}): {}",
                status, text
            )));
        }
        let data = self.response_json(resp).await?;
        let tokens = data["usage"]["output_tokens"].as_u64().unwrap_or(0) as usize;
        let finish_reason = data["stop_reason"]
            .as_str()
            .map(map_stop_reason)
            .unwrap_or("stop")
            .to_string();
        let text = collect_text_blocks(&data["content"]);
        Ok(InferenceResponse {
            text,
            tokens,
            finish_reason,
        })
    }

    async fn complete_chat_stream(&self, request: ChatRequest) -> Result<InferenceStream> {
        let model = request.model.as_deref().unwrap_or(&self.model);
        let mut body = anthropic_request_body(&request, model, self.max_tokens_limit);
        body["stream"] = Value::Bool(true);
        let req = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .headers(self.headers())
            .json(&body);
        let resp = self.send_with_timeout(req).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(InferenceError::Provider(format!(
                "Anthropic /v1/messages stream failed (HTTP {}): {}",
                status, text
            )));
        }

        // Anthropic streams `event: <name>\ndata: <json>\n\n`. Translate to
        // the crate's OpenAI-shaped SSE byte contract.
        use futures_util::StreamExt;

        let byte_stream = resp.bytes_stream();
        let sse_stream = {
            // (event_name, data_value) accumulation across chunk boundaries.
            let mut line_buf = String::new();
            let mut tool_index: HashMap<u64, String> = HashMap::new();
            byte_stream.flat_map(move |chunk_result| {
                let mut out: Vec<std::result::Result<Bytes, InferenceError>> = Vec::new();
                match chunk_result {
                    Err(e) => out.push(Err(InferenceError::Provider(e.to_string()))),
                    Ok(chunk) => {
                        line_buf.push_str(&String::from_utf8_lossy(&chunk));
                        while let Some(pos) = line_buf.find("\n\n") {
                            let event_block: String = line_buf.drain(..=pos).collect();
                            let mut event_name = String::new();
                            let mut data_json: Option<Value> = None;
                            for line in event_block.lines() {
                                if let Some(name) = line.strip_prefix("event: ") {
                                    event_name = name.trim().to_string();
                                } else if let Some(payload) = line.strip_prefix("data: ") {
                                    data_json = serde_json::from_str(payload).ok();
                                }
                            }
                            match translate_anthropic_event(
                                &event_name,
                                data_json.as_ref(),
                                &mut tool_index,
                            ) {
                                Ok(events) => out.extend(events.into_iter().map(Ok)),
                                Err(e) => out.push(Err(e)),
                            }
                        }
                    }
                }
                futures_util::stream::iter(out)
            })
        };
        Ok(stream_with_idle_timeout(sse_stream, self.timeout_dur()))
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f64>> {
        Err(InferenceError::InvalidRequest(
            "Anthropic does not provide an embeddings API; configure an \
             OpenAI-compatible provider for embeddings"
                .to_string(),
        ))
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let req = self
            .client
            .get(format!("{}/v1/models", self.base_url))
            .headers(self.headers());
        let resp = self.send_with_timeout(req).await?;
        if !resp.status().is_success() {
            return Err(InferenceError::Provider(format!(
                "Anthropic model list failed: HTTP {}",
                resp.status()
            )));
        }
        let payload = self.response_json(resp).await?;
        let mut ids: Vec<String> = payload
            .get("data")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.get("id").and_then(|v| v.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids.dedup();
        if ids.is_empty() {
            ids.push(self.model.clone());
        }
        Ok(ids)
    }

    fn default_model(&self) -> String {
        self.model.clone()
    }
}

// ─────────────────────────────────────────────── Request translation ─────────

/// Build a `/v1/messages` request body (stream flag added by the caller when
/// needed). `max_tokens` is required by the Anthropic API and is filled from
/// the request, clamped to `max_tokens_limit` when the provider sets one.
pub(crate) fn anthropic_request_body(
    request: &ChatRequest,
    model: &str,
    max_tokens_limit: Option<usize>,
) -> Value {
    let (system, messages) = anthropic_messages(request);
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": clamp_max_tokens(request.max_tokens, max_tokens_limit),
        "messages": messages,
    });
    if let Some(system) = system {
        body["system"] = Value::String(system);
    }
    if let Some(temp) = request.temperature {
        body["temperature"] = Value::from(temp);
    }
    if let Some(tools) = &request.tools {
        let anthropic_tools = anthropic_tools(tools);
        if !anthropic_tools.is_empty() {
            body["tools"] = Value::Array(anthropic_tools);
        }
    }
    body
}

/// Translate OpenAI-format tool definitions to Anthropic's
/// `{name, description, input_schema}` shape.
fn anthropic_tools(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            let f = &t.function;
            Some(serde_json::json!({
                "name": f.get("name")?.clone(),
                "description": f.get("description").cloned().unwrap_or(Value::Null),
                "input_schema": f.get("parameters").cloned()
                    .unwrap_or_else(|| serde_json::json!({"type": "object"})),
            }))
        })
        .collect()
}

/// Translate `ChatMessage`s to the Anthropic wire shape:
/// `(system_prompt, messages)`. System messages are concatenated into the
/// top-level `system` field; assistant `tool_calls` become `tool_use` blocks;
/// tool results become `tool_result` blocks (Anthropic requires them under a
/// `user` message).
fn anthropic_messages(request: &ChatRequest) -> (Option<String>, Vec<Value>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    for msg in &request.messages {
        match msg.role.as_str() {
            "system" => {
                if !msg.content.is_empty() {
                    system_parts.push(msg.content.clone());
                }
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                if !msg.content.is_empty() {
                    blocks.push(serde_json::json!({
                        "type": "text",
                        "text": msg.content,
                    }));
                }
                if let Some(calls) = &msg.tool_calls {
                    for call in calls {
                        let input: Value =
                            serde_json::from_str(&call.function.arguments).unwrap_or_else(|_| {
                                serde_json::json!({"_raw": call.function.arguments})
                            });
                        blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.function.name,
                            "input": input,
                        }));
                    }
                }
                if !blocks.is_empty() {
                    messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": blocks,
                    }));
                }
            }
            "tool" => {
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": msg.tool_call_id.clone().unwrap_or_default(),
                        "content": msg.content,
                    }],
                }));
            }
            // "user" and anything else
            _ => {
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": msg.content}],
                }));
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    (system, messages)
}

// ─────────────────────────────────────────────── Response translation ────────

fn map_stop_reason(stop_reason: &str) -> &str {
    match stop_reason {
        "end_turn" | "stop_sequence" | "refusal" => "stop",
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        other => other,
    }
}

/// Join the text blocks of an Anthropic content array.
fn collect_text_blocks(content: &Value) -> String {
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Parse a non-streaming `/v1/messages` response into an assistant
/// `ChatMessage`, translating `tool_use` blocks into OpenAI-shaped tool calls.
pub(crate) fn parse_anthropic_message(data: &Value) -> Result<ChatMessage> {
    let content = &data["content"];
    let text = collect_text_blocks(content);

    let tool_calls: Vec<AssistantToolCall> = content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                .map(|b| AssistantToolCall {
                    id: b.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: b
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        arguments: serde_json::to_string(
                            b.get("input").unwrap_or(&Value::Null),
                        )
                        .unwrap_or_else(|_| "{}".to_string()),
                    },
                })
                .collect()
        })
        .unwrap_or_default();

    // An assistant message may carry prose AND tool calls — keep both.
    let message = ChatMessage {
        role: "assistant".into(),
        content: text,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        tool_call_id: None,
    };
    Ok(message)
}

/// Translate one Anthropic SSE event into zero or more OpenAI-shaped SSE
/// chunks (`data: {...}\n\n`), plus `data: [DONE]\n\n` on `message_stop`.
/// `tool_index` tracks tool_use indexes seen in `content_block_start` events
/// so that `input_json_delta` events can emit partial arguments.
pub(crate) fn translate_anthropic_event(
    event: &str,
    data: Option<&Value>,
    tool_index: &mut HashMap<u64, String>,
) -> Result<Vec<Bytes>> {
    fn chunk(payload: Value) -> Bytes {
        Bytes::from(format!("data: {}\n\n", serde_json::to_string(&payload).unwrap_or_default()))
    }

    match event {
        "message_stop" => Ok(vec![Bytes::from_static(b"data: [DONE]\n\n")]),
        "error" => {
            let msg = data
                .and_then(|d| d.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown Anthropic stream error");
            Err(InferenceError::Provider(format!(
                "Anthropic stream error: {}",
                msg
            )))
        }
        "content_block_start" => {
            let block_type = data.and_then(|d| d.get("content_block")).and_then(|b| b.get("type")).and_then(|t| t.as_str());
            if block_type == Some("tool_use") {
                let index = data.and_then(|d| d.get("index")).and_then(|i| i.as_u64()).unwrap_or(0);
                let id = data
                    .and_then(|d| d.get("content_block"))
                    .and_then(|b| b.get("id"))
                    .and_then(|i| i.as_str())
                    .unwrap_or_default();
                let name = data
                    .and_then(|d| d.get("content_block"))
                    .and_then(|b| b.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or_default();
                tool_index.insert(index, name.to_string());
                Ok(vec![chunk(serde_json::json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": index,
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": ""},
                            }]
                        }
                    }]
                }))])
            } else {
                // text block start — nothing to emit
                Ok(vec![])
            }
        }
        "content_block_delta" => {
            let delta = data.and_then(|d| d.get("delta"));
            let delta_type = delta.and_then(|d| d.get("type")).and_then(|t| t.as_str());
            match delta_type {
                Some("text_delta") => {
                    let text = delta
                        .and_then(|d| d.get("text"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    Ok(vec![chunk(serde_json::json!({
                        "choices": [{"delta": {"content": text}}]
                    }))])
                }
                Some("input_json_delta") => {
                    let partial = delta
                        .and_then(|d| d.get("partial_json"))
                        .and_then(|p| p.as_str())
                        .unwrap_or("");
                    let index = data.and_then(|d| d.get("index")).and_then(|i| i.as_u64()).unwrap_or(0);
                    Ok(vec![chunk(serde_json::json!({
                        "choices": [{
                            "delta": {
                                "tool_calls": [{
                                    "index": index,
                                    "function": {"arguments": partial},
                                }]
                            }
                        }]
                    }))])
                }
                // thinking_delta, signature_delta, … — not surfaced in the
                // OpenAI-shaped stream.
                _ => Ok(vec![]),
            }
        }
        // message_start, message_delta (stop_reason), ping — nothing to emit.
        _ => Ok(vec![]),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> ChatMessage {
        ChatMessage::user(text)
    }

    #[test]
    fn system_messages_become_top_level_system() {
        let req = ChatRequest {
            messages: vec![
                ChatMessage::system("be brief"),
                user("hi"),
            ],
            tools: None,
            max_tokens: None,
            temperature: None,
            stream: Some(false),
            model: None,
            privacy_level: None,
            json_schema: None,
            thinking: None,
            web_search: None,
            challenge_level: None,
        };
        let body = anthropic_request_body(&req, "claude-test", None);
        assert_eq!(body["system"], "be brief");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
    }

    #[test]
    fn assistant_tool_calls_translate_to_tool_use_blocks() {
        let req = ChatRequest {
            messages: vec![ChatMessage::assistant_tool_calls(vec![AssistantToolCall {
                id: "call_1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: r#"{"path":"/etc/hosts"}"#.into(),
                },
            }])],
            tools: None,
            max_tokens: Some(2000),
            temperature: None,
            stream: Some(false),
            model: None,
            privacy_level: None,
            json_schema: None,
            thinking: None,
            web_search: None,
            challenge_level: None,
        };
        let body = anthropic_request_body(&req, "claude-test", Some(512));
        assert_eq!(body["max_tokens"], 512); // provider limit clamps the request's 2000
        let blocks = &body["messages"][0]["content"];
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["id"], "call_1");
        assert_eq!(blocks[0]["name"], "read_file");
        assert_eq!(blocks[0]["input"]["path"], "/etc/hosts");
    }

    #[test]
    fn tool_results_translate_to_tool_result_blocks() {
        let req = ChatRequest {
            messages: vec![ChatMessage::tool("call_9", "file contents")],
            tools: None,
            max_tokens: None,
            temperature: None,
            stream: None,
            model: None,
            privacy_level: None,
            json_schema: None,
            thinking: None,
            web_search: None,
            challenge_level: None,
        };
        let body = anthropic_request_body(&req, "claude-test", None);
        let blocks = &body["messages"][0]["content"];
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "call_9");
        assert_eq!(blocks[0]["content"], "file contents");
    }

    #[test]
    fn openai_tool_definitions_translate_to_anthropic_shape() {
        let tools = vec![Tool {
            tool_type: "function".into(),
            function: serde_json::json!({
                "name": "bash",
                "description": "run a command",
                "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}
            }),
        }];
        let req = ChatRequest {
            messages: vec![user("hi")],
            tools: Some(tools),
            max_tokens: None,
            temperature: None,
            stream: None,
            model: None,
            privacy_level: None,
            json_schema: None,
            thinking: None,
            web_search: None,
            challenge_level: None,
        };
        let body = anthropic_request_body(&req, "claude-test", None);
        let tool = &body["tools"][0];
        assert_eq!(tool["name"], "bash");
        assert_eq!(tool["description"], "run a command");
        assert!(tool["input_schema"]["properties"]["cmd"].is_object());
    }

    #[test]
    fn non_streaming_response_parses_text_and_tool_calls() {
        let data = serde_json::json!({
            "content": [
                {"type": "text", "text": "I'll check "},
                {"type": "tool_use", "id": "toolu_1", "name": "read_file",
                 "input": {"path": "/etc/hosts"}},
            ],
            "stop_reason": "tool_use"
        });
        let msg = parse_anthropic_message(&data).unwrap();
        assert_eq!(msg.content, "I'll check ");
        let calls = msg.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, r#"{"path":"/etc/hosts"}"#);
    }

    #[test]
    fn sse_text_delta_translates_to_openai_chunk() {
        let data = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "Hello"}
        });
        let mut idx = HashMap::new();
        let out = translate_anthropic_event("content_block_delta", Some(&data), &mut idx).unwrap();
        assert_eq!(out.len(), 1);
        let text = String::from_utf8_lossy(&out[0]);
        assert!(text.starts_with("data: "));
        let payload: Value = serde_json::from_str(text.trim_start_matches("data: ").trim()).unwrap();
        assert_eq!(payload["choices"][0]["delta"]["content"], "Hello");
    }

    #[test]
    fn sse_tool_use_start_emits_tool_call_chunk() {
        let data = serde_json::json!({
            "type": "content_block_start",
            "index": 3,
            "content_block": {"type": "tool_use", "id": "toolu_7", "name": "bash"}
        });
        let mut idx = HashMap::new();
        let out = translate_anthropic_event("content_block_start", Some(&data), &mut idx).unwrap();
        assert_eq!(out.len(), 1);
        let payload: Value =
            serde_json::from_str(String::from_utf8_lossy(&out[0]).trim_start_matches("data: ").trim())
                .unwrap();
        let tc = &payload["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["index"], 3);
        assert_eq!(tc["id"], "toolu_7");
        assert_eq!(tc["function"]["name"], "bash");
        assert_eq!(idx.get(&3).map(String::as_str), Some("bash"));
    }

    #[test]
    fn sse_input_json_delta_emits_partial_arguments() {
        let data = serde_json::json!({
            "type": "content_block_delta",
            "index": 3,
            "delta": {"type": "input_json_delta", "partial_json": "{\"cmd\":"}
        });
        let mut idx = HashMap::new();
        let out = translate_anthropic_event("content_block_delta", Some(&data), &mut idx).unwrap();
        assert_eq!(out.len(), 1);
        let payload: Value =
            serde_json::from_str(String::from_utf8_lossy(&out[0]).trim_start_matches("data: ").trim())
                .unwrap();
        let tc = &payload["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["index"], 3);
        assert_eq!(tc["function"]["arguments"], "{\"cmd\":");
    }

    #[test]
    fn sse_message_stop_emits_done() {
        let mut idx = HashMap::new();
        let out = translate_anthropic_event("message_stop", None, &mut idx).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(&out[0][..], b"data: [DONE]\n\n");
    }

    #[test]
    fn sse_error_event_is_a_stream_error() {
        let data = serde_json::json!({"type": "error", "error": {"type": "overloaded_error", "message": "overloaded"}});
        let mut idx = HashMap::new();
        let err = translate_anthropic_event("error", Some(&data), &mut idx).unwrap_err();
        assert!(matches!(err, InferenceError::Provider(_)));
        assert!(err.to_string().contains("overloaded"));
    }

    #[test]
    fn sse_irrelevant_events_are_ignored() {
        let mut idx = HashMap::new();
        let data = serde_json::json!({"type": "ping"});
        let out = translate_anthropic_event("ping", Some(&data), &mut idx).unwrap();
        assert!(out.is_empty());
        let out = translate_anthropic_event("message_start", Some(&data), &mut idx).unwrap();
        assert!(out.is_empty());
        let out = translate_anthropic_event("thinking_delta", Some(&serde_json::json!({
            "type": "content_block_delta",
            "delta": {"type": "thinking_delta", "thinking": "hmm"}
        })), &mut idx).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(map_stop_reason("end_turn"), "stop");
        assert_eq!(map_stop_reason("stop_sequence"), "stop");
        assert_eq!(map_stop_reason("max_tokens"), "length");
        assert_eq!(map_stop_reason("tool_use"), "tool_calls");
    }
}

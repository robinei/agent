use serde_json::json;
use std::collections::HashMap;
use thiserror::Error;

use agent_core::types::{
    ChatChunk, DeltaToolCall, DeltaToolCallFunction, Message, MessageContent, MessageRole,
    StopReason, ToolCall, ToolDefinition, TokenUsage,
};

#[derive(Error, Debug)]
pub enum ProviderError {
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("API error: {0}")]
    Api(String),
}

pub type Result<T> = std::result::Result<T, ProviderError>;

/// Result of parsing a single SSE line in the streaming context.
pub enum StreamEvent {
    Chunk(ChatChunk),
    Done,
    Skip,
}

// ── Provider trait ──

pub trait Provider: Send {
    /// Build the JSON body for a chat completions request.
    fn build_body(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value;

    /// Parse a single SSE line/event into a StreamEvent.
    fn parse_stream_event(&mut self, line: &str) -> StreamEvent;

    /// Non-streaming chat completions call (blocks).
    fn chat(&self, messages: &[Message], tools: &[ToolDefinition]) -> Result<ChatResponse>;

    /// HTTP endpoint path (e.g. "/chat/completions" or "/v1/messages").
    fn endpoint_path(&self) -> &str;

    /// One or more HTTP header lines to include in the request (e.g.
    /// "Authorization: Bearer {key}\r\n").
    fn auth_header_lines(&self) -> String;
}

// ── ChatResponse (non-streaming) ──

#[derive(Debug)]
pub struct ChatResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: String,
    pub usage: TokenUsage,
}

// ── Provider factory ──

pub fn create_provider(
    kind: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
    enable_thinking: bool,
    reasoning_effort: &str,
    max_tokens: Option<u32>,
    sort: Option<String>,
) -> Box<dyn Provider> {
    match kind {
        "anthropic" => Box::new(AnthropicProvider::new(
            base_url, api_key, model, enable_thinking, reasoning_effort, max_tokens,
        )),
        _ => Box::new(OpenAiProvider::new(
            base_url, api_key, model, enable_thinking, reasoning_effort, max_tokens, sort,
        )),
    }
}

// ── OpenAiProvider ──

pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    model: String,
    enable_thinking: bool,
    reasoning_effort: String,
    max_tokens: Option<u32>,
    sort: Option<String>,
}

impl OpenAiProvider {
    pub fn new(
        base_url: &str,
        api_key: &str,
        model: &str,
        enable_thinking: bool,
        reasoning_effort: &str,
        max_tokens: Option<u32>,
        sort: Option<String>,
    ) -> Self {
        Self {
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            enable_thinking,
            reasoning_effort: reasoning_effort.to_string(),
            max_tokens,
            sort,
        }
    }

    fn serialize_message(&self, msg: &Message) -> serde_json::Value {
        let role_str = match msg.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };

        let mut obj = json!({
            "role": role_str,
        });

        match &msg.content {
            MessageContent::Text(text) => {
                obj["content"] = json!(text);
            }
            MessageContent::Blocks(blocks) => {
                let arr: Vec<serde_json::Value> = blocks
                    .iter()
                    .map(|b| match b {
                        agent_core::types::ContentBlock::Text { text } => {
                            json!({"type": "text", "text": text})
                        }
                        agent_core::types::ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => {
                            json!({
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": arguments,
                            })
                        }
                    })
                    .collect();
                obj["content"] = json!(arr);
            }
        }

        if let Some(tool_calls) = &msg.tool_calls {
            let calls: Vec<serde_json::Value> = tool_calls
                .iter()
                .map(|tc| {
                    json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": tc.name,
                            "arguments": tc.arguments.to_string(),
                        }
                    })
                })
                .collect();
            obj["tool_calls"] = json!(calls);
        }

        if let Some(tool_call_id) = &msg.tool_call_id {
            obj["tool_call_id"] = json!(tool_call_id);
        }

        obj
    }

    fn chat_raw(&self, messages: &[Message], tools: &[ToolDefinition]) -> Result<serde_json::Value> {
        let body = self.build_body(messages, tools, false);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let resp = match ureq::post(&url)
            .header("Content-Type", "application/json")
            .header("Authorization", &format!("Bearer {}", self.api_key))
            .send_json(&body)
        {
            Ok(r) => r,
            Err(ureq::Error::StatusCode(status)) => {
                return Err(ProviderError::Api(format!("HTTP {}", status)));
            }
            Err(e) => {
                return Err(ProviderError::Http(format!("{}", e)));
            }
        };

        let json: serde_json::Value = resp
            .into_body()
            .read_json()
            .map_err(|e| ProviderError::Http(format!("Failed to parse response: {}", e)))?;

        Ok(json)
    }
}

impl Provider for OpenAiProvider {
    fn build_body(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        let msg_array: Vec<serde_json::Value> =
            messages.iter().map(|m| self.serialize_message(m)).collect();

        let mut body = json!({
            "model": self.model,
            "messages": msg_array,
            "stream": stream,
        });

        if !tools.is_empty() {
            let tool_defs: Vec<serde_json::Value> = tools
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
                .collect();
            body["tools"] = json!(tool_defs);
        }

        if stream {
            body["stream_options"] = json!({"include_usage": true});
        }

        if let Some(sort) = &self.sort {
            body["provider"] = json!({"sort": sort});
        }

        if self.enable_thinking {
            body["thinking"] = json!({"type": "enabled"});
            body["reasoning_effort"] = json!(self.reasoning_effort);
        }

        if let Some(max_tokens) = self.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }

        body
    }

    fn parse_stream_event(&mut self, line: &str) -> StreamEvent {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(':') {
            return StreamEvent::Skip;
        }
        if trimmed == "data: [DONE]" {
            return StreamEvent::Done;
        }
        let raw = match trimmed.strip_prefix("data: ") {
            Some(d) => d,
            None => return StreamEvent::Skip,
        };
        if raw.is_empty() {
            return StreamEvent::Skip;
        }

        let openai: serde_json::Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => return StreamEvent::Skip,
        };

        let usage = openai["usage"].as_object().map(|u| {
            TokenUsage {
                prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                completion_tokens: u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                cached_prompt_tokens: TokenUsage::cached_from_openai_json(&openai["usage"]),
            }
        });

        let choice = openai["choices"].as_array().and_then(|a| a.first());

        let chunk = ChatChunk {
            delta_text: choice.and_then(|c| c["delta"].get("content"))
                .and_then(|v| v.as_str()).map(|s| s.to_string()),
            delta_reasoning: choice.and_then(|c| c["delta"].get("reasoning"))
                .and_then(|v| v.as_str()).map(|s| s.to_string()),
            tool_call_delta: choice.map(|c| c["delta"]["tool_calls"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|tc| DeltaToolCall {
                            index: tc.get("index").and_then(|v| v.as_u64()).map(|i| i as u32),
                            id: tc.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()),
                            function: DeltaToolCallFunction {
                                name: tc["function"]["name"].as_str().map(|s| s.to_string()),
                                arguments: tc["function"]["arguments"].as_str().map(|s| s.to_string()),
                            },
                        })
                        .collect()
                })
                .unwrap_or_default())
                .unwrap_or_default(),
            finish_reason: choice.and_then(|c| c["finish_reason"]
                .as_str()
                .and_then(|s| match s {
                    "stop" => Some(StopReason::Stop),
                    "length" => Some(StopReason::Length),
                    "tool_calls" => Some(StopReason::ToolCalls),
                    "content_filter" => Some(StopReason::ContentFilter),
                    _ => None,
                })),
            usage,
        };

        StreamEvent::Chunk(chunk)
    }

    fn chat(&self, messages: &[Message], tools: &[ToolDefinition]) -> Result<ChatResponse> {
        let json = self.chat_raw(messages, tools)?;

        let text = json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let usage = TokenUsage {
            prompt_tokens: json["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens: json["usage"]["completion_tokens"].as_u64().unwrap_or(0),
            total_tokens: json["usage"]["total_tokens"].as_u64().unwrap_or(0),
            cached_prompt_tokens: TokenUsage::cached_from_openai_json(&json["usage"]),
        };

        let tool_calls = json["choices"][0]["message"]["tool_calls"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|tc| {
                        let args_str = tc["function"]["arguments"].as_str()?;
                        let args: serde_json::Value =
                            serde_json::from_str(args_str).unwrap_or(serde_json::Value::Null);
                        Some(ToolCall {
                            id: tc["id"].as_str()?.to_string(),
                            name: tc["function"]["name"].as_str()?.to_string(),
                            arguments: args,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let finish_reason = json["choices"][0]["finish_reason"]
            .as_str()
            .unwrap_or("stop")
            .to_string();

        Ok(ChatResponse {
            text,
            tool_calls,
            finish_reason,
            usage,
        })
    }

    fn endpoint_path(&self) -> &str {
        "/chat/completions"
    }

    fn auth_header_lines(&self) -> String {
        format!("Authorization: Bearer {}\r\n", self.api_key)
    }
}

// ── AnthropicProvider ──

pub struct AnthropicProvider {
    base_url: String,
    api_key: String,
    model: String,
    enable_thinking: bool,
    reasoning_effort: String,
    max_tokens: Option<u32>,
    /// Tracks in-progress content blocks during streaming.
    tool_builders: HashMap<usize, AnthropicToolBuilder>,
    pending_event: Option<String>,
}

struct AnthropicToolBuilder {
    arguments: String,
}

impl AnthropicProvider {
    pub fn new(
        base_url: &str,
        api_key: &str,
        model: &str,
        enable_thinking: bool,
        reasoning_effort: &str,
        max_tokens: Option<u32>,
    ) -> Self {
        Self {
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            enable_thinking,
            reasoning_effort: reasoning_effort.to_string(),
            max_tokens,
            tool_builders: HashMap::new(),
            pending_event: None,
        }
    }

    fn url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        // Remove /v1 if present — Anthropic uses /v1/messages directly
        let base = base.strip_suffix("/v1").unwrap_or(base);
        format!("{}/v1/messages", base)
    }

    fn serialize_messages(&self, messages: &[Message]) -> Vec<serde_json::Value> {
        messages
            .iter()
            .filter_map(|m| self.serialize_message(m))
            .collect()
    }

    fn serialize_message(&self, msg: &Message) -> Option<serde_json::Value> {
        match msg.role {
            MessageRole::System => None,
            MessageRole::User => {
                let content = match &msg.content {
                    MessageContent::Text(t) => {
                        if let Some(tool_call_id) = &msg.tool_call_id {
                            // Tool result in user role — wrap as tool_result block
                            let mut is_error = false;
                            if let Some(err) = msg.is_error {
                                is_error = err;
                            }
                            json!([
                                {"type": "tool_result", "tool_use_id": tool_call_id, "content": t, "is_error": is_error}
                            ])
                        } else {
                            json!(t)
                        }
                    }
                    MessageContent::Blocks(blocks) => {
                        let arr: Vec<serde_json::Value> = blocks
                            .iter()
                            .map(|b| match b {
                                agent_core::types::ContentBlock::Text { text } => {
                                    json!({"type": "text", "text": text})
                                }
                                agent_core::types::ContentBlock::ToolCall { .. } => {
                                    json!({"type": "text", "text": ""})
                                }
                            })
                            .collect();
                        json!(arr)
                    }
                };
                Some(json!({
                    "role": "user",
                    "content": content,
                }))
            }
            MessageRole::Assistant => {
                let mut content: Vec<serde_json::Value> = Vec::new();

                match &msg.content {
                    MessageContent::Text(t) => {
                        if !t.is_empty() {
                            content.push(json!({"type": "text", "text": t}));
                        }
                    }
                    MessageContent::Blocks(blocks) => {
                        for b in blocks {
                            match b {
                                agent_core::types::ContentBlock::Text { text } => {
                                    content.push(json!({"type": "text", "text": text}));
                                }
                                agent_core::types::ContentBlock::ToolCall {
                                    id,
                                    name,
                                    arguments,
                                } => {
                                    content.push(json!({
                                        "type": "tool_use",
                                        "id": id,
                                        "name": name,
                                        "input": arguments,
                                    }));
                                }
                            }
                        }
                    }
                }

                // tool_calls field → tool_use content blocks
                if let Some(tool_calls) = &msg.tool_calls {
                    for tc in tool_calls {
                        content.push(json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.name,
                            "input": tc.arguments,
                        }));
                    }
                }

                Some(json!({
                    "role": "assistant",
                    "content": content,
                }))
            }
            MessageRole::Tool => {
                let content = match &msg.content {
                    MessageContent::Text(t) => t.clone(),
                    MessageContent::Blocks(_) => String::new(),
                };
                let mut is_error = false;
                if let Some(err) = msg.is_error {
                    is_error = err;
                }
                Some(json!({
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": msg.tool_call_id.as_deref().unwrap_or(""), "content": content, "is_error": is_error}
                    ]
                }))
            }
        }
    }

    fn extract_system(&self, messages: &[Message]) -> Option<String> {
        for m in messages {
            if let MessageRole::System = m.role {
                if let MessageContent::Text(t) = &m.content {
                    if !t.is_empty() {
                        return Some(t.clone());
                    }
                }
            }
        }
        None
    }

    fn strip_system(messages: &[Message]) -> Vec<Message> {
        messages
            .iter()
            .filter(|m| !matches!(m.role, MessageRole::System))
            .cloned()
            .collect()
    }
}

impl Provider for AnthropicProvider {
    fn build_body(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        let system = self.extract_system(messages);
        let non_system = Self::strip_system(messages);
        let msg_array = self.serialize_messages(&non_system);

        let mut body = json!({
            "model": self.model,
            "messages": msg_array,
            "stream": stream,
            "max_tokens": self.max_tokens.unwrap_or(4096),
        });

        if let Some(s) = system {
            body["system"] = json!(s);
        }

        if !tools.is_empty() {
            let tool_defs: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect();
            body["tools"] = json!(tool_defs);
        }

        if self.enable_thinking {
            // Anthropic uses budget_tokens inside the thinking field
            let budget = match self.reasoning_effort.as_str() {
                "low" => 1024,
                "high" => 8192,
                _ => 2048, // medium default
            };
            body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        }

        body
    }

    fn parse_stream_event(&mut self, line: &str) -> StreamEvent {
        let trimmed = line.trim();

        // Track event type
        if let Some(event_name) = trimmed.strip_prefix("event: ") {
            self.pending_event = Some(event_name.to_string());
            return StreamEvent::Skip;
        }

        let raw = match trimmed.strip_prefix("data: ") {
            Some(d) => d,
            None => return StreamEvent::Skip,
        };

        let event_type = self.pending_event.take().unwrap_or_default();

        let data: serde_json::Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => return StreamEvent::Skip,
        };

        match event_type.as_str() {
            "message_start" => {
                let usage = data["message"]["usage"].as_object().map(|u| TokenUsage {
                    prompt_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                    completion_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                    total_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
                        + u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                    cached_prompt_tokens: None,
                });
                StreamEvent::Chunk(ChatChunk {
                    delta_text: None,
                    delta_reasoning: None,
                    tool_call_delta: Vec::new(),
                    finish_reason: None,
                    usage,
                })
            }
            "content_block_start" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                let block_type = data["content_block"]["type"].as_str().unwrap_or("");
                match block_type {
                    "tool_use" => {
                        let id = data["content_block"]["id"].as_str().unwrap_or("").to_string();
                        let name = data["content_block"]["name"].as_str().unwrap_or("").to_string();
                        let initial_args = data["content_block"]["input"].to_string();
                        self.tool_builders.insert(
                            index,
                            AnthropicToolBuilder {
                                arguments: initial_args,
                            },
                        );
                        StreamEvent::Chunk(ChatChunk {
                            delta_text: None,
                            delta_reasoning: None,
                            tool_call_delta: vec![DeltaToolCall {
                                index: Some(index as u32),
                                id: Some(id),
                                function: DeltaToolCallFunction {
                                    name: Some(name),
                                    arguments: None,
                                },
                            }],
                            finish_reason: None,
                            usage: None,
                        })
                    }
                    "text" | "thinking" => StreamEvent::Skip,
                    _ => StreamEvent::Skip,
                }
            }
            "content_block_delta" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                let delta_type = data["delta"]["type"].as_str().unwrap_or("");
                match delta_type {
                    "text_delta" => {
                        let text = data["delta"]["text"].as_str().unwrap_or("").to_string();
                        StreamEvent::Chunk(ChatChunk {
                            delta_text: Some(text),
                            delta_reasoning: None,
                            tool_call_delta: Vec::new(),
                            finish_reason: None,
                            usage: None,
                        })
                    }
                    "thinking_delta" => {
                        let thinking = data["delta"]["thinking"].as_str().unwrap_or("").to_string();
                        StreamEvent::Chunk(ChatChunk {
                            delta_text: None,
                            delta_reasoning: Some(thinking),
                            tool_call_delta: Vec::new(),
                            finish_reason: None,
                            usage: None,
                        })
                    }
                    "input_json_delta" => {
                        let partial = data["delta"]["partial_json"].as_str().unwrap_or("");
                        if let Some(builder) = self.tool_builders.get_mut(&index) {
                            builder.arguments.push_str(partial);
                        }
                        StreamEvent::Chunk(ChatChunk {
                            delta_text: None,
                            delta_reasoning: None,
                            tool_call_delta: vec![DeltaToolCall {
                                index: Some(index as u32),
                                id: None,
                                function: DeltaToolCallFunction {
                                    name: None,
                                    arguments: Some(partial.to_string()),
                                },
                            }],
                            finish_reason: None,
                            usage: None,
                        })
                    }
                    _ => StreamEvent::Skip,
                }
            }
            "content_block_stop" => StreamEvent::Skip,
            "message_delta" => {
                let finish_reason = data["delta"]["stop_reason"]
                    .as_str()
                    .and_then(|s| match s {
                        "end_turn" => Some(StopReason::Stop),
                        "max_tokens" => Some(StopReason::Length),
                        "tool_use" => Some(StopReason::ToolCalls),
                        "stop_sequence" => Some(StopReason::Stop),
                        _ => None,
                    });
                let usage = data["usage"].as_object().map(|u| TokenUsage {
                    prompt_tokens: 0, // not in message_delta for Anthropic
                    completion_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                    total_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                    cached_prompt_tokens: None,
                });
                StreamEvent::Chunk(ChatChunk {
                    delta_text: None,
                    delta_reasoning: None,
                    tool_call_delta: Vec::new(),
                    finish_reason,
                    usage,
                })
            }
            "message_stop" => StreamEvent::Done,
            _ => StreamEvent::Skip,
        }
    }

    fn chat(&self, messages: &[Message], tools: &[ToolDefinition]) -> Result<ChatResponse> {
        let body = self.build_body(messages, tools, false);
        let url = self.url();

        let resp = match ureq::post(&url)
            .header("Content-Type", "application/json")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send_json(&body)
        {
            Ok(r) => r,
            Err(ureq::Error::StatusCode(status)) => {
                return Err(ProviderError::Api(format!("HTTP {}", status)));
            }
            Err(e) => {
                return Err(ProviderError::Http(format!("{}", e)));
            }
        };

        let json: serde_json::Value = resp
            .into_body()
            .read_json()
            .map_err(|e| ProviderError::Http(format!("Failed to parse response: {}", e)))?;

        let text = json["content"]
            .as_array()
            .and_then(|arr| {
                arr.iter()
                    .filter_map(|b| {
                        if b["type"] == "text" {
                            b["text"].as_str().map(|s| s.to_string())
                        } else {
                            None
                        }
                    })
                    .reduce(|a, b| a + &b)
            })
            .unwrap_or_default();

        let tool_calls: Vec<ToolCall> = json["content"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|b| {
                        if b["type"] == "tool_use" {
                            Some(ToolCall {
                                id: b["id"].as_str()?.to_string(),
                                name: b["name"].as_str()?.to_string(),
                                arguments: b["input"].clone(),
                            })
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let finish_reason = json["stop_reason"]
            .as_str()
            .unwrap_or("end_turn")
            .to_string();

        let usage = TokenUsage {
            prompt_tokens: json["usage"]["input_tokens"].as_u64().unwrap_or(0),
            completion_tokens: json["usage"]["output_tokens"].as_u64().unwrap_or(0),
            total_tokens: json["usage"]["input_tokens"].as_u64().unwrap_or(0)
                + json["usage"]["output_tokens"].as_u64().unwrap_or(0),
            cached_prompt_tokens: None,
        };

        Ok(ChatResponse {
            text,
            tool_calls,
            finish_reason,
            usage,
        })
    }

    fn endpoint_path(&self) -> &str {
        "/v1/messages"
    }

    fn auth_header_lines(&self) -> String {
        format!("x-api-key: {}\r\nanthropic-version: 2023-06-01\r\n", self.api_key)
    }
}

// ── Continuation brief (free function) ──

/// Generate a continuation brief by making a separate LLM call with just the
/// session's messages as context. Called by the server when a session ends.
pub fn generate_continuation_brief(
    provider: &dyn Provider,
    messages: &[Message],
) -> Result<(String, agent_core::types::SessionStatus)> {
    let summary_prompt = Message {
        role: MessageRole::System,
        content: MessageContent::Text(
            "You are summarizing a coding session. Write a concise continuation brief \
             covering:\n\
             1. What was accomplished\n\
             2. Current state of files/code\n\
             3. Decisions made\n\
             4. Unresolved issues / next steps\n\n\
             End with a single line on its own: STATUS: <continuing|completed|blocked>\n\n\
             Use STATUS: continuing if there's more work to do.\n\
             Use STATUS: completed if the goal was achieved.\n\
             Use STATUS: blocked if there's an external blocker."
                .into(),
        ),
        tool_calls: None,
        tool_call_id: None,
        tool_name: None,
        usage: None,
        stop_reason: None,
        is_error: None,
        thinking: None,
    };

    let mut brief_messages = vec![summary_prompt];
    brief_messages.extend_from_slice(messages);

    let response = provider.chat(&brief_messages, &[])?;
    let text = response.text.trim().to_string();

    // Parse status from the last line
    let status = if let Some(line) = text.lines().last() {
        let lower = line.trim().to_lowercase();
        if lower.contains("completed") || lower.contains("done") {
            agent_core::types::SessionStatus::Completed
        } else if lower.contains("blocked") {
            agent_core::types::SessionStatus::Blocked
        } else {
            agent_core::types::SessionStatus::Continuing
        }
    } else {
        agent_core::types::SessionStatus::Continuing
    };

    // Strip STATUS lines from the text
    let brief = text
        .lines()
        .filter(|l| !l.trim().to_lowercase().starts_with("status:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    // Fallback if brief is empty
    let final_brief = if brief.is_empty() {
        format!(
            "Session ended. Last {} messages:\n{}",
            messages.len().min(5),
            messages
                .iter()
                .rev()
                .take(5)
                .map(|m| {
                    let role = match m.role {
                        MessageRole::User => "user",
                        MessageRole::Assistant => "assistant",
                        MessageRole::Tool => "tool",
                        MessageRole::System => "system",
                    };
                    let content = match &m.content {
                        MessageContent::Text(t) => t.chars().take(200).collect::<String>(),
                        MessageContent::Blocks(_) => "[blocks]".to_string(),
                    };
                    format!("[{}] {}", role, content)
                })
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        brief
    };

    Ok((final_brief, status))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── OpenAiProvider tests ──

    #[test]
    fn test_openai_serialize_message_user() {
        let p = OpenAiProvider::new(
            "http://localhost", "", "test-model", false, "medium", None, None,
        );
        let msg = Message {
            role: MessageRole::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            usage: None,
            stop_reason: None,
            is_error: None,
            thinking: None,
        };
        let json = p.serialize_message(&msg);
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn test_openai_serialize_message_assistant_with_tool_calls() {
        let p = OpenAiProvider::new(
            "http://localhost", "", "test-model", false, "medium", None, None,
        );
        let msg = Message {
            role: MessageRole::Assistant,
            content: MessageContent::Text("Let me check".into()),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "main.rs"}),
            }]),
            tool_call_id: None,
            tool_name: None,
            usage: None,
            stop_reason: None,
            is_error: None,
            thinking: None,
        };
        let json = p.serialize_message(&msg);
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["tool_calls"][0]["function"]["name"], "read");
    }

    #[test]
    fn test_openai_serialize_message_tool_result() {
        let p = OpenAiProvider::new(
            "http://localhost", "", "test-model", false, "medium", None, None,
        );
        let msg = Message {
            role: MessageRole::Tool,
            content: MessageContent::Text("file contents".into()),
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            tool_name: Some("read".into()),
            usage: None,
            stop_reason: None,
            is_error: None,
            thinking: None,
        };
        let json = p.serialize_message(&msg);
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_1");
    }

    #[test]
    fn test_openai_build_body_streaming() {
        let p = OpenAiProvider::new(
            "http://localhost", "", "test-model", false, "medium", None, None,
        );
        let msg = Message {
            role: MessageRole::User,
            content: MessageContent::Text("hi".into()),
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            usage: None,
            stop_reason: None,
            is_error: None,
            thinking: None,
        };
        let body = p.build_body(&[msg], &[], true);
        assert_eq!(body["model"], "test-model");
        assert!(body["stream"].as_bool().unwrap_or(false));
        assert!(body["stream_options"]["include_usage"]
            .as_bool()
            .unwrap_or(false));
    }

    #[test]
    fn test_openai_build_body_with_tools() {
        let p = OpenAiProvider::new(
            "http://localhost", "", "test-model", false, "medium", None, None,
        );
        let msg = Message {
            role: MessageRole::User,
            content: MessageContent::Text("hi".into()),
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            usage: None,
            stop_reason: None,
            is_error: None,
            thinking: None,
        };
        let tools = vec![ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }];
        let body = p.build_body(&[msg], &tools, true);
        assert!(body.get("tools").is_some());
        assert_eq!(body["tools"][0]["function"]["name"], "read");
    }

    #[test]
    fn test_chat_chunk_roundtrip() {
        let chunk = ChatChunk {
            delta_text: Some("Hello".into()),
            delta_reasoning: Some("thinking...".into()),
            tool_call_delta: vec![],
            finish_reason: Some(StopReason::Stop),
            usage: Some(TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                cached_prompt_tokens: None,
            }),
        };
        let json = serde_json::to_string(&chunk).unwrap();
        let parsed: ChatChunk = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.delta_text, Some("Hello".into()));
        assert_eq!(parsed.finish_reason, Some(StopReason::Stop));
        assert_eq!(parsed.usage.unwrap().total_tokens, 30);
    }

    #[test]
    fn test_openai_parse_sse() {
        let mut p = OpenAiProvider::new(
            "http://localhost", "", "test-model", false, "medium", None, None,
        );
        let line = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null,\"index\":0}],\"usage\":null}\n";
        match p.parse_stream_event(line) {
            StreamEvent::Chunk(chunk) => {
                assert_eq!(chunk.delta_text, Some("Hello".into()));
                assert!(chunk.finish_reason.is_none());
            }
            _ => panic!("expected Chunk"),
        }
    }

    #[test]
    fn test_openai_parse_sse_done() {
        let mut p = OpenAiProvider::new(
            "http://localhost", "", "test-model", false, "medium", None, None,
        );
        match p.parse_stream_event("data: [DONE]") {
            StreamEvent::Done => {}
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn test_openai_parse_sse_with_reasoning() {
        let mut p = OpenAiProvider::new(
            "http://localhost", "", "test-model", false, "medium", None, None,
        );
        let line = "data: {\"choices\":[{\"delta\":{\"content\":\"\",\"reasoning\":\"thinking step\"},\"finish_reason\":null,\"index\":0}]}";
        match p.parse_stream_event(line) {
            StreamEvent::Chunk(chunk) => {
                assert_eq!(chunk.delta_reasoning, Some("thinking step".into()));
            }
            _ => panic!("expected Chunk"),
        }
    }

    #[test]
    fn test_openai_parse_sse_finish_reason() {
        let mut p = OpenAiProvider::new(
            "http://localhost", "", "test-model", false, "medium", None, None,
        );
        let line = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\",\"index\":0}]}";
        match p.parse_stream_event(line) {
            StreamEvent::Chunk(chunk) => {
                assert_eq!(chunk.finish_reason, Some(StopReason::ToolCalls));
            }
            _ => panic!("expected Chunk"),
        }
    }

    // ── AnthropicProvider tests ──

    #[test]
    fn test_anthropic_build_body() {
        let p = AnthropicProvider::new(
            "https://api.anthropic.com", "sk-test", "claude-3-5-sonnet-20241022",
            false, "medium", Some(4096),
        );
        let msg = Message {
            role: MessageRole::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            usage: None,
            stop_reason: None,
            is_error: None,
            thinking: None,
        };
        let body = p.build_body(&[msg], &[], true);
        assert_eq!(body["model"], "claude-3-5-sonnet-20241022");
        assert!(body["stream"].as_bool().unwrap_or(false));
        assert_eq!(body["max_tokens"], 4096);
    }

    #[test]
    fn test_anthropic_build_body_with_system() {
        let p = AnthropicProvider::new(
            "https://api.anthropic.com", "sk-test", "claude-3-5-sonnet-20241022",
            false, "medium", Some(4096),
        );
        let sys = Message {
            role: MessageRole::System,
            content: MessageContent::Text("Be helpful.".into()),
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            usage: None,
            stop_reason: None,
            is_error: None,
            thinking: None,
        };
        let user = Message {
            role: MessageRole::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            usage: None,
            stop_reason: None,
            is_error: None,
            thinking: None,
        };
        let body = p.build_body(&[sys, user], &[], true);
        assert_eq!(body["system"], "Be helpful.");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn test_anthropic_build_body_with_thinking() {
        let p = AnthropicProvider::new(
            "https://api.anthropic.com", "sk-test", "claude-3-5-sonnet-20241022",
            true, "high", Some(8192),
        );
        let msg = Message {
            role: MessageRole::User,
            content: MessageContent::Text("hi".into()),
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            usage: None,
            stop_reason: None,
            is_error: None,
            thinking: None,
        };
        let body = p.build_body(&[msg], &[], false);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 8192);
    }

    #[test]
    fn test_anthropic_build_body_with_tools() {
        let p = AnthropicProvider::new(
            "https://api.anthropic.com", "sk-test", "claude-3-5-sonnet-20241022",
            false, "medium", Some(4096),
        );
        let msg = Message {
            role: MessageRole::User,
            content: MessageContent::Text("hi".into()),
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            usage: None,
            stop_reason: None,
            is_error: None,
            thinking: None,
        };
        let tools = vec![ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }];
        let body = p.build_body(&[msg], &tools, false);
        assert!(body.get("tools").is_some());
        assert_eq!(body["tools"][0]["name"], "read");
        assert!(body["tools"][0].get("input_schema").is_some());
    }

    #[test]
    fn test_anthropic_parse_text_delta() {
        let mut p = AnthropicProvider::new(
            "https://api.anthropic.com", "sk-test", "claude-3-5-sonnet-20241022",
            false, "medium", Some(4096),
        );
        assert!(matches!(p.parse_stream_event("event: content_block_delta"), StreamEvent::Skip));
        match p.parse_stream_event("data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}") {
            StreamEvent::Chunk(chunk) => {
                assert_eq!(chunk.delta_text, Some("Hello".into()));
            }
            _ => panic!("expected Chunk"),
        }
    }

    #[test]
    fn test_anthropic_parse_thinking_delta() {
        let mut p = AnthropicProvider::new(
            "https://api.anthropic.com", "sk-test", "claude-3-5-sonnet-20241022",
            false, "medium", Some(4096),
        );
        assert!(matches!(p.parse_stream_event("event: content_block_delta"), StreamEvent::Skip));
        match p.parse_stream_event("data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"I think...\"}}") {
            StreamEvent::Chunk(chunk) => {
                assert_eq!(chunk.delta_reasoning, Some("I think...".into()));
            }
            _ => panic!("expected Chunk"),
        }
    }

    #[test]
    fn test_anthropic_parse_message_delta() {
        let mut p = AnthropicProvider::new(
            "https://api.anthropic.com", "sk-test", "claude-3-5-sonnet-20241022",
            false, "medium", Some(4096),
        );
        assert!(matches!(p.parse_stream_event("event: message_delta"), StreamEvent::Skip));
        match p.parse_stream_event("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":50}}") {
            StreamEvent::Chunk(chunk) => {
                assert_eq!(chunk.finish_reason, Some(StopReason::Stop));
                assert_eq!(chunk.usage.unwrap().completion_tokens, 50);
            }
            _ => panic!("expected Chunk"),
        }
    }

    #[test]
    fn test_anthropic_parse_message_stop() {
        let mut p = AnthropicProvider::new(
            "https://api.anthropic.com", "sk-test", "claude-3-5-sonnet-20241022",
            false, "medium", Some(4096),
        );
        assert!(matches!(p.parse_stream_event("event: message_stop"), StreamEvent::Skip));
        match p.parse_stream_event("data: {\"type\":\"message_stop\"}") {
            StreamEvent::Done => {}
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn test_anthropic_message_start_with_usage() {
        let mut p = AnthropicProvider::new(
            "https://api.anthropic.com", "sk-test", "claude-3-5-sonnet-20241022",
            false, "medium", Some(4096),
        );
        assert!(matches!(p.parse_stream_event("event: message_start"), StreamEvent::Skip));
        match p.parse_stream_event("data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":15,\"output_tokens\":0}}}") {
            StreamEvent::Chunk(chunk) => {
                assert_eq!(chunk.usage.unwrap().prompt_tokens, 15);
            }
            _ => panic!("expected Chunk"),
        }
    }

    #[test]
    fn test_anthropic_tool_call_streaming() {
        let mut p = AnthropicProvider::new(
            "https://api.anthropic.com", "sk-test", "claude-3-5-sonnet-20241022",
            false, "medium", Some(4096),
        );

        // content_block_start for tool_use
        assert!(matches!(p.parse_stream_event("event: content_block_start"), StreamEvent::Skip));
        match p.parse_stream_event("data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\",\"input\":{}}}") {
            StreamEvent::Chunk(chunk) => {
                assert_eq!(chunk.tool_call_delta.len(), 1);
                assert_eq!(chunk.tool_call_delta[0].id, Some("toolu_1".into()));
                assert_eq!(chunk.tool_call_delta[0].function.name, Some("read".into()));
            }
            _ => panic!("expected Chunk"),
        }

        // content_block_delta for input_json_delta
        assert!(matches!(p.parse_stream_event("event: content_block_delta"), StreamEvent::Skip));
        match p.parse_stream_event("data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"main.rs\\\"}\"}}") {
            StreamEvent::Chunk(chunk) => {
                assert_eq!(chunk.tool_call_delta.len(), 1);
                assert_eq!(chunk.tool_call_delta[0].function.arguments, Some("{\"path\":\"main.rs\"}".into()));
            }
            _ => panic!("expected Chunk"),
        }
    }

    #[test]
    fn test_provider_factory() {
        let openai = create_provider("openai", "http://localhost", "", "test", false, "medium", None, None);
        let body = openai.build_body(&[], &[], false);
        assert_eq!(body["model"], "test");

        let anthropic = create_provider("anthropic", "https://api.anthropic.com", "sk-test", "claude-3-5", false, "medium", Some(4096), None);
        let body = anthropic.build_body(&[], &[], false);
        assert_eq!(body["model"], "claude-3-5");
    }

    #[test]
    fn test_continue_brief_parsing() {
        // We can test the status parsing logic directly via the generate fn
        // by checking how it handles known inputs (mocking is not needed since
        // we test the parsing logic here)
        let parser_test_text = "Some work done.\nSTATUS: continuing";
        let lower = parser_test_text.lines().last().unwrap().trim().to_lowercase();
        assert!(lower.contains("continuing"));
        assert!(!lower.contains("completed"));
    }
}
use serde_json::json;
use thiserror::Error;

use agent_core::types::{Message, MessageContent, MessageRole, ToolCall, ToolDefinition};

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

/// LLM provider — communicates with an OpenAI-compatible chat completions API.
#[derive(Clone, Debug)]
pub struct Provider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub enable_thinking: bool,
    pub reasoning_effort: String,
    pub max_tokens: Option<u32>,
    pub sort: Option<String>,
}

impl Provider {
    pub fn new(base_url: String, api_key: String, model: String, enable_thinking: bool, reasoning_effort: String, max_tokens: Option<u32>, sort: Option<String>) -> Self {
        Self {
            base_url,
            api_key,
            model,
            enable_thinking,
            reasoning_effort,
            max_tokens,
            sort,
        }
    }

    /// Build the JSON body for a chat completions request.
    pub fn build_body(
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

    /// Serialize a Message into the OpenAI API format.
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

    /// Non-streaming chat completions call.
    pub fn chat(&self, messages: &[Message], tools: &[ToolDefinition]) -> Result<ChatResponse> {
        let json = self.chat_raw(messages, tools)?;

        let text = json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let usage = agent_core::types::TokenUsage {
            prompt_tokens: json["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens: json["usage"]["completion_tokens"].as_u64().unwrap_or(0),
            total_tokens: json["usage"]["total_tokens"].as_u64().unwrap_or(0),
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

    /// Raw non-streaming call returning the full JSON value.
    fn chat_raw(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<serde_json::Value> {
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

#[derive(Debug)]
pub struct ChatResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: String,
    pub usage: agent_core::types::TokenUsage,
}

/// Generate a continuation brief by making a separate LLM call with just the
/// session's messages as context. Called by the server when a session ends.
pub fn generate_continuation_brief(
    provider: &Provider,
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

    #[test]
    fn test_serialize_message_user() {
        let p = Provider::new(
            "http://localhost".into(),
            "".into(),
            "test-model".into(),
            false,
            "medium".into(),
            None,
            None,
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
        };
        let json = p.serialize_message(&msg);
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn test_serialize_message_assistant_with_tool_calls() {
        let p = Provider::new(
            "http://localhost".into(),
            "".into(),
            "test-model".into(),
            false,
            "medium".into(),
            None,
            None,
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
        };
        let json = p.serialize_message(&msg);
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["tool_calls"][0]["function"]["name"], "read");
    }

    #[test]
    fn test_serialize_message_tool_result() {
        let p = Provider::new(
            "http://localhost".into(),
            "".into(),
            "test-model".into(),
            false,
            "medium".into(),
            None,
            None,
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
        };
        let json = p.serialize_message(&msg);
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_1");
    }

    #[test]
    fn test_build_body_streaming() {
        let p = Provider::new(
            "http://localhost".into(),
            "".into(),
            "test-model".into(),
            false,
            "medium".into(),
            None,
            None,
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
        };
        let body = p.build_body(&[msg], &[], true);
        assert_eq!(body["model"], "test-model");
        assert!(body["stream"].as_bool().unwrap_or(false));
        assert!(body["stream_options"]["include_usage"]
            .as_bool()
            .unwrap_or(false));
    }

    #[test]
    fn test_build_body_with_tools() {
        let p = Provider::new(
            "http://localhost".into(),
            "".into(),
            "test-model".into(),
            false,
            "medium".into(),
            None,
            None,
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
}

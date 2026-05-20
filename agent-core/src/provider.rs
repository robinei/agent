use log::info;
use serde_json::json;
use std::io::Read;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;
use thiserror::Error;

use crate::rpc::{LlmRequest, LlmResponse, PipeOut};
use crate::types::{ChatStream, Message, MessageContent, MessageRole, ToolDefinition, ToolCall};

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

/// Trait for LLM providers. `run_agent` is generic over this trait so it
/// works with both the real HTTP provider (server-side) and the pipe provider
/// (worker-side).
pub trait LlmProvider {
    fn stream_chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatStream>;
}

/// LLM provider — communicates with an OpenAI-compatible chat completions API.
#[derive(Clone, Debug)]
pub struct Provider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub enable_thinking: bool,
}

impl LlmProvider for Provider {
    fn stream_chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatStream> {
        // INTENTIONAL: no AGENT_TEST_STUB check here — that logic moved to
        // lifecycle.rs::handle_llm_request so the worker never sees it.
        let body = self.build_body(messages, tools, true);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        info!(
            "LLM request: {} messages, {} tools, model={}",
            messages.len(),
            tools.len(),
            self.model
        );

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

        let reader = resp.into_body().into_reader();
        Ok(ChatStream::new(reader))
    }
}

    impl Provider {
    pub fn new(base_url: String, api_key: String, model: String, enable_thinking: bool) -> Self {
        Self {
            base_url,
            api_key,
            model,
            enable_thinking,
        }
    }

    /// Build the JSON body for a chat completions request.
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

        if self.enable_thinking {
            body["thinking"] = json!({"type": "enabled"});
            body["reasoning_effort"] = json!("high");
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
                        crate::types::ContentBlock::Text { text } => {
                            json!({"type": "text", "text": text})
                        }
                        crate::types::ContentBlock::ToolCall { id, name, arguments } => {
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

        let usage = crate::types::TokenUsage {
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

#[derive(Debug)]
pub struct ChatResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: String,
    pub usage: crate::types::TokenUsage,
}

/// Generate a continuation brief by making a separate LLM call with just the
/// session's messages as context. Called by the server when a session ends.
pub fn generate_continuation_brief(
    provider: &Provider,
    messages: &[Message],
) -> Result<(String, crate::types::SessionStatus)> {
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
            crate::types::SessionStatus::Completed
        } else if lower.contains("blocked") {
            crate::types::SessionStatus::Blocked
        } else {
            crate::types::SessionStatus::Continuing
        }
    } else {
        crate::types::SessionStatus::Continuing
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

// ── ChannelReader ──
// Feeds LlmResponse::Chunk bytes into ChatStream.
// LlmResponse::Done / channel-closed => EOF (Ok(0)).
// LlmResponse::Error => io::Error::other(message).

struct ChannelReader {
    rx: mpsc::Receiver<LlmResponse>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, dst: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.buf.len() {
            let n = std::cmp::min(dst.len(), self.buf.len() - self.pos);
            dst[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            if self.pos >= self.buf.len() {
                self.buf.clear();
                self.pos = 0;
            }
            return Ok(n);
        }
        match self.rx.recv() {
            Ok(LlmResponse::Chunk { data, .. }) => {
                self.buf = data.into_bytes();
                self.pos = 0;
                let n = std::cmp::min(dst.len(), self.buf.len());
                dst[..n].copy_from_slice(&self.buf[..n]);
                self.pos = n;
                if self.pos >= self.buf.len() {
                    self.buf.clear();
                    self.pos = 0;
                }
                Ok(n)
            }
            Ok(LlmResponse::Done { .. }) => Ok(0),
            Ok(LlmResponse::Error { message, .. }) => {
                Err(std::io::Error::other(message))
            }
            Err(_) => Ok(0),
        }
    }
}

// ── PipeProvider ──
// Worker-side LLM provider that sends LlmRequest over the pipe and reads
// LlmResponse chunks back. The server proxies the actual HTTP call.

pub struct PipeProvider {
    out_tx: mpsc::Sender<String>,
    llm_register_tx: mpsc::Sender<mpsc::Sender<LlmResponse>>,
    next_id: AtomicU64,
}

impl PipeProvider {
    pub fn new(
        out_tx: mpsc::Sender<String>,
        llm_register_tx: mpsc::Sender<mpsc::Sender<LlmResponse>>,
    ) -> Self {
        Self {
            out_tx,
            llm_register_tx,
            next_id: AtomicU64::new(0),
        }
    }
}

impl LlmProvider for PipeProvider {
    fn stream_chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatStream> {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let (chunk_tx, chunk_rx) = mpsc::channel::<LlmResponse>();

        // Register the sender BEFORE sending the request so the stdin-reader
        // thread installs it before the first Chunk arrives.
        // INVARIANT: the agent loop is single-threaded — only one LlmRequest
        // is in flight at a time, so there is never more than one registered
        // sender.
        self.llm_register_tx
            .send(chunk_tx)
            .map_err(|e| ProviderError::Io(std::io::Error::other(e)))?;

        let req = PipeOut::Llm(LlmRequest {
            id,
            messages: messages.to_vec(),
            tools: tools.to_vec(),
        });
        let json = serde_json::to_string(&req)
            .map_err(ProviderError::Json)?;
        self.out_tx
            .send(json)
            .map_err(|e| ProviderError::Io(std::io::Error::other(e)))?;

        let reader = ChannelReader {
            rx: chunk_rx,
            buf: Vec::new(),
            pos: 0,
        };
        Ok(ChatStream::from_reader(Box::new(std::io::BufReader::new(reader))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_message_user() {
        let p = Provider::new("http://localhost".into(), "".into(), "test-model".into(), false);
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
        let p = Provider::new("http://localhost".into(), "".into(), "test-model".into(), false);
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
        let p = Provider::new("http://localhost".into(), "".into(), "test-model".into(), false);
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
        let p = Provider::new("http://localhost".into(), "".into(), "test-model".into(), false);
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
        assert!(body["stream_options"]["include_usage"].as_bool().unwrap_or(false));
    }

    #[test]
    fn test_channel_reader_chunk() {
        let (tx, rx) = mpsc::channel::<LlmResponse>();
        let data = "data: hello\n".to_string();
        tx.send(LlmResponse::Chunk { id: 0, data }).unwrap();
        tx.send(LlmResponse::Done { id: 0 }).unwrap();
        let mut reader = ChannelReader { rx, buf: vec![], pos: 0 };
        let mut buf = [0u8; 32];
        let n = reader.read(&mut buf).unwrap();
        let got = std::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(got, "data: hello\n");
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0); // EOF
    }

    #[test]
    fn test_channel_reader_error() {
        let (tx, rx) = mpsc::channel::<LlmResponse>();
        tx.send(LlmResponse::Error {
            id: 0,
            message: "oops".into(),
        })
        .unwrap();
        let mut reader = ChannelReader { rx, buf: vec![], pos: 0 };
        let mut buf = [0u8; 32];
        let err = reader.read(&mut buf).unwrap_err();
        assert_eq!(err.to_string(), "oops");
    }

    #[test]
    fn test_channel_reader_eof_on_done() {
        let (tx, rx) = mpsc::channel::<LlmResponse>();
        tx.send(LlmResponse::Done { id: 0 }).unwrap();
        let mut reader = ChannelReader { rx, buf: vec![], pos: 0 };
        let mut buf = [0u8; 32];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_channel_reader_eof_on_closed_channel() {
        let (tx, rx) = mpsc::channel::<LlmResponse>();
        drop(tx);
        let mut reader = ChannelReader { rx, buf: vec![], pos: 0 };
        let mut buf = [0u8; 32];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_build_body_with_tools() {
        let p = Provider::new("http://localhost".into(), "".into(), "test-model".into(), false);
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

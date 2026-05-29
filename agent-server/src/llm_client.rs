use agent_core::config::ProviderConfig;
use agent_core::rpc::{LlmRequest, LlmResponse};
use tokio_stream::StreamExt as _;
use tokio_util::io::StreamReader;

use crate::provider::{self, ChatResponse, StreamEvent};

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("HTTP {0}: {1}")]
    Status(u16, String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("provider error: {0}")]
    Provider(#[from] provider::ProviderError),
    #[error("empty response")]
    EmptyResponse,
}

/// Reqwest-based LLM client.
///
/// Replaces `LlmHandler` (~515 lines of hand-rolled TLS + chunked HTTP).
/// `reqwest` with `rustls-tls` handles TLS, HTTP, chunked transfer, and
/// connection reuse. No C deps.
#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
}

impl LlmClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(), // Arc inside; clone-cheap
        }
    }

    /// Stream a completion, sending chunks to `tx`.
    pub async fn stream_completion(
        &self,
        req: &LlmRequest,
        cfg: &ProviderConfig,
        tx: &tokio::sync::mpsc::Sender<LlmResponse>,
    ) -> Result<(), LlmError> {
        let provider = provider::create_provider(
            &cfg.kind,
            &cfg.base_url,
            &cfg.api_key,
            &cfg.model,
            cfg.enable_thinking,
            &cfg.reasoning_effort,
            cfg.max_tokens,
            cfg.sort.clone(),
        );

        let body = provider.build_body(&req.messages, &req.tools, true);
        let url = provider.url();

        let resp = self
            .http
            .post(&url)
            .headers(provider_auth_headers(cfg))
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(LlmError::Status(status.as_u16(), body_text));
        }

        let mut provider = provider; // need mut for parse_stream_event
        use tokio::io::AsyncBufReadExt;
        let mut line_buf = String::new();
        let mut reader = StreamReader::new(
            resp.bytes_stream().map(|r| r.map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, e)
            })),
        );
        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf).await {
                Ok(0) => break,  // EOF
                Ok(_) => {}
                Err(e) => return Err(LlmError::Io(e)),
            }
            let trimmed = line_buf.trim_end();
            match provider.parse_stream_event(trimmed) {
                StreamEvent::Chunk(chunk) => {
                    let data = serde_json::to_string(&chunk)?;
                    let _ = tx
                        .send(LlmResponse::Chunk {
                            id: req.id,
                            data,
                        })
                        .await;
                }
                StreamEvent::Done => {
                    let _ = tx.send(LlmResponse::Done { id: req.id }).await;
                    return Ok(());
                }
                StreamEvent::Skip => {}
            }
        }

        // If we exhaust the stream without a Done, send Done anyway
        let _ = tx.send(LlmResponse::Done { id: req.id }).await;
        Ok(())
    }

    /// Non-streaming completion. Used by `generate_continuation_brief`.
    pub async fn complete(
        &self,
        req: &LlmRequest,
        cfg: &ProviderConfig,
    ) -> Result<ChatResponse, LlmError> {
        let provider = provider::create_provider(
            &cfg.kind,
            &cfg.base_url,
            &cfg.api_key,
            &cfg.model,
            cfg.enable_thinking,
            &cfg.reasoning_effort,
            cfg.max_tokens,
            cfg.sort.clone(),
        );

        let body = provider.build_body(&req.messages, &req.tools, false);
        let url = provider.url();

        let resp = self
            .http
            .post(&url)
            .headers(provider_auth_headers(cfg))
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(LlmError::Status(status.as_u16(), body_text));
        }

        let response_json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LlmError::Http(format!("Failed to parse response: {}", e)))?;

        // Delegate to the provider for response parsing
        let chat_resp = provider.parse_chat_response(&response_json)?;
        Ok(chat_resp)
    }
}

/// Build auth header map from ProviderConfig.
fn provider_auth_headers(cfg: &ProviderConfig) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    match cfg.kind.as_str() {
        "anthropic" => {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(&cfg.api_key) {
                headers.insert("x-api-key", val);
            }
            if let Ok(val) = reqwest::header::HeaderValue::from_str("2023-06-01") {
                headers.insert("anthropic-version", val);
            }
        }
        _ => {
            let bearer = format!("Bearer {}", cfg.api_key);
            if let Ok(val) = reqwest::header::HeaderValue::from_str(&bearer) {
                headers.insert(reqwest::header::AUTHORIZATION, val);
            }
        }
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::rpc::LlmRequest;
    use agent_core::types::{Message, MessageContent, MessageRole, ToolDefinition};

    /// Helper to create a minimal config for testing.
    fn test_config() -> ProviderConfig {
        ProviderConfig {
            kind: "openai".into(),
            base_url: "http://127.0.0.1:9999".into(),
            api_key: "test-key".into(),
            model: "test-model".into(),
            enable_thinking: false,
            reasoning_effort: "medium".into(),
            max_tokens: Some(100),
            sort: None,
        }
    }

    #[tokio::test]
    async fn test_stream_completion_no_server() {
        // Without a mock server, this should fail. We just assert it returns
        // an error rather than panicking.
        let client = LlmClient::new();
        let req = LlmRequest {
            id: 1,
            messages: vec![Message {
                role: MessageRole::User,
                content: MessageContent::Text("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                usage: None,
                stop_reason: None,
                is_error: None,
                thinking: None,
            }],
            tools: vec![],
            routing_id: None,
        };
        let cfg = test_config();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);

        let result = client.stream_completion(&req, &cfg, &tx).await;
        assert!(result.is_err(), "expected error connecting to non-existent server");
    }

    #[tokio::test]
    async fn test_complete_no_server() {
        let client = LlmClient::new();
        let req = LlmRequest {
            id: 1,
            messages: vec![Message {
                role: MessageRole::User,
                content: MessageContent::Text("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                usage: None,
                stop_reason: None,
                is_error: None,
                thinking: None,
            }],
            tools: vec![],
            routing_id: None,
        };
        let cfg = test_config();
        let result = client.complete(&req, &cfg).await;
        assert!(result.is_err(), "expected error connecting to non-existent server");
    }

    #[test]
    fn test_provider_auth_headers() {
        let cfg = ProviderConfig {
            kind: "openai".into(),
            base_url: "http://localhost".into(),
            api_key: "sk-test123".into(),
            model: "gpt-4".into(),
            enable_thinking: false,
            reasoning_effort: "medium".into(),
            max_tokens: Some(1000),
            sort: None,
        };
        let headers = provider_auth_headers(&cfg);
        assert_eq!(
            headers.get(reqwest::header::AUTHORIZATION).unwrap(),
            "Bearer sk-test123"
        );

        let cfg_anth = ProviderConfig {
            kind: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            api_key: "sk-ant-test".into(),
            model: "claude-3-opus".into(),
            enable_thinking: false,
            reasoning_effort: "medium".into(),
            max_tokens: Some(1000),
            sort: None,
        };
        let headers = provider_auth_headers(&cfg_anth);
        assert_eq!(headers.get("x-api-key").unwrap(), "sk-ant-test");
        assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
    }
}

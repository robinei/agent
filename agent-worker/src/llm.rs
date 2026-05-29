//! Worker-side LLM client — hides PipeIn/PipeOut marshalling behind a
//! streaming async API.
//!
//! `WorkerLlmClient` sends `LlmRequest` to the server (via `PipeOut::Llm`)
//! and returns an `LlmStream` that yields text chunks as they arrive.
//! The main loop calls `route()` to forward inbound `LlmResponse` messages
//! to the correct in-flight stream.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use tokio::sync::mpsc;

use agent_core::rpc::{LlmRequest, LlmResponse, PipeOut};
use agent_core::types::{ChatChunk, Message, TokenUsage, ToolCall, ToolDefinition};

// ── ChatResponse ──

/// The assembled response from a streamed LLM request.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub text: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub finish_reason: String,
    pub usage: TokenUsage,
}

// ── Errors ──

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("LLM error: {0}")]
    Api(String),
    #[error("Stream ended without response")]
    NoResponse,
    #[error("Stream cancelled")]
    Cancelled,
}

impl From<String> for LlmError {
    fn from(s: String) -> Self {
        LlmError::Api(s)
    }
}

// ── WorkerLlmClient ──

/// Thread-local (not Send) LLM client that proxies through the server pipe.
///
/// INTENTIONAL: `!Send` because `pending` is `Rc<RefCell<…>>`. This is fine:
/// `WorkerLlmClient` lives inside the `block_on` main loop which holds
/// `!Send` state freely (see TOKIO.md Concurrency model).
pub struct WorkerLlmClient {
    /// Sender for PipeOut messages (written to stdout → server).
    pipe_tx: mpsc::Sender<PipeOut>,
    /// In-flight LLM requests, keyed by request id.
    pending: Rc<RefCell<HashMap<u64, mpsc::Sender<LlmResponse>>>>,
    /// Monotonic request id counter.
    next_id: Cell<u64>,
}

impl WorkerLlmClient {
    pub fn new(pipe_tx: mpsc::Sender<PipeOut>) -> Self {
        Self {
            pipe_tx,
            pending: Rc::new(RefCell::new(HashMap::new())),
            next_id: Cell::new(0),
        }
    }

    /// Start a streaming LLM request. Returns immediately with an `LlmStream`
    /// that yields text chunks as the server streams them back.
    pub fn request(
        &self,
        messages: Vec<Message>,
        tools: Vec<ToolDefinition>,
        routing: Option<String>,
    ) -> LlmStream {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);

        let (tx, rx) = mpsc::channel::<LlmResponse>(64);
        self.pending.borrow_mut().insert(id, tx);

        let req = LlmRequest {
            id,
            messages,
            tools,
            routing_id: routing,
        };

        // Send the request — ignore error (pipe closed = worker is shutting down)
        let _ = self.pipe_tx.try_send(PipeOut::Llm(req));

        LlmStream {
            id,
            rx,
            builder: Some(ResponseBuilder::new()),
            pending: Rc::downgrade(&self.pending),
        }
    }

    /// Non-streaming completion. Builds a request, drains the stream, returns
    /// the assembled `ChatResponse`. Used by auto-title and summaries.
    pub async fn complete(
        &self,
        messages: Vec<Message>,
        tools: Vec<ToolDefinition>,
    ) -> Result<ChatResponse, LlmError> {
        use futures_util::StreamExt;
        let mut stream = self.request(messages, tools, None);
        while let Some(chunk) = stream.next().await {
            let _ = chunk?; // discard text, we just need the final ChatResponse
        }
        Ok(stream.finish())
    }

    /// Route an inbound `LlmResponse` to the correct in-flight stream.
    /// Called by the main loop when it receives `PipeIn::Llm`.
    pub(crate) fn route(&self, resp: LlmResponse) {
        let id = resp.id();
        if let Some(tx) = self.pending.borrow().get(&id) {
            let _ = tx.try_send(resp);
        }
    }

    /// Remove a pending entry (called by LlmStream::drop).
    fn remove(&self, id: u64) {
        self.pending.borrow_mut().remove(&id);
    }
}

// ── ResponseBuilder ──

/// Accumulates streamed chunks into a ChatResponse.
struct ResponseBuilder {
    text: String,
    tool_calls: Vec<ToolCall>,
    finish_reason: Option<String>,
    usage: Option<TokenUsage>,
}

impl ResponseBuilder {
    fn new() -> Self {
        Self {
            text: String::new(),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
        }
    }

    fn apply_chunk(&mut self, data: &str) -> Result<(), LlmError> {
        if let Ok(chunk) = serde_json::from_str::<ChatChunk>(data) {
            if let Some(t) = chunk.delta_text {
                self.text.push_str(&t);
            }
            if let Some(reason) = chunk.finish_reason {
                self.finish_reason = Some(format!("{:?}", reason));
            }
            if let Some(u) = chunk.usage {
                self.usage = Some(u);
            }
            for delta in chunk.tool_call_delta {
                // Tool call accumulation logic
                if let Some(id) = delta.id {
                    if let Some(name) = delta.function.name {
                        self.tool_calls.push(ToolCall {
                            id,
                            name,
                            arguments: serde_json::Value::Null,
                        });
                    }
                }
                // Accumulate arguments into the last tool call
                if let Some(args) = delta.function.arguments {
                    if let Some(last) = self.tool_calls.last_mut() {
                        if let serde_json::Value::Null = last.arguments {
                            match serde_json::from_str(&args) {
                                Ok(val) => last.arguments = val,
                                Err(_) => {
                                    // Partial JSON — append as string
                                    last.arguments = serde_json::Value::String(args);
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> ChatResponse {
        let tool_calls = if self.tool_calls.is_empty() {
            None
        } else {
            Some(self.tool_calls)
        };
        ChatResponse {
            text: self.text,
            tool_calls,
            finish_reason: self.finish_reason.unwrap_or_else(|| "stop".into()),
            usage: self.usage.unwrap_or_else(|| TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                cached_prompt_tokens: None,
            }),
        }
    }
}

// ── LlmStream ──

/// A streaming LLM response that yields text chunks.
pub struct LlmStream {
    id: u64,
    rx: mpsc::Receiver<LlmResponse>,
    builder: Option<ResponseBuilder>,
    pending: std::rc::Weak<RefCell<HashMap<u64, mpsc::Sender<LlmResponse>>>>,
}

impl LlmStream {
    /// Consume the stream and return the accumulated ChatResponse.
    /// Panics if called while the stream has unconsumed chunks.
    pub fn finish(mut self) -> ChatResponse {
        self.builder
            .take()
            .expect("finish called after finish")
            .finish()
    }
}

impl futures_util::Stream for LlmStream {
    type Item = Result<String, LlmError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.rx).poll_recv(cx) {
            Poll::Ready(Some(LlmResponse::Chunk { data, .. })) => {
                if let Some(ref mut builder) = self.builder {
                    if let Err(e) = builder.apply_chunk(&data) {
                        return Poll::Ready(Some(Err(e)));
                    }
                }
                // Try to extract delta_text from the chunk for the stream item
                if let Ok(chunk) = serde_json::from_str::<agent_core::types::ChatChunk>(&data) {
                    if let Some(text) = chunk.delta_text {
                        return Poll::Ready(Some(Ok(text)));
                    }
                    // For tool calls / usage-only chunks, yield empty string
                    Poll::Ready(Some(Ok(String::new())))
                } else {
                    Poll::Ready(Some(Ok(data)))
                }
            }
            Poll::Ready(Some(LlmResponse::Done { .. })) => {
                // Stream complete — the ResponseBuilder has the final data
                Poll::Ready(None)
            }
            Poll::Ready(Some(LlmResponse::Error { message, .. })) => {
                Poll::Ready(Some(Err(LlmError::Api(message))))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for LlmStream {
    fn drop(&mut self) {
        // Clean up the pending entry so the server doesn't accumulate dead entries.
        if let Some(pending) = self.pending.upgrade() {
            pending.borrow_mut().remove(&self.id);
        }
    }
}

// HashMap imported above; needed for the pending field type
// (used in WorkerLlmClient and LlmStream)

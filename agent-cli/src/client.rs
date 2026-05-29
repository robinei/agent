//! HTTP and WebSocket client for the agent-server API.
//!
//! Uses `reqwest` for HTTP (tree CRUD) and `tokio-tungstenite` for
//! WebSocket (session streaming). Fully async — no blocking I/O.

use agent_core::rpc::WsCommand;
use agent_core::types::TreeMeta;
use tokio_tungstenite::tungstenite;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("WebSocket error: {0}")]
    Ws(String),
    #[error("{0}")]
    Other(String),
}

impl From<reqwest::Error> for ClientError {
    fn from(e: reqwest::Error) -> Self {
        ClientError::Http(e.to_string())
    }
}

pub type ClientResult<T> = std::result::Result<T, ClientError>;

/// Strip optional http(s):// prefix and trailing slash to get a clean
/// `host:port` string suitable for both HTTP and WS URL construction.
fn host_port(server: &str) -> String {
    let s = server
        .strip_prefix("http://")
        .or_else(|| server.strip_prefix("https://"))
        .unwrap_or(server)
        .trim_end_matches('/');
    s.to_string()
}

/// Build an HTTP base URL from a server address.
fn base_url(server: &str) -> String {
    let hp = host_port(server);
    format!("http://{}", hp)
}

/// Build a WebSocket URL from a server address + tree id.
fn ws_url(server: &str, tree_id: &str) -> String {
    let hp = host_port(server);
    format!("ws://{}/trees/{}/ws", hp, tree_id)
}

// ── AgentClient (HTTP CRUD) ───────────────────────────────────────────────

/// HTTP client for tree CRUD operations against the agent-server REST API.
#[derive(Clone)]
pub struct AgentClient {
    base: String,
    http: reqwest::Client,
    server: String, // original server string
}

impl AgentClient {
    pub fn new(server: &str) -> Self {
        Self {
            base: base_url(server),
            http: reqwest::Client::new(),
            server: server.to_string(),
        }
    }

    pub fn server_addr(&self) -> &str {
        &self.server
    }

    pub fn ws_url_for(&self, tree_id: &str) -> String {
        ws_url(&self.server, tree_id)
    }

    async fn get_json<T: for<'a> serde::Deserialize<'a>>(
        &self,
        path: &str,
    ) -> ClientResult<T> {
        let url = format!("{}{}", self.base, path);
        Ok(self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn post_json<T: for<'a> serde::Deserialize<'a>>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> ClientResult<T> {
        let url = format!("{}{}", self.base, path);
        Ok(self
            .http
            .post(&url)
            .json(body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn post_empty(&self, path: &str) -> ClientResult<()> {
        let url = format!("{}{}", self.base, path);
        self.http
            .post(&url)
            .json(&serde_json::json!({}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn list_trees(&self) -> ClientResult<Vec<TreeMeta>> {
        self.get_json("/trees").await
    }

    pub async fn create_tree(
        &self,
        title: Option<&str>,
        repo_path: Option<&str>,
        model: Option<&str>,
        writable: &[std::path::PathBuf],
        network: Option<bool>,
        hide: &[std::path::PathBuf],
        unhide: &[std::path::PathBuf],
    ) -> ClientResult<TreeMeta> {
        let mut body = serde_json::Map::new();
        if let Some(t) = title {
            body.insert("title".into(), serde_json::Value::String(t.to_string()));
        }
        if let Some(p) = repo_path {
            body.insert(
                "repo_path".into(),
                serde_json::Value::String(p.to_string()),
            );
        }
        if let Some(m) = model {
            body.insert("model".into(), serde_json::Value::String(m.to_string()));
        }
        let sandbox = serde_json::json!({
            "writable": writable,
            "network": network,
            "hide": hide,
            "unhide": unhide,
        });
        body.insert("sandbox".into(), sandbox);
        self.post_json("/trees", &serde_json::Value::Object(body))
            .await
    }

    pub async fn get_tree(&self, id: &str) -> ClientResult<TreeMeta> {
        self.get_json(&format!("/trees/{}", id)).await
    }

    pub async fn stop_agent(&self, tree_id: &str) -> ClientResult<()> {
        self.post_empty(&format!("/trees/{}/stop", tree_id))
            .await
    }
}

// ── AgentSession (WebSocket streaming) ────────────────────────────────────

/// A WebSocket session connected to a specific tree's WS endpoint.
/// Wraps `tokio_tungstenite::WebSocketStream` for async send/recv.
pub struct AgentSession {
    /// The underlying WebSocket stream.
    /// Made public so callers can use `futures_util::StreamExt::next()` / `SinkExt::send()`
    /// directly in `select!` loops (see `interactive.rs`).
    pub ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl AgentSession {
    /// Connect to a tree's WebSocket endpoint.
    ///
    /// `url_or_server` can be either a full `ws://` URL or a `host:port` string;
    /// in the latter case `tree_id` is used to build the path.
    pub async fn connect(url_or_server: &str, tree_id: &str) -> ClientResult<Self> {
        let url = if url_or_server.starts_with("ws://") || url_or_server.starts_with("wss://") {
            url_or_server.to_string()
        } else {
            ws_url(url_or_server, tree_id)
        };
        let (ws, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| ClientError::Ws(e.to_string()))?;
        Ok(Self { ws })
    }

    /// Send a `WsCommand::Message` to the worker.
    pub async fn send_message(&mut self, text: &str) -> ClientResult<()> {
        let cmd = WsCommand::Message {
            params: agent_core::rpc::MessageParams {
                text: text.into(),
            },
        };
        let s = serde_json::to_string(&cmd)?;
        use futures_util::SinkExt;
        self.ws
            .send(tungstenite::Message::Text(s.into()))
            .await
            .map_err(|e| ClientError::Ws(e.to_string()))?;
        Ok(())
    }

    /// Send a `WsCommand::Stop`.
    pub async fn send_stop(&mut self) -> ClientResult<()> {
        let s = serde_json::to_string(&WsCommand::Stop)?;
        use futures_util::SinkExt;
        self.ws
            .send(tungstenite::Message::Text(s.into()))
            .await
            .map_err(|e| ClientError::Ws(e.to_string()))?;
        Ok(())
    }

    /// Read the next `ServerEvent` from the WebSocket.
    /// Automatically responds to ping frames; all other non-text messages are
    /// silently skipped.
    pub async fn next_event(&mut self) -> Option<Result<agent_core::types::ServerEvent, ClientError>> {
        use futures_util::StreamExt;
        loop {
            match self.ws.next().await {
                Some(Ok(tungstenite::Message::Text(s))) => {
                    return Some(
                        serde_json::from_str(&s).map_err(|e| ClientError::Json(e)),
                    );
                }
                Some(Ok(tungstenite::Message::Ping(p))) => {
                    use futures_util::SinkExt;
                    let _ = self.ws.send(tungstenite::Message::Pong(p)).await;
                }
                Some(Ok(tungstenite::Message::Close(_))) => return None,
                Some(Err(e)) => return Some(Err(ClientError::Ws(e.to_string()))),
                _ => {}
            }
        }
    }
}
